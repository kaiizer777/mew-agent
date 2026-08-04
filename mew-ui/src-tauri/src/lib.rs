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

#[derive(Default)]
struct AppState {
    active_session: Arc<Mutex<Option<ActiveSession>>>,
    /// Phase 10.4: shared in-process classify cache. Re-used
    /// across all `ChatAgent::classify_cached` calls so a
    /// re-typed or re-classified identical message skips the
    /// LLM round trip. `Arc` so every spawned task can clone
    /// it cheaply.
    classify_cache: Arc<mew_agent::classify_cache::ClassifyCache>,
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
                    app_clone,
                    on_event,
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
    app_handle: AppHandle,
    on_event: tauri::ipc::Channel<mew_agent::agent::AgentEvent>,
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
    // Preview panel (screenshot polling below).
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


    // Live Preview: poll a JPEG screenshot every 500 ms and push it to
    // the frontend via the `agent-screencast-frame` event. Using periodic
    // CDP captureScreenshot instead of Page.startScreencast because
    // startScreencast is unreliable across Chrome builds/headless modes.
    // 2 fps is plenty for watching an agent browse.
    let page_for_shot = page.clone();
    let app_for_shot = app_handle.clone();
    tauri::async_runtime::spawn(async move {
        let interval = std::time::Duration::from_millis(500);
        loop {
            tokio::time::sleep(interval).await;
            match mew_cdp::capture_screenshot(&page_for_shot).await {
                Ok(data) => {
                    let _ = app_for_shot.emit("agent-screencast-frame", data);
                }
                Err(e) => {
                    // Phase 10.1: the screencast popover goes quiet
                    // on a CDP screenshot error (the page is gone,
                    // the browser is gone, etc.). The chat task is
                    // usually still running on a different
                    // perception path; warn the user once via the
                    // chat list so they don't think the whole app
                    // is frozen, then stop the poll.
                    tracing::warn!(
                        event = "screencast_poll_stopped",
                        error = %e,
                        "screenshot poll stopped; sending a one-shot chat notice"
                    );
                    let user_msg = error_message::for_user(&e, "refresh the live preview");
                    let _ = app_for_shot.emit(
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
            let _ = on_event.send(event);
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
        page: &'a mew_cdp::ReExportedPage,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<mew_agent::handoff::BrowserResult>> + Send + 'a>,
    > {
        // `handoff` is currently unused — the agent's
        // constructor already ran with the orchestrator's
        // typed subtask list (via the
        // `PrefabricatedAgentFactory`'s caller, the
        // orchestrator's `dispatch_browser_task`). Phase 3.1
        // will route the handoff's constraints and
        // `originating_message_id` into the agent
        // constructor.
        let _ = handoff;

        // `take` the agent out of the factory so we can
        // move it into the async block. The lock is held
        // only for the `take` — we never hold it across an
        // `.await`, so there is no `Send`-across-await
        // hazard. The `std::sync::Mutex` is correct here
        // because we never need to wait on it.
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
            let result = agent.run(page).await;
            match result {
                Ok(text) => Ok(mew_agent::handoff::BrowserResult::done(
                    session_id,
                    text,
                    Vec::new(),
                    None,
                    None, // raw_transcript_ref: future — agent should expose
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
    .invoke_handler(tauri::generate_handler![send_message, get_config_summary, pause_session, resume_session])
    .setup(|app| {
      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }
      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
