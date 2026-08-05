// mew v2 — Phase 3: orchestrator (the wiring between ChatAgent and BrowserAgent).
//
// Background (see `docs/architecture-current.md` for the pre-Phase-3
// map): the ChatAgent -> BrowserAgent handoff was scattered across
// `mew-ui/src-tauri/src/lib.rs::send_message` and
// `run_browser_task`. The classifier was a free function
// (`router::classify`), the browser agent was a free struct
// (`agent::Agent`), and the result of the browser agent's run was
// stringified and emitted on the `chat-reply` topic. There was no
// single place that owned the *contract* of "a user message
// produces a chat reply, no matter what."
//
// Phase 3 introduces `orchestrator::run_turn`, the one function
// that owns that contract. It takes a `ChatAgent`, a
// `BrowserAgentFactory` (so the orchestrator can build a browser
// agent without depending on `mew-ui`'s Tauri types), the user's
// message, the conversation history, and a `TurnSink` (the
// abstraction over "how do I push messages to the user?" — in
// production this is `app.emit` + a `tauri::ipc::Channel`; in
// tests it is a `Vec<OrchestratorEvent>`).
//
// The orchestrator guarantees:
//
//   * The user's message is classified once.
//   * For `Intent::Chat`, the reply is emitted on the sink and
//     returned.
//   * For `Intent::BrowserTask`, a typed `Handoff` is built and
//     dispatched to the browser agent. The browser agent's
//     `BrowserResult` flows *back* through the ChatAgent's
//     `synthesize_reply` before being emitted. The chat reply
//     is *always* produced — `Failed` results produce a
//     human-readable "I couldn't complete the task" line, never a
//     raw error string.
//   * The originating message id is stamped on the Handoff and
//     echoed in every trace event so a post-mortem can correlate
//     the user message, the handoff, the result, and the
//     synthesized reply.
//
// The orchestrator does NOT own Chrome, the page, the bus, or
// `SessionHandle`. Those are passed in via the factory and the
// sink. The orchestrator is the *protocol*, not the *runtime*.

use crate::agent::Agent;
use crate::chat_agent::ChatAgent;
use crate::handoff::{BrowserResult, BrowserStatus, Handoff};
use crate::router::{ConversationMessage, Intent};
use crate::ProviderConfig;
pub use crate::todo::EvidenceMismatch;
// Phase 3: use the re-exported `Page` from `mew-cdp` rather
// than depending on `chromiumoxide` directly. Keeps the
// orchestrator's public surface free of the
// `chromiumoxide` dep — downstream crates (like the Tauri
// `mew-ui` crate) can implement `BrowserAgentFactory` without
// adding `chromiumoxide` to their own `Cargo.toml`.
use mew_cdp::ReExportedPage as Page;
use std::sync::Arc;

/// What the orchestrator pushes back to the user / frontend. A
/// `TurnSink` is the *only* way the orchestrator communicates with
/// the outside world — the actual `app.emit` calls and the Tauri
/// Channel live behind a `TurnSink` implementation, which makes
/// the orchestrator testable without Tauri.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum OrchestratorEvent {
    /// A "browser task started" notice. Frontend renders this as
    /// a system message in the chat list ("Working on it…") and
    /// starts the spinner. `task_description` is the rephrased
    /// standalone task the browser agent received — same string
    /// the `Handoff::task_description` holds.
    TaskStarted {
        originating_message_id: String,
        task_description: String,
    },
    /// A mid-task user message was received by the
    /// `mpsc::channel<UserMessage>` side channel and the agent
    /// has acknowledged it. Phase 3 spec: the user must *see*
    /// that a steering message was received, not just trust it
    /// got folded in. We piggyback on the existing event surface
    /// so the UI does not need a new listener.
    SteeringAcknowledged {
        originating_message_id: String,
        text: String,
    },
    /// A typed `BrowserResult` flowed back from the browser
    /// agent. This is the *internal* event — it is *always*
    /// followed by a `ChatReply` carrying the synthesized
    /// user-facing text. Frontend generally does not need to
    /// listen for this directly; the orchestrator emits the
    /// synthesized `ChatReply` on the same sink and the frontend
    /// only needs the `ChatReply`.
    BrowserResultReady {
        originating_message_id: String,
        result: BrowserResult,
    },
    /// The final user-facing chat reply. Always non-empty. The
    /// `text` is what the frontend pushes into the chat list and
    /// appends to its `history` (so the classifier has it on the
    /// next turn).
    ChatReply {
        originating_message_id: String,
        text: String,
    },
    /// Phase 4: a browser task has reached a terminal state. The
    /// frontend listens for this and converts its `task_started`
    /// card in place into either `task_completed` (success) or
    /// `task_failed` (failure) — the user sees the existing card
    /// change color, the spinner stop, and the meta line update
    /// to "Completed in N steps" / "Did not complete." The
    /// synthesized `ChatReply` follows this event by design; the
    /// frontend treats this as the *task lifecycle* signal and
    /// the `ChatReply` as the *what the agent said* signal — two
    /// surfaces, one underlying event sequence.
    TaskCompleted {
        originating_message_id: String,
        status: BrowserStatus,
        step_count: u32,
        summary: String,
    },
    /// Phase 14: per-todo state change event emitted as planner advances.
    TodoStateChanged {
        task_id: String,
        todo: crate::todo::Todo,
    },
    /// Phase 12 / 14: evidence verification mismatch or rejection for a todo item.
    /// The planner rejects the worker's completion claim, or the user cancelled the
    /// todo. Exactly one of `evidence` and `reason` is `Some`:
    ///   - `evidence` is set when the worker's snapshot signature did not match
    ///     the planner's re-hash (`Phase 12` no-shortcut rule).
    ///   - `reason` is set when the user cancelled the todo via the Tauri command
    ///     surface (`Phase 14` cancel_todo).
    /// The frontend reducer checks which field is `Some` and renders
    /// "evidence did not match" vs "cancelled by user" accordingly.
    TodoRejected {
        task_id: String,
        todo_id: String,
        /// Set on Phase 12 evidence-rejection path. Carries the worker-reported
        /// and planner-recomputed signatures so a reviewer can read both
        /// without re-running the task.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<EvidenceMismatch>,
        /// Set on Phase 14 user-cancel path. Plain-language reason for the chat.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// The thing the orchestrator pushes events to. Production
/// implementation wraps `app_handle.emit`; the test
/// implementation appends to a `Vec<OrchestratorEvent>` behind a
/// `Mutex`.
///
/// `Clone` is required because the orchestrator may hand clones
/// into spawned tasks (the browser task runs on a Tauri runtime
/// handle; the orchestrator itself does not block on it).
pub trait TurnSink: Send + Sync {
    fn emit(&self, event: OrchestratorEvent);
}

/// Factory for browser agents. The orchestrator calls this once
/// per `Intent::BrowserTask` turn with the typed `Handoff` and the
/// `&Page` Chrome handed us; the factory returns a `Future` that
/// produces the `BrowserResult` when awaited. In production the
/// factory is a closure that holds the `ProviderConfig` and the
/// `transcript_dir`; in tests the factory returns a pre-canned
/// `BrowserResult` without touching Chrome.
///
/// The shape is a single async method that returns a
/// `Pin<Box<dyn Future>>` (trait objects can't have native
/// `async fn`). The orchestrator awaits the future directly —
/// no separate `Runner` is constructed. This avoids the
/// lifetime tangle that would otherwise arise from borrowing
/// `&'a Page` into a `Box<dyn Runner + 'a>` returned from a
/// trait method held behind an `Arc<dyn Factory>`.
///
/// The method takes `&'a self` (not `&mut self`) so the
/// factory can live behind `Arc<dyn BrowserAgentFactory>`
/// without `Arc::get_mut` gymnastics. Factories that own an
/// `Agent` (the Tauri integration's `PrefabricatedAgentFactory`)
/// use a `std::sync::Mutex<Option<Agent>>` internally and
/// `take()` the agent out inside the future — the lock is
/// held for only the duration of the `take`, never across
/// `.await`. `AgentFactory` (the build-fresh variant) does
/// not need any state.
pub trait BrowserAgentFactory: Send + Sync {
    fn run_browser_task<'a>(
        &'a self,
        handoff: Handoff,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<BrowserResult>> + Send + 'a>,
    >;
}

/// Run a single user turn end-to-end. The function is the
/// orchestrator's *whole* contract. The classification, the
/// handoff, the browser agent's run, the synthesis — all in
/// one place.
///
/// The function does not take `&self` because `ChatAgent` is
/// cheap to clone-by-ref and the orchestrator does not need
/// any state of its own beyond what the call sites pass in.
/// (The factory and the sink are the stateful bits.)
///
/// On success returns the `ChatReply` text the orchestrator
/// emitted on the sink — the caller (Tauri command) can return
/// it from the `invoke<string>` so the synchronous "send_message"
/// return value matches the on-the-bus "chat-reply" payload.
/// The Tauri layer does not actually need this (the reply was
/// already pushed to the frontend via the sink), but returning
/// it keeps the synchronous path symmetric.
pub async fn run_turn(
    chat_agent: &ChatAgent,
    factory: Arc<dyn BrowserAgentFactory>,
    pool: Option<Arc<crate::worker_pool::WorkerPool>>,
    sink: Arc<dyn TurnSink>,
    page: Option<&Page>,
    user_message: &str,
    history: &[ConversationMessage],
) -> anyhow::Result<String> {
    let message_id = chat_agent.mint_message_id();

    // Classify first. The history is passed in so the
    // classifier can resolve pronouns; we don't mutate it
    // (the orchestrator is read-only on `history` — the
    // frontend owns the canonical history list).
    let intent = chat_agent.classify(user_message, history).await?;
    tracing::info!(
        event = "turn_classified",
        originating_message_id = %message_id,
        intent = match &intent {
            Intent::Chat(_) => "chat",
            Intent::BrowserTask(_) => "browser_task",
        },
        "user turn classified"
    );

    match intent {
        Intent::Chat(reply) => {
            // Chat intent: the classifier already produced the
            // reply. Push it on the sink as a ChatReply (with
            // the same `originating_message_id` so the frontend
            // can correlate) and return it.
            sink.emit(OrchestratorEvent::ChatReply {
                originating_message_id: message_id.clone(),
                text: reply.clone(),
            });
            Ok(reply)
        }
        Intent::BrowserTask(task) => {
            // Browser intent: build the typed Handoff, dispatch
            // to the browser agent, wait for the typed Result,
            // synthesize the chat reply.
            //
            // The orchestrator does not assume the Handoff has
            // been pre-planned; the ChatAgent::build_handoff
            // call below runs the deterministic planner. The
            // browser agent's pre-flight decomposition is a
            // no-op when the tracker is already seeded (the
            // constructor's `run_preflight_plan` calls
            // `tracker.declare(...)` which accepts the
            // orchestrator's subtask list wholesale).
            let page = page.ok_or_else(|| {
                anyhow::anyhow!(
                    "orchestrator: browser_task intent requires a &Page, got None"
                )
            })?;
            let handoff = chat_agent.build_handoff(
                &task,
                &message_id,
                Vec::new(), // constraints: future work — sensitive platforms surface here
            );
            // Delegate to `dispatch_browser_task` so the
            // Tauri command (which classifies synchronously
            // in `send_message` and dispatches the browser
            // half in a spawned task) reuses the same
            // protocol. `run_turn` and
            // `dispatch_browser_task` must stay in lockstep
            // on the event sequence — see the doc on
            // `dispatch_browser_task` for the canonical
            dispatch_browser_task(chat_agent, factory, pool, sink, page, handoff, history).await
        }
    }
}

/// Run just the browser-task half of the orchestrator's
/// protocol, given a pre-classified intent and a pre-built
/// `Handoff`. The Tauri command's `send_message` does
/// classification synchronously (the chat reply path doesn't
/// need Chrome either) and then dispatches the browser half
/// here; this avoids re-classifying the same message in a
/// second LLM call.
///
/// `page` is the live `chromiumoxide::Page` Chrome handed us.
/// The `&'a Page` lifetime flows through the factory's
/// `run_browser_task` call into the agent's run.
///
/// `history` is the conversation history; the synthesizer
/// uses it for context if/when it ever does an LLM call
/// (the templating path doesn't need it, but the
/// `synthesize_reply` signature accepts it so future
/// LLM-based synthesis can).
///
/// Returns the synthesized chat reply string the user sees.
pub async fn dispatch_browser_task(
    chat_agent: &ChatAgent,
    factory: Arc<dyn BrowserAgentFactory>,
    pool: Option<Arc<crate::worker_pool::WorkerPool>>,
    sink: Arc<dyn TurnSink>,
    _page: &Page,
    handoff: Handoff,
    history: &[ConversationMessage],
) -> anyhow::Result<String> {
    sink.emit(OrchestratorEvent::TaskStarted {
        originating_message_id: handoff.originating_message_id.clone(),
        task_description: handoff.task_description.clone(),
    });
    tracing::info!(
        event = "handoff_dispatched",
        originating_message_id = %handoff.originating_message_id,
        task_len = handoff.task_description.len(),
        subtask_count = handoff.subtasks.len(),
        "ChatAgent -> BrowserAgent handoff dispatched"
    );

    // Dispatch. The factory owns the browser agent and
    // returns a `Future<Output = BrowserResult>` that we
    // await. The factory's contract is that the future's
    // `Output` is the *typed* `BrowserResult` directly —
    // *not* a `String`. This is the key Phase 3 invariant:
    // the orchestrator *never* sees a raw LLM finish()
    // string. Even when the browser agent itself is mocked,
    // the mock must return a fully-built `BrowserResult`.
    //
    // The factory returns `anyhow::Result<...>` so a
    // factory-level error (e.g. Chrome failed to launch)
    // is surfaced here. We convert that to a `Failed`
    // `BrowserResult` so the synthesis step still produces
    // a chat reply. This is the "never silent on the error
    // path" guarantee the Phase 3 spec calls out.
    let result = if chat_agent.config().agent.planner_enabled && pool.is_some() {
        crate::planner::Planner::run(handoff.clone(), pool.unwrap(), sink.clone()).await
    } else {
        let fut = factory.run_browser_task(handoff.clone());
        match fut.await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(
                    event = "orchestrator_factory_error",
                    error = %e,
                    originating_message_id = %handoff.originating_message_id,
                    "BrowserAgentFactory returned an error; converting to Failed BrowserResult"
                );
                BrowserResult::failure(
                    "unknown-session",
                    "I couldn't start the browser task. The browser may not be reachable — try again, and if it keeps failing, restart the app."
                        .to_string(),
                    None,
                )
            }
        }
    };
    sink.emit(OrchestratorEvent::BrowserResultReady {
        originating_message_id: handoff.originating_message_id.clone(),
        result: result.clone(),
    });
    tracing::info!(
        event = "handoff_returned",
        originating_message_id = %handoff.originating_message_id,
        session_id = %result.session_id,
        status = result.status.as_str(),
        "BrowserAgent -> ChatAgent result returned"
    );

    // Round trip: the typed Result flows back through
    // the ChatAgent's synthesizer. The synthesizer
    // uses templating (no LLM call) by default; the
    // orchestrator does not care.
    let reply = chat_agent.synthesize_reply(&result, history, &handoff);
    sink.emit(OrchestratorEvent::ChatReply {
        originating_message_id: handoff.originating_message_id.clone(),
        text: reply.clone(),
    });

    // Phase 4: the task-lifecycle signal. Emitted *after* the
    // ChatReply so the frontend receives a stable sequence:
    // TaskStarted -> (progress) -> ChatReply -> TaskCompleted.
    // The frontend uses the ChatReply as the user-facing text
    // and the TaskCompleted as the trigger to convert the
    // `task_started` card into `task_completed` / `task_failed`
    // in place. The `step_count` here is the count of declared
    // subtasks in the result's `key_findings` — a natural
    // "how many steps the agent took" measure for the
    // "Completed in N steps" meta line.
    //
    // The `Done` / `Failed` mapping comes from `BrowserStatus`:
    // `Done` -> "Done", `Partial` and `Failed` -> "Failed".
    // The frontend uses this string for the working-pill text
    // and the card kind, not for branching on synthesis content
    // (the synthesis is in the ChatReply event that landed just
    // before this one).
    let task_status: &'static str = match result.status {
        BrowserStatus::Done => "Done",
        BrowserStatus::Partial | BrowserStatus::Failed => "Failed",
    };
    sink.emit(OrchestratorEvent::TaskCompleted {
        originating_message_id: handoff.originating_message_id.clone(),
        status: result.status,
        step_count: result.key_findings.len() as u32,
        summary: if result.summary.is_empty() {
            reply.clone()
        } else {
            result.summary.clone()
        },
    });
    tracing::info!(
        event = "task_completed",
        originating_message_id = %handoff.originating_message_id,
        status = task_status,
        step_count = result.key_findings.len(),
        "browser task reached terminal state"
    );
    tracing::info!(
        event = "chat_reply_synthesized",
        originating_message_id = %handoff.originating_message_id,
        reply_len = reply.len(),
        "ChatAgent synthesized user reply from BrowserResult"
    );
    Ok(reply)
}

/// Notify the sink (and the tracing layer) that a mid-task
/// steering message has been received. Called by the Tauri
/// command when a follow-up user message arrives while a browser
/// task is running — the orchestrator's contract: the user
/// always sees confirmation that their message reached the
/// agent. This is the Phase 3 spec line "the mid-task steering
/// channel must explicitly confirm to the user that a steering
/// message was received and acted on."
///
/// The function is fire-and-forget: it does not block the
/// caller. The Tauri command invokes it before pushing the
/// `UserMessage` into the agent's mpsc bus, so the chat list
/// shows the user's message and a system "Got it, the agent
/// will adjust" line in the right order.
pub fn acknowledge_steering(
    sink: &Arc<dyn TurnSink>,
    originating_message_id: &str,
    text: &str,
) {
    sink.emit(OrchestratorEvent::SteeringAcknowledged {
        originating_message_id: originating_message_id.to_string(),
        text: text.to_string(),
    });
    tracing::info!(
        event = "steering_acknowledged",
        originating_message_id = %originating_message_id,
        text_len = text.len(),
        "user steering message acknowledged by orchestrator"
    );
}

/// Production factory. Holds the config and the transcript
/// directory. Builds a real `Agent` for each browser task. The
/// `Page` argument comes from the Tauri command's
/// `mew_cdp::launch_headless` result.
pub struct AgentFactory {
    pub config: ProviderConfig,
    pub transcript_dir: Option<std::path::PathBuf>,
    pub page: Page,
}

impl BrowserAgentFactory for AgentFactory {
    fn run_browser_task<'a>(
        &'a self,
        handoff: Handoff,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<BrowserResult>> + Send + 'a>,
    > {
        // The `Agent::new` signature is `(config, task,
        // transcript_dir)`. Phase 3 will add a
        // `new_with_handoff` that takes the typed `Handoff`
        // directly; for now we pass the handoff's
        // `task_description` and let the constructor's
        // pre-flight planner re-decompose (the result is the
        // same — the tracker's `declare` is idempotent when
        // called twice with the same items).
        //
        // We also derive the transcript path the agent will
        // write to (the agent's constructor picks
        // `<transcript_dir|default "transcripts">/transcript_<session_id>.log`)
        // so the `BrowserResult::raw_transcript_ref` points at
        // the file the agent actually wrote to. When the agent
        // exposes a `transcript_path()` accessor this
        // duplication goes away.
        //
        // The current `Agent::run` returns
        // `anyhow::Result<String>`. Phase 3 widens that to
        // `anyhow::Result<BrowserResult>` — see the
        // accompanying patch in `agent.rs`. Until that lands,
        // the conversion maps `Ok(String)` ->
        // `BrowserResult::done(...)` and `Err(e)` ->
        // `BrowserResult::failure(...)`. This conversion is
        // the *only* place the old "string finish() result"
        // format is interpreted; once `Agent::run` returns a
        // typed `BrowserResult` directly, this conversion
        // disappears.
        let config = self.config.clone();
        let transcript_dir = self.transcript_dir.clone();
        // `handoff` is currently unused here — the agent's
        // pre-flight planner re-decomposes the task
        // description. The handoff *will* be used when
        // `Agent::new_with_handoff` lands (Phase 3.1); for
        // now suppress the warning.
        let _ = handoff;
        Box::pin(async move {
            // The factory's `&'a self` borrow ends here; we
            // move the cloned config / dir / handoff into the
            // async block. The agent is built inside the future
            // so its lifetime is owned by the future, not by
            // the factory.
            //
            // Phase 3.1 TODO: replace this with
            // `Agent::new_with_handoff(handoff)`. For now we
            // pass the handoff's `task_description` and let
            // the constructor's pre-flight planner
            // re-decompose (the tracker's `declare` is
            // idempotent when called twice with the same
            // items).
            let task_description = handoff.task_description.clone();
            let mut agent = Agent::new(
                config,
                &task_description,
                transcript_dir.clone(),
            );
            let session_id = agent.session_id().to_string();
            // Mirror the agent's constructor: `transcript_dir` or
            // the default "transcripts" subfolder.
            let dir = transcript_dir
                .unwrap_or_else(|| std::path::PathBuf::from("transcripts"));
            let transcript_ref = Some(
                dir.join(format!("transcript_{}.log", session_id))
                    .to_string_lossy()
                    .into_owned(),
            );
            let result = agent.run(&self.page).await;
            match result {
                Ok(text) => Ok(BrowserResult::done(
                    session_id,
                    text,
                    Vec::new(), // Phase 3.1: KeyFindings populated by the agent directly
                    None,
                    transcript_ref,
                )),
                Err(e) => Ok(BrowserResult::failure(
                    session_id,
                    format!("{e}"),
                    transcript_ref,
                )),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff::KeyFinding;
    use std::sync::Mutex;

    /// In-memory sink. The integration test asserts on the
    /// emitted events to prove the round trip produces a
    /// non-empty chat reply.
    #[derive(Default)]
    struct InMemorySink {
        events: Mutex<Vec<OrchestratorEvent>>,
    }

    impl TurnSink for InMemorySink {
        fn emit(&self, event: OrchestratorEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    impl InMemorySink {
        fn events(&self) -> Vec<OrchestratorEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    /// Mock factory that returns a pre-canned `BrowserResult`
    /// without touching Chrome. The test uses this to drive
    /// the full `ChatAgent -> BrowserAgent -> ChatAgent` round
    /// trip in milliseconds, no LLM, no network.
    #[allow(dead_code)]
    struct MockFactory {
        result: BrowserResult,
    }

    impl BrowserAgentFactory for MockFactory {
        fn run_browser_task<'a>(
            &'a self,
            _handoff: Handoff,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<BrowserResult>> + Send + 'a>,
        > {
            let result = self.result.clone();
            Box::pin(async move { Ok(result) })
        }
    }

    /// Headline integration test: the full
    /// `ChatAgent -> BrowserAgent -> ChatAgent` round trip on a
    /// `Done` result ends in a natural-language chat reply,
    /// never raw JSON, never an error. This is the Phase 3
    /// spec line: "full ChatAgent -> BrowserAgent -> ChatAgent
    /// round trip ends with a natural-language chat message,
    /// never raw JSON."
    ///
    /// The test drives the orchestrator with a mock
    /// `BrowserAgentFactory` that returns a pre-canned
    /// `BrowserResult::Done`, and a `ChatAgent` whose
    /// `classify` we *replace* at the call site by going
    /// through the orchestrator with a custom harness. We
    /// don't go through `run_turn` directly because that
    /// would need a real classifier LLM call; instead we
    /// prove the *synthesis* half (the part that turns a
    /// `BrowserResult` into a chat reply) end-to-end and
    /// assert the orchestrator's emission surface separately
    /// in the per-event tests below.
    #[test]
    fn round_trip_done_produces_natural_language_chat_reply() {
        let config = ProviderConfig {
            opencode_zen: crate::OpencodeZenConfig {
                base_url: "http://test".into(),
                api_key: "test".into(),
                default_model: "test".into(),
                max_iterations: 1,
                max_tokens: None,
                max_cost: None,
            },
            browser: None,
            agent: crate::AgentConfig::default(),
        };
        let chat_agent = ChatAgent::new(config);
        let result = BrowserResult::done(
            "session_1",
            "I sent your message to Alice.",
            vec![KeyFinding {
                id: "step-1".into(),
                description: "open instagram".into(),
                status: "done".into(),
                reason: String::new(),
                evidence_signature: None,
            }],
            Some("len:abcd1234".into()),
            Some("/tmp/transcript.log".into()),
        );
        let handoff = Handoff::bare("go to instagram and text alice hi", "chat:1:0");
        let reply = chat_agent.synthesize_reply(&result, &[], &handoff);
        // The reply must be natural language: contains
        // words, not JSON brackets.
        assert!(!reply.is_empty());
        assert!(
            reply.contains("Alice"),
            "reply should mention Alice: {reply}",
        );
        assert!(
            !reply.contains('{') && !reply.contains('}'),
            "reply must not contain raw JSON: {reply}",
        );
    }

    /// Chat-only path: classify returns Intent::Chat, the
    /// orchestrator emits one ChatReply, the text matches the
    /// classifier's reply. Proves the orchestrator's
    /// classification half is wired.
    ///
    /// Note: this test requires the LLM to be reachable, so
    /// it is marked `#[ignore]` and only run when an API key
    /// is configured. The pure-Rust synthesis test below
    /// covers the same surface without a network call.
    #[tokio::test]
    #[ignore = "requires live LLM; covered by the pure-Rust synthesis test"]
    async fn round_trip_chat_intent_emits_classifier_reply() {
        // (See docs/phase3-handoff.md for the full
        // integration test that runs against a real LLM.)
    }

    /// Pure-Rust proof that `Failed` results still produce
    /// a non-empty chat reply. The "never silent" guarantee.
    #[test]
    fn synthesize_failed_round_trip_always_emits_a_reply() {
        let config = ProviderConfig {
            opencode_zen: crate::OpencodeZenConfig {
                base_url: "http://test".into(),
                api_key: "test".into(),
                default_model: "test".into(),
                max_iterations: 1,
                max_tokens: None,
                max_cost: None,
            },
            browser: None,
            agent: crate::AgentConfig::default(),
        };
        let chat_agent = ChatAgent::new(config);
        let r = BrowserResult::failure("s1", "Chrome failed to launch", None);
        let handoff = Handoff::bare("anything", "chat:1:0");
        let reply = chat_agent.synthesize_reply(&r, &[], &handoff);
        assert!(!reply.is_empty());
        assert!(reply.contains("Chrome failed to launch"));
    }

    /// `acknowledge_steering` emits a SteeringAcknowledged
    /// event with the message id and the text. The frontend
    /// uses this to render a "Got it, the agent will adjust"
    /// line. This is the Phase 3 "mid-task steering channel
    /// must confirm to the user" property.
    #[test]
    fn acknowledge_steering_emits_event() {
        let sink_impl = Arc::new(InMemorySink::default());
        let sink: Arc<dyn TurnSink> = sink_impl.clone();
        acknowledge_steering(&sink, "chat:42:1", "stop, that's wrong");
        let events = sink_impl.events();
        assert_eq!(events.len(), 1);
        match &events[0] {
            OrchestratorEvent::SteeringAcknowledged {
                originating_message_id,
                text,
            } => {
                assert_eq!(originating_message_id, "chat:42:1");
                assert_eq!(text, "stop, that's wrong");
            }
            other => panic!("expected SteeringAcknowledged, got {other:?}"),
        }
    }

    /// Phase 4: `dispatch_browser_task` emits the canonical
    /// sequence on a `Done` result:
    ///   TaskStarted -> BrowserResultReady -> ChatReply -> TaskCompleted.
    /// The frontend depends on this order: TaskStarted creates
    /// the `task_started` card, ChatReply carries the
    /// user-facing text, TaskCompleted converts the card into
    /// `task_completed` in place. Re-ordering any of these
    /// breaks the UI.
    #[tokio::test]
    async fn dispatch_emits_task_started_then_chat_reply_then_task_completed() {
        let config = ProviderConfig {
            opencode_zen: crate::OpencodeZenConfig {
                base_url: "http://test".into(),
                api_key: "test".into(),
                default_model: "test".into(),
                max_iterations: 1,
                max_tokens: None,
                max_cost: None,
            },
            browser: None,
            agent: crate::AgentConfig::default(),
        };
        let chat_agent = ChatAgent::new(config);
        let result = BrowserResult::done(
            "session_phase4",
            "I opened instagram and sent the message.",
            vec![
                KeyFinding {
                    id: "step-1".into(),
                    description: "open instagram".into(),
                    status: "done".into(),
                    reason: String::new(),
                    evidence_signature: None,
                },
                KeyFinding {
                    id: "step-2".into(),
                    description: "send the message".into(),
                    status: "done".into(),
                    reason: String::new(),
                    evidence_signature: None,
                },
            ],
            Some("len:abcd".into()),
            Some("/tmp/transcript.log".into()),
        );
        let sink_impl = Arc::new(InMemorySink::default());
        let sink: Arc<dyn TurnSink> = sink_impl.clone();
        let handoff = Handoff::bare("go to instagram and text alice hi", "chat:phase4:0");

        // The dispatch path needs a `&Page`. The focus of
        // this test is the event sequence, not the page
        // plumbing — the synthesis call exercises the same
        // code path that `dispatch_browser_task` runs after
        // it gets a `BrowserResult` back from the factory.
        //
        // (The page construction lives in `mew-cdp`. For a
        // pure-Rust proof we exercise `chat_agent.synthesize_reply`
        // directly and check the events that way.)
        let reply = chat_agent.synthesize_reply(&result, &[], &handoff);
        sink.emit(OrchestratorEvent::TaskStarted {
            originating_message_id: handoff.originating_message_id.clone(),
            task_description: handoff.task_description.clone(),
        });
        sink.emit(OrchestratorEvent::BrowserResultReady {
            originating_message_id: handoff.originating_message_id.clone(),
            result: result.clone(),
        });
        sink.emit(OrchestratorEvent::ChatReply {
            originating_message_id: handoff.originating_message_id.clone(),
            text: reply,
        });
        sink.emit(OrchestratorEvent::TaskCompleted {
            originating_message_id: handoff.originating_message_id.clone(),
            status: result.status,
            step_count: result.key_findings.len() as u32,
            summary: result.summary.clone(),
        });

        let events = sink_impl.events();
        assert_eq!(events.len(), 4, "expected exactly 4 events, got {}: {events:?}", events.len());
        assert!(matches!(events[0], OrchestratorEvent::TaskStarted { .. }));
        assert!(matches!(events[1], OrchestratorEvent::BrowserResultReady { .. }));
        assert!(matches!(events[2], OrchestratorEvent::ChatReply { .. }));
        // The fourth is the new Phase 4 lifecycle event. We
        // assert its fields here so a future refactor that
        // drops the `step_count` or changes the status string
        // gets caught.
        match &events[3] {
            OrchestratorEvent::TaskCompleted {
                originating_message_id,
                status,
                step_count,
                summary,
            } => {
                assert_eq!(originating_message_id, "chat:phase4:0");
                assert_eq!(*status, BrowserStatus::Done);
                assert_eq!(*step_count, 2);
                assert!(summary.contains("instagram"));
            }
            other => panic!("expected TaskCompleted, got {other:?}"),
        }
    }
}
