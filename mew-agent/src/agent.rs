use crate::ProviderConfig;
use crate::captcha_telemetry;
use crate::chat::{MessageBus, UserMessage};
use crate::completeness::{CompletenessTracker, DeclareItem, MarkOutcome, SubTaskStatus};
use crate::pacing::{PacingDecision, PacingGuard};
use crate::session::{SessionError, SessionHandle};
use crate::summarizer::{self, ProgressLine};
use serde::{Deserialize, Serialize};
use chromiumoxide::Page;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;
use std::io::Write;
use mew_perception::state::PerceptionState;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum AgentEvent {
    State(crate::session::TransitionRecord),
    Tool { timestamp_secs: u64, name: String, args: String, result: String },
    Summary { timestamp_secs: u64, text: String },
    /// Phase 5: live progress line. Emitted after every tool
    /// dispatch (templated) and at finish() time. The
    /// frontend's "agent is working" pill and the per-task
    /// "live progress" sub-list both consume these. The
    /// `kind` is one of the strings documented on
    /// `summarizer::ProgressKind::as_str` so the frontend
    /// can key icon/color without parsing the text.
    ProgressLine {
        timestamp_secs: u64,
        text: String,
        kind: String,
        success: bool,
    },
}

pub struct Agent {
    config: ProviderConfig,
    messages: Vec<serde_json::Value>,
    client: reqwest::Client,
    total_tokens: usize,
    iterations: usize,
    state: Arc<Mutex<PerceptionState>>,
    session_id: String,
    current_url: Option<String>,
    transcript_file: Option<std::fs::File>,
    force_snapshot: bool,
    /// Phase 5: live progress ring buffer. Holds the most
    /// recent `live_lines_cap` (default 5) templated progress
    /// lines. Each new tool dispatch pushes one line, drops
    /// the oldest if over cap, and emits the line on the
    /// `event_tx` channel as an `AgentEvent::ProgressLine`.
    /// The buffer is also read at the end of the run to
    /// feed the end-of-task LLM summarizer with the "what
    /// did the agent actually do" context.
    live_progress: summarizer::LiveProgress,
    /// Phase 5: the verbosity + cap config the loop uses to
    /// decide how to format each line. Set at construction
    /// from `config.agent.summarization`.
    summarization_cfg: summarizer::SummarizationConfig,
    /// Phase 12.1: explicit state-machine handle. Cloned cheaply and handed to
    /// any external thread (UI, signal handler, Step 13 chat reader) that needs
    /// to pause/resume/stop the loop. The ReAct loop calls `checkpoint()` on
    /// this between iterations.
    session: SessionHandle,
    /// Phase 13.1: live chat channel. The CLI calls `take_message_sender()`
    /// to get the sender half; the ReAct loop drains the receiver at every
    /// checkpoint via `drain_user_messages()`. Wrapped in `Option` so the
    /// loop can detect "no bus attached" and skip the drain entirely.
    /// Sized to None after `take_message_sender` so we don't double-take.
    bus: Option<MessageBus>,
    /// Phase 15.1: completeness tracker. Owns the canonical list of
    /// sub-items the model has declared for the current task, and the
    /// "evidence is a fresh snapshot" rule the loop enforces on
    /// `mark_subtask_done`. The `finish()` tool handler is gated on
    /// this; the per-subtask end-of-session summary is read from this.
    completeness: CompletenessTracker,
    /// Phase 15.1: how many `finish()` calls have been made *for the
    /// current gate pass*. The first call is a "force a re-prompt if
    /// any subtask is still incomplete"; the second call is the real
    /// finish. We keep this separate from `CompletenessTracker::
    /// finish_attempts` because the loop wants to reset on every
    /// snapshot re-prompt (so the LLM gets a *fresh* chance), while
    /// the tracker counts lifetime attempts for the summary.
    finish_calls_this_gate: usize,
    /// Phase 15.1: when the gate is open and `finish()` is honored, the
    /// match-arm stores the model's result string here, and the
    /// post-match block in `run_inner` returns it via `Ok(...)`. This
    /// keeps the post-match tool-result logging path shared between
    /// the gated and ungated cases.
    pending_finish_result: Option<String>,
    /// Phase 15.1: set to `true` once the per-subtask summary has been
    /// written to the transcript. The outer `run` wrapper checks this
    /// so the summary is written exactly once — either by the finish
    /// handler (gate open) or by the error/stop path (gate never
    /// closed or session terminated mid-task). The 15.1 spec says
    /// "log at the end of every session," not "log multiple times";
    /// this flag enforces that.
    summary_written: bool,
    /// Phase 17.1: pacing guard. Tracks the current streak of
    /// same-type actions and emits a sleep-before-dispatch when a
    /// tight loop is detected. Built from the config's `agent.pacing`
    /// block at construction; fully disabled when the block is
    /// absent or `enabled: false` — then the guard is a no-op and
    /// adds zero latency to any dispatch.
    pacing: PacingGuard,
    /// Phase 3.3 (regression): queue of user-note file writes that
    /// were captured mid-iteration (e.g. by the third drain right
    /// after the LLM call returns) but whose on-disk write was
    /// deferred so it lands *after* the in-flight tool's
    /// `TOOL CALL:` line. Flushed by the top-of-iteration drain
    /// at the start of the next iteration. Each entry is
    /// `(timestamp_secs, text)` — the same shape
    /// `drain_and_apply_user_messages` writes today.
    pending_user_note_writes: Vec<(u64, String)>,
    /// Phase 4.1: Channel to stream events to the frontend.
    pub event_tx: Option<tokio::sync::mpsc::UnboundedSender<AgentEvent>>,
    /// Phase 1: optional handle to the per-session structured tracing
    /// layer installed by the constructor. When `Some`, every LLM
    /// request/response, tool dispatch, and resolution decision
    /// is also written as a JSON line to the session's trace file
    /// (in addition to the human-readable transcript). When `None`,
    /// the tracing layer is not installed and the loop incurs zero
    /// per-iteration overhead. The default path is `None` — the
    /// tracing layer is opt-in via `MEW_TRACING_DIR` so production
    /// runs are not penalised.
    tracing_layer: Option<Arc<crate::tracing_layer::SessionJsonLayer>>,
    /// Phase 1: cached guard so the loop can check whether the
    /// structured tracing layer is installed without paying for a
    /// per-iter `Option::is_some()` that the borrow checker would
    /// otherwise require us to extract from `&self` repeatedly.
    tracing_enabled: bool,
    /// Phase 1.5: thread-local subscriber guard. Set by the
    /// constructor when `tracing_dir` is `Some` — dropping the
    /// agent drops the guard and the thread-local override
    /// reverts. This is what actually wires the JSONL layer into
    /// the `tracing::info!` event flow when the global subscriber
    /// slot is already taken (CLI's `tracing_subscriber::fmt()`,
    /// Tauri's logger). Without this, the JSONL file would only
    /// get the explicit "structured tracing active" line written
    /// by the constructor, never the per-iteration events.
    #[allow(dead_code)] // Held for the agent's lifetime; existence is the point.
    tracing_guard: Option<crate::tracing_layer::SessionTraceGuard>,
    /// Phase 2: sensitive-platform routing table. Loaded from
    /// `config/sensitive_platforms.toml` at construction; if the
    /// file is missing or fails to parse, the agent falls back to
    /// the empty table (no routing, existing behavior unchanged).
    /// The navigate tool handler consults this *before* the
    /// known-sites map so a host that's both in the map and
    /// sensitive (e.g. `instagram`) is routed via search instead
    /// of a direct nav.
    sensitive_platforms: mew_nav::SensitivePlatforms,
    /// Phase 6: resilience hook state. Tracks the *prior*
    /// iteration's page shape so the session-loss detector can
    /// tell "this page used to be a dashboard" from "the user
    /// navigated to /login on purpose". Updated at the end of
    /// every perception cycle by `page_looks_dashboard_like`.
    /// The "force re-snapshot" flag is set by the ref-recovery
    /// hook when a stale ref is auto-recovered; the perception
    /// block at the top of the next iteration checks it and
    /// takes a full snapshot instead of a diff.
    prior_was_dashboard_like: bool,
    /// Phase 6: the per-iteration resilience finding (if any).
    /// Read by the dispatch site (the irreversible gate uses
    /// it; the navigate site reads it for backoff). Set in
    /// the perception block, cleared at the top of the next
    /// perception cycle. `None` means "no finding this
    /// iteration" — the common case.
    pending_backoff_secs: Option<u64>,
    /// Phase 6: the modal-dismiss ref the perception step
    /// detected. When `Some`, the next tool dispatch is
    /// redirected to click this ref before the LLM's planned
    /// action. Cleared after consumption.
    pending_modal_dismiss: Option<String>,
    /// Phase 6: ref-recovery bookkeeping. Number of
    /// automatic retries for the current ref-drift event. The
    /// ref-recovery hook bumps this on each auto-retry;
    /// the dispatch site resets it on a successful action
    /// (i.e. not a stale-ref error).
    ref_recovery_attempts: u32,
    /// Phase 8: the most recent captcha / challenge page the
    /// resilience detector saw. Set by the perception-time
    /// hook on first detection; cleared on a subsequent
    /// `Continue` perception. Read by the dispatch / finish
    /// paths so the synthesized chat reply carries the
    /// user-actionable hint and so the loop unwinds
    /// gracefully to a `BrowserResult::failure` with a
    /// captcha-specific reason (the orchestrator's catch-all
    /// path, the only way to surface a "user, please solve
    /// this" chat message before the loop would otherwise
    /// sit in `Paused` indefinitely).
    pending_captcha: Option<mew_resilience::Challenge>,
    /// Phase 8: local-only captcha telemetry. `record()` is
    /// called from the resilience hook on every challenge
    /// detection; `flush()` is called at session end so the
    /// in-memory counters land in the on-disk JSON file
    /// before the agent drops. `summary()` is exposed to the
    /// CLI / Tauri layer for the user-visible "what
    /// challenges did we hit today?" line. Cheap to share
    /// (the inner `Mutex<Inner>` is `Send + Sync` and
    /// cloning the handle is `Arc` deep; the agent holds
    /// one handle, the CLI / Tauri command can hold
    /// another, both update the same in-memory state).
    captcha_telemetry: std::sync::Arc<captcha_telemetry::CaptchaTelemetry>,
}

impl Agent {
    /// Build a new `Agent`.
    ///
    /// `transcript_dir` (Phase 4, Bug 3 fix) controls where the on-disk
    /// transcript is written. When `None` (the default), the agent falls
    /// back to the historical behavior of writing to a relative
    /// `transcripts/` folder under the current working directory. That
    /// is fine for `mew-cli` and the example harnesses (they run from
    /// the workspace root, so `./transcripts/` lands at the repo root,
    /// gitignored by `/transcripts/` in `.gitignore`).
    ///
    /// The Tauri UI calls this with an absolute directory resolved from
    /// `app_handle.path().app_data_dir()` so the file lands in the
    /// OS-appropriate per-user app-data location and *outside* the
    /// `src-tauri/` tree. Tauri's dev-mode file watcher recursively
    /// watches `src-tauri/`, so any transcript write under
    /// `src-tauri/transcripts/` was triggering a `cargo run` rebuild
    /// that killed the running agent and Chrome and restarted the app
    /// (the Bug 3 restart loop the user observed).
    pub fn new(
        config: ProviderConfig,
        task: &str,
        transcript_dir: Option<std::path::PathBuf>,
    ) -> Self {
        // Phase 1: structured tracing is opt-in. The env var is read
        // here (not at the call site) so the CLI and the Tauri
        // wrapper don't both have to pass it through. The variable
        // name is fixed (`MEW_TRACING_DIR`) so an operator can flip
        // it on for one run without code changes.
        let tracing_dir = std::env::var_os("MEW_TRACING_DIR")
            .map(std::path::PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty());
        Self::new_with_tracing(config, task, transcript_dir, tracing_dir)
    }

    /// Phase 1: full constructor with explicit `tracing_dir`. Split
    /// from `new` so tests / programmatic callers can install the
    /// tracing layer without going through the env var, and so the
    /// Tauri wrapper can re-use the same path it already resolves
    /// for transcripts (one folder, two log files).
    pub fn new_with_tracing(
        config: ProviderConfig,
        task: &str,
        transcript_dir: Option<std::path::PathBuf>,
        tracing_dir: Option<std::path::PathBuf>,
    ) -> Self {
        // Phase 17.1: clone the pacing config out of `config`
        // before `config` is moved into `Self`. The pacing guard
        // is built from this clone; if we tried to read
        // `config.agent.pacing` after the move it'd be a use-after-
        // move error.
        let pacing_config = config.agent.pacing.clone();
        // Phase 5: same trick for the summarization config.
        // We hold the cloned value, build the live progress
        // buffer from `live_lines_cap`, then move the cloned
        // config into the struct.
        let summarization_cfg = config.agent.summarization.clone();
        let live_lines_cap = summarization_cfg.live_lines_cap;
        let system_prompt = "You are mew, a visible browser agent. You drive a real Chromium window.
You can perceive pages via accessibility-tree snapshots and take actions like click, type, scroll.
If you need to interact with an element that has no meaningful accessible name/role (e.g. an empty button or canvas), use the `vision_inspect` tool first to visually inspect it.
You must achieve the user's objective by observing the state, choosing a tool, and waiting for the next turn.
When you are completely done and have the final answer or outcome, call finish() with the result. CRITICAL RULE: If an action times out or fails (e.g., stale reference), NEVER report in finish() that the action 'was performed', 'succeeded', or 'was completed'. You must explicitly state that you attempted the action and it failed/timed out, and do not conflate the attempt with success.

COMPLETENESS PROTOCOL (mandatory for multi-item tasks):
If the task contains multiple similar sub-actions (e.g. 'do X for each of these N things', 'send a message to each person on the list', 'fill in N form fields'), you MUST at the start call the `declare_subtasks` tool with a short id and one-line description for each sub-item. The `items` parameter must be a JSON array of objects with `id` and `description` fields. The agent will track this list in code, not in your memory.
For each sub-item, after you take a fresh `snapshot()` and verify the expected state change on the page, call `mark_subtask_done(id, snapshot_signature)` where `snapshot_signature` is the value the snapshot tool returns in its result message (it looks like `len:0123abcd`). You cannot mark a subtask done without a fresh snapshot — the tool will reject any signature that doesn't match the most recent on-screen snapshot.
If you cannot complete a sub-item, call `mark_subtask_skipped(id, reason)` (deliberately out of scope) or `mark_subtask_failed(id, reason)` (attempted but could not verify). Both are accepted as terminal states.
Calling `finish()` while any subtask is still pending is intercepted: the agent will force a fresh snapshot and re-prompt you to either mark each pending item done/skipped/failed or explicitly justify it. The gate is not a suggestion.";

        let messages = vec![
            json!({
                "role": "system",
                "content": system_prompt
            }),
            json!({
                "role": "user",
                "content": format!("Task: {}", task)
            })
        ];

        let session_id = format!("session_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs());
        // Phase 17.1+ tidy: write transcripts to a dedicated
        // /transcripts/ subfolder instead of the project root.
        // Rationale: every mew run used to drop a
        // `transcript_<session_id>.log` next to the source, which
        // polluted the project root and made it easy to miss
        // genuine files. The folder is gitignored (see
        // .gitignore's `/transcripts/` rule) so the noise never
        // reaches git. We `create_dir_all` here so the first run
        // on a fresh checkout just works — no setup step needed.
        // If the directory can't be created (read-only fs, etc.)
        // we fall back to the in-memory path (transcript_file =
        // None) and keep running; nothing else in the agent
        // depends on the file being on disk.
        //
        // Phase 4 (Bug 3 fix): `transcript_dir` is an optional
        // override. When the Tauri UI calls us it passes an
        // absolute path resolved from `app_handle.path()
        // .app_data_dir()` so the file lands *outside* the Tauri
        // source tree — Tauri's dev-mode file watcher recursively
        // watches `src-tauri/`, and writing the transcript under
        // `src-tauri/transcripts/` was triggering a `cargo run`
        // rebuild that killed the running agent and Chrome and
        // restarted the app. See the matching `.taurignore` in
        // `mew-ui/src-tauri/` for the second layer of defense.
        let transcript_dir = transcript_dir
            .unwrap_or_else(|| std::path::PathBuf::from("transcripts"));
        let _ = std::fs::create_dir_all(&transcript_dir);
        let transcript_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(transcript_dir.join(format!("transcript_{}.log", session_id)))
            .ok();

        let session = SessionHandle::new(session_id.clone());

        // Phase 1: install the per-session structured tracing layer
        // when `tracing_dir` is provided. The install path is in
        // two stages:
        //   1. Try to install the layer as the *global* subscriber.
        //      This is the only way the layer can receive events
        //      from other threads (the Tauri runtime spawns
        //      browser-task work on a different thread, and the
        //      thread-local override below only affects the
        //      current thread). On the CLI, the global slot is
        //      usually already owned by `tracing_subscriber::fmt()`
        //      and this returns Err — expected.
        //   2. Always try to install the layer as a *thread-local*
        //      subscriber override. This is what actually wires
        //      the JSONL file into the `tracing::info!` event
        //      flow for the agent's own thread. Stacking the
        //      layer on top of the global fmt subscriber means
        //      every event goes to BOTH: human-readable stderr
        //      AND the structured JSONL file. The returned guard
        //      is held in `tracing_guard` for the agent's
        //      lifetime; dropping the agent drops the guard and
        //      the override reverts.
        //
        // Without the thread-local stage, the JSONL file would
        // only get the explicit "structured tracing active" line
        // written by the constructor — every `tracing::info!`
        // event emitted from inside the loop would go to the
        // global fmt subscriber and *not* the file. The smoke
        // test `test_phase1_tracing_fallback` proves this is the
        // case before the fix and proves it is fixed after.
        let tracing_layer = match tracing_dir.as_ref() {
            Some(dir) => {
                let path = crate::tracing_layer::session_log_path(dir, &session_id);
                match crate::tracing_layer::SessionJsonLayer::new(session_id.clone(), path.clone()) {
                    Ok(layer) => {
                        let layer = std::sync::Arc::new(layer);
                        // Stage 1: try global. Best-effort.
                        let _ = crate::tracing_layer::try_install_global(layer.clone());
                        // Stage 2: thread-local override. Always
                        // succeeds; the guard is held for the
                        // session's lifetime.
                        let guard = crate::tracing_layer::try_install_thread_local(layer.clone());
                        eprintln!(
                            "[mew-agent] Phase 1: structured tracing active -> {}",
                            path.display()
                        );
                        Some((layer, guard))
                    }
                    Err(e) => {
                        eprintln!(
                            "[mew-agent] Phase 1: failed to open trace log at {}: {}",
                            path.display(),
                            e
                        );
                        None
                    }
                }
            }
            None => None,
        };
        let (tracing_layer, tracing_guard) = match tracing_layer {
            Some((layer, guard)) => (Some(layer), Some(guard)),
            None => (None, None),
        };
        let tracing_enabled = tracing_layer.is_some();

        // Record the initial "Start" transition to the transcript so the log
        // is never empty. The handle already pushed a Start record into its
        // own history; we mirror that to the file transcript here.
        if let Some(mut file) = transcript_file.as_ref() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let entry = format!(
                "[{}] [{}] STATE: -> Running (start)\n\n",
                now, session_id
            );
            let _ = file.write_all(entry.as_bytes());
        }

        Self {
            config,
            messages,
            client: reqwest::Client::new(),
            total_tokens: 0,
            iterations: 0,
            state: Arc::new(Mutex::new(PerceptionState::new())),
            session_id,
            current_url: None,
            transcript_file,
            force_snapshot: false,
            // Phase 5: live progress buffer. The cap is read
            // from the user-configured `live_lines_cap`
            // (default 5). The buffer is the source of truth
            // for "what just happened" lines the frontend
            // shows in the per-task live progress sub-list.
            live_progress: summarizer::LiveProgress::new(live_lines_cap),
            summarization_cfg,
            session,
            bus: Some(MessageBus::new()),
            completeness: CompletenessTracker::new(),
            finish_calls_this_gate: 0,
            pending_finish_result: None,
            summary_written: false,
            // Phase 17.1: build the pacing guard from the
            // `agent.pacing` block in config.yaml. When the block
            // is absent or `enabled: false`, the guard is a no-op
            // for the lifetime of the agent — no dispatch site has
            // to special-case it.
            pacing: PacingGuard::new(pacing_config),
            // Phase 3.3 (regression): empty queue. Filled by the
            // third-drain (post-LLM-call) and flushed by the next
            // iteration's top drain. See the field doc for why.
            pending_user_note_writes: Vec::new(),
            event_tx: None,
            // Phase 1: populated by the install block above. When
            // `tracing_dir` is `Some`, `tracing_layer` is a working
            // handle to the per-session JSON file writer and
            // `tracing_guard` keeps the thread-local override live
            // for the agent's lifetime. When `None`, both fields
            // stay at their default and the loop incurs zero
            // overhead at every dispatch site.
            tracing_layer,
            tracing_enabled,
            tracing_guard,
            // Phase 2: load the sensitive-platform routing table.
            // The loader walks parents like `load_config` does, so
            // it works from any CWD. If the file is missing (the
            // pre-Phase-2 default) we get an empty table and the
            // existing resolver branches run unchanged.
            sensitive_platforms: mew_nav::SensitivePlatforms::load_from_default_location(),
            // Phase 6: no prior page is "dashboard-like" at the
            // start of a session — the first navigation defines
            // the baseline. The session-loss detector treats the
            // first iteration as "fresh navigation" so a login
            // form on the very first page (the common case for
            // "log in to your account" tasks) is not misclassified
            // as a session-loss event.
            prior_was_dashboard_like: false,
            // Phase 6: no backoff pending at session start.
            pending_backoff_secs: None,
            // Phase 6: no modal to dismiss at session start.
            pending_modal_dismiss: None,
            // Phase 6: no in-flight ref-recovery retry.
            ref_recovery_attempts: 0,
            // Phase 8: no captcha pending at session start.
            // The resilience hook sets this on the first
            // captcha / challenge detection; the dispatch and
            // finish paths read it to produce a captcha-
            // specific user-facing chat message.
            pending_captcha: None,
            // Phase 8: local-only captcha telemetry. Loaded
            // from `<transcript_dir>/captcha_telemetry.json`
            // when present (so a returning user sees the
            // cumulative counts), or empty otherwise. The
            // `Arc` is so a future Tauri command or CLI
            // subcommand can hold a sibling handle and
            // read the summary without going through
            // the agent.
            //
            // Using the same `transcript_dir` as the
            // transcripts keeps the on-disk artifact
            // set small and colocated: one folder,
            // two log files (transcripts + telemetry).
            // The Tauri layer overrides `transcript_dir`
            // to the OS app-data dir; the telemetry
            // path follows the same override.
            captcha_telemetry: {
                let telemetry_path = captcha_telemetry::default_persist_path(&transcript_dir);
                std::sync::Arc::new(
                    captcha_telemetry::CaptchaTelemetry::load_or_default(Some(telemetry_path)),
                )
            },
        }
    }

    pub fn with_event_sender(mut self, tx: tokio::sync::mpsc::UnboundedSender<AgentEvent>) -> Self {
        self.event_tx = Some(tx);
        self
    }

    /// Phase 5: produce a templated one-liner for a tool
    /// dispatch and push it to the live progress buffer +
    /// emit it on the event channel.
    ///
    /// Called once per tool call by the post-match block in
    /// `run_inner`, immediately after the existing
    /// `AgentEvent::Tool` emission. The function is
    /// deliberately a no-op for unknown tool names — the
    /// templated path is the source of truth, and any
    /// custom tool added later just needs to land on
    /// `summarizer::summarize`'s known list to be
    /// surfaced in the UI.
    ///
    /// Returns the `ProgressLine` that was pushed (and
    /// emitted) so callers that need to use it for a follow-up
    /// step (the end-of-task LLM call) don't have to re-derive
    /// it.
    fn emit_progress_line(
        &mut self,
        name: &str,
        args: &serde_json::Value,
        result: &str,
    ) -> Option<ProgressLine> {
        let (kind, text, success) = summarizer::summarize(
            name,
            args,
            result,
            self.summarization_cfg.verbosity,
        )?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let line = ProgressLine::new(kind, text, ts, success);
        let pushed = self.live_progress.push(line);
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(AgentEvent::ProgressLine {
                timestamp_secs: pushed.timestamp_secs,
                text: pushed.text.clone(),
                kind: pushed.kind.as_str().to_string(),
                success: pushed.success,
            });
        }
        Some(pushed)
    }

    /// Phase 5: cheap accessor for the live progress
    /// buffer's snapshot. Used by the end-of-task LLM
    /// summarizer (which wants "what was the agent
    /// actually doing?") and by tests.
    pub fn live_progress_snapshot(&self) -> Vec<ProgressLine> {
        self.live_progress.snapshot()
    }

    /// Phase 5: end-of-task LLM call. Produces a 1-2 sentence
    /// user-facing reply by asking the model to summarize the
    /// task + the recent live progress. Falls back to the raw
    /// `finish()` text on any error (LLM down, parse error,
    /// etc.) so the user always sees *something* — the
    /// "never silent on the error path" guarantee is
    /// preserved.
    ///
    /// The call is bounded: a short system prompt, max_tokens
    /// 200, and a single user message with the task and the
    /// last `at_most` progress lines. Total latency is
    /// dominated by the LLM's first-token time (typically
    /// ~600ms for a small model on a 7B-class host).
    ///
    /// `None` is returned when `summarization.end_of_task_llm_summary`
    /// is `false` (the user opted out). In that case the
    /// caller uses the raw `finish()` text as-is.
    pub async fn end_of_task_summarize(
        &self,
        task_description: &str,
        raw_finish_text: &str,
    ) -> Option<String> {
        if !self.summarization_cfg.end_of_task_llm_summary {
            return None;
        }
        // Phase 10.4 fast-path: if the raw finish() text is
        // already short and chat-shaped, the LLM rewriter is
        // wasted work — the user will see the same string either
        // way. We pass the text through unchanged by returning
        // `Some(text)` so the caller's `match` arm picks the
        // LLM path (and avoids the `_ => res.clone()` fallback
        // that would also work but reads less clearly in the
        // trace).
        //
        // Heuristic: short (<= 280 chars), starts with a
        // non-template character (no leading "I clicked",
        // "I typed", "I navigated" — those are templated
        // transcript summaries, exactly the kind of text the
        // rewriter exists to clean up), no embedded JSON, and
        // no embedded tool-call traces. The "no JSON" check is
        // the only one with a real cost: a single pass for `{`
        // or `[` at line start.
        if let Some(passthrough) = self.try_passthrough_finish(raw_finish_text) {
            tracing::debug!(
                event = "end_of_task_summary_passthrough",
                len = raw_finish_text.len(),
                "raw finish() text already chat-shaped; skipping LLM rewriter"
            );
            return Some(passthrough);
        }
        let recent = self.live_progress.recent_text(20);
        let system = summarizer::end_of_task_prompt(task_description, &recent);
        let body = serde_json::json!({
            "model": self.config.opencode_zen.default_model,
            "max_tokens": 200,
            "temperature": 0.2,
            "messages": [
                { "role": "system", "content": system },
                {
                    "role": "user",
                    "content": format!(
                        "Now write the user-facing reply. Reference what was done concretely. The raw finish() text the agent produced is below; do NOT copy it verbatim — write a natural reply. Raw finish text: {}\n\nReply (1-2 sentences, plain text, no JSON, no preamble):",
                        raw_finish_text
                    ),
                }
            ]
        });
        let url = format!("{}/chat/completions", self.config.opencode_zen.base_url);
        let res = match self
            .client
            .post(&url)
            .header(
                "Authorization",
                format!("Bearer {}", self.config.opencode_zen.api_key),
            )
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    event = "end_of_task_summary_failed",
                    error = %e,
                    "end-of-task LLM call failed at the HTTP layer; falling back to raw finish() text"
                );
                return None;
            }
        };
        if !res.status().is_success() {
            tracing::warn!(
                event = "end_of_task_summary_failed",
                status = %res.status(),
                "end-of-task LLM call returned non-success; falling back to raw finish() text"
            );
            return None;
        }
        let json: serde_json::Value = match res.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    event = "end_of_task_summary_failed",
                    error = %e,
                    "end-of-task LLM call returned an unparseable body; falling back to raw finish() text"
                );
                return None;
            }
        };
        // Standard OpenAI-shape: `choices[0].message.content`.
        let text = json
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|m| m.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if text.is_empty() {
            return None;
        }
        Some(text)
    }

    /// Phase 10.4 fast-path helper: decide whether the raw
    /// `finish()` text the agent emitted is already good
    /// enough to ship to the user as-is, without burning an
    /// LLM call to rewrite it. Returns `Some(text)` when the
    /// text passes the heuristic, `None` when the rewriter
    /// should still run.
    ///
    /// The rules (intentionally conservative — a false negative
    /// just means we fire the LLM call, a false positive would
    /// leak templated transcript to the user):
    ///
    ///   1. **Short.** <= 280 characters. Templated transcripts
    ///      (the "I clicked X. I typed Y. I called finish()"
    ///      shape the agent's system prompt encourages for
    ///      multi-step tasks) are usually longer than that.
    ///   2. **No template-y prefix.** A leading "I clicked",
    ///      "I typed", "I navigated", "I scrolled", or
    ///      "I performed" is the agent narrating its tool
    ///      calls — exactly the kind of text the rewriter
    ///      exists to clean up. We pass those through *only*
    ///      when the text is very short (<= 120 chars) AND
    ///      has no embedded tool name.
    ///   3. **No JSON.** An embedded `{` at line start or a
    ///      `[` at line start means the agent dumped a
    ///      structured payload (the "I will reply with raw
    ///      JSON" failure mode) — the rewriter must clean
    ///      that up.
    ///   4. **No "TOOL CALL" / "TRANSCRIPT" trace marker.**
    ///      These only appear when the agent is leaking the
    ///      raw transcript into the finish() text.
    ///
    /// Each rule is one cheap check; the function is `O(n)` in
    /// the text length with a small constant.
    pub fn try_passthrough_finish(&self, raw: &str) -> Option<String> {
        // Phase 10.4: delegate to the pure module so the unit
        // tests can lock the heuristic in place without
        // constructing a full `Agent`. See
        // `mew_agent::end_of_task_passthrough::passthrough_check`
        // for the rule list and rationale.
        crate::end_of_task_passthrough::passthrough_check(raw)
    }

    /// Phase 1: install the structured per-session tracing layer.
    /// Must be called *before* `run()` and only once. Returns `Self`
    /// so it can be chained after `new`/`new_with_tracing`. The
    /// returned `Arc<SessionJsonLayer>` is also stashed on the agent
    /// so other call sites (the URL resolver, the session handle)
    /// can read it cheaply via `tracing_layer()`.
    pub fn with_tracing_layer(
        mut self,
        layer: Arc<crate::tracing_layer::SessionJsonLayer>,
    ) -> Self {
        self.tracing_layer = Some(layer);
        self.tracing_enabled = true;
        self
    }

    /// Phase 1: cheap accessor. Returns `true` if the structured
    /// tracing layer is installed — used at hot-path dispatch sites
    /// to decide whether to emit a `tracing::info!` line. Returning
    /// `false` means the loop pays for one branch and zero JSON
    /// serialization per call.
    pub fn tracing_enabled(&self) -> bool {
        self.tracing_enabled
    }

    /// Phase 1: read-only handle to the tracing layer. Used by the
    /// URL resolution span (it lives in `mew-nav`, not `mew-agent`,
    /// but the agent's caller passes the layer through so the
    /// resolution span can attach to the session's file).
    pub fn tracing_layer(&self) -> Option<&Arc<crate::tracing_layer::SessionJsonLayer>> {
        self.tracing_layer.as_ref()
    }

    /// Borrow the session handle. External code (CLI, future UI thread, the
    /// Step 13 chat reader) calls `pause()`/`resume()`/`stop()` on the
    /// returned clone.
    pub fn session_handle(&self) -> SessionHandle {
        self.session.clone()
    }

    /// Phase 13.1: hand the sender half of the message bus to a caller
    /// (typically the CLI's stdin reader thread). The agent loop keeps the
    /// receiver and drains it between iterations. Called once at startup,
    /// before `run()`. Returns the tokio mpsc sender — clones are cheap,
    /// so a future UI thread or signal handler can also call this if
    /// multiple input sources are needed.
    pub fn take_message_sender(&mut self) -> tokio::sync::mpsc::Sender<UserMessage> {
        let bus = self
            .bus
            .as_mut()
            .expect("MessageBus already taken or not initialized");
        bus.take_sender()
    }

    /// Phase 13.1: borrow the bus (mutable) so the loop can drain pending
    /// messages at the checkpoint. Returns `None` if the bus was never
    /// created (e.g. a future test that wants to run the agent with no
    /// chat input at all).
    fn bus_mut(&mut self) -> Option<&mut MessageBus> {
        self.bus.as_mut()
    }

    /// Phase 13.1: test/integration helper. Public so the example tests
    /// in `mew-agent/examples/` can drive the real `Agent`'s chat path
    /// without spinning up Chrome. The CLI never calls this — the real
    /// loop calls `drain_and_apply_user_messages` directly each
    /// iteration.
    pub fn drain_user_messages(&mut self) -> Vec<UserMessage> {
        match self.bus.as_mut() {
            Some(b) => b.drain_pending(),
            None => Vec::new(),
        }
    }

    /// Phase 13.1: test/integration helper. Append a user note exactly
    /// the way `drain_and_apply_user_messages` does, so example tests
    /// can drive the real Agent and inspect the resulting history.
    /// Not called by the real loop — it goes through the drain path.
    pub fn apply_user_message_for_test(&mut self, msg: &UserMessage) {
        self.messages.push(json!({
            "role": "user",
            "content": format!("[user note while task is running] {}", msg.text),
        }));
        if let Some(mut file) = self.transcript_file.as_ref() {
            let line = format!(
                "[{}] [{}] USER: {}\n\n",
                msg.timestamp_secs, self.session_id, msg.text
            );
            let _ = file.write_all(line.as_bytes());
        }
    }

    /// Phase 13.1: test/integration helper. Read-only view of the
    /// conversation history. Used by example tests to assert messages
    /// were appended.
    pub fn history_snapshot(&self) -> Vec<serde_json::Value> {
        self.messages.clone()
    }

    /// Read-only accessor for the session id. Used by example tests
    /// to find the transcript file path on disk.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Phase 8: read-only handle to the captcha telemetry. The
    /// CLI / Tauri command holds a sibling `Arc` clone so a
    /// future Tauri command can call `summary()` without
    /// going through the agent. The handle is cheap to
    /// clone (`Arc` deep) and the snapshot read is
    /// `O(rows)`.
    pub fn captcha_telemetry(&self) -> std::sync::Arc<captcha_telemetry::CaptchaTelemetry> {
        self.captcha_telemetry.clone()
    }

    /// Phase 2: pre-flight decomposition. Called exactly once
    /// from the top of `run_inner` before the first iteration.
    /// Pure-Rust, no network, no LLM cost.
    ///
    /// Steps:
    ///   1. Read the original task string from the user-role
    ///      message in `self.messages` (the constructor put it
    ///      there as `"Task: {task}"`; we strip the prefix).
    ///   2. Call `planner::plan(&task)`. The deterministic rules
    ///      split the task on ` and ` / `, ` / ` then ` / `; ` /
    ///      ` & ` / ` + ` and produce a `Plan` with one
    ///      `SubTask` per clause.
    ///   3. Seed the `CompletenessTracker` with the plan's
    ///      subtasks via `declare`. After this, the tracker owns
    ///      the canonical list and the LLM's later
    ///      `declare_subtasks` calls can only re-declare while
    ///      every subtask is still Pending (the existing rule).
    ///   4. Append a `PLAN:` block to the system prompt so every
    ///      subsequent LLM call sees the broken-down subtasks.
    ///      The LLM is still free to amend (e.g. "step-1 has a
    ///      sub-step-1a") but the canonical list is the code's.
    ///   5. Write a `PREFLIGHT:` line to the transcript and emit
    ///      a `preflight_plan` tracing event.
    ///
    /// Returns `Err` only on a malformed messages list (no
    /// system + user pair). The deterministic planner itself
    /// never fails — even an empty task produces an empty plan
    /// with a clear rationale.
    fn run_preflight_plan(&mut self) -> anyhow::Result<()> {
        // Find the task string. The constructor builds
        // `messages[0] = system`, `messages[1] = user: "Task: {task}"`.
        // We tolerate a missing "Task: " prefix so a future
        // refactor of the message format doesn't silently break
        // the planner — we just use the whole content as the
        // task.
        let task = self
            .messages
            .iter()
            .find(|m| {
                m.get("role")
                    .and_then(|r| r.as_str())
                    .map(|r| r == "user")
                    .unwrap_or(false)
            })
            .and_then(|m| m.get("content").and_then(|c| c.as_str()))
            .map(|s| s.strip_prefix("Task: ").unwrap_or(s).to_string())
            .ok_or_else(|| anyhow::anyhow!("preflight: no user-role message found"))?;

        let plan = crate::planner::plan(&task);

        // Seed the tracker. Empty plans are valid (the LLM may
        // not want to declare anything, and the tracker treats
        // an empty subtask list as "gate is a no-op" — same
        // behavior as today).
        if !plan.subtasks.is_empty() {
            let n = self.completeness.declare(plan.subtasks.clone())
                .map_err(|e| anyhow::anyhow!("preflight: declare failed: {e}"))?;
            tracing::info!(
                event = "preflight_plan",
                subtask_count = n,
                escalated = plan.escalated,
                rationale = %plan.rationale,
                "pre-flight decomposition produced a plan"
            );
        } else {
            tracing::info!(
                event = "preflight_plan",
                subtask_count = 0,
                escalated = plan.escalated,
                rationale = %plan.rationale,
                "pre-flight decomposition produced an empty plan"
            );
        }

        // Append the `PLAN:` block to the system prompt. We
        // do this even when the plan is empty — the literal
        // line `PLAN: (none — single undifferentiated task)`
        // makes it unambiguous to the LLM that no subtask
        // tracking is expected, which is itself useful.
        let plan_block = render_plan_block(&plan);
        if let Some(sys) = self.messages.first_mut() {
            // `serde_json::Value` doesn't expose a mutable string
            // accessor, so we read the current content, build the
            // new one, and overwrite the field. The `content` key
            // is always a String (the constructor wrote it that
            // way); we treat anything else as a no-op rather than
            // panicking on a future format change.
            let current = sys
                .get("content")
                .and_then(|c| c.as_str())
                .map(str::to_string);
            if let Some(current) = current {
                let new_content = format!("{}\n\n{}", current, plan_block);
                if let Some(obj) = sys.as_object_mut() {
                    obj.insert("content".to_string(), serde_json::Value::String(new_content));
                }
            }
        }

        // Transcript line. Mirrors the `[ts] [session_id]`
        // format used elsewhere so a transcript reviewer can
        // find the preflight easily.
        if let Some(mut file) = self.transcript_file.as_ref() {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let line = format!(
                "[{}] [{}] PREFLIGHT: subtasks={} escalated={} rationale=\"{}\"\n\n",
                ts,
                self.session_id,
                plan.subtasks.len(),
                plan.escalated,
                plan.rationale
            );
            let _ = file.write_all(line.as_bytes());
        }

        Ok(())
    }

    /// Phase 15.1: test helper. Build a minimal `Agent` without
    /// writing a transcript file or loading a real provider config.
    /// Used by `examples/test_completeness.rs` to drive the
    /// completeness surface in isolation — no Chrome, no LLM, no
    /// transcript side-effects. The real CLI never calls this.
    #[doc(hidden)]
    pub fn new_for_test(task: &str) -> Self {
        let dummy_config = ProviderConfig {
            opencode_zen: crate::OpencodeZenConfig {
                base_url: "http://test/".into(),
                api_key: "test".into(),
                default_model: "test".into(),
                max_iterations: 1,
                max_tokens: None,
                max_cost: None,
            },
            browser: None,
            agent: crate::AgentConfig::default(),
        };
        let mut s = Self::new(dummy_config, task, None);
        // Drop the on-disk transcript so the test doesn't leave
        // artifacts behind. The test calls `write_summary` against a
        // temp file it owns.
        s.transcript_file = None;
        // Phase 17.1: also force the pacing guard to disabled. The
        // default `AgentConfig::default()` leaves `pacing.enabled`
        // as false, but we replace the guard with the explicit
        // `disabled()` constructor so tests that incidentally call
        // into the pacing path can't be affected by a future change
        // to the default.
        s.pacing = PacingGuard::disabled();
        // Phase 3.3 (regression): the new field is already
        // initialized to an empty Vec by `Self::new`; nothing to do
        // here. (If a test wants to assert on the queue, it can
        // mutate the field directly via the existing accessor
        // pattern.)
        s
    }

    /// Phase 15.1: test helper. Mutable accessor for the
    /// `CompletenessTracker` so the example tests can drive the
    /// `record_snapshot` / `mark_done` / `write_summary` paths the
    /// real loop drives. Real callers go through the tool handlers.
    #[doc(hidden)]
    pub fn completeness_mut(&mut self) -> &mut CompletenessTracker {
        &mut self.completeness
    }

    /// Phase 15.1: test helper. Read-only accessor for the
    /// completeness tracker. Some test paths only need to read.
    #[doc(hidden)]
    pub fn completeness(&self) -> &CompletenessTracker {
        &self.completeness
    }

    /// Phase 15.1: test helper. Read-only accessor for the
    /// session id string. The test uses it as the
    /// `write_summary` session_id argument. (Distinct from
    /// `session_id()` which returns `&str` from `&self`; the
    /// test wants the same thing but it's renamed here for
    /// clarity in the test file.)
    #[doc(hidden)]
    pub fn session_id_for_test(&self) -> &str {
        &self.session_id
    }

    /// Phase 17.1: test helper. Mutable accessor for the
    /// `PacingGuard` so the example test (`test_pacing.rs`)
    /// can swap the default-disabled guard for an enabled one
    /// with custom range / threshold settings. The CLI never
    /// calls this — pacing config comes from `config.yaml`'s
    /// `agent.pacing` block.
    #[doc(hidden)]
    pub fn pacing_mut_for_test(&mut self) -> &mut PacingGuard {
        &mut self.pacing
    }

    /// Phase 13.1: test/integration helper. Trigger the same
    /// truncate-with-notes-preservation the real loop uses on
    /// navigation. Used by example tests to assert the preservation
    /// behavior on the real Agent, not just on a stand-in simulator.
    pub fn truncate_for_test(&mut self, keep_front: usize) {
        self.truncate_preserving_user_notes(keep_front);
    }

    /// Best-effort: write a state transition line to the transcript file,
    /// using the format defined on `SessionHandle::format_transition_line` so
    /// tests and the live agent agree on the exact shape.
    fn write_state_line(&self, record: &crate::session::TransitionRecord) {
        if let Some(mut f) = self.transcript_file.as_ref() {
            let reason_part = record
                .reason
                .as_deref()
                .map(|r| format!(" reason={}", r))
                .unwrap_or_default();
            let line = format!(
                "[{}] [{}] STATE: {} -> {} ({}){}\n\n",
                record.timestamp_secs,
                self.session_id,
                record.from.as_str(),
                record.to.as_str(),
                record.kind.as_str(),
                reason_part
            );
            let _ = f.write_all(line.as_bytes());
        }
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(AgentEvent::State(record.clone()));
        }
    }

    /// Phase 13.1: drain any pending user-typed messages and fold them into
    /// the running conversation history as `role: user` entries, plus log
    /// them to the transcript. The LLM on the *next* iteration sees them
    /// alongside the freshest snapshot/diff, with no task restart, no
    /// state wipe, and no special-cased "injection" system prompt.
    ///
    /// Empty-drain is a true no-op: this is the per-iteration path when the
    /// user has typed nothing, and it must add zero latency and zero log
    /// noise (per the 13.2 spec).
    /// `pub` so the example tests in `mew-agent/examples/` can
    /// drive the real per-iteration drain path the live loop uses. The
    /// CLI never calls this directly — only the ReAct loop in `run_inner`.
    pub fn drain_and_apply_user_messages(&mut self) {
        // Phase 3.3 (regression): flush any user-note file writes
        // deferred from a previous iteration's mid-flight drain
        // *before* draining the live bus. The flush goes to disk
        // first so any line numbers from the prior iteration are
        // committed before the new observation/user-note lines are
        // appended. This keeps the on-disk order consistent with
        // the chronological order of events.
        self.flush_pending_user_note_writes();

        // No bus attached => no chat input => nothing to do.
        let pending = match self.bus_mut() {
            Some(b) => b.drain_pending(),
            None => return,
        };
        if pending.is_empty() {
            return;
        }

        // Count so the println + transcript can show "N messages applied"
        // without listing every line on the console (which would be noisy
        // for a 4-message burst).
        let count = pending.len();
        println!(
            "[chat] {} pending user message(s) from stdin — folding into history",
            count
        );

        for msg in pending {
            // Echo to the console so the user sees their own message land
            // in the conversation, in order. This is the "you can type here
            // anytime" affordance working as intended.
            println!("[chat] user: {}", msg.text);

            // Append to the live history as a real `role: user` message.
            // The system prompt is unchanged — this is exactly how the
            // user-typed first message got in, so subsequent injections
            // look identical to the LLM.
            self.messages.push(json!({
                "role": "user",
                "content": format!("[user note while task is running] {}", msg.text),
            }));

            // Mirror to the transcript file. Same shape as a tool call
            // entry, so the transcript can be searched with the same tools.
            if let Some(mut file) = self.transcript_file.as_ref() {
                let line = format!(
                    "[{}] [{}] USER: {}\n\n",
                    msg.timestamp_secs, self.session_id, msg.text
                );
                let _ = file.write_all(line.as_bytes());
            }
        }
    }

    /// Phase 3.3 (regression): drain the live chat channel exactly like
    /// `drain_and_apply_user_messages` does, but **defer** the
    /// transcript file write to the *next* iteration's top-of-iter
    /// drain.
    ///
    /// This is the third drain in the loop, called immediately after
    /// the LLM call returns and the assistant message has been pushed
    /// to `self.messages`, but *before* the in-flight tool is
    /// dispatched. The use case is: a user-typed redirect arrives
    /// during the LLM call; without this drain, the redirect would
    /// wait until the next iteration's top drain, by which time the
    /// in-flight tool may have called `finish()` and the session
    /// would end without ever seeing the redirect.
    ///
    /// We still want the on-disk transcript to reflect the
    /// chronological order: LLM-decided-tool first, then user-typed
    /// redirect, then tool-dispatched. Writing the file here would
    /// put the redirect ahead of the in-flight tool's `TOOL CALL:`
    /// line, breaking that order. So we queue the file write and let
    /// the next iteration's top drain flush it after the in-flight
    /// tool's line has been written.
    ///
    /// If the session terminates *before* the next iteration's top
    /// drain runs (e.g. the in-flight tool was `finish()` and the
    /// loop exits), the queue is flushed in `run`'s terminal
    /// transition block so the user note still appears in the
    /// transcript.
    pub fn drain_and_apply_user_messages_defer_file(&mut self) {
        // No bus attached => no chat input => nothing to do.
        let pending = match self.bus_mut() {
            Some(b) => b.drain_pending(),
            None => return,
        };
        if pending.is_empty() {
            return;
        }

        let count = pending.len();
        println!(
            "[chat] {} pending user message(s) — folding into history (deferred file write)",
            count
        );

        for msg in pending {
            println!("[chat] user: {}", msg.text);

            // Always append to `self.messages` so the LLM in the
            // *next* iteration sees the user note. The file write
            // is what we defer.
            self.messages.push(json!({
                "role": "user",
                "content": format!("[user note while task is running] {}", msg.text),
            }));

            // Queue the file write for the next iteration's
            // top-of-iter drain to flush, OR for the terminal
            // cleanup in `run` if no next iteration runs.
            self.pending_user_note_writes
                .push((msg.timestamp_secs, msg.text));
        }
    }

    /// Phase 3.3 (regression): flush any deferred user-note file
    /// writes to the transcript. Called by the top-of-iteration
    /// drain and by the terminal cleanup in `run`.
    fn flush_pending_user_note_writes(&mut self) {
        if self.pending_user_note_writes.is_empty() {
            return;
        }
        if let Some(mut file) = self.transcript_file.as_ref() {
            for (ts, text) in self.pending_user_note_writes.drain(..) {
                let line = format!("[{}] [{}] USER: {}\n\n", ts, self.session_id, text);
                let _ = file.write_all(line.as_bytes());
            }
        } else {
            // No file open (e.g. a test using `new_for_test`); just
            // drop the queued writes — they were never going to
            // land on disk anyway.
            self.pending_user_note_writes.clear();
        }
    }

    /// Phase 1: tiny helper that returns the list of tool names
    /// exposed to the LLM. Used as a span field on the LLM call so
    /// the trace log records which tool surface the model is being
    /// asked to pick from at this iteration. We do not log the
    /// parameter schemas here — the LLM already knows them, and the
    /// list alone is enough to tell "did the schema change mid-session?"
    fn tool_names_for_log(&self) -> Vec<&'static str> {
        vec![
            "navigate",
            "click",
            "type",
            "scroll",
            "press_key",
            "snapshot",
            "vision_inspect",
            "finish",
            "declare_subtasks",
            "mark_subtask_done",
            "mark_subtask_skipped",
            "mark_subtask_failed",
        ]
    }

    fn get_tools_schema(&self) -> serde_json::Value {
        json!([
            {
                "type": "function",
                "function": {
                    "name": "navigate",
                    "description": "Navigate to a site. Pass the bare name (e.g. 'instagram', 'wikipedia', 'anthropic') or a full URL. The resolver rewrites bare names and routes sensitive platforms (instagram, x/twitter, facebook, linkedin, tiktok) through a Google search entry automatically. Do NOT pass 'site:' operators, search-engine query strings, or the resolved google.com URL — pass the user's actual site name or URL and the resolver will produce the right thing. Examples: navigate(url='instagram'), navigate(url='https://anthropic.com'), navigate(url='openai').",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "url": {
                                "type": "string",
                                "description": "A bare site name (e.g. 'instagram') or a full URL (e.g. 'https://anthropic.com'). Do not include 'site:' operators, query strings, or already-resolved google.com URLs."
                            }
                        },
                        "required": ["url"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "click",
                    "description": "Click an element by its ref_id",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "ref": { "type": "string" }
                        },
                        "required": ["ref"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "type",
                    "description": "Type text into an element by its ref_id",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "ref": { "type": "string" },
                            "text": { "type": "string" }
                        },
                        "required": ["ref", "text"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "scroll",
                    "description": "Scroll the page",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "direction": { "type": "string", "enum": ["up", "down"] }
                        },
                        "required": ["direction"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "press_key",
                    "description": "Press a key",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "key": { "type": "string" }
                        },
                        "required": ["key"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "snapshot",
                    "description": "Take a snapshot to observe page changes"
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "vision_inspect",
                    "description": "Inspect a region visually if it lacks an accessible name/role. Returns visual description.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "ref": { "type": "string" }
                        },
                        "required": ["ref"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "finish",
                    "description": "Complete the task with a final result. Subject to the completeness gate: if you previously called declare_subtasks, every subtask must be in a terminal state (done/skipped/failed) before this call will be honored. The first finish() call while any subtask is still pending is intercepted: the agent will force a fresh snapshot and re-prompt you to resolve each pending item.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "result": { "type": "string" }
                        },
                        "required": ["result"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "declare_subtasks",
                    "description": "Declare the list of sub-items this task contains. Call this once at the start of a multi-item task with one entry per sub-item. The agent will track this list in code and require each item to be marked done (with a fresh snapshot as evidence), skipped, or failed before finish() is honored. If the task is a single undifferentiated unit, you do not need to call this tool.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "items": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "id": { "type": "string", "description": "Short identifier for this sub-item, e.g. an underscore-or-hyphen name like msg_to_alice" },
                                        "description": { "type": "string", "description": "One-line description of the sub-item" }
                                    },
                                    "required": ["id", "description"]
                                }
                            }
                        },
                        "required": ["items"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "mark_subtask_done",
                    "description": "Mark a previously-declared subtask as done. Requires a fresh snapshot to have been taken since the last mark; the call will be rejected with a stale-evidence error if not. Pass the `snapshot_signature` returned by the most recent snapshot.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string", "description": "The subtask id (as declared in declare_subtasks)" },
                            "snapshot_signature": { "type": "string", "description": "The page-state signature from the most recent snapshot" }
                        },
                        "required": ["id", "snapshot_signature"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "mark_subtask_skipped",
                    "description": "Mark a subtask as deliberately skipped (out of scope, not applicable, or the user already handled it). Provide a short reason. Skipped is a terminal status and counts as resolved for the finish() gate.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "reason": { "type": "string" }
                        },
                        "required": ["id", "reason"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "mark_subtask_failed",
                    "description": "Mark a subtask as failed (attempted but could not verify success on screen). Provide a short reason describing what was tried and what went wrong. Failed is a terminal status and counts as resolved for the finish() gate, but will be reported in the per-subtask end-of-session summary.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "reason": { "type": "string" }
                        },
                        "required": ["id", "reason"]
                    }
                }
            }
        ])
    }

    /// Phase 13.1: when a navigation (or full-replace) wipes most of the
    /// conversation history, preserve any user-typed steering notes so the
    /// LLM in the next iteration still sees them. Without this helper, a
    /// note typed just before a navigation would be silently dropped by
    /// `truncate(2)`, defeating the "no state wipe" guarantee.
    ///
    /// The convention is the prefix written in `drain_and_apply_user_messages`.
    /// Anything that doesn't match the system / task / steering-note shape
    /// is dropped along with the rest of the trimmed window.
    const USER_NOTE_PREFIX: &'static str = "[user note while task is running]";

    fn truncate_preserving_user_notes(&mut self, keep_front: usize) {
        if self.messages.len() <= keep_front {
            return;
        }
        let head: Vec<serde_json::Value> = self.messages.drain(..keep_front).collect();
        let kept_notes: Vec<serde_json::Value> = self
            .messages
            .iter()
            .filter(|m| {
                m.get("role").and_then(|r| r.as_str()) == Some("user")
                    && m.get("content")
                        .and_then(|c| c.as_str())
                        .map(|c| c.starts_with(Self::USER_NOTE_PREFIX))
                        .unwrap_or(false)
            })
            .cloned()
            .collect();
        let mut rebuilt = head;
        rebuilt.extend(kept_notes);
        self.messages = rebuilt;
    }

    fn trim_in_page_history(&mut self, k: usize) {
        // Keep all `system` and `user` messages (diffs).
        // Keep only the last `k` pairs of `assistant` and `tool` messages.
        let mut assistant_tool_indices = Vec::new();
        for (i, msg) in self.messages.iter().enumerate() {
            if let Some(role) = msg.get("role").and_then(|r| r.as_str()) {
                if role == "assistant" || role == "tool" {
                    assistant_tool_indices.push(i);
                }
            }
        }

        let keep_count = k * 2;
        if assistant_tool_indices.len() > keep_count {
            let drop_count = assistant_tool_indices.len() - keep_count;
            let indices_to_drop: std::collections::HashSet<_> = assistant_tool_indices.into_iter().take(drop_count).collect();
            
            let mut new_messages = Vec::new();
            for (i, msg) in self.messages.drain(..).enumerate() {
                if !indices_to_drop.contains(&i) {
                    new_messages.push(msg);
                }
            }
            self.messages = new_messages;
        }
    }

    pub async fn run(&mut self, page: &Page) -> anyhow::Result<String> {
        // Phase 1: top-level span for the whole session. Every event
        // emitted inside `run_inner` (and its descendants) carries
        // `span = "react_loop"`, making per-session filtering trivial
        // (e.g. `jq 'select(.span == "react_loop" and .event == "llm_response")'`).
        let react_span = tracing::info_span!("react_loop", session_id = %self.session_id);
        let _enter = react_span.enter();
        tracing::info!(event = "session_start", session_id = %self.session_id, "session started");

        // Outer wrapper so the state machine always reflects how the loop
        // exited. Even if `loop { ... }` is broken out of unexpectedly, we
        // mark Done/Failed based on the result and log the transition.
        let run_result = self.run_inner(page).await;

        // Phase 1: terminal span event. Lets a post-mortem script
        // quickly find the end of a session without scanning the
        // whole file.
        let outcome = match &run_result {
            Ok(s) => format!("ok:{}", s.len()),
            Err(e) => format!("err:{}", e),
        };
        tracing::info!(event = "session_end", session_id = %self.session_id, outcome = %outcome, "session ended");

        // Phase 15.1: the per-subtask end-of-session summary is
        // written for *every* exit path — success (finish() via
        // pending_finish_result), error (iteration limit, LLM
        // failure, hard crash), and external stop(). The success path
        // already wrote one inside the finish() handler before
        // returning; skip it there to avoid double-writing. The
        // `summary_written` flag is the canonical "have we already
        // emitted the summary?" signal. We previously used
        // `pending_finish_result.is_none()` but `take()` in the post-
        // match block nukes that field, so the check fired twice.
        //
        // Phase 3.3 (regression): before we write the terminal
        // summary, flush any deferred user-note file writes that
        // came in via the third drain. If the in-flight tool was
        // `finish()` and the loop is exiting, no next iteration
        // will run to flush the queue — so we have to do it here
        // to keep the user note from being lost from the
        // transcript. The flush goes to disk first so the user
        // note lands before the session's terminal summary line.
        self.flush_pending_user_note_writes();

        // Phase 8: flush the captcha telemetry to disk so the
        // on-disk counters reflect *this* session's detections
        // (and pre-navigate `mark_expected` calls). The next
        // session's `load_or_default` reads the same file and
        // sees the cumulative state. Cheap — JSON file, a few
        // hundred bytes for normal use.
        self.captcha_telemetry.flush();

        if !self.summary_written {
            // Build a short task summary from the original task line
            // in `self.messages`. Same shape the finish() handler
            // uses; we duplicate the lookup so this branch doesn't
            // need a different field to be set.
            let task_summary = self
                .messages
                .iter()
                .find_map(|m| {
                    m.get("role")
                        .and_then(|r| r.as_str())
                        .filter(|r| *r == "user")
                        .and_then(|_| m.get("content").and_then(|c| c.as_str()))
                })
                .unwrap_or("(no task recorded)")
                .to_string();
            let summary_text = self.completeness.write_summary(
                self.transcript_file.as_ref(),
                &self.session_id,
                &task_summary,
            );
            if let Some(tx) = &self.event_tx {
                let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                let _ = tx.send(AgentEvent::Summary {
                    timestamp_secs: ts,
                    text: summary_text,
                });
            }
            self.summary_written = true;
        }

        // Transition to terminal state based on how we exited, and log it.
        let terminal = match &run_result {
            Ok(_) => self.session.complete().await,
            Err(e) => {
                let reason = format!("loop terminated: {}", e);
                self.session.fail(reason).await
            }
        };

        // If the terminal transition itself succeeded, log it. If it failed
        // (e.g. loop was already stopped by a caller), fall through and just
        // log whatever the latest recorded history says.
        match terminal {
            Ok(_) => {
                let history = self.session.history().await;
                if let Some(last) = history.last() {
                    self.write_state_line(last);
                }
            }
            Err(SessionError::TerminalState(_)) => {
                // External stop() beat us to the terminal; the caller already
                // logged their own transition. Nothing more to do.
            }
            Err(e) => {
                eprintln!("warning: could not record terminal state: {}", e);
            }
        }

        run_result
    }

    async fn run_inner(&mut self, page: &Page) -> anyhow::Result<String> {
        // Phase 2: pre-flight decomposition. Runs *once* before
        // the first iteration, on the agent's own thread, with
        // no network and no LLM cost. Seeds the
        // `CompletenessTracker` and appends a `PLAN:` block to
        // the system prompt so every subsequent LLM call sees
        // the broken-down subtasks.
        //
        // We do this *inside* `run_inner` (not the constructor)
        // so:
        //   * The plan reflects the task string actually held in
        //     `self.messages` (the constructor's `task` argument
        //     could in principle diverge from the messages — they
        //     both come from the same caller, but a future change
        //     to the message format would silently break a
        //     constructor-time plan).
        //   * Failures here are returned as a session-start error,
        //     not a constructor error — the constructor stays
        //     infallible from the caller's point of view.
        //   * Tracing is in scope (we're inside the `react_loop`
        //     span).
        if let Err(e) = self.run_preflight_plan() {
            // Pre-flight is a hard requirement: a session that
            // starts without a plan is exactly the failure mode
            // Phase 2 is closing. Fail loudly so the call site
            // sees the cause instead of the agent silently
            // starting with an empty tracker.
            tracing::error!(
                event = "preflight_plan_failed",
                error = %e,
                "pre-flight decomposition failed; refusing to start session"
            );
            return Err(e);
        }
        loop {
            // Phase 12.1: every iteration starts by checking the state
            // machine. `Running` is a fast no-op (returns immediately). A
            // `Paused` state parks here until `resume()`. A terminal state
            // (Stopped/Done/Failed) breaks the loop with a typed error so the
            // outer wrapper can record the right transition.
            //
            // Wired in at the *top* of the loop on top of the existing
            // iteration-limit / token-limit checks. The spec also requires a
            // checkpoint right after the tool call is parsed; that one lives
            // further down inside the `if let Some(calls) = tool_calls` block.
            if let Err(e) = self.session.checkpoint().await {
                // The session moved to a terminal state (Stopped/Done/Failed)
                // or some other cancellation. Surface as a regular Err so the
                // outer `run` records `Failed` with the reason.
                return Err(anyhow::anyhow!(e.to_string()));
            }

            if self.iterations >= self.config.opencode_zen.max_iterations {
                println!("Iteration limit reached ({}). Halting.", self.iterations);
                return Err(anyhow::anyhow!("Iteration limit reached"));
            }
            if let Some(max_t) = self.config.opencode_zen.max_tokens {
                if self.total_tokens >= max_t {
                    println!("Token limit reached ({} / {}). Halting.", self.total_tokens, max_t);
                    return Err(anyhow::anyhow!("Token limit reached"));
                }
            }

            self.iterations += 1;
            println!("--- Iteration {} ---", self.iterations);

            // Check for navigation
            let current_page_url = page.url().await.ok().flatten().unwrap_or_default();
            let is_navigation = if let Some(ref old_url) = self.current_url {
                old_url != &current_page_url
            } else {
                true
            };
            self.current_url = Some(current_page_url.clone());

            if is_navigation {
                println!("Navigation detected: Resetting history and forcing full snapshot.");
                // Phase 13.1: preserve any pending user-typed steering notes
                // across the navigation reset, per the "no state wipe" rule.
                self.truncate_preserving_user_notes(2); // Keep system (0) and task (1) plus any user notes
                // Phase 17.1: also reset the pacing streak. A streak
                // of clicks from the previous page is meaningless
                // on the new page — the new page might be a
                // different site with totally different cadence
                // expectations, and we don't want a navigation to
                // *not* reset and then have the very first click
                // on the new page get paced.
                self.pacing.reset();
                // Phase 6: also reset the ref-recovery budget
                // on navigation. A stale ref on the *previous*
                // page has no meaning on a fresh page — every
                // ref is by definition new. The next stale-ref
                // event on the new page gets a fresh full
                // budget.
                self.ref_recovery_attempts = 0;
                // Phase 6: the prior "is dashboard-like" flag
                // is also stale on a fresh page; reset to
                // false. The session-loss detector on the next
                // perception cycle will re-evaluate from the
                // fresh tree.
                self.prior_was_dashboard_like = false;
            } else {
                // Justify K=5: 5 recent actions provide enough short-term memory (e.g. opened dropdown, scrolled down, typed input)
                // to continue the task without hallucinating or losing the immediate thread of action, while dropping older stale results.
                self.trim_in_page_history(5);
            }

            // Phase 13.1: drain the live chat channel *after* navigation has
            // potentially reset history, so user-typed notes land in the
            // post-truncate history and the LLM in this very iteration
            // sees them alongside the freshest page state. This is the
            // "no task restart, no state wipe" guarantee from the spec.
            //
            // Non-blocking (try_recv under the hood of `drain_pending`) so
            // the default is "steer while running" — a blocking recv here
            // would let the user pause the agent by simply not typing.
            // Empty-drain is a true no-op per the 13.2 checklist.
            self.drain_and_apply_user_messages();

            // Step 1: Perceive state
            //
            // Phase 4 (Bug 4 fix): increase the per-call timeout
            // from 1s to 5s, the retry count from 3 to 4 with
            // 250ms gaps, and add a re-settle inside the retry
            // loop. The earlier 1s + 50ms config was a band-aid:
            // on a page where the AX tree is being populated just
            // after `wait_for_page_settled` returned `settled=true`,
            // the first CDP call can return an empty/partial tree
            // (observed even on example.com — the DOM is settled
            // but the accessibility tree itself takes a beat to
            // populate). The settle wait up-front filters out the
            // "loadEventFired but DOM empty" case, so by the time
            // we get here the page should be ready — but the AX
            // tree can still be empty transiently. When that
            // happens we re-call `wait_for_page_settled` (it'll
            // return instantly if the page is now ready, or wait
            // another bounded 10s if it's still populating) and
            // retry the tree extraction.
            //
            // Total worst case: 5s (first call) + 4 × (250ms gap +
            // 5s call) + 4 × 10s re-settle = ~67s, which is the
            // bound for a page that genuinely never settles.
            // Realistic case on a healthy page: 1 fast retry with
            // no re-settle needed.
            let observation = {
                let mut state = self.state.lock().await;

                let mut tree_res = tokio::time::timeout(
                    tokio::time::Duration::from_secs(5),
                    mew_perception::extract_tree(page, true),
                )
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Timeout extracting tree")));
                let mut retries = 0;
                while tree_res.is_err() && retries < 4 {
                    tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
                    // Re-settle: if the AX tree was empty, the
                    // page may have re-rendered between the first
                    // wait and the perception call (React strict
                    // mode, lazy hydration, etc). One more
                    // bounded wait — usually returns instantly
                    // because the page is now stable.
                    let _ = mew_cdp::wait_for_page_settled(page).await;
                    tree_res = tokio::time::timeout(
                        tokio::time::Duration::from_secs(5),
                        mew_perception::extract_tree(page, true),
                    )
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("Timeout extracting tree")));
                    retries += 1;
                }

                let (tree, ref_map, _) = match tree_res {
                    Ok(res) => res,
                    Err(e) => {
                        println!("Failed to extract tree after retries: {}", e);
                        (mew_perception::TreeNode {
                            id: "0".to_string(),
                            role: "RootWebArea".to_string(),
                            name: "Error: Failed to load page state".to_string(),
                            value: "".to_string(),
                            category: mew_perception::NodeCategory::Structural,
                            ref_id: None,
                            backend_node_id: None,
                            children: vec![],
                        }, std::collections::HashMap::new(), std::time::Duration::from_secs(0))
                    }
                };

                let mut is_full_replace = false;
                let mut computed_diff = None;

                if !is_navigation {
                    if let Some(prev) = state.get_previous_tree(&self.session_id) {
                        let diff = mew_perception::diff::compute_diff(prev, &tree);
                        // Heuristic for full page replacement
                        if diff.removed.len() > 50 && diff.added.len() > 50 {
                            is_full_replace = true;
                        } else {
                            computed_diff = Some(diff);
                        }
                    }
                }

                let obs_text = if is_navigation || is_full_replace || self.force_snapshot || state.get_previous_tree(&self.session_id).is_none() {
                    mew_perception::diff::serialize_full_tree(&tree)
                } else {
                    computed_diff.unwrap().serialize_compact()
                };

                self.force_snapshot = false;

                // Phase 6: clone the tree before save_tree
                // consumes it. The clone is the one we return
                // to the caller for the resilience hook. The
                // cost is one recursive struct clone per
                // perception cycle (typically a few hundred
                // nodes), acceptable for the safety it buys.
                let tree_for_resilience = tree.clone();
                state.save_tree(&self.session_id, tree);
                // Phase 15.1: while we still hold the lock and have
                // `obs_text` in scope, compute the cheap page-state
                // signature and record the snapshot against the
                // completeness tracker. The signature is a short
                // stable hash of the obs text so the per-subtask
                // summary can say "evidence: iter N, sig=X" without
                // us re-snapshotting at the end.
                let snapshot_signature = {
                    use std::collections::hash_map::DefaultHasher;
                    use std::hash::{Hash, Hasher};
                    let mut h = DefaultHasher::new();
                    obs_text.len().hash(&mut h);
                    if obs_text.len() > 200 {
                        obs_text[..200].hash(&mut h);
                        obs_text[obs_text.len() - 200..].hash(&mut h);
                    } else {
                        obs_text.hash(&mut h);
                    }
                    format!("len:{:08x}", h.finish())
                };
                self.completeness
                    .record_snapshot(self.iterations, snapshot_signature.clone());
                // Phase 1: emit a structured snapshot event so the
                // trace log can be diffed across runs by signature.
                // The signature is the same opaque hash the
                // completeness tracker uses; recording it here means
                // a reviewer can ask "did the perception block
                // actually take a fresh snapshot between these two
                // tool calls?" without reading the transcript.
                tracing::info!(
                    event = "snapshot_recorded",
                    iter = self.iterations,
                    signature = %snapshot_signature,
                    obs_bytes = obs_text.len(),
                    is_full_replace = is_full_replace,
                    "snapshot recorded"
                );
                // Phase 6: return the cloned `tree` so the
                // resilience hook (right after the perception
                // block) can walk it for modal / rate-limit /
                // session-loss signals without re-snapshotting.
                // The original is owned by `save_tree` above;
                // this clone is the one that flows to the
                // resilience hook. The detectors see exactly
                // the tree the LLM sees.
                (obs_text, ref_map, is_full_replace, tree_for_resilience)
            };

            // The perception block returned whether the diff heuristic
            // detected a full-page replace. If it did, reset history
            // *now* — outside the `state` lock, so we can mutate
            // `self.messages` without borrowing conflicts.
            //
            // Phase 13.1: use the note-preserving truncate so a user-
            // typed steering note isn't lost across the reset.
            let (obs_text, ref_map, is_full_replace, tree) = observation;
            if is_full_replace {
                println!("Full page replace detected via diff: Resetting history and forcing full snapshot.");
                self.truncate_preserving_user_notes(2);
            }

            let obs_summary = format!("Observation: {} bytes", obs_text.len());
            println!("--- {} ---", obs_summary);
            println!("{}\n----------------------------------", obs_text);

            // Phase 6: run the resilience hook *after* the
            // perception block produces a fresh tree, *before*
            // the LLM call. The hook scans the tree for the
            // three page-wide failure modes (modal / rate
            // limit / session loss) and decides what the loop
            // should do. The outcome is cached on
            // `self.pending_backoff_secs` /
            // `self.pending_modal_dismiss` so the dispatch
            // site can act on it without re-walking the tree.
            //
            // We use the *prior* iteration's
            // `prior_was_dashboard_like` flag (which was
            // updated at the *end* of the last perception
            // cycle) and then refresh it for the *next*
            // iteration based on the current tree.
            let report = crate::resilience::evaluate_page(
                &tree,
                self.prior_was_dashboard_like,
            );
            match &report.outcome {
                crate::resilience::ResilienceHookOutcome::Continue => {}
                crate::resilience::ResilienceHookOutcome::AutoDismiss { dismiss_ref } => {
                    crate::resilience::log_resilience_event(
                        self.transcript_file.as_ref(),
                        &self.session_id,
                        "modal_autodismiss",
                        &format!("dismiss_ref={dismiss_ref}"),
                    );
                    self.pending_modal_dismiss = Some(dismiss_ref.clone());
                }
                crate::resilience::ResilienceHookOutcome::Backoff { secs } => {
                    crate::resilience::log_resilience_event(
                        self.transcript_file.as_ref(),
                        &self.session_id,
                        "rate_limit_backoff",
                        &format!("secs={secs}"),
                    );
                    self.pending_backoff_secs = Some(*secs);
                }
                crate::resilience::ResilienceHookOutcome::SurfaceAsFinding { summary, kind } => {
                    crate::resilience::log_resilience_event(
                        self.transcript_file.as_ref(),
                        &self.session_id,
                        kind,
                        summary,
                    );
                    // Surface as a system note the LLM will see
                    // on the next user-role message. The note
                    // is appended to the existing observation
                    // block below so the LLM has both the
                    // fresh tree *and* the typed finding.
                    self.messages.push(serde_json::json!({
                        "role": "user",
                        "content": format!(
                            "[resilience:{}] {}\n\nThe page state above is the latest observation. Decide your next step accordingly.",
                            kind, summary
                        ),
                    }));
                }
                crate::resilience::ResilienceHookOutcome::PauseForUser { .. } => {
                    // PauseForUser is a *dispatch*-time concern,
                    // not a perception-time concern. Recorded
                    // by the dispatch site (see below), not here.
                }
                crate::resilience::ResilienceHookOutcome::ForceSnapshot { .. } => {
                    // ForceSnapshot is set by the ref-recovery
                    // hook, not the page-state hook. No-op here.
                }
                crate::resilience::ResilienceHookOutcome::PauseForCaptcha { challenge } => {
                    // Phase 8: a challenge / CAPTCHA page was
                    // detected. The default response is to
                    // hand the page to the user — mew runs a
                    // real visible browser, so the human is
                    // the safest solver. We do three things
                    // here:
                    //
                    //   1. Stash the `Challenge` on
                    //      `pending_captcha` so a future
                    //      recovery code path / tests can
                    //      see *what* fired.
                    //   2. Log a structured resilience event
                    //      + tracing warning so the
                    //      transcript and the JSONL trace
                    //      both show the pause.
                    //   3. Pause the session (the existing
                    //      `Paused` state) so a future
                    //      `resume()` (after the user has
                    //      solved the challenge) continues
                    //      the loop. The next perception
                    //      cycle on resume will detect
                    //      whether the challenge is gone.
                    //
                    //   4. Return Err from `run_inner` so
                    //      the orchestrator's catch-all
                    //      produces a `BrowserResult
                    //      ::failure` whose `failure_reason`
                    //      is the user-actionable hint. The
                    //      synthesizer turns that into a
                    //      chat message — the "user, please
                    //      solve this in-window" message the
                    //      spec requires. The session
                    //      transitions `Paused -> Failed`
                    //      via the outer wrapper; both
                    //      states are valid; the `Paused`
                    //      before `Failed` is the "user can
                    //      inspect what we paused on" arc.
                    //
                    //   5. Push a typed user-role message
                    //      so a future `resume()` that
                    //      loops back to an LLM turn
                    //      carries the captcha context.
                    //      Cheap; doesn't affect the
                    //      short-circuit.
                    //
                    //   6. Phase 8: record the challenge in
                    //      the local telemetry. The
                    //      `record()` call is keyed on the
                    //      host (or "(unknown)" when the
                    //      detector couldn't infer one);
                    //      the `mark_expected` call is a
                    //      no-op here because the agent
                    //      doesn't know whether the host is
                    //      in the sensitive-platforms table
                    //      (it would have known *before*
                    //      the navigation, not at
                    //      detection time). The
                    //      `mark_expected` call is in the
                    //      pre-navigate pacing site.
                    self.pending_captcha = Some(challenge.clone());
                    self.captcha_telemetry.record(
                        challenge.domain_hint.as_deref().unwrap_or("unknown"),
                        challenge.kind,
                    );
                    crate::resilience::log_resilience_event(
                        self.transcript_file.as_ref(),
                        &self.session_id,
                        "CaptchaPause",
                        &format!(
                            "challenge_kind={} domain={} hint=\"{}\"",
                            challenge.kind.as_str(),
                            challenge.domain_hint.as_deref().unwrap_or("(unknown)"),
                            challenge.kind.user_action_hint()
                        ),
                    );
                    tracing::warn!(
                        event = "captcha_challenge_detected",
                        session_id = %self.session_id,
                        kind = challenge.kind.as_str(),
                        domain = %challenge.domain_hint.as_deref().unwrap_or(""),
                        "challenge page detected; pausing session for human solver"
                    );
                    // Pause first (sets the `Paused` state
                    // machine marker). The outer wrapper
                    // will then call `fail()` with the
                    // captcha reason; `Paused -> Failed`
                    // is a valid transition so the
                    // overall state machine path is
                    // `Running -> Paused -> Failed`.
                    if let Err(e) = self.session.pause(Some(format!(
                        "captcha:{} ({})",
                        challenge.kind.as_str(),
                        challenge.domain_hint.as_deref().unwrap_or("unknown host")
                    ))).await {
                        // The session was already in a
                        // terminal state — extremely rare
                        // (only possible if `stop()` was
                        // racing). Log and fall through;
                        // we still want to return Err so
                        // the orchestrator produces a
                        // chat message.
                        tracing::warn!(
                            event = "captcha_pause_failed",
                            error = %e,
                            "could not pause session for captcha; returning Err anyway"
                        );
                    }
                    self.messages.push(serde_json::json!({
                        "role": "user",
                        "content": format!(
                            "[resilience:captcha] {}\n\n{}",
                            challenge.label,
                            challenge.kind.user_action_hint()
                        ),
                    }));
                    // Return Err from run_inner so the
                    // outer `run` wrapper records
                    // `Failed` (with the captcha reason
                    // as the failure text) and the
                    // orchestrator's catch-all path
                    // turns it into a user-facing chat
                    // message via the synthesizer. The
                    // error string is what the user
                    // will see in the chat list.
                    return Err(anyhow::anyhow!(
                        "captcha challenge detected: {} — {}",
                        challenge.label,
                        challenge.kind.user_action_hint()
                    ));
                }
            }
            // Refresh the "did the prior look like a dashboard?"
            // flag for the *next* iteration. The session-loss
            // detector in the next perception cycle will use
            // this value.
            self.prior_was_dashboard_like =
                crate::resilience::page_looks_dashboard_like(&tree);

            // Phase 6: if the rate-limit hook asked for a
            // backoff, sleep *now* (before the LLM call) so the
            // next observation is taken *after* the cooldown
            // window. The sleep is on the agent's own thread,
            // not the LLM thread, so it adds zero LLM cost.
            if let Some(secs) = self.pending_backoff_secs.take() {
                println!("[resilience] rate-limit backoff: sleeping {}s", secs);
                tokio::time::sleep(tokio::time::Duration::from_secs(secs)).await;
                // Force a re-snapshot after the sleep so the
                // LLM sees fresh state, not the rate-limit page
                // that triggered the backoff.
                self.force_snapshot = true;
            }

            self.messages.push(serde_json::json!({
                "role": "user",
                "content": format!("Current page state:\n{}", obs_text)
            }));

            // Step 2: Call LLM
            // Phase 1: per-call span. The message count and the
            // tool names are the two pieces of information most
            // useful for diagnosing "what did the LLM see at this
            // iteration?" — recorded as span fields so the JSON
            // log has them without the caller having to inspect
            // the request body.
            let llm_span = tracing::info_span!(
                "llm_call",
                iter = self.iterations,
                msg_count = self.messages.len(),
                tool_names = ?self.tool_names_for_log(),
            );
            let _llm_enter = llm_span.enter();

            let url = format!("{}/chat/completions", self.config.opencode_zen.base_url);
            let body = json!({
                "model": self.config.opencode_zen.default_model,
                "messages": self.messages,
                "tools": self.get_tools_schema(),
                "tool_choice": "auto"
            });

            // Phase 1: log the request body length and the tool
            // names, but NOT the full body (it can be 10k+ tokens
            // of conversation history). The full body is recoverable
            // from the existing transcript file; the trace log is
            // for "what fired and how big was it" filtering.
            tracing::info!(
                event = "llm_request",
                iter = self.iterations,
                model = %self.config.opencode_zen.default_model,
                msg_count = self.messages.len(),
                body_bytes = body.to_string().len(),
                "LLM request dispatched"
            );

            let mut res_json: Option<serde_json::Value> = None;
            let mut backoff = 2;
            for attempt in 1..=5 {
                let res = self.client.post(&url)
                    .header("Authorization", format!("Bearer {}", self.config.opencode_zen.api_key))
                    .json(&body)
                    .send()
                    .await;

                match res {
                    Ok(response) if response.status().is_success() => {
                        res_json = Some(response.json().await?);
                        break;
                    },
                    Ok(response) => {
                        let err = response.text().await.unwrap_or_default();
                        println!("LLM API returned error (attempt {}): {}", attempt, err);
                    },
                    Err(e) => {
                        println!("LLM API request failed (attempt {}): {}", attempt, e);
                    }
                }
                if attempt < 5 {
                    println!("Retrying LLM API in {} seconds...", backoff);
                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff)).await;
                    backoff *= 2;
                }
            }

            let res_json = match res_json {
                Some(j) => j,
                None => anyhow::bail!("LLM API returned error consistently after 5 attempts"),
            };

            // Phase 1: log the response. The four fields most useful
            // for diagnosing failure modes: finish_reason (does the
            // LLM think it's done?), tool_call count (did it pick a
            // tool?), content (any free-text reply), and the
            // per-iteration token usage. We deliberately log the
            // *names* of the tools the model called, not the full
            // arguments — full args are already in the transcript.
            let response_message = &res_json["choices"][0]["message"];
            let finish_reason = res_json["choices"][0]["finish_reason"]
                .as_str()
                .unwrap_or("");
            let tool_call_names: Vec<String> = response_message
                .get("tool_calls")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .map(|c| {
                            c["function"]["name"]
                                .as_str()
                                .unwrap_or("?")
                                .to_string()
                        })
                        .collect()
                })
                .unwrap_or_default();
            let content_preview: String = response_message
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .chars()
                .take(200)
                .collect();
            let usage_total = res_json
                .get("usage")
                .and_then(|u| u.get("total_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            tracing::info!(
                event = "llm_response",
                iter = self.iterations,
                finish_reason = %finish_reason,
                tool_call_count = tool_call_names.len(),
                tool_call_names = ?tool_call_names,
                content_preview = %content_preview,
                usage_total = usage_total,
                "LLM response received"
            );
            
            if let Some(usage) = res_json.get("usage") {
                let step_total = usage.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let step_prompt = usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let step_completion = usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                
                let mut cached = 0;
                if let Some(c) = usage.get("prompt_tokens_details").and_then(|v| v.get("cached_tokens")).and_then(|v| v.as_u64()) {
                    cached = c;
                } else if let Some(c) = usage.get("cached_tokens").and_then(|v| v.as_u64()) {
                    cached = c;
                }
                
                self.total_tokens += step_total as usize;
                
                println!("--- Token Usage for Step {} ---", self.iterations);
                println!("Raw usage response: {}", serde_json::to_string_pretty(usage).unwrap_or_default());
                println!("Prompt: {} (Cached: {}), Completion: {}, Total this step: {}", step_prompt, cached, step_completion, step_total);
                let billed_input = step_prompt.saturating_sub(cached);
                println!("Effective (Billed) Input Tokens: {}", billed_input);
                println!("Session Cumulative Total: {}", self.total_tokens);
                println!("-------------------------------");
            }

            let message = &res_json["choices"][0]["message"];
            let mut assistant_msg = message.clone();
            // Remove reasoning arrays/objects if any since they can break strict schemas occasionally on next turns
            if let Some(obj) = assistant_msg.as_object_mut() {
                obj.remove("reasoning_details");
            }
            self.messages.push(assistant_msg.clone());

            // Phase 3.3 (regression): drain the live chat channel a
            // THIRD time, immediately after the LLM call returns and
            // the assistant message has been pushed to history, but
            // *before* the tool is dispatched.
            //
            // The LLM call is the longest single step in an iteration
            // (typically 3-10s for a tool-calling response). Any user
            // redirect pushed *during* the LLM call is invisible to
            // both the top-of-iteration drain and the just-before-LLM
            // drain (which both ran before this LLM call started). The
            // only way to catch such a push in time for the LLM in the
            // *next* iteration is to drain right here, after the LLM
            // returns.
            //
            // We deliberately do *not* write the drained user note
            // straight to the transcript file here. Writing it now
            // would put the `USER:` line ahead of the in-flight
            // tool's `TOOL CALL:` line on disk, even though the LLM
            // decided the tool call *before* the user typed. The
            // chronological order is: LLM-decided-tool → user-typed →
            // tool-dispatched; the transcript should reflect that.
            //
            // To preserve both invariants — user note lands in
            // `self.messages` so the LLM sees it next iteration, AND
            // the transcript file order matches the chronology — we
            // queue the user note's file write into
            // `self.pending_user_note_writes`. That queue is flushed
            // by the *next* iteration's top-of-iteration drain,
            // *after* the in-flight tool's `TOOL CALL:` line has been
            // logged. This keeps the on-disk order correct.
            self.drain_and_apply_user_messages_defer_file();

            let tool_calls = message.get("tool_calls").and_then(|v| v.as_array());
            if let Some(calls) = tool_calls {
                if calls.is_empty() {
                    // No tools? fallback
                    self.messages.push(json!({
                        "role": "user",
                        "content": "You didn't call any tools. Please use a tool to proceed."
                    }));
                    continue;
                }

                // Process first tool call (single action per turn for now)
                let call = &calls[0];
                let call_id = call["id"].as_str().unwrap_or("unknown_id");
                let func = &call["function"];
                let name = func["name"].as_str().unwrap_or("");
                let args_str = func["arguments"].as_str().unwrap_or("{}");

                println!("LLM Called Tool: {} with args: {}", name, args_str);

                // Phase 1: log the tool dispatch as a structured
                // event. The args are recorded as the raw JSON string
                // (not parsed fields) so a `jq` query can pull them
                // out later without us maintaining a per-tool schema
                // in the log layer. The result is recorded by the
                // existing post-match logging block; we just need the
                // *dispatch* event to anchor the start of a tool call
                // in the trace.
                tracing::info!(
                    event = "tool_dispatch",
                    iter = self.iterations,
                    tool = %name,
                    args = %args_str,
                    "tool dispatched"
                );

                // Phase 12.1: checkpoint after the tool call is parsed, before
                // it is executed. This is the point where pause()/resume()/
                // stop() can safely interrupt without leaving the browser in a
                // half-mutated state. We log the current state so a pause
                // shows up in the transcript with a real timestamp; the resume
                // line is logged the next time the state changes back to
                // Running.
                let pre_exec_state = self.session.state().await;
                if let Err(e) = self.session.checkpoint().await {
                    // If the session went terminal (e.g. caller called
                    // stop() between the LLM response and the dispatch), log
                    // the transition we observed and bail.
                    let post = self.session.history().await;
                    if let Some(last) = post.last() {
                        if last.from != last.to {
                            self.write_state_line(last);
                        }
                    }
                    let _ = pre_exec_state; // documented, retained for the next state read
                    return Err(anyhow::anyhow!(e.to_string()));
                }
                // If state changed through the checkpoint (Running -> Paused ->
                // Running or Running -> Stopped), log it. We use a snapshot of
                // the history because the post-checkpoint state is the one the
                // loop will actually use.
                let post_history = self.session.history().await;
                if let Some(last) = post_history.last() {
                    if last.from != last.to {
                        self.write_state_line(last);
                    }
                }

                // Phase 6: the original `args_str` and
                // `args` are kept (the LLM's intent, logged
                // to the transcript). The dispatch site uses
                // `effective_name` / `effective_args` which
                // are the modal-dismiss-override-aware
                // versions — see the block below. The match
                // arm reads from `effective_args` so the
                // override flows through naturally.
                let args: serde_json::Value = serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
                let mut tool_result = String::new();

                // Phase 6: modal-dismiss override. If the
                // perception step detected a dismissable modal
                // and stored the close-button ref, *redirect*
                // the LLM's chosen tool to click that ref first.
                // The LLM's original choice is logged but not
                // executed; the LLM sees the dismiss result in
                // the next observation and re-plans from there.
                //
                // Why override at the dispatch boundary: the
                // perception step is the only place that knows
                // the modal exists; the dispatch step is the
                // only place that can actually click something.
                // Wiring both is cleaner than smuggling the
                // dismiss ref through `self.messages` as a
                // system-injected tool call (which would change
                // the LLM's tool-calling history and break
                // replay).
                let mut effective_name = name.to_string();
                let mut effective_args_str = args_str.to_string();
                let mut effective_args = args.clone();
                if let Some(dismiss_ref) = self.pending_modal_dismiss.take() {
                    crate::resilience::log_resilience_event(
                        self.transcript_file.as_ref(),
                        &self.session_id,
                        "modal_dismiss_dispatch",
                        &format!("dismiss_ref={dismiss_ref} original_tool={name}"),
                    );
                    println!("[resilience] modal-dismiss override: clicking {} instead of {}", dismiss_ref, name);
                    effective_name = "click".to_string();
                    effective_args_str = format!("{{\"ref\":\"{}\"}}", dismiss_ref);
                    effective_args = serde_json::json!({ "ref": dismiss_ref });
                }

                // Phase 6: irreversible-action gate. If the
                // effective tool is in the irreversible table,
                // transition the session to `Paused` and surface
                // a confirmation request to the user instead of
                // executing. The LLM gets a typed tool result
                // explaining the gate, and the user can resume
                // (via the Tauri command path) to confirm.
                if let Some(outcome) = crate::resilience::evaluate_dispatch(
                    &effective_name, &effective_args,
                ) {
                    if let crate::resilience::ResilienceHookOutcome::PauseForUser { target, action_kind } = outcome {
                        crate::resilience::log_resilience_event(
                            self.transcript_file.as_ref(),
                            &self.session_id,
                            "irreversible_gate",
                            &format!("action={} target={}", action_kind, target),
                        );
                        // Pause the session so the loop parks
                        // until the user confirms. The state
                        // machine in `session.rs` already
                        // supports Running -> Paused; the
                        // checkpoint at the top of the next
                        // iteration will block on this.
                        let pause_reason = format!(
                            "irreversible action: {} (target: {}) — awaiting user confirmation",
                            action_kind, target
                        );
                        if let Err(e) = self.session.pause(Some(pause_reason.clone())).await {
                            tracing::warn!(
                                event = "irreversible_pause_failed",
                                error = %e,
                                "could not transition to Paused; continuing with gate-as-error instead"
                            );
                        } else {
                            // Log the state transition so the
                            // transcript reviewer sees the
                            // pause fired.
                            let history = self.session.history().await;
                            if let Some(last) = history.last() {
                                if last.from != last.to {
                                    self.write_state_line(last);
                                }
                            }
                        }
                        // Push a "please confirm" message to
                        // the chat list so the user sees a
                        // natural-language prompt — never
                        // raw JSON, per the Phase 4
                        // communicator-first guarantee.
                        self.messages.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "content": format!(
                                "IRREVERSIBLE ACTION GATE: I'm about to {} (target: {}). I need your confirmation before I proceed. The agent is now paused — please confirm in the chat to resume, or send a steering message to redirect.",
                                action_kind, target
                            ),
                        }));
                        if let Some(mut file) = self.transcript_file.as_ref() {
                            let timestamp = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs();
                            let log_entry = format!(
                                "[{}] [{}] GATE-IRREVERSIBLE: action={} target={}\n\n",
                                timestamp, self.session_id, action_kind, target
                            );
                            let _ = file.write_all(log_entry.as_bytes());
                        }
                        // Do not execute the tool. The next
                        // iteration's checkpoint will park
                        // here (state == Paused) until the
                        // user resumes.
                        tool_result = format!(
                            "GATE: irreversible action {} (target: {}) paused for user confirmation. Loop will park on next iteration until resumed.",
                            action_kind, target
                        );
                        // Skip the rest of the dispatch. The
                        // checkpoint at the top of the next
                        // iteration will catch the Paused
                        // state and park the loop. We push
                        // the tool result above so the LLM
                        // sees the gate in its history.
                        let timestamp = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        if let Some(tx) = &self.event_tx {
                            let _ = tx.send(AgentEvent::Tool {
                                timestamp_secs: timestamp,
                                name: effective_name.clone(),
                                args: effective_args_str.clone(),
                                result: tool_result.clone(),
                            });
                        }
                        self.emit_progress_line(&effective_name, &effective_args, &tool_result);
                        continue;
                    }
                }

                // Phase 17.1: pacing guard. Decide *before* dispatch
                // whether the next action should be preceded by a
                // sleep. The guard is a no-op when the config block
                // is absent or `enabled: false`, so this is free in
                // the common case. When pacing *does* fire, we log
                // the decision to the transcript (so reviewers can
                // see it) and then sleep.
                //
                // Why here, not in each match arm: pacing is a
                // single decision per tool call, not per tool — we
                // want exactly one sleep per paced action, and
                // exactly one transcript line. Putting it at the
                // dispatch boundary (one call site) makes that
                // structural rather than convention-based.
                let pacing_decision = self.pacing.before_action(&effective_name);
                match &pacing_decision {
                    PacingDecision::NoPacing => {
                        // No log line, no sleep. (17.1 spec: log
                        // *when pacing is applied*, not every
                        // no-op — that would be spam on every
                        // iteration.)
                    }
                    PacingDecision::Pace { delay, .. } => {
                        // Log via the shared helper so the format
                        // matches the other transcript lines.
                        if let Some(action) = crate::pacing::PacedAction::from_tool_name(name) {
                            crate::pacing::log_pacing_decision(
                                self.transcript_file.as_ref(),
                                &self.session_id,
                                action,
                                &pacing_decision,
                            );
                        }
                        tokio::time::sleep(*delay).await;
                    }
                }

                let normalize_ref = |r: &str| -> String {
                    if !r.starts_with('@') { format!("@{}", r) } else { r.to_string() }
                };

                // Phase 6: the match arm keys on `effective_name`
                // (which is the modal-dismiss-override-aware
                // name) rather than the LLM's original `name`.
                // When the modal-dismiss override fired,
                // `effective_name == "click"` and the click arm
                // runs with the dismiss ref as its arg. The
                // original tool name is preserved in
                // `effective_args_str` for the transcript.
                //
                // `args` (the LLM's original args) is
                // shadowed to `effective_args` so the match
                // arm reads the override-aware value without
                // a per-arm find/replace. The original
                // `args_str` is unchanged and continues to
                // be used in the transcript / event-emit
                // lines below.
                let args = &effective_args;
                match effective_name.as_str() {
                    "navigate" => {
                        let url = args["url"].as_str().unwrap_or("");

                        if url.contains("force-crash") {
                            anyhow::bail!("Deliberately forced crash mid-task!");
                        }

                        // Phase 14.1: URL resolution layer. The LLM may have
                        // said `navigate("instagram")` (just a bare name) —
                        // we don't trust its guess, we resolve it ourselves.
                        // The resolver picks one of the following paths:
                        //   - via-search / via-search-confirm (Phase 2):
                        //     the host is in `config/sensitive_platforms.toml`;
                        //     we route through a Google search results page
                        //     instead of a direct nav. This is the structural
                        //     fix for Bug #1's referrer-less-bot-detection
                        //     failure mode.
                        //   - map-hit: known site, instant
                        //   - direct-guess: tried `https://{x}.com` and it
                        //     loaded inside a 4s probe timeout
                        //   - search-fallback: gave up, sent to Google
                        // The path taken is logged to the transcript and
                        // surfaced in the tool result so the next LLM
                        // iteration knows the URL it ended up on.
                        let resolution = mew_nav::resolve_with_probe_sensitive(
                            page,
                            url,
                            &self.sensitive_platforms,
                        )
                        .await;
                        let resolved_url = &resolution.url;

                        // Log the resolution decision to the transcript.
                        // This is the "transparent in the transcript" line
                        // 14.1 requires — when reviewing a transcript you
                        // can see *why* a site did what it did.
                        if let Some(mut file) = self.transcript_file.as_ref() {
                            let ts = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs();
                            let line = format!(
                                "[{}] [{}] NAV-RESOLVE: input=\"{}\" path={} resolved_url={}\n\n",
                                ts,
                                self.session_id,
                                resolution.original_input,
                                resolution.path.as_str(),
                                resolved_url
                            );
                            let _ = file.write_all(line.as_bytes());
                        }
                        println!(
                            "[nav-resolve] \"{}\" -> {} (via {})",
                            resolution.original_input,
                            resolved_url,
                            resolution.path.as_str()
                        );

                        // Domain allowlist check — runs against the
                        // *resolved* URL, not the LLM's raw input. If the
                        // resolver sent us to a search-engine results
                        // page, the check still runs against google.com,
                        // which is already in the default allowlist.
                        let mut allowed = true;
                        if let Some(ref allowed_domains) = self.config.agent.allowed_domains {
                            if let Ok(parsed_url) = url::Url::parse(resolved_url) {
                                if let Some(domain) = parsed_url.domain() {
                                    if !allowed_domains.iter().any(|d| domain == d || domain.ends_with(&format!(".{}", d))) {
                                        allowed = false;
                                    }
                                }
                            }
                        }

                        if !allowed {
                            tool_result = format!("ERROR: Action failed (Navigation). Domain for URL '{}' (resolved from '{}' via {}) is not in the allowlist. Do not attempt this domain.", resolved_url, resolution.original_input, resolution.path.as_str());
                        } else {
                            // Phase 8: pre-navigate pacing for known
                            // challengers. The theory of the case:
                            // a `known_to_challenge_bots = true` host
                            // is overwhelmingly likely to *serve* a
                            // challenge page on first hit. Bursting
                            // in with no delay is the worst possible
                            // pattern — bot detectors see a fresh
                            // session with no referrer history, no
                            // human-like pacing, and no warm-up
                            // actions. A 1.5-3s "settle in" delay
                            // before the navigation is the cheapest
                            // possible signal we can send: this
                            // browser is moving at human speed, not
                            // script speed.
                            //
                            // The delay is bounded (3s worst case)
                            // so an unattended task isn't blocked
                            // indefinitely. It's also *only* fired
                            // when the sensitive-platforms table
                            // flags the host — a normal site like
                            // github.com gets no extra delay.
                            //
                            // We sleep *before* the `for attempt in
                            // 1..=3` retry loop so a 429 / page-
                            // crash on attempt 1 doesn't double the
                            // pacing. The `extra_pre_nav_pacing_ms`
                            // field is read fresh on every navigate
                            // so a config edit (or a future per-
                            // session override) takes effect on the
                            // next call.
                            if let Ok(parsed) = url::Url::parse(resolved_url) {
                                if let Some(host) = parsed.host_str() {
                                    if self.sensitive_platforms.is_known_challenger(host) {
                                        // Deterministic-but-jittered
                                        // delay. We use a small
                                        // window (1500-3000 ms)
                                        // so the user doesn't see
                                        // a noticeable hang on
                                        // most navigations but
                                        // the bot detector sees
                                        // a non-script cadence.
                                        // The `nano_time() ^
                                        // iteration` salt keeps
                                        // consecutive navigations
                                        // from landing on the
                                        // same exact delay (which
                                        // would itself be a
                                        // detectable pattern).
                                        let nanos = std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .map(|d| d.subsec_nanos() as u64)
                                            .unwrap_or(0)
                                            ^ (self.iterations as u64);
                                        let jitter = (nanos % 1500) as u64; // 0-1499 ms
                                        let pre_nav_ms = 1500 + jitter; // 1500-2999 ms
                                        tracing::info!(
                                            event = "pre_nav_known_challenger_pacing",
                                            session_id = %self.session_id,
                                            host = %host,
                                            delay_ms = pre_nav_ms,
                                            "sleeping before navigate on a known challenger"
                                        );
                                        crate::resilience::log_resilience_event(
                                            self.transcript_file.as_ref(),
                                            &self.session_id,
                                            "PreNavPacing",
                                            &format!("host={host} delay_ms={pre_nav_ms}"),
                                        );
                                        tokio::time::sleep(
                                            tokio::time::Duration::from_millis(pre_nav_ms),
                                        )
                                        .await;
                                        // Phase 8: pre-seed the
                                        // telemetry as `expected`
                                        // when we *predict* a
                                        // challenge will fire.
                                        // The `record()` call
                                        // (in the captcha
                                        // detection arm above)
                                        // will then increment an
                                        // already-`expected` row
                                        // — the summary marks
                                        // these distinctly from
                                        // "new" rows that
                                        // appeared without a
                                        // pre-navigate hint.
                                        self.captcha_telemetry.mark_expected(host);
                                    }
                                }
                            }

                            let mut success = false;
                            let mut backoff = 1;
                            for attempt in 1..=3 {
                                match tokio::time::timeout(tokio::time::Duration::from_secs(15), mew_cdp::navigate(page, resolved_url)).await {
                                    Ok(Ok(_)) => {
                                        // Phase 4 (Bug 4 fix): replace the
                                        // fixed 2s sleep with a proper
                                        // DOM-content wait. The old sleep
                                        // was a band-aid: on JS-heavy pages
                                        // (GitHub, SPAs) the page's
                                        // `loadEventFired` event fires well
                                        // before the SPA has populated the
                                        // DOM, so the AX tree at this point
                                        // was just the RootWebArea + a single
                                        // `ignored/uninteresting` child with
                                        // `busy: true` — see the pre-fix
                                        // capture in
                                        // `tests-output/phase4_bug4_repro_github_pre_fix.out.txt`.
                                        //
                                        // `wait_for_page_settled` polls
                                        // document.readyState + body +
                                        // aria-busy + text length, bounded
                                        // at 10s. Fast pages (example.com)
                                        // settle on the first poll (~0ms
                                        // added). Slow pages (GitHub) wait
                                        // until the SPA actually populates.
                                        // Total worst case is 10s + 200ms
                                        // floor, which is correct: we want
                                        // the page, not the load event.
                                        let settle = mew_cdp::wait_for_page_settled(page).await;
                                        if !settle.settled {
                                            println!(
                                                "[navigate] page did not fully settle after {}ms ({} polls) — proceeding anyway; perception may need to retry",
                                                settle.elapsed_ms, settle.polls
                                            );
                                        } else {
                                            println!(
                                                "[navigate] page settled in {}ms ({} polls)",
                                                settle.elapsed_ms, settle.polls
                                            );
                                        }
                                        // Tell the LLM (and the user reading
                                        // logs) which path got us here. If
                                        // the LLM asked for "instagram" and
                                        // we ended up on a Google results
                                        // page, the next turn can see that
                                        // and adjust.
                                        tool_result = format!(
                                            "Navigated successfully (resolved \"{}\" -> \"{}\" via {})",
                                            resolution.original_input,
                                            resolved_url,
                                            resolution.path.as_str()
                                        );
                                        success = true;
                                        break;
                                    },
                                    Ok(Err(e)) => {
                                        // The navigate CDP call errored
                                        // (rare; usually means the browser
                                        // hung up). Before we surface the
                                        // error, give the page one more
                                        // chance to settle — sometimes the
                                        // navigation actually succeeded at
                                        // the browser level and only the
                                        // future returned an error.
                                        let _ = mew_cdp::wait_for_page_settled(page).await;
                                        tool_result = format!("ERROR: Action failed (Navigation). {}. Do not assume success.", e);
                                    },
                                    Err(_) => {
                                        // Navigation timed out at 15s. The
                                        // page may still be partially
                                        // loaded (the CDP call hung, not
                                        // the network). One more settle
                                        // attempt — if the page eventually
                                        // finishes loading we still want
                                        // the LLM to be able to act on it.
                                        let _ = mew_cdp::wait_for_page_settled(page).await;
                                        tool_result = "ERROR: Action failed (Timeout while navigating). The navigation timed out and did not complete successfully. Do not assume your action succeeded.".to_string();
                                    },
                                }
                                if attempt < 3 && !success {
                                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff)).await;
                                    backoff *= 2;
                                }
                            }
                        }
                    },
                    "click" => {
                        let r = args["ref"].as_str().unwrap_or("");
                        let r_norm = normalize_ref(r);
                        if let Some(backend_id) = ref_map.get(&r_norm) {
                            // Phase 16.1: pre-compute the element's center and slide
                            // the visible ghost cursor there before the real click.
                            // This is the *only* place we read `visible_cursor` from
                            // config at click time — keep it scoped to the click
                            // branch so other tools (type, scroll, navigate) are
                            // completely unaffected.
                            let cursor_enabled = self
                                .config
                                .browser
                                .as_ref()
                                .map(|b| b.visible_cursor)
                                .unwrap_or(false);
                            if cursor_enabled {
                                if let Ok(Some((cx, cy))) =
                                    mew_cdp::compute_element_center(page, backend_id.clone()).await
                                {
                                    // Slide cursor toward the target first. The CSS
                                    // transition (180ms) on the cursor div handles
                                    // the visible motion, so this moveTo starts the
                                    // slide immediately.
                                    mew_cdp::move_cursor(page, cx, cy).await;
                                    // Sleep a touch longer than the transition so the
                                    // slide is fully complete before the click
                                    // fires. Spec: 100-200ms.
                                    tokio::time::sleep(tokio::time::Duration::from_millis(200))
                                        .await;
                                }
                            }

                            let mut success = false;
                            let mut backoff = 1;
                            for attempt in 1..=3 {
                                match tokio::time::timeout(tokio::time::Duration::from_secs(1), mew_cdp::click_ref(page, backend_id.clone())).await {
                                    Ok(Ok(_)) => {
                                        // Fire the click ripple exactly once, on the
                                        // successful attempt. We re-compute the
                                        // center here rather than caching the cx/cy
                                        // outside the loop because the element may
                                        // have moved between attempts; the ripple
                                        // should land on the actual final target.
                                        if cursor_enabled {
                                            if let Ok(Some((cx, cy))) =
                                                mew_cdp::compute_element_center(page, backend_id.clone()).await
                                            {
                                                mew_cdp::move_cursor_and_ripple(page, cx, cy).await;
                                            }
                                        }
                                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                                        tool_result = "Clicked successfully".to_string();
                                        // Phase 6: a successful
                                        // click resets the
                                        // ref-recovery budget.
                                        // The next stale ref
                                        // event gets a fresh
                                        // full budget.
                                        self.ref_recovery_attempts = 0;
                                        success = true;
                                        break;
                                    },
                                    Ok(Err(e)) => {
                                        if let mew_cdp::StaleRefError::NotFound(_) = e {
                                            // Phase 6: ref-recovery
                                            // hook. The CDP layer
                                            // returned a stale-ref
                                            // error; ask the
                                            // resilience crate to
                                            // decide retry vs
                                            // escalate based on
                                            // the action kind and
                                            // the current ref map.
                                            // A `Retry` sets
                                            // `force_snapshot =
                                            // true` (existing
                                            // behavior) but ALSO
                                            // bumps the per-iter
                                            // retry counter so
                                            // the budget can be
                                            // enforced.
                                            let current_refs: std::collections::HashMap<String, ()> = ref_map
                                                .iter()
                                                .map(|(k, _)| (k.clone(), ()))
                                                .collect();
                                            let recovery_inputs = mew_resilience::ref_recovery::RefRecoveryInputs {
                                                supplied_ref: &r_norm,
                                                current_ref_map: &current_refs,
                                                target_desc: None,
                                                description_index: None,
                                                action: mew_resilience::ref_recovery::RefActionKind::Click,
                                                attempts_so_far: self.ref_recovery_attempts,
                                            };
                                            let recovery_cfg = mew_resilience::ref_recovery::RefRecoveryConfig::default();
                                            match mew_resilience::ref_recovery::attempt_recovery(
                                                &recovery_cfg,
                                                &recovery_inputs,
                                            ) {
                                                mew_resilience::ref_recovery::RefRecoveryOutcome::Retry { new_ref, attempts_so_far } => {
                                                    crate::resilience::log_resilience_event(
                                                        self.transcript_file.as_ref(),
                                                        &self.session_id,
                                                        "ref_recovery_retry",
                                                        &format!("old={} new={} attempt={}", r_norm, new_ref, attempts_so_far),
                                                    );
                                                    self.ref_recovery_attempts = attempts_so_far;
                                                    self.force_snapshot = true;
                                                    tool_result = format!(
                                                        "Stale ref detected ({}). Recovery: re-snapshot and retry with the new ref. The agent will re-snapshot and pick a fresh ref next iteration.",
                                                        r_norm
                                                    );
                                                    success = true;
                                                    break;
                                                }
                                                mew_resilience::ref_recovery::RefRecoveryOutcome::EscalateToLLM { reason, attempts_so_far } => {
                                                    crate::resilience::log_resilience_event(
                                                        self.transcript_file.as_ref(),
                                                        &self.session_id,
                                                        "ref_recovery_escalate",
                                                        &format!("ref={} attempt={}", r_norm, attempts_so_far),
                                                    );
                                                    self.ref_recovery_attempts = attempts_so_far;
                                                    self.force_snapshot = true;
                                                    tool_result = format!(
                                                        "ERROR: Action failed (Stale Element Reference). {}. The element was removed from the DOM. Your action had NO effect. Re-evaluate the new page state and decide your next step.",
                                                        reason
                                                    );
                                                    success = true;
                                                    break;
                                                }
                                                mew_resilience::ref_recovery::RefRecoveryOutcome::AbortWithReason { reason, attempts_so_far } => {
                                                    crate::resilience::log_resilience_event(
                                                        self.transcript_file.as_ref(),
                                                        &self.session_id,
                                                        "ref_recovery_abort",
                                                        &format!("ref={} attempt={}", r_norm, attempts_so_far),
                                                    );
                                                    self.ref_recovery_attempts = attempts_so_far;
                                                    self.force_snapshot = true;
                                                    tool_result = format!(
                                                        "ERROR: Action aborted (Stale Element Reference). {}. Your action had NO effect.",
                                                        reason
                                                    );
                                                    success = true;
                                                    break;
                                                }
                                            }
                                        } else {
                                            tool_result = format!("ERROR: Action failed (Click). {}. Do not assume success.", e);
                                        }
                                    },
                                    Err(_) => tool_result = "ERROR: Action failed (Timeout while clicking). The click action timed out and did not complete successfully. Do not assume your action succeeded.".to_string(),
                                }
                                if attempt < 3 && !success {
                                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff)).await;
                                    backoff *= 2;
                                }
                            }
                        } else {
                            tool_result = format!("ref_id {} (normalized: {}) not found on page", r, r_norm);
                            println!("{}", tool_result);
                        }
                    },
                    "type" => {
                        let r = args["ref"].as_str().unwrap_or("");
                        let r_norm = normalize_ref(r);
                        let text = args["text"].as_str().unwrap_or("");
                        if let Some(backend_id) = ref_map.get(&r_norm) {
                            let mut success = false;
                            let mut backoff = 1;
                            for attempt in 1..=3 {
                                match tokio::time::timeout(tokio::time::Duration::from_secs(15), mew_cdp::type_ref(page, backend_id.clone(), text)).await {
                                    Ok(Ok(_)) => {
                                        tool_result = "Typed successfully".to_string();
                                        // Phase 6: reset ref-recovery
                                        // budget on success.
                                        self.ref_recovery_attempts = 0;
                                        success = true;
                                        break;
                                    },
                                    Ok(Err(e)) => {
                                        if let mew_cdp::StaleRefError::NotFound(_) = e {
                                            // Phase 6: ref-recovery
                                            // hook (same shape as the
                                            // click arm above). For
                                            // `type` the recovery
                                            // crate's Type-specific
                                            // rule applies: if the
                                            // page has gone empty of
                                            // interactive elements,
                                            // the recovery is
                                            // `AbortWithReason` rather
                                            // than `Retry` — a
                                            // non-idempotent action
                                            // shouldn't auto-retry
                                            // against a guessed
                                            // target.
                                            let current_refs: std::collections::HashMap<String, ()> = ref_map
                                                .iter()
                                                .map(|(k, _)| (k.clone(), ()))
                                                .collect();
                                            let recovery_inputs = mew_resilience::ref_recovery::RefRecoveryInputs {
                                                supplied_ref: &r_norm,
                                                current_ref_map: &current_refs,
                                                target_desc: None,
                                                description_index: None,
                                                action: mew_resilience::ref_recovery::RefActionKind::Type,
                                                attempts_so_far: self.ref_recovery_attempts,
                                            };
                                            let recovery_cfg = mew_resilience::ref_recovery::RefRecoveryConfig::default();
                                            match mew_resilience::ref_recovery::attempt_recovery(
                                                &recovery_cfg,
                                                &recovery_inputs,
                                            ) {
                                                mew_resilience::ref_recovery::RefRecoveryOutcome::Retry { new_ref, attempts_so_far } => {
                                                    crate::resilience::log_resilience_event(
                                                        self.transcript_file.as_ref(),
                                                        &self.session_id,
                                                        "ref_recovery_retry",
                                                        &format!("old={} new={} attempt={}", r_norm, new_ref, attempts_so_far),
                                                    );
                                                    self.ref_recovery_attempts = attempts_so_far;
                                                    self.force_snapshot = true;
                                                    tool_result = format!(
                                                        "Stale ref detected ({}). Recovery: re-snapshot and retry with the new ref.",
                                                        r_norm
                                                    );
                                                    success = true;
                                                    break;
                                                }
                                                mew_resilience::ref_recovery::RefRecoveryOutcome::EscalateToLLM { reason, attempts_so_far } => {
                                                    crate::resilience::log_resilience_event(
                                                        self.transcript_file.as_ref(),
                                                        &self.session_id,
                                                        "ref_recovery_escalate",
                                                        &format!("ref={} attempt={}", r_norm, attempts_so_far),
                                                    );
                                                    self.ref_recovery_attempts = attempts_so_far;
                                                    self.force_snapshot = true;
                                                    tool_result = format!(
                                                        "ERROR: Action failed (Stale Element Reference). {}. The element was removed from the DOM. Your action had NO effect.",
                                                        reason
                                                    );
                                                    success = true;
                                                    break;
                                                }
                                                mew_resilience::ref_recovery::RefRecoveryOutcome::AbortWithReason { reason, attempts_so_far } => {
                                                    crate::resilience::log_resilience_event(
                                                        self.transcript_file.as_ref(),
                                                        &self.session_id,
                                                        "ref_recovery_abort",
                                                        &format!("ref={} attempt={}", r_norm, attempts_so_far),
                                                    );
                                                    self.ref_recovery_attempts = attempts_so_far;
                                                    self.force_snapshot = true;
                                                    tool_result = format!(
                                                        "ERROR: Action aborted (Stale Element Reference). {}. A type action against a guessed target would be unsafe.",
                                                        reason
                                                    );
                                                    success = true;
                                                    break;
                                                }
                                            }
                                        } else {
                                            tool_result = format!("ERROR: Action failed (Type). {}. Do not assume success.", e);
                                        }
                                    },
                                    Err(_) => tool_result = "ERROR: Action failed (Timeout while typing). The type action timed out and did not complete successfully. Do not assume your action succeeded.".to_string(),
                                }
                                if attempt < 3 && !success { 
                                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff)).await; 
                                    backoff *= 2; 
                                }
                            }
                        } else {
                            tool_result = format!("ref_id {} (normalized: {}) not found on page", r, r_norm);
                            println!("{}", tool_result);
                        }
                    },
                    "scroll" => {
                        let dir = args["direction"].as_str().unwrap_or("down");
                        let d = if dir == "up" { mew_cdp::ScrollDirection::Up } else { mew_cdp::ScrollDirection::Down };
                        match tokio::time::timeout(tokio::time::Duration::from_secs(15), mew_cdp::scroll(page, d, 800)).await {
                            Ok(Ok(_)) => {
                                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                                tool_result = "Scrolled successfully".to_string();
                            },
                            Ok(Err(e)) => tool_result = format!("ERROR: Action failed (Scroll). {}. Do not assume success.", e),
                            Err(_) => tool_result = "ERROR: Action failed (Timeout while scrolling). The scroll action timed out and did not complete successfully. Do not assume your action succeeded.".to_string(),
                        }
                    },
                    "press_key" => {
                        let key = args["key"].as_str().unwrap_or("");
                        match tokio::time::timeout(tokio::time::Duration::from_secs(15), mew_cdp::press_key(page, key)).await {
                            Ok(Ok(_)) => {
                                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                                tool_result = "Key pressed successfully".to_string();
                            },
                            Ok(Err(e)) => tool_result = format!("ERROR: Action failed (Press Key). {}. Do not assume success.", e),
                            Err(_) => tool_result = "ERROR: Action failed (Timeout while pressing key). The press_key action timed out and did not complete successfully. Do not assume your action succeeded.".to_string(),
                        }
                    },
                    "snapshot" => {
                        // Phase 15.1: the snapshot tool MUST return the
                        // current page-state signature so the model can
                        // pass it to `mark_subtask_done` as evidence.
                        // Without surfacing the signature here, the
                        // model is forced to guess one, and any guess
                        // will be rejected by the tracker's evidence
                        // rule — making the gate look broken when it
                        // is actually working correctly. The signature
                        // is recorded in the perception block *after*
                        // the snapshot, so by the time this tool
                        // handler runs (next iteration), it's the
                        // freshest one available.
                        let sig = self
                            .completeness
                            .last_snapshot_signature
                            .clone()
                            .unwrap_or_else(|| "(none yet — perception block has not recorded a snapshot for this session)".to_string());
                        tool_result = format!(
                            "Snapshot taken. Observe the new page state in the next user message. Current snapshot_signature: {}",
                            sig
                        );
                    },
                    "vision_inspect" => {
                        let r = args["ref"].as_str().unwrap_or("");
                        let r_norm = normalize_ref(r);
                        if let Some(backend_id) = ref_map.get(&r_norm) {
                            match tokio::time::timeout(tokio::time::Duration::from_secs(15), mew_cdp::screenshot_region(page, backend_id.clone())).await {
                                Ok(Ok((base64_data, x, y, w, h))) => {
                                    println!("Region screenshot captured: {}x{} at {},{}", w, h, x, y);
                                    let vision_body = serde_json::json!({
                                        "model": "mimo-v2.5-free",
                                        "messages": [
                                            {
                                                "role": "user",
                                                "content": [
                                                    {"type": "text", "text": "Describe what you see in this cropped UI region. Identify if it looks like a button, icon, or specific widget."},
                                                    {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", base64_data)}}
                                                ]
                                            }
                                        ]
                                    });
                                    let url = format!("{}/chat/completions", self.config.opencode_zen.base_url);
                                    let v_res_future = self.client.post(&url)
                                        .header("Authorization", format!("Bearer {}", self.config.opencode_zen.api_key))
                                        .json(&vision_body)
                                        .send();
                                        
                                    match tokio::time::timeout(tokio::time::Duration::from_secs(30), v_res_future).await {
                                        Ok(Ok(resp)) => {
                                            if let Ok(v_json) = resp.json::<serde_json::Value>().await {
                                                if let Some(desc) = v_json["choices"][0]["message"]["content"].as_str() {
                                                    // Phase 6: wrap the
                                                    // raw description
                                                    // in the
                                                    // vision-confidence
                                                    // gate. The
                                                    // score() pure
                                                    // function derives
                                                    // a confidence
                                                    // from the LLM's
                                                    // own text (and
                                                    // the box size);
                                                    // the threshold
                                                    // (0.5) decides
                                                    // whether the
                                                    // description is
                                                    // acceptable. Low
                                                    // confidence ->
                                                    // surface a
                                                    // typed error so
                                                    // the LLM knows
                                                    // the vision
                                                    // result is not
                                                    // reliable, and
                                                    // suggest the
                                                    // tighter-crop
                                                    // coords the loop
                                                    // would use to
                                                    // re-shoot.
                                                    let verdict = crate::resilience::evaluate_vision(
                                                        desc,
                                                        Some((x, y, w, h)),
                                                    );
                                                    let threshold = 0.5_f32;
                                                    if verdict.confidence.is_acceptable(threshold) {
                                                        tool_result = format!(
                                                            "Vision fallback result for region {}:\nDescription: {}\nBounds: {}x{} at {},{}\nConfidence: {:.2}",
                                                            r, desc, w, h, x, y, verdict.confidence.score
                                                        );
                                                    } else {
                                                        crate::resilience::log_resilience_event(
                                                            self.transcript_file.as_ref(),
                                                            &self.session_id,
                                                            "vision_ambiguity",
                                                            &format!(
                                                                "ref={} confidence={:.2} threshold={:.2}",
                                                                r, verdict.confidence.score, threshold
                                                            ),
                                                        );
                                                        let tighten = verdict
                                                            .tighten_crop
                                                            .map(|(tx, ty, tw, th)| {
                                                                format!(
                                                                    " Suggested tighter crop: x={:.0} y={:.0} w={:.0} h={:.0} (use this for the next screenshot).",
                                                                    tx, ty, tw, th
                                                                )
                                                            })
                                                            .unwrap_or_default();
                                                        tool_result = format!(
                                                            "Vision result is not reliable (confidence {:.2}, threshold {:.2}). The LLM's description was too vague to act on.{} Try a different region or use a structured accessibility-tree element instead of vision.",
                                                            verdict.confidence.score, threshold, tighten
                                                        );
                                                        // Phase 6: do not
                                                        // mark the vision
                                                        // call a success
                                                        // when the
                                                        // confidence is
                                                        // below threshold.
                                                        // The LLM should
                                                        // pick a different
                                                        // approach
                                                        // (snapshot +
                                                        // accessibility
                                                        // tree).
                                                    }
                                                } else {
                                                    tool_result = "Vision API returned no description".to_string();
                                                }
                                            } else {
                                                tool_result = "Vision API returned invalid JSON".to_string();
                                            }
                                        },
                                        Ok(Err(e)) => tool_result = format!("Vision API call failed: {}", e),
                                        Err(_) => tool_result = "Vision API call timed out".to_string(),
                                    }
                                },
                                Ok(Err(e)) => tool_result = format!("Failed to take screenshot of region: {}", e),
                                Err(_) => tool_result = "Timeout while taking screenshot of region".to_string(),
                            }
                        } else {
                            tool_result = format!("ref_id {} (normalized: {}) not found on page", r, r_norm);
                            println!("{}", tool_result);
                        }
                    },
                    "finish" => {
                        // Phase 15.1: the finish() tool is gated by the
                        // completeness tracker. If subtasks were declared
                        // and any are still pending, this first call is
                        // intercepted — we do NOT return Ok(res). Instead
                        // we set `tool_result` to a re-prompt the LLM sees
                        // on the next iteration, log the gate-trigger
                        // event, and continue the loop.
                        //
                        // The 15.1 spec is explicit: "force one more
                        // snapshot() and require the model to explicitly
                        // justify each incomplete item." We do both: the
                        // re-prompt is injected as a tool-result error
                        // (so the LLM is told to act on it next turn)
                        // AND we set force_snapshot so the next iteration
                        // gets a fresh tree, not a stale diff.
                        let res = args["result"].as_str().unwrap_or("").to_string();
                        self.completeness.record_finish_attempt();
                        self.finish_calls_this_gate += 1;

                        if self.completeness.gate_open() {
                            // Gate clear — let finish() through. Write the
                            // per-subtask summary to the transcript first
                            // (the spec requires it be logged at the end of
                            // every session), then return.
                            let task_summary = self
                                .messages
                                .iter()
                                .find_map(|m| {
                                    m.get("role").and_then(|r| r.as_str())
                                        .filter(|r| *r == "user")
                                        .and_then(|_| m.get("content").and_then(|c| c.as_str()))
                                })
                                .unwrap_or("(no task recorded)")
                                .to_string();
                            let summary_text = self.completeness.write_summary(
                                self.transcript_file.as_ref(),
                                &self.session_id,
                                &task_summary,
                            );
                            if let Some(tx) = &self.event_tx {
                                let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                let _ = tx.send(AgentEvent::Summary {
                                    timestamp_secs: ts,
                                    text: summary_text,
                                });
                            }
                            self.summary_written = true;
                            // Phase 5: end-of-task LLM summarizer.
                            // The agent's raw `finish()` text is
                            // usually a list of "I clicked X. I
                            // typed Y. I called finish()" — not
                            // user-friendly. The LLM call rewrites
                            // it into a 1-2 sentence reply that
                            // references the actual people/sites/
                            // values the steps mention. On any
                            // failure (network, parse, model
                            // refusal) we fall back to the raw
                            // `res` so the user always sees
                            // *something* — the never-silent
                            // guarantee.
                            //
                            // The task description fed to the
                            // summarizer strips the "Task: " prefix
                            // the system prompt adds (the LLM
                            // already sees the raw task; the
                            // summary should too).
                            let task_for_summary = task_summary
                                .strip_prefix("Task: ")
                                .unwrap_or(&task_summary);
                            let final_text = match self
                                .end_of_task_summarize(task_for_summary, &res)
                                .await
                            {
                                Some(s) if !s.is_empty() => s,
                                _ => res.clone(),
                            };
                            println!("Task finished with result: {}", final_text);
                            let timestamp = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs();
                            if let Some(mut file) = self.transcript_file.as_ref() {
                                let log_entry = format!(
                                    "[{}] [{}] TOOL CALL: {} (args: {})\nRESULT: Task finished: {}\n\n",
                                    timestamp, obs_summary, name, args_str, final_text
                                );
                                let _ = file.write_all(log_entry.as_bytes());
                            }
                            if let Some(tx) = &self.event_tx {
                                let _ = tx.send(AgentEvent::Tool {
                                    timestamp_secs: timestamp,
                                    name: name.to_string(),
                                    args: args_str.to_string(),
                                    result: format!("Task finished: {}", final_text),
                                });
                            }
                            // Stash the result so the post-match block
                            // can return it. We use a small String
                            // captured via `pending_finish_result` so
                            // the existing match-arm exit path is reused.
                            self.pending_finish_result = Some(final_text);
                            // Use a non-error tool_result to satisfy the
                            // match's fallthrough; the real Ok return
                            // happens after the match.
                            tool_result = "(gate open — finish accepted; will return on next iteration)".to_string();
                        } else {
                            // Gate closed — first call is a re-prompt.
                            // Don't return. Mark gate triggered, force a
                            // fresh snapshot next iteration, and tell the
                            // LLM exactly what to do.
                            self.completeness.note_gate_triggered();
                            self.force_snapshot = true;
                            let pending_ids: Vec<String> = self
                                .completeness
                                .subtasks
                                .iter()
                                .filter(|s| !matches!(s.status, SubTaskStatus::Done | SubTaskStatus::Skipped { .. } | SubTaskStatus::Failed { .. }))
                                .map(|s| s.id.clone())
                                .collect();
                            let pending_descs: Vec<String> = self
                                .completeness
                                .subtasks
                                .iter()
                                .filter(|s| !matches!(s.status, SubTaskStatus::Done | SubTaskStatus::Skipped { .. } | SubTaskStatus::Failed { .. }))
                                .map(|s| format!("  - id={}: {}", s.id, s.description))
                                .collect();
                            let reason = format!(
                                "GATE BLOCKED: finish() called while {} subtask(s) are still pending. The agent is now forcing a fresh snapshot and re-prompting you. You must, on the next turn, do one of the following for each pending item:\n  - Call mark_subtask_done(id, snapshot_signature) if the most recent snapshot confirms the sub-item is complete on screen. The signature will be in the next observation.\n  - Call mark_subtask_skipped(id, reason) if the sub-item is genuinely out of scope.\n  - Call mark_subtask_failed(id, reason) if you tried and could not verify success.\nPending items:\n{}\nA blanket finish() is not accepted. The next iteration will start with a fresh snapshot so you have on-screen evidence to mark each item done.",
                                pending_ids.len(),
                                pending_descs.join("\n")
                            );
                            println!("[completeness-gate] finish() blocked: {}", reason.replace("\n", " | "));
                            if let Some(mut file) = self.transcript_file.as_ref() {
                                let timestamp = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_secs();
                                let log_entry = format!(
                                    "[{}] [{}] GATE: finish() intercepted, pending={}, reason={}\n\n",
                                    timestamp,
                                    self.session_id,
                                    pending_ids.len(),
                                    reason.replace("\n", " | ")
                                );
                                let _ = file.write_all(log_entry.as_bytes());
                            }
                            tool_result = reason;
                            // Reset the per-gate counter so the LLM
                            // gets exactly one more chance after the
                            // forced re-prompt — if it calls finish()
                            // again with pending items still open, the
                            // same gate fires again (not a permanent
                            // block, just a per-attempt re-prompt).
                            self.finish_calls_this_gate = 0;
                        }
                    },
                    "declare_subtasks" => {
                        // Phase 15.1: the LLM populates the canonical
                        // sub-item list. We accept JSON shape
                        // `{ items: [{ id, description }, ...] }`.
                        let items_raw = args.get("items").and_then(|v| v.as_array());
                        let mut declared: Vec<DeclareItem> = Vec::new();
                        if let Some(arr) = items_raw {
                            for it in arr {
                                let id = it.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let desc = it.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                if id.is_empty() {
                                    tool_result = format!("ERROR: declare_subtasks item missing 'id': {}", it);
                                    break;
                                }
                                if desc.is_empty() {
                                    tool_result = format!("ERROR: declare_subtasks item '{}' missing 'description'", id);
                                    break;
                                }
                                declared.push(DeclareItem { id, description: desc });
                            }
                        }
                        if tool_result.is_empty() {
                            match self.completeness.declare(declared.clone()) {
                                Ok(n) => {
                                    println!(
                                        "[completeness] declared {} subtask(s); tracker now: {}",
                                        n,
                                        self.completeness.inline_status()
                                    );
                                    if let Some(mut file) = self.transcript_file.as_ref() {
                                        let timestamp = std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_secs();
                                        let ids: Vec<String> = declared.iter().map(|d| d.id.clone()).collect();
                                        let log_entry = format!(
                                            "[{}] [{}] DECLARE: subtasks={} ids={:?}\n\n",
                                            timestamp,
                                            self.session_id,
                                            n,
                                            ids
                                        );
                                        let _ = file.write_all(log_entry.as_bytes());
                                    }
                                    let list: Vec<String> = declared
                                        .iter()
                                        .map(|d| format!("  - id={} desc=\"{}\"", d.id, d.description))
                                        .collect();
                                    tool_result = format!(
                                        "Declared {} subtask(s). For each, take a snapshot and call mark_subtask_done(id, snapshot_signature) when you see the expected state change. Skipped/failed are also terminal.\n{}",
                                        n,
                                        list.join("\n")
                                    );
                                }
                                Err(e) => {
                                    tool_result = format!("ERROR: declare_subtasks rejected: {}", e);
                                }
                            }
                        }
                    },
                    "mark_subtask_done" => {
                        // Phase 15.1: the LLM marks a subtask complete.
                        // The tracker rejects the call if the supplied
                        // signature doesn't match the most recent
                        // snapshot, forcing the model to actually look
                        // at the page (or call snapshot() first).
                        let id = args["id"].as_str().unwrap_or("").to_string();
                        let sig = args["snapshot_signature"].as_str().unwrap_or("").to_string();
                        if id.is_empty() {
                            tool_result = "ERROR: mark_subtask_done requires 'id'".to_string();
                        } else {
                            match self.completeness.mark_done(&id, &sig) {
                                MarkOutcome::MarkedDone { evidence_iteration, evidence_signature } => {
                                    println!(
                                        "[completeness] subtask '{}' marked done (evidence: iter {}, sig {})",
                                        id, evidence_iteration, evidence_signature
                                    );
                                    if let Some(mut file) = self.transcript_file.as_ref() {
                                        let timestamp = std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_secs();
                                        let log_entry = format!(
                                            "[{}] [{}] MARK DONE: id={} evidence_iter={} evidence_sig={}\n\n",
                                            timestamp, self.session_id, id, evidence_iteration, evidence_signature
                                        );
                                        let _ = file.write_all(log_entry.as_bytes());
                                    }
                                    tool_result = format!(
                                        "Subtask '{}' marked done with fresh-snapshot evidence (iter {}, sig {}). Tracker: {}",
                                        id,
                                        evidence_iteration,
                                        evidence_signature,
                                        self.completeness.inline_status()
                                    );
                                }
                                MarkOutcome::StaleEvidence { last_snapshot_iteration, current_iteration } => {
                                    println!(
                                        "[completeness] mark_done '{}' REJECTED: stale evidence (last snapshot at iter {}, current iter {})",
                                        id, last_snapshot_iteration, current_iteration
                                    );
                                    tool_result = format!(
                                        "ERROR: mark_subtask_done for '{}' rejected — your snapshot_signature does not match the most recent on-screen snapshot. Take a fresh snapshot() first, observe the page, and retry with that signature. The agent has recorded snapshot signatures as the perception block runs.",
                                        id
                                    );
                                }
                                MarkOutcome::UnknownId => {
                                    tool_result = format!(
                                        "ERROR: no subtask with id '{}' is currently declared. Call declare_subtasks first or check the id spelling.",
                                        id
                                    );
                                }
                                MarkOutcome::AlreadyTerminal { current } => {
                                    tool_result = format!(
                                        "ERROR: subtask '{}' is already in terminal status '{}' and cannot be marked done again.",
                                        id,
                                        current.as_str()
                                    );
                                }
                                MarkOutcome::MarkedSkipped { .. } => {
                                    // unreachable on mark_done
                                    unreachable!()
                                }
                                MarkOutcome::MarkedExhausted { .. } => {
                                    // unreachable on mark_done — only the
                                    // budget guard's `mark_exhausted` call
                                    // produces this variant (Phase 7).
                                    unreachable!()
                                }
                            }
                        }
                    },
                    "mark_subtask_skipped" => {
                        let id = args["id"].as_str().unwrap_or("").to_string();
                        let reason = args["reason"].as_str().unwrap_or("").to_string();
                        if id.is_empty() {
                            tool_result = "ERROR: mark_subtask_skipped requires 'id'".to_string();
                        } else if reason.is_empty() {
                            tool_result = format!("ERROR: mark_subtask_skipped for '{}' requires a non-empty 'reason'", id);
                        } else {
                            match self.completeness.mark_skipped(&id, reason.clone()) {
                                MarkOutcome::MarkedSkipped { reason: r } => {
                                    println!(
                                        "[completeness] subtask '{}' marked skipped: {}",
                                        id, r
                                    );
                                    if let Some(mut file) = self.transcript_file.as_ref() {
                                        let timestamp = std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_secs();
                                        let log_entry = format!(
                                            "[{}] [{}] MARK SKIPPED: id={} reason={}\n\n",
                                            timestamp, self.session_id, id, r
                                        );
                                        let _ = file.write_all(log_entry.as_bytes());
                                    }
                                    tool_result = format!(
                                        "Subtask '{}' marked skipped (reason: {}). Tracker: {}",
                                        id,
                                        r,
                                        self.completeness.inline_status()
                                    );
                                }
                                MarkOutcome::UnknownId => {
                                    tool_result = format!(
                                        "ERROR: no subtask with id '{}' is currently declared.",
                                        id
                                    );
                                }
                                MarkOutcome::AlreadyTerminal { current } => {
                                    tool_result = format!(
                                        "ERROR: subtask '{}' is already in terminal status '{}'.",
                                        id,
                                        current.as_str()
                                    );
                                }
                                MarkOutcome::StaleEvidence { .. } => unreachable!(),
                                MarkOutcome::MarkedDone { .. } => unreachable!(),
                                MarkOutcome::MarkedExhausted { .. } => unreachable!(),
                            }
                        }
                    },
                    "mark_subtask_failed" => {
                        let id = args["id"].as_str().unwrap_or("").to_string();
                        let reason = args["reason"].as_str().unwrap_or("").to_string();
                        if id.is_empty() {
                            tool_result = "ERROR: mark_subtask_failed requires 'id'".to_string();
                        } else if reason.is_empty() {
                            tool_result = format!("ERROR: mark_subtask_failed for '{}' requires a non-empty 'reason'", id);
                        } else {
                            match self.completeness.mark_failed(&id, reason.clone()) {
                                MarkOutcome::MarkedSkipped { reason: r } => {
                                    // mark_failed reuses the MarkedSkipped
                                    // variant in the enum because they
                                    // share "terminal + reason" semantics
                                    // for the dispatcher. The tracker
                                    // itself records Failed.
                                    println!(
                                        "[completeness] subtask '{}' marked failed: {}",
                                        id, r
                                    );
                                    if let Some(mut file) = self.transcript_file.as_ref() {
                                        let timestamp = std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_secs();
                                        let log_entry = format!(
                                            "[{}] [{}] MARK FAILED: id={} reason={}\n\n",
                                            timestamp, self.session_id, id, r
                                        );
                                        let _ = file.write_all(log_entry.as_bytes());
                                    }
                                    tool_result = format!(
                                        "Subtask '{}' marked failed (reason: {}). Tracker: {}",
                                        id,
                                        r,
                                        self.completeness.inline_status()
                                    );
                                }
                                MarkOutcome::UnknownId => {
                                    tool_result = format!(
                                        "ERROR: no subtask with id '{}' is currently declared.",
                                        id
                                    );
                                }
                                MarkOutcome::AlreadyTerminal { current } => {
                                    tool_result = format!(
                                        "ERROR: subtask '{}' is already in terminal status '{}'.",
                                        id,
                                        current.as_str()
                                    );
                                }
                                MarkOutcome::StaleEvidence { .. } => unreachable!(),
                                MarkOutcome::MarkedDone { .. } => unreachable!(),
                                MarkOutcome::MarkedExhausted { .. } => unreachable!(),
                            }
                        }
                    },
                    _ => {
                        tool_result = format!("Unknown tool '{}'", effective_name);
                    }
                }

                println!("Tool result: {}", tool_result);

                // Phase 1: log the tool result alongside the
                // dispatch event. We record the result length (not
                // the full text) so a quick filter can spot
                // abnormally long or short results without bloating
                // the log; the full text is in the transcript. For
                // navigate specifically, also record the resolved
                // URL so a grep can find "which branch of the URL
                // resolver actually fired?" in one place.
                let tool_result_preview: String = tool_result.chars().take(200).collect();
                tracing::info!(
                    event = "tool_result",
                    iter = self.iterations,
                    tool = %name,
                    result_len = tool_result.len(),
                    result_preview = %tool_result_preview,
                    "tool finished"
                );
                // Append tool response
                self.messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": &tool_result
                }));

                // Log to transcript
                let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                if let Some(mut file) = self.transcript_file.as_ref() {
                    let log_entry = format!("[{}] [{}] TOOL CALL: {} (args: {})\nRESULT: {}\n\n", timestamp, obs_summary, name, args_str, tool_result);
                    let _ = file.write_all(log_entry.as_bytes());
                }
                if let Some(tx) = &self.event_tx {
                    let _ = tx.send(AgentEvent::Tool {
                        timestamp_secs: timestamp,
                        name: name.to_string(),
                        args: args_str.to_string(),
                        result: tool_result.clone(),
                    });
                }

                // Phase 5: live progress line. Cheap, templated,
                // no LLM. The line is pushed to the agent's
                // `LiveProgress` buffer (capped at
                // `live_lines_cap`) and emitted on the event
                // channel as `AgentEvent::ProgressLine`. The
                // frontend's "agent is working" pill and the
                // per-task live progress sub-list both consume
                // these events. Unknown tool names are silently
                // skipped — see `summarizer::summarize`.
                //
                // Phase 6: pass `effective_name` /
                // `effective_args` so when the modal-dismiss
                // override fired, the progress line says
                // "Clicked @e1 (modal dismiss)" instead of the
                // original LLM tool. The user sees what
                // actually happened, not the redirected intent.
                self.emit_progress_line(&effective_name, &effective_args, &tool_result);

                // Phase 15.1: if the finish() tool handler stashed a
                // result (gate open), exit the loop now with that
                // result. The transcript logging above is the same
                // shape every other tool uses, so the per-tool log
                // line still records the finish() call.
                if let Some(result) = self.pending_finish_result.take() {
                    return Ok(result);
                }
            } else {
                let content = message["content"].as_str().unwrap_or("");
                println!("LLM generated text without tool call: {}", content);
                self.messages.push(json!({
                    "role": "user",
                    "content": "Please output a valid tool call."
                }));
            }
        }
    }
}

/// Phase 2: render the `PLAN:` block that is appended to the
/// agent's system prompt. The block is plain text, designed to be
/// read by the LLM on every iteration. Format:
///
/// ```text
/// PLAN (pre-flight decomposition):
///   1. [step-1] go to instagram
///   2. [step-2] text my friend hi
/// You must complete each subtask in order, calling
/// mark_subtask_done(id) after verifying each on a fresh snapshot.
/// You may re-declare via declare_subtasks while every subtask is
/// still Pending, but the canonical list above is owned by the
/// code — re-declaration is for refinement, not replacement.
/// ```
///
/// When the plan is empty (single-clause task), the block says
/// so explicitly so the LLM doesn't try to declare a 1-item
/// subtask list and trigger a re-declaration churn loop.
fn render_plan_block(plan: &crate::planner::Plan) -> String {
    if plan.subtasks.is_empty() {
        return "PLAN (pre-flight decomposition):\n  (none — single undifferentiated task)\nYou are not required to call declare_subtasks. Call finish() directly when the task is complete.".to_string();
    }
    let mut out = String::from(
        "PLAN (pre-flight decomposition):\n",
    );
    for (i, sub) in plan.subtasks.iter().enumerate() {
        out.push_str(&format!(
            "  {}. [{}] {}\n",
            i + 1,
            sub.id,
            sub.description
        ));
    }
    out.push_str(
        "You must complete each subtask in order. After verifying each one on a fresh snapshot, call mark_subtask_done(id) where `id` is the bracketed id above. You may re-declare via declare_subtasks while every subtask is still Pending, but the canonical list above is owned by the code — re-declaration is for refinement, not replacement. Calling finish() while any subtask is Pending is intercepted; the gate will force a snapshot re-prompt.",
    );
    out
}
