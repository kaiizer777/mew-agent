use mew_agent::{load_config, ProviderConfig};
use mew_agent::session::SessionHandle;
use mew_agent::chat::UserMessage;
use mew_agent::chat_agent::ChatAgent;
use mew_agent::orchestrator::{
    self, acknowledge_steering, OrchestratorEvent, TurnSink,
};
use mew_agent::router::ConversationMessage;
use std::sync::Arc;
use tokio::sync::Mutex;
use tauri::{AppHandle, Manager, State, Emitter};

mod error_message;

/// Phase 1.2: real proof that mew-agent compiles in and runs.
/// Loads config.yaml from the workspace root, returns a tiny summary of
/// fields pulled from the actual parsed config — not a hardcoded string.
#[tauri::command]
fn get_config_summary() -> Result<String, String> {
    let cfg: ProviderConfig = load_config()
        .map_err(|e| error_message::for_user(&e, "load the configuration file"))?;
    Ok(format!(
        "model={} base_url={} max_iter={} browser_binary={:?}",
        cfg.opencode_zen.default_model,
        cfg.opencode_zen.base_url,
        cfg.opencode_zen.max_iterations,
        cfg.browser
            .as_ref()
            .and_then(|b| b.binary_path.as_deref())
    ))
}

#[derive(serde::Deserialize)]
pub struct FrontendMessage {
    role: String,
    content: String,
}

// Phase 3.1: Tauri managed state to hold the running agent session
struct ActiveSession {
    #[allow(dead_code)] // Will be used in Phase 6 for pause/resume via UI buttons
    handle: SessionHandle,
    tx: tokio::sync::mpsc::Sender<UserMessage>,
    /// Phase 3: the message id of the user turn that
    /// *originated* this browser session. The orchestrator's
    /// `acknowledge_steering` events need it for correlation;
    /// without it a follow-up "Got it, the agent will adjust"
    /// line would have no link back to the user turn that
    /// started the whole thing.
    originating_message_id: String,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct TaskHandle {
    pub task_id: String,
    pub todos: Vec<mew_agent::todo::Todo>,
}

/// Factory for BrowserAgentWorker in the Tauri shell.
struct TauriWorkerFactory {
    app_handle: AppHandle,
}

impl mew_agent::orchestrator::BrowserAgentFactory for TauriWorkerFactory {
    fn run_browser_task<'a>(
        &'a self,
        handoff: mew_agent::handoff::Handoff,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<mew_agent::handoff::BrowserResult>> + Send + 'a>,
    > {
        let app_handle = self.app_handle.clone();
        Box::pin(async move {
            let config = load_config()?;
            let (browser, page, handler_task, job) = mew_cdp::launch_headless(
                config.browser.as_ref().and_then(|b| b.binary_path.clone()),
                config.browser.as_ref().map(|b| b.visible_cursor).unwrap_or(false),
            ).await?;

            let transcript_dir = app_handle.path().app_data_dir().ok().map(|d| d.join("transcripts"));
            let mut agent = mew_agent::agent::Agent::new(config, &handoff.task_description, transcript_dir);
            let session_id = agent.session_id().to_string();

            let res = agent.run(&page).await;
            let _ = mew_cdp::shutdown(browser, handler_task, job).await;

            match res {
                Ok(text) => Ok(mew_agent::handoff::BrowserResult::done(
                    session_id,
                    text,
                    Vec::new(),
                    None,
                    None,
                )),
                Err(e) => Ok(mew_agent::handoff::BrowserResult::failure(
                    session_id,
                    format!("{e}"),
                    None,
                )),
            }
        })
    }
}

struct AppState {
    active_session: Arc<Mutex<Option<ActiveSession>>>,
    /// Phase 10.4: shared in-process classify cache. Re-used
    /// across all `ChatAgent::classify_cached` calls so a
    /// re-typed or re-classified identical message skips the
    /// LLM round trip. `Arc` so every spawned task can clone
    /// it cheaply.
    classify_cache: Arc<mew_agent::classify_cache::ClassifyCache>,
    /// Phase 14: long-lived worker pool for outer Planner execution.
    worker_pool: std::sync::Mutex<Option<Arc<mew_agent::worker_pool::WorkerPool>>>,
    signal_counter: std::sync::atomic::AtomicU64,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            active_session: Arc::new(Mutex::new(None)),
            classify_cache: Arc::new(mew_agent::classify_cache::ClassifyCache::default()),
            worker_pool: std::sync::Mutex::new(None),
            signal_counter: std::sync::atomic::AtomicU64::new(1),
        }
    }
}

impl AppState {
    pub fn get_or_init_worker_pool(&self, app_handle: &AppHandle) -> Arc<mew_agent::worker_pool::WorkerPool> {
        let mut lock = self.worker_pool.lock().expect("worker pool lock poisoned");
        if let Some(pool) = lock.as_ref() {
            return pool.clone();
        }

        let factory: Arc<dyn mew_agent::orchestrator::BrowserAgentFactory> =
            Arc::new(TauriWorkerFactory { app_handle: app_handle.clone() });
        let worker = mew_agent::worker::BrowserAgentWorker::new("worker-1", factory);
        let pool = Arc::new(mew_agent::worker_pool::WorkerPool::new(vec![worker]));
        *lock = Some(pool.clone());
        pool
    }

    pub fn next_signal_id(&self) -> u64 {
        self.signal_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

/// Phase 3: Tauri implementation of the orchestrator's
/// `TurnSink`. Maps each `OrchestratorEvent` to the right
/// `app.emit` call so the frontend listeners (`chat-reply`,
/// `agent-state`, `chat-task-started`, `chat-steering-ack`)
/// receive the data they expect.
///
/// The struct is `Clone`-able because the orchestrator may
/// hand a clone to a spawned task. The Tauri `AppHandle` is
/// already cheap-to-clone (it's a wrapper around an `Arc`).
#[derive(Clone)]
struct TauriSink {
    app_handle: AppHandle,
}

impl TurnSink for TauriSink {
    fn emit(&self, event: OrchestratorEvent) {
        match event {
            OrchestratorEvent::TaskStarted {
                originating_message_id,
                task_description,
            } => {
                // The frontend listens for "chat-task-started"
                // and renders a "Working on it…" system message
                // in the chat list with the task description.
                let _ = self.app_handle.emit(
                    "chat-task-started",
                    serde_json::json!({
                        "originating_message_id": originating_message_id,
                        "task_description": task_description,
                    }),
                );
            }
            OrchestratorEvent::SteeringAcknowledged {
                originating_message_id,
                text,
            } => {
                let _ = self.app_handle.emit(
                    "chat-steering-ack",
                    serde_json::json!({
                        "originating_message_id": originating_message_id,
                        "text": text,
                    }),
                );
            }
            OrchestratorEvent::BrowserResultReady {
                originating_message_id,
                ..
            } => {
                // The frontend does not need a separate event
                // for the typed Result — the synthesized
                // `ChatReply` immediately following is what
                // the chat list shows. We emit a "raw" event
                // only for tracing / debugging visibility.
                let _ = self.app_handle.emit(
                    "browser-result-ready",
                    serde_json::json!({
                        "originating_message_id": originating_message_id,
                    }),
                );
            }
            OrchestratorEvent::ChatReply {
                originating_message_id,
                text,
            } => {
                // Same topic the Phase 1.5 fix already used;
                // the frontend's existing `listen("chat-reply", ...)`
                // handler pushes the payload into the chat
                // list and appends it to history. We add the
                // `originating_message_id` as a sibling field
                // so the frontend can correlate if it wants
                // to (the current frontend ignores it).
                let _ = self.app_handle.emit(
                    "chat-reply",
                    serde_json::json!({
                        "originating_message_id": originating_message_id,
                        "text": text,
                    }),
                );
            }
            OrchestratorEvent::TaskCompleted {
                originating_message_id,
                status,
                step_count,
                summary,
            } => {
                // Phase 4: the task-lifecycle signal. The
                // frontend listens for `chat-task-completed`
                // and converts its `task_started` card in
                // place into `task_completed` (success,
                // green left-border) or `task_failed`
                // (failure, red left-border). The synthesized
                // `chat-reply` is the user-facing text; this
                // event is the structural signal.
                //
                // The status is emitted as the string
                // "Done" / "Failed" (the same spelling
                // `BrowserStatus::as_str` would return) so
                // the frontend can key on it without needing
                // to deserialize a typed enum.
                let _ = self.app_handle.emit(
                    "chat-task-completed",
                    serde_json::json!({
                        "originating_message_id": originating_message_id,
                        "status": status.as_str(),
                        "step_count": step_count,
                        "summary": summary,
                    }),
                );
            }
            OrchestratorEvent::TodoStateChanged { task_id, todo } => {
                let _ = self.app_handle.emit(
                    "todo-state-changed",
                    serde_json::json!({
                        "task_id": task_id,
                        "todo": todo,
                    }),
                );
            }
            OrchestratorEvent::TodoRejected { task_id, todo_id, evidence, reason } => {
                let mut payload = serde_json::json!({
                    "task_id": task_id,
                    "todo_id": todo_id,
                });
                if let Some(ev) = evidence {
                    payload["evidence"] = serde_json::json!({
                        "worker_signature": ev.worker_signature,
                        "planner_signature": ev.planner_signature,
                        "reason": ev.reason,
                    });
                }
                if let Some(r) = reason {
                    payload["reason"] = serde_json::Value::String(r);
                }
                let _ = self.app_handle.emit("todo-rejected", payload);
            }
        }
    }
}


#[tauri::command]
async fn send_message(
    text: String,
    history: Vec<FrontendMessage>,
    on_event: tauri::ipc::Channel<mew_agent::agent::AgentEvent>,
    app_handle: AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    // Phase 1: structured tracing around the mea-ui -> mew-agent
    // handoff boundary. This is the *single* place where a user
    // message crosses from frontend into the agent stack; every
    // Bug #2 investigation starts here. We log:
    //   * whether the message was routed to an active session
    //   * the classify() decision and the reply/task text length
    //   * the dispatch (or its failure) into run_browser_task
    // so a trace review can answer "did the result of the agent
    // loop ever come back to the chat?" without reading code.
    let handoff_span = tracing::info_span!("ui_handoff", text_len = text.len());
    let _handoff_enter = handoff_span.enter();

    // Step 3.1: Route subsequent messages to running agent if active.
    // Phase 3 also acknowledges the steering through the
    // orchestrator's `acknowledge_steering` helper so the user
    // sees a "Got it, the agent will adjust" line in the chat
    // list — the spec line "the mid-task steering channel must
    // explicitly confirm to the user that a steering message was
    // received and acted on."
    let mut session_lock = state.active_session.lock().await;
    if let Some(session) = session_lock.as_ref() {
        let msg = UserMessage {
            text: text.clone(),
            timestamp_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        // Try to send. If the channel is closed (agent loop exited but state wasn't cleared),
        // we'll get an error and fall through.
        if session.tx.send(msg).await.is_ok() {
            tracing::info!(
                event = "ui_handoff_routed",
                target = "active_session",
                "message routed to running agent"
            );
            // Phase 3: emit the steering acknowledgement via
            // the orchestrator's helper. We use the *original*
            // session's originating message id — the user is
            // steering the task that was started by that
            // earlier turn, not by this new turn.
            let sink: Arc<dyn TurnSink> =
                Arc::new(TauriSink { app_handle: app_handle.clone() });
            acknowledge_steering(
                &sink,
                &session.originating_message_id,
                &text,
            );
            return Ok(format!("[Routed to running agent] {}", text));
        } else {
            // The agent loop is dead. Clear it from state and fall through to classification.
            tracing::info!(
                event = "ui_handoff_session_dead",
                "active session channel closed; falling through to classifier"
            );
            *session_lock = None;
        }
    }

    // Release lock before async classification
    drop(session_lock);

    let cfg = load_config().map_err(|e| {
        tracing::error!(event = "ui_handoff_load_config_failed", error = %e, "load_config failed");
        error_message::for_user(&e, "load the configuration file")
    })?;

    // Build a ChatAgent once per turn. The orchestrator
    // pattern: every user message is owned by the ChatAgent
    // (it does the classification AND, on the way back, the
    // synthesis). Re-creating per turn is cheap; the
    // `classify_cache` handle is shared from the
    // `AppState` so an identical re-prompt skips the
    // network round trip on the second attempt.
    let chat_agent = ChatAgent::new(cfg.clone())
        .with_classify_cache(state.classify_cache.clone());
    let context: Vec<ConversationMessage> = history
        .into_iter()
        .map(|msg| ConversationMessage {
            role: msg.role,
            content: msg.content,
        })
        .collect();

    // Classify via the ChatAgent. The orchestrator's `run_turn`
    // would do this *and* dispatch the browser agent, but the
    // Tauri command's lifecycle (launch Chrome in a spawned
    // task, `&Page` borrowed only inside that task) doesn't
    // fit the single-call orchestrator shape. We split the
    // orchestrator's job in two:
    //
    //   1. Classify here. The reply (or "browser task")
    //      comes back synchronously.
    //   2. If `Intent::BrowserTask`, spawn a task that
    //      launches Chrome and then calls
    //      `orchestrator::run_turn` for the actual
    //      `ChatAgent -> BrowserAgent -> ChatAgent` round
    //      trip. The `&Page` is borrowed only inside that
    //      task, so the orchestrator's `'a Page` lifetime
    //      works.
    //
    // The classification half is the chat-only path; it
    // doesn't touch Chrome, so it can run inline. The browser
    // path is the one that needs a `&Page` and therefore
    // belongs in a spawned task.
    let intent = chat_agent.classify_cached(&text, &context).await.map_err(|e| {
        tracing::error!(
            event = "ui_handoff_classify_failed",
            error = %e,
            "classify() returned an error"
        );
        error_message::for_user(&e, "understand your message")
    })?;

    use mew_agent::router::Intent;
    match intent {
        Intent::Chat(reply) => {
            tracing::info!(
                event = "ui_handoff_classify",
                intent = "chat",
                reply_len = reply.len(),
                "classify() returned chat intent"
            );
            // Phase 3: the chat path also flows through the
            // sink so the frontend's `chat-reply` listener
            // pushes it into the chat list (and the
            // synchronous `invoke<string>` return value stays
            // consistent with the emitted payload — they
            // both carry the same text).
            let sink: Arc<dyn TurnSink> =
                Arc::new(TauriSink { app_handle: app_handle.clone() });
            let message_id = chat_agent.mint_message_id();
            sink.emit(OrchestratorEvent::ChatReply {
                originating_message_id: message_id,
                text: reply.clone(),
            });
            Ok(reply)
        }
        Intent::BrowserTask(task) => {
            tracing::info!(
                event = "ui_handoff_classify",
                intent = "browser_task",
                task_len = task.len(),
                "classify() returned browser_task intent; dispatching"
            );
            // Spawn the agent task. The task owns Chrome
            // (launched via `mew_cdp::launch_headless`) and
            // calls the orchestrator with the real `&Page`.
            let state_clone = state.active_session.clone();
            let app_clone = app_handle.clone();
            let cfg_clone = cfg.clone();
            let task_clone = task.clone();
            let chat_agent_clone = ChatAgent::new(cfg.clone());
            let pool_for_task = if cfg.agent.planner_enabled {
                Some(state.get_or_init_worker_pool(&app_handle))
            } else {
                None
            };

            // Phase 10.1: the spawned task needs `app_handle` for
            // both the browser-task work (moved into
            // `run_browser_task`) AND, on the failure path, for
            // emitting a user-facing chat reply. Clone the handle
            // up front so the error branch still has it.
            let app_for_error = app_clone.clone();
            tauri::async_runtime::spawn(async move {
                tracing::info!(
                    event = "ui_handoff_spawn",
                    task_len = task_clone.len(),
                    "browser task spawn entered"
                );
                if let Err(e) = run_browser_task(
                    task_clone,
                    cfg_clone,
                    state_clone,
                    pool_for_task,
                    app_clone,
                    Some(on_event),
                    chat_agent_clone,
                )
                .await
                {
                    tracing::error!(
                        event = "ui_handoff_spawn_failed",
                        error = %e,
                        "run_browser_task returned an error"
                    );
                    // Phase 10.1: push a user-facing chat message so
                    // the user sees *why* the task bailed in the
                    // chat list, not just the trace log. The full
                    // anyhow chain is still in the trace; the chat
                    // gets a plain-language line.
                    let user_msg = error_message::for_user(&e, "run the browser task");
                    let sink: Arc<dyn TurnSink> =
                        Arc::new(TauriSink { app_handle: app_for_error });
                    sink.emit(OrchestratorEvent::ChatReply {
                        originating_message_id: String::new(),
                        text: user_msg,
                    });
                } else {
                    tracing::info!(
                        event = "ui_handoff_spawn_ok",
                        "run_browser_task returned cleanly"
                    );
                }
            });

            Ok(format!("[Browser Task Started] {}", task))
        }
    }
}

async fn run_browser_task(
    task_desc: String,
    config: ProviderConfig,
    state: Arc<Mutex<Option<ActiveSession>>>,
    pool: Option<Arc<mew_agent::worker_pool::WorkerPool>>,
    app_handle: AppHandle,
    on_event: Option<tauri::ipc::Channel<mew_agent::agent::AgentEvent>>,
    chat_agent: ChatAgent,
) -> anyhow::Result<()> {
    // Phase 1: span the browser task end-to-end. The handoff
    // boundary log is in `send_message`; this span complements it
    // with the lifecycle of the spawned task itself. The "result
    // never reaches the chat" Bug #2 hypothesis is verified or
    // refuted by the events emitted at the bottom of this
    // function — see the comment at the `Ok(_)` match arm.
    let task_span = tracing::info_span!("browser_task", task_len = task_desc.len());
    let _task_enter = task_span.enter();

    // Surface minimal status ping
    let _ = app_handle.emit("agent-state", "Started");
    tracing::info!(event = "browser_task_started", "browser task running");

    // Headless launch: no OS window, no taskbar entry. The browser is
    // controlled entirely via CDP; the user sees it through the Live
    // Preview panel (screencast stream + immediate first frame below).
    let (browser, page, handler_task, job) = mew_cdp::launch_headless(
        config.browser.as_ref().and_then(|b| b.binary_path.clone()),
        config.browser.as_ref().map(|b| b.visible_cursor).unwrap_or(false),
    ).await.map_err(|e| {
        tracing::error!(
            event = "browser_task_launch_chrome_failed",
            error = %e,
            "mew_cdp::launch_headless returned an error"
        );
        anyhow::anyhow!("{}", error_message::for_user(&e, "start the browser"))
    })?;


    // ----------------------------------------------------------------
    // Phase 16.2: live preview, v2.
    //
    // Old behavior: a 500 ms `capture_screenshot` poll started
    // AFTER `launch_headless` returned, AFTER the agent loop
    // dispatched. The user saw a blank "Waiting for first frame…"
    // placeholder for 1–3 s (Chrome cold start) + up to 500 ms
    // (poll cadence) before the first frame painted. By then the
    // agent had already taken 1–2 ReAct steps and the live
    // preview was visibly "behind" the chat list.
    //
    // New behavior: as soon as Chrome is up, we (a) start a
    // push-based `Page.startScreencast` stream and (b) take one
    // synchronous `capture_screenshot` to ship a frame right now.
    // We also pre-navigate to a small inline "Loading…" data URL
    // so the first frame shows real browser chrome instead of an
    // empty white `about:blank` square.
    //
    // The screencast pump task lives for the entire browser task;
    // it exits when the `UnboundedSender` is dropped (i.e. when
    // `screencast_tx` goes out of scope at the end of this
    // function, or when `shutdown` closes the underlying browser).
    // ----------------------------------------------------------------
    let (screencast_tx, mut screencast_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, i32)>();
    let app_for_shot = app_handle.clone();

    // Pre-navigate the page to a tiny inline "Loading…"
    // document. This makes the very first frame the user sees
    // look like a real browser tab loading a page (chrome,
    // address bar mock, "Loading…" body) instead of an empty
    // white `about:blank` square. CDP `Page.navigate` is
    // fire-and-forget for our purposes — the next screencast
    // frame will pick up the rendered document.
    {
        use chromiumoxide::cdp::browser_protocol::page::NavigateParams;
        let loading_html = "data:text/html;charset=utf-8,\
<!doctype html><html><head><title>mew agent</title>\
<style>html,body{margin:0;height:100%;\
background:linear-gradient(180deg,#f8fafc 0%,#eef2ff 100%);\
font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;\
color:#1e293b;display:flex;align-items:center;justify-content:center;\
flex-direction:column}\
h1{font-weight:500;font-size:14px;letter-spacing:0.04em;margin:0 0 12px;\
color:#475569}.dot{width:8px;height:8px;border-radius:50%;background:#2563eb;\
box-shadow:0 0 0 0 rgba(37,99,235,.6);animation:ping 1.4s cubic-bezier(0,0,.2,1) infinite}\
@keyframes ping{0%{box-shadow:0 0 0 0 rgba(37,99,235,.55)}\
80%,100%{box-shadow:0 0 0 14px rgba(37,99,235,0)}}</style>\
</head><body><div class=\"dot\"></div>\
<h1>mew agent &middot; starting browser&hellip;</h1></body></html>";
        match NavigateParams::builder().url(loading_html).build() {
            Ok(nav_params) => {
                if let Err(e) = page.execute(nav_params).await {
                    tracing::warn!(
                        event = "live_preview_loading_navigate_failed",
                        error = %e,
                        "pre-navigate to loading.html failed; falling back to about:blank"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    event = "live_preview_loading_navigate_build_failed",
                    error = %e,
                    "build NavigateParams failed; falling back to about:blank"
                );
            }
        }
    }

    // Start the push-based screencast + grab a synchronous
    // first frame so the UI has something to paint the moment
    // the task returns. This runs concurrently with the agent
    // dispatch — the user sees the preview fill in *while* the
    // ReAct loop is thinking.
    let screencast_started = mew_cdp::start_screencast_with_first_frame(
        &page,
        screencast_tx.clone(),
    )
    .await
    .is_ok();

    if !screencast_started {
        tracing::warn!(
            event = "live_preview_screencast_start_failed",
            "start_screencast_with_first_frame failed; falling back to legacy 500ms poll"
        );
        // Fallback: keep the old poll-based behavior so a
        // screencast failure doesn't silently kill the preview.
        let page_for_shot = page.clone();
        let app_for_shot_fb = app_for_shot.clone();
        tauri::async_runtime::spawn(async move {
            let interval = std::time::Duration::from_millis(500);
            loop {
                tokio::time::sleep(interval).await;
                match mew_cdp::capture_screenshot(&page_for_shot).await {
                    Ok(data) => {
                        let _ = app_for_shot_fb.emit("agent-screencast-frame", data);
                    }
                    Err(e) => {
                        tracing::warn!(
                            event = "screencast_poll_stopped",
                            error = %e,
                            "screenshot poll stopped; sending a one-shot chat notice"
                        );
                        let user_msg =
                            error_message::for_user(&e, "refresh the live preview");
                        let _ = app_for_shot_fb.emit(
                            "chat-reply",
                            serde_json::json!({
                                "originating_message_id": String::new(),
                                "text": user_msg,
                            }),
                        );
                        break;
                    }
                }
            }
        });
    } else {
        // Screencast pump: forward every (jpeg_b64, device_width)
        // tuple from the channel to the frontend as
        // `agent-screencast-frame`. The first item the channel
        // yields is the synchronous one-shot screenshot, so the
        // UI gets a frame before the next animation frame even
        // if the screencast event stream takes a moment to
        // produce its first EventScreencastFrame.
        let app_for_pump = app_for_shot.clone();
        tauri::async_runtime::spawn(async move {
            while let Some((data, _device_width)) = screencast_rx.recv().await {
                let _ = app_for_pump.emit("agent-screencast-frame", data);
            }
            tracing::info!(
                event = "live_preview_screencast_pump_exit",
                "screencast pump task ended (sender dropped)"
            );
        });
    }

    // Drop our local copy of the sender so the pump task exits
    // cleanly when this function returns. The clone inside the
    // screencast task itself is the only long-lived sender.
    drop(screencast_tx);



    // Phase 4 (Bug 3 fix): resolve a transcript directory that lives
    // OUTSIDE the Tauri source tree. Tauri's dev-mode file watcher
    // (see `tauri-cli/src/interface/rust.rs::run_dev_watcher` and
    // `tauri-cli/src/interface/rust.rs::get_watch_folders`) recursively
    // watches the entire `src-tauri/` directory and treats any change
    // in it as a reason to rebuild + restart the app. Writing the
    // agent's on-disk transcript under `src-tauri/transcripts/` was
    // triggering that watcher on every iteration, which printed
    // `File ... changed. Rebuilding application...`, killed the
    // running agent and its Chrome window, and started a fresh chat
    // — the Bug 3 restart loop.
    //
    // We use Tauri's `app_data_dir()` (Tauri 2.11.x API on
    // `app.path()`) so the file lands at the OS-appropriate
    // per-user app-data location:
    //   Windows: %APPDATA%/<bundle_identifier>/transcripts/
    //   macOS:   ~/Library/Application Support/<bundle_identifier>/transcripts/
    //   Linux:   $XDG_DATA_HOME/<bundle_identifier>/transcripts/
    //
    // If the resolution fails (very unusual on desktop) we log a
    // warning and let `Agent::new` fall back to the relative
    // `transcripts/` path. The second layer of defense — a
    // `.taurignore` in `mew-ui/src-tauri/` excluding the folder
    // even if the path ever lands there again — is set up in
    // `mew-ui/src-tauri/.taurignore`.
    let transcript_dir = match app_handle.path().app_data_dir() {
        Ok(dir) => {
            let transcripts = dir.join("transcripts");
            eprintln!(
                "[mew-ui] Bug 3 fix: writing transcripts to {} (outside src-tauri/)",
                transcripts.display()
            );
            Some(transcripts)
        }
        Err(e) => {
            eprintln!(
                "[mew-ui] WARNING: app_data_dir() failed ({}); falling back to default transcript path",
                e
            );
            None
        }
    };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            if let Some(ch) = on_event.as_ref() {
                let _ = ch.send(event);
            }
        }
    });

    let mut agent = mew_agent::agent::Agent::new(config, &task_desc, transcript_dir).with_event_sender(tx);
    let chat_tx = agent.take_message_sender();
    let session_handle = agent.session_handle();
    let session_id = agent.session_id().to_string();

    // Phase 3: mint the originating message id for this turn
    // *before* we register the active session, so the
    // `acknowledge_steering` call (which receives a mid-task
    // follow-up message) can correlate back to the user turn
    // that started the task.
    let originating_message_id = chat_agent.mint_message_id();
    {
        let mut lock = state.lock().await;
        *lock = Some(ActiveSession {
            handle: session_handle,
            tx: chat_tx,
            originating_message_id: originating_message_id.clone(),
        });
    }

    // ----------------------------------------------------------------
    // Phase 3: ChatAgent -> BrowserAgent -> ChatAgent round trip.
    //
    // The Tauri command's `send_message` already classified the
    // user turn as `Intent::BrowserTask(task)` and dispatched us.
    // The orchestrator's `run_turn` would do that AND the
    // browser-agent dispatch in one call, but the `&Page` here
    // is borrowed only inside this task — we keep the
    // classification half in `send_message` (chat-only path
    // doesn't need Chrome either) and run the browser half of
    // the orchestrator's protocol manually below.
    //
    // The protocol is identical to `run_turn`'s
    // `Intent::BrowserTask` branch:
    //
    //   1. Build the typed `Handoff` from the task and the
    //      deterministic planner.
    //   2. Build the `TauriSink` (the orchestrator's
    //      `TurnSink` impl for Tauri events).
    //   3. Construct a one-off `AgentFactory` that *re-uses*
    //      the already-built `agent` (so the steering mpsc
    //      bus and session handle stay wired up).
    //   4. Call `orchestrator::run_turn` with `&page`,
    //      awaiting the synthesized chat reply.
    // ----------------------------------------------------------------
    let handoff = chat_agent.build_handoff(
        &task_desc,
        &originating_message_id,
        Vec::new(), // constraints: future work — sensitive platforms surface here
    );
    let sink: Arc<dyn orchestrator::TurnSink> = Arc::new(TauriSink {
        app_handle: app_handle.clone(),
    });
    let factory: Arc<dyn orchestrator::BrowserAgentFactory> =
        Arc::new(PrefabricatedAgentFactory {
            agent: std::sync::Mutex::new(Some(agent)),
        });

    // Emit the "task started" event before dispatching so the
    // user gets a "Working on it…" line immediately. (The
    // orchestrator's `run_turn` also emits this, but we want
    // the synchronous return from `send_message` to be paired
    // with an immediate UI event — the user is staring at the
    // chat list waiting for acknowledgement.)
    let _ = app_handle.emit("agent-state", "Started");

    // Run the round trip. This is the Phase 3 hot loop:
    // BrowserAgent runs, returns a typed `BrowserResult`,
    // ChatAgent synthesizes the user reply, sink emits
    // `chat-reply`. If the browser agent's run returns Err
    // (e.g. loop termination), the orchestrator converts to
    // a `Failed` `BrowserResult` so the user still sees a
    // chat message.
    let _reply = orchestrator::dispatch_browser_task(
        &chat_agent,
        factory,
        pool,
        sink,
        &page,
        handoff,
        &[],
    )
    .await?;

    // Clear the active session so the next message goes back
    // to the classifier.
    {
        let mut lock = state.lock().await;
        *lock = None;
    }

    let _ = app_handle.emit("agent-state", "Done");
    tracing::info!(
        event = "browser_task_result_summary",
        session_id = %session_id,
        "browser task complete; result synthesized through ChatAgent -> BrowserAgent -> ChatAgent round trip"
    );

    let _ = mew_cdp::shutdown(browser, handler_task, job).await;

    Ok(())
}

/// Phase 3: thin adapter that lets the orchestrator pattern
/// *re-use* an already-constructed `Agent` (the one whose
/// `mpsc::Sender<UserMessage>` and `SessionHandle` are already
/// registered with Tauri's `AppState` for steering).
///
/// `AgentFactory::run_browser_task` would build a fresh
/// `Agent`; in the Tauri integration we already built the
/// agent so the steering wiring is in place. This factory
/// just hands the existing agent back to the orchestrator.
///
/// The `agent` lives behind a `std::sync::Mutex<Option<...>>`
/// so the trait's `&self` method can `take()` it out and
/// move it into the async block. The lock is held for only
/// the duration of the `take`, never across `.await`.
struct PrefabricatedAgentFactory {
    agent: std::sync::Mutex<Option<mew_agent::agent::Agent>>,
}

impl orchestrator::BrowserAgentFactory for PrefabricatedAgentFactory {
    fn run_browser_task<'a>(
        &'a self,
        handoff: mew_agent::handoff::Handoff,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<mew_agent::handoff::BrowserResult>> + Send + 'a>,
    > {
        let _ = handoff;

        let mut agent = match self.agent.lock().expect("PrefabricatedAgentFactory mutex poisoned").take() {
            Some(a) => a,
            None => {
                return Box::pin(async move {
                    Err(anyhow::anyhow!(
                        "PrefabricatedAgentFactory: agent already consumed"
                    ))
                });
            }
        };
        let session_id = agent.session_id().to_string();
        Box::pin(async move {
            let config = load_config()?;
            let (browser, page, handler_task, job) = mew_cdp::launch_headless(
                config.browser.as_ref().and_then(|b| b.binary_path.clone()),
                config.browser.as_ref().map(|b| b.visible_cursor).unwrap_or(false),
            ).await?;

            let result = agent.run(&page).await;
            let _ = mew_cdp::shutdown(browser, handler_task, job).await;

            match result {
                Ok(text) => Ok(mew_agent::handoff::BrowserResult::done(
                    session_id,
                    text,
                    Vec::new(),
                    None,
                    None,
                )),
                Err(e) => Ok(mew_agent::handoff::BrowserResult::failure(
                    session_id,
                    format!("{e}"),
                    None,
                )),
            }
        })
    }
}

#[tauri::command]
async fn pause_session(state: State<'_, AppState>) -> Result<String, String> {
    let lock = state.active_session.lock().await;
    if let Some(session) = lock.as_ref() {
        if let Err(e) = session.handle.pause(None).await {
            return Err(error_message::for_std_error(&e, "pause the task"));
        }
        return Ok("Paused".to_string());
    }
    Err(error_message::for_user(
        &anyhow::anyhow!("No active session"),
        "pause the task",
    ))
}

#[tauri::command]
async fn resume_session(state: State<'_, AppState>) -> Result<String, String> {
    let lock = state.active_session.lock().await;
    if let Some(session) = lock.as_ref() {
        if let Err(e) = session.handle.resume(None).await {
            return Err(error_message::for_std_error(&e, "resume the task"));
        }
        return Ok("Resumed".to_string());
    }
    Err(error_message::for_user(
        &anyhow::anyhow!("No active session"),
        "resume the task",
    ))
}

/// Phase 14: outer planner task entry point.
#[tauri::command]
async fn start_task(
    app: AppHandle,
    state: State<'_, AppState>,
    message: String,
    history: Vec<FrontendMessage>,
) -> Result<TaskHandle, String> {
    let task_id = uuid::Uuid::new_v4().to_string();
    let cfg = load_config().map_err(|e| {
        error_message::for_user(&e, "load the configuration file")
    })?;

    let chat_agent = ChatAgent::new(cfg.clone())
        .with_classify_cache(state.classify_cache.clone());
    let context: Vec<ConversationMessage> = history
        .into_iter()
        .map(|msg| ConversationMessage {
            role: msg.role,
            content: msg.content,
        })
        .collect();

    let intent = chat_agent.classify_cached(&message, &context).await.map_err(|e| {
        error_message::for_user(&e, "understand your message")
    })?;

    let sink: Arc<dyn TurnSink> = Arc::new(TauriSink { app_handle: app.clone() });

    use mew_agent::router::Intent;
    match intent {
        Intent::Chat(reply) => {
            sink.emit(OrchestratorEvent::ChatReply {
                originating_message_id: task_id.clone(),
                text: reply,
            });
            Ok(TaskHandle {
                task_id,
                todos: Vec::new(),
            })
        }
        Intent::BrowserTask(task) => {
            if !cfg.agent.planner_enabled {
                // Fallback mode (planner_enabled: false): run single ReAct task in background
                let state_session = state.active_session.clone();
                let app_clone = app.clone();
                let cfg_clone = cfg.clone();
                let task_clone = task.clone();
                let chat_agent_clone = ChatAgent::new(cfg.clone());
                let app_for_error = app_clone.clone();

                tauri::async_runtime::spawn(async move {
                    if let Err(e) = run_browser_task(
                        task_clone,
                        cfg_clone,
                        state_session,
                        None, // legacy mode doesn't use the worker pool
                        app_clone,
                        None,
                        chat_agent_clone,
                    ).await {
                        let user_msg = error_message::for_user(&e, "run the browser task");
                        let sink_err: Arc<dyn TurnSink> = Arc::new(TauriSink { app_handle: app_for_error });
                        sink_err.emit(OrchestratorEvent::ChatReply {
                            originating_message_id: String::new(),
                            text: user_msg,
                        });
                    }
                });

                Ok(TaskHandle {
                    task_id,
                    todos: Vec::new(),
                })
            } else {
                // Planner enabled mode: decompose into typed todos and submit first todo
                let handoff = chat_agent.build_handoff(&task, &task_id, Vec::new());
                let todos = mew_agent::planner::decompose_to_todos(&handoff);

                for todo in &todos {
                    sink.emit(OrchestratorEvent::TodoStateChanged {
                        task_id: task_id.clone(),
                        todo: todo.clone(),
                    });
                }

                if let Some(first_todo) = todos.first() {
                    let pool = state.get_or_init_worker_pool(&app);
                    let _rx = pool.submit(first_todo.clone(), handoff).map_err(|e| {
                        error_message::for_user(&anyhow::anyhow!("{e:?}"), "submit subtask to planner")
                    })?;
                }

                Ok(TaskHandle {
                    task_id,
                    todos,
                })
            }
        }
    }
}

#[tauri::command]
async fn pause_todo(
    app: AppHandle,
    state: State<'_, AppState>,
    task_id: String,
    _todo_id: String,
) -> Result<(), String> {
    let pool = state.get_or_init_worker_pool(&app);
    let sig_id = state.next_signal_id();
    pool.signal(mew_agent::supervisor::SupervisorCommand::new(
        sig_id,
        mew_agent::supervisor::SupervisorSignal::Pause,
    ));
    let _ = task_id;
    Ok(())
}

#[tauri::command]
async fn resume_todo(
    app: AppHandle,
    state: State<'_, AppState>,
    task_id: String,
    _todo_id: String,
) -> Result<(), String> {
    let pool = state.get_or_init_worker_pool(&app);
    let sig_id = state.next_signal_id();
    pool.signal(mew_agent::supervisor::SupervisorCommand::new(
        sig_id,
        mew_agent::supervisor::SupervisorSignal::Resume,
    ));
    let _ = task_id;
    Ok(())
}

#[tauri::command]
async fn cancel_todo(
    app: AppHandle,
    state: State<'_, AppState>,
    task_id: String,
    todo_id: String,
) -> Result<(), String> {
    let pool = state.get_or_init_worker_pool(&app);
    let sig_id = state.next_signal_id();
    pool.signal(mew_agent::supervisor::SupervisorCommand::new(
        sig_id,
        mew_agent::supervisor::SupervisorSignal::Cancel,
    ));

    let sink: Arc<dyn TurnSink> = Arc::new(TauriSink { app_handle: app.clone() });
    sink.emit(OrchestratorEvent::TodoRejected {
        task_id,
        todo_id,
        evidence: None,
        reason: Some("cancelled by user".to_string()),
    });
    Ok(())
}

#[tauri::command]
async fn replan(
    app: AppHandle,
    state: State<'_, AppState>,
    task_id: String,
) -> Result<(), String> {
    let pool = state.get_or_init_worker_pool(&app);
    let cancel_sig_id = state.next_signal_id();
    pool.signal(mew_agent::supervisor::SupervisorCommand::new(
        cancel_sig_id,
        mew_agent::supervisor::SupervisorSignal::Cancel,
    ));

    let handoff = mew_agent::handoff::Handoff::bare("replan task", &task_id);
    let new_todos = mew_agent::planner::decompose_to_todos(&handoff);

    let replan_sig_id = state.next_signal_id();
    pool.signal(mew_agent::supervisor::SupervisorCommand::new(
        replan_sig_id,
        mew_agent::supervisor::SupervisorSignal::Replan(new_todos.clone()),
    ));

    let sink: Arc<dyn TurnSink> = Arc::new(TauriSink { app_handle: app.clone() });
    for todo in new_todos {
        sink.emit(OrchestratorEvent::TodoStateChanged {
            task_id: task_id.clone(),
            todo,
        });
    }

    Ok(())
}

/// Phase 18 ships with this; Phase 14 declares the signature only and stubs the body.
#[tauri::command]
async fn stop_task(
    _state: State<'_, AppState>,
    _task_id: String,
) -> Result<(), String> {
    Err(error_message::for_user(
        &anyhow::anyhow!("not yet implemented"),
        "stop task",
    ))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  // No `tracing` global subscriber is installed here on purpose.
  //
  // The Tauri UI is a `log`-facade consumer, not a `tracing` global
  // subscriber. `tracing` events from `mew-agent` flow through the
  // per-session JSONL layer (`mew-agent::tracing_layer`) using a
  // thread-local override that the `ChatAgent` constructor wires
  // up — that path does NOT depend on a `tracing` global. And
  // anything that uses the `log::*` macros (chromiumoxide, reqwest,
  // h2, etc.) gets forwarded to the webview console by
  // `tauri-plugin-log` below.
  //
  // An earlier version of this function called
  // `tracing_subscriber::registry().try_init()` to set a
  // `tracing-subscriber` env-filter + fmt layer as the *tracing*
  // global. That was redundant (the JSONL layer is what we
  // actually use) AND it panicked the Tauri setup hook with
  // "attempted to set a logger after the logging system was
  // already initialized" because the `tracing-subscriber` default
  // feature set pulls in `tracing-log`, and `tracing-log`'s
  // `LogTracer::init()` claims the `log::Log` global — which
  // `tauri-plugin-log` then can't claim. So we just don't do
  // either init here; the webview console + the opt-in JSONL
  // side-channel cover all the diagnostics we need.
  tauri::Builder::default()
    .manage(AppState::default())
    .invoke_handler(tauri::generate_handler![
        send_message,
        get_config_summary,
        pause_session,
        resume_session,
        start_task,
        pause_todo,
        resume_todo,
        cancel_todo,
        replan,
        stop_task
    ])
    .setup(|app| {
      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }
      // Phase 16.3: workspace-root hint for the resolver.
      //
      // `mew-nav::SensitivePlatforms::load_from_default_location`
      // and `mew-agent::load_config` both walk the parent
      // directory tree looking for the workspace root. The
      // walk works for a dev build whose CWD is somewhere
      // inside the repo (the Tauri binary runs with CWD
      // `target/debug/` for a `cargo tauri dev` build, and
      // the walk finds the file on the first parent hop).
      //
      // For a release build the binary can live somewhere
      // that has no `config/sensitive_platforms.toml` in
      // any parent at all (a Windows installer drops it
      // in `Program Files\mew-ui\`, a macOS bundle in
      // `mew-ui.app/Contents/MacOS/`). The parent walk
      // fails, the sensitive-platforms table is empty, and
      // the LLM can't reach instagram.com / twitter.com /
      // linkedin.com because direct nav to those hosts is
      // blocked by the allowlist.
      //
      // The `MEW_WORKSPACE_DIR` env var is the escape
      // hatch. We compute the most likely workspace root
      // from the executable's own path and set it once
      // here, before any agent task is created. If the env
      // var is already set (an operator's deliberate
      // override), we keep it.
      if std::env::var("MEW_WORKSPACE_DIR").is_err() {
        if let Some(workspace_root) = detect_workspace_root() {
          // SAFETY: this `set_var` runs in the main
          // thread before any agent task is spawned.
          // Tauri's setup hook is single-threaded at this
          // point — the async runtime hasn't started
          // task spawning yet. Setting an env var is a
          // documented no-panic pattern when the main
          // thread holds no locks the runtime waits on.
          std::env::set_var("MEW_WORKSPACE_DIR", &workspace_root);
          eprintln!(
            "[mew-ui] Phase 16.3: set MEW_WORKSPACE_DIR={} (inferred from exe path)",
            workspace_root
          );
        }
      }
      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}

/// Infer the workspace root from the running executable's
/// path. The binary lives at one of:
///   * `<repo>/target/debug/app.exe`        — `cargo run` /
///     `cargo tauri dev` dev build
///   * `<repo>/target/release/app.exe`      — `cargo tauri
///     build` release build
///   * `<install>/mew-ui.exe`               — MSI / NSIS
///     installer (no parent contains `Cargo.toml`)
///   * `<bundle>/mew-ui.app/Contents/MacOS/mew-ui`
///     — macOS `.app` bundle
///
/// We walk up from the binary's directory looking for a
/// `Cargo.toml` whose first non-comment line contains the
/// `[workspace]` marker. The first hit is the workspace
/// root. If no workspace root is found (release install
/// with no repo on disk), we return `None` and the
/// `MEW_WORKSPACE_DIR` env var stays unset — the agent
/// just runs without sensitive-platform routing, same as
/// the pre-fix behavior.
fn detect_workspace_root() -> Option<String> {
    use std::path::PathBuf;
    let exe = std::env::current_exe().ok()?;
    let mut dir: PathBuf = exe.parent()?.to_path_buf();
    const MAX_DEPTH: usize = 12;
    for _ in 0..MAX_DEPTH {
        let candidate = dir.join("Cargo.toml");
        if candidate.exists() {
            // Cheap check for the workspace marker.
            // We look for a `[workspace]` section header
            // — that's enough to distinguish the repo
            // root from a sub-crate's Cargo.toml
            // (e.g. `mew-ui/src-tauri/Cargo.toml`,
            // which has a `[lib]` block but not a
            // `[workspace]` block).
            if let Ok(content) = std::fs::read_to_string(&candidate) {
                if content.lines().any(|l| l.trim() == "[workspace]") {
                    return Some(dir.to_string_lossy().into_owned());
                }
            }
        }
        if !dir.pop() {
            break;
        }
    }
    None
}
