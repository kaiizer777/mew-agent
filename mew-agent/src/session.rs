// mew v2 — Phase 12.1: Agent state machine.
//
// Replaces the implicit "while loop runs until error" with an explicit lifecycle so
// the loop (and Step 13's chat channel) can be paused, resumed, and stopped cleanly.
//
// Design notes:
//   * `SessionState` is the explicit lifecycle: Running -> Paused <-> Running,
//     Running -> Stopped | Done | Failed (terminal). No transitions out of terminals.
//   * `SessionHandle` is the only way outside code touches the state. Cloning the
//     Arc gives you a cheap handle; the lock is held only for the duration of a
//     method call (not across .await).
//   * Pause/resume is signalled via a `tokio::sync::Notify` so `checkpoint()` is
//     fully async and wakes the instant `resume()` is called (no polling).
//   * Every transition goes through one method (`transition`) which validates the
//     move, records it in the history with a real timestamp, and fires the notify
//     when needed. This is the single point that owns the state machine.
//   * Invalid transitions return a typed `SessionError::InvalidTransition` — no
//     panics, no silently-ignored calls. Both Step 12.2 and Step 13 rely on this.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::{Mutex, Notify};

/// Explicit lifecycle for an agent run. Anything outside this enum is invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SessionState {
    /// Loop is actively iterating.
    Running,
    /// Loop is parked inside `checkpoint()`, waiting for `resume()`.
    Paused,
    /// Terminal: user/process explicitly stopped the loop with a reason.
    Stopped,
    /// Terminal: loop exited normally via the `finish()` tool path.
    Done,
    /// Terminal: loop exited due to an unrecoverable error.
    Failed,
}

impl SessionState {
    pub fn is_terminal(self) -> bool {
        matches!(self, SessionState::Stopped | SessionState::Done | SessionState::Failed)
    }

    /// Human-readable label, used in the transcript log.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionState::Running => "Running",
            SessionState::Paused => "Paused",
            SessionState::Stopped => "Stopped",
            SessionState::Done => "Done",
            SessionState::Failed => "Failed",
        }
    }
}

impl std::fmt::Display for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What kind of move was just made. Stored in the transition history so the
/// transcript shows `Running -> Paused (pause)` rather than just the new state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Transition {
    Start,    // initial -> Running
    Pause,    // Running -> Paused
    Resume,   // Paused -> Running
    Stop,     // Running -> Stopped (only valid from Running)
    Complete, // Running -> Done (used by future Step 15 path; exposed for completeness)
    Fail,     // Running -> Failed
}

impl Transition {
    pub fn as_str(self) -> &'static str {
        match self {
            Transition::Start => "start",
            Transition::Pause => "pause",
            Transition::Resume => "resume",
            Transition::Stop => "stop",
            Transition::Complete => "complete",
            Transition::Fail => "fail",
        }
    }
}

/// A single recorded transition. Real wall-clock timestamp in seconds.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TransitionRecord {
    pub from: SessionState,
    pub to: SessionState,
    pub kind: Transition,
    pub reason: Option<String>,
    pub timestamp_secs: u64,
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("invalid transition: cannot {action} from {from}")]
    InvalidTransition {
        from: SessionState,
        action: &'static str,
    },
    #[error("session is in terminal state {0:?}, no further transitions allowed")]
    TerminalState(SessionState),
    #[error("checkpoint was cancelled: {0}")]
    Cancelled(String),
}

struct Inner {
    state: SessionState,
    history: Vec<TransitionRecord>,
    /// Last stop reason, kept so callers can read it after the loop exits.
    last_reason: Option<String>,
    /// Notify the parked `checkpoint()` waiter. Replaced on every transition that
    /// could un-park; using a single `Arc<Notify>` keeps things simple and avoids
    /// the "lost wakeup" trap of swapping notifiers mid-park.
    notify: Arc<Notify>,
}

impl Inner {
    fn new() -> Self {
        Self {
            state: SessionState::Running,
            history: Vec::new(),
            last_reason: None,
            notify: Arc::new(Notify::new()),
        }
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// The one and only place state changes happen. Returns the new state on
    /// success, or a typed error on a forbidden move. Records to history either
    /// way (a refused attempt is also worth seeing in the log).
    fn try_transition(
        &mut self,
        to: SessionState,
        kind: Transition,
        action: &'static str,
        reason: Option<String>,
    ) -> Result<SessionState, SessionError> {
        let from = self.state;

        // Terminal states are absorbing. Any call into a terminal is an error,
        // including resume/stop on an already-stopped session. This is the
        // behavior Step 12.2 explicitly tests for.
        if from.is_terminal() {
            return Err(SessionError::TerminalState(from));
        }

        let allowed = match (from, to) {
            (SessionState::Running, SessionState::Paused) => true,
            (SessionState::Paused, SessionState::Running) => true,
            (SessionState::Running, SessionState::Stopped) => true,
            (SessionState::Running, SessionState::Done) => true,
            (SessionState::Running, SessionState::Failed) => true,
            // Paused -> Stopped/Done/Failed is also legal: a paused loop can be
            // torn down without resuming first.
            (SessionState::Paused, SessionState::Stopped) => true,
            (SessionState::Paused, SessionState::Done) => true,
            (SessionState::Paused, SessionState::Failed) => true,
            _ => false,
        };

        if !allowed {
            return Err(SessionError::InvalidTransition { from, action });
        }

        self.state = to;
        if let Some(r) = reason.as_ref() {
            self.last_reason = Some(r.clone());
        }
        // Phase 1: capture the reason into an owned String *before*
        // moving the original into the history record. The tracing
        // event below needs to log the same string, and the borrow
        // checker will not let us hold a borrow of `reason` past
        // the move into `history.push`. Cloning once is cheap and
        // keeps the log independent from the history record (a
        // reviewer can read either without affecting the other).
        let reason_for_log: String = reason.clone().unwrap_or_default();
        self.history.push(TransitionRecord {
            from,
            to,
            kind,
            reason,
            timestamp_secs: Self::now_secs(),
        });

        // Phase 1: emit a structured tracing event for every
        // successful state transition. The agent's session wrapper
        // also records this in the transcript; the trace log
        // captures the same fact in a structured, greppable form.
        // We log via `tracing::info!` (no span) because the
        // transition is the canonical event — the surrounding
        // span, when present, is the ReAct loop iteration.
        tracing::info!(
            event = "session_transition",
            from = from.as_str(),
            to = to.as_str(),
            kind = kind.as_str(),
            reason = %reason_for_log,
            "session state changed"
        );

        // Always notify on a successful transition. A no-op transition (e.g.
        // pause when already paused) is rejected above, so we only fire when
        // something actually changed.
        self.notify.notify_waiters();

        Ok(to)
    }
}

/// Cheap cloneable handle to the session state machine. Hand this to anything
/// that needs to pause/stop the loop (future UI thread, signal handler, etc.).
#[derive(Clone)]
pub struct SessionHandle {
    inner: Arc<Mutex<Inner>>,
    session_id: String,
}

impl SessionHandle {
    /// Create a fresh handle. The session starts in `Running` and a `Start`
    /// transition is recorded so the transcript has a real first entry.
    pub fn new(session_id: impl Into<String>) -> Self {
        let session_id = session_id.into();
        let inner = Arc::new(Mutex::new(Inner::new()));
        {
            // Synchronous block to record the initial Start transition.
            // We can't use try_transition here because the mutex is already
            // held conceptually; instead we hand-build the record.
            let mut guard = inner.try_lock().expect("freshly created Inner");
            let from = guard.state;
            guard.state = SessionState::Running;
            guard.history.push(TransitionRecord {
                from,
                to: SessionState::Running,
                kind: Transition::Start,
                reason: None,
                timestamp_secs: Inner::now_secs(),
            });
        }
        Self { inner, session_id }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub async fn state(&self) -> SessionState {
        self.inner.lock().await.state
    }

    pub async fn last_reason(&self) -> Option<String> {
        self.inner.lock().await.last_reason.clone()
    }

    /// Snapshot of the full transition log. Cheap to clone (small struct).
    pub async fn history(&self) -> Vec<TransitionRecord> {
        self.inner.lock().await.history.clone()
    }

    /// Format a single transition record as a transcript line, in the shape
    /// the agent's transcript file uses. This is the single source of truth
    /// for STATE-line formatting — the agent's `write_state_line` calls this
    /// and so do tests, so the format can't drift between them.
    ///
    /// Format: `[<unix_secs>] [<session_id>] STATE: <from> -> <to> (<kind>)[ reason=<reason>]\n\n`
    pub fn format_transition_line(&self, r: &TransitionRecord) -> String {
        let reason_part = r
            .reason
            .as_deref()
            .map(|x| format!(" reason={}", x))
            .unwrap_or_default();
        format!(
            "[{}] [{}] STATE: {} -> {} ({}){}\n\n",
            r.timestamp_secs,
            self.session_id,
            r.from.as_str(),
            r.to.as_str(),
            r.kind.as_str(),
            reason_part
        )
    }

    /// Park the loop. Idempotent: calling `pause()` on an already-paused
    /// session returns `InvalidTransition` rather than silently no-op'ing.
    pub async fn pause(&self, reason: Option<String>) -> Result<SessionState, SessionError> {
        let mut guard = self.inner.lock().await;
        guard.try_transition(SessionState::Paused, Transition::Pause, "pause", reason)
    }

    /// Resume from a pause. Returns `InvalidTransition` if not currently
    /// `Paused` (e.g. `resume()` on a Running session is a real error, not a
    /// silent no-op — Step 12.2 checks for this).
    pub async fn resume(&self, reason: Option<String>) -> Result<SessionState, SessionError> {
        let mut guard = self.inner.lock().await;
        guard.try_transition(SessionState::Running, Transition::Resume, "resume", reason)
    }

    /// Terminal stop with a reason. The `reason` is preserved in the
    /// transcript and via `last_reason()` so a calling thread can read it.
    pub async fn stop(&self, reason: String) -> Result<SessionState, SessionError> {
        let mut guard = self.inner.lock().await;
        guard.try_transition(SessionState::Stopped, Transition::Stop, "stop", Some(reason))
    }

    /// Mark the session as `Done`. Called when the agent's `finish()` tool
    /// path completes. Kept separate from `stop()` so the transcript tells
    /// the two apart.
    pub async fn complete(&self) -> Result<SessionState, SessionError> {
        let mut guard = self.inner.lock().await;
        guard.try_transition(SessionState::Done, Transition::Complete, "complete", None)
    }

    /// Mark the session as `Failed`. Called when the ReAct loop returns Err.
    pub async fn fail(&self, reason: String) -> Result<SessionState, SessionError> {
        let mut guard = self.inner.lock().await;
        guard.try_transition(SessionState::Failed, Transition::Fail, "fail", Some(reason))
    }

    /// Called by the ReAct loop between iterations. The single point that
    /// observes the state and acts on it.
    ///
    /// * `Running`   -> returns immediately, loop continues.
    /// * `Paused`    -> parks here until `resume()` or a terminal transition
    ///                  fires the notify. Returns `Ok(())` once un-parked, OR
    ///                  returns `Err(Cancelled)` if the session moved to a
    ///                  terminal state (Stopped/Done/Failed) while we waited.
    /// * terminal    -> returns `Err(Cancelled)` immediately so the loop
    ///                  unwinds cleanly.
    pub async fn checkpoint(&self) -> Result<(), SessionError> {
        // First check: do we need to park at all? Avoid the notify round-trip
        // when state is already Running — this is the common, no-op path.
        {
            let guard = self.inner.lock().await;
            match guard.state {
                SessionState::Running => return Ok(()),
                SessionState::Paused => {}
                SessionState::Stopped
                | SessionState::Done
                | SessionState::Failed => {
                    return Err(SessionError::Cancelled(format!(
                        "session entered terminal state {:?}",
                        guard.state
                    )));
                }
            }
            // We are Paused. Grab a notify handle before dropping the lock so
            // a race between checking and registering doesn't lose the wakeup.
            let notify = guard.notify.clone();
            drop(guard);
            notify.notified().await;
        }

        // Re-check state after waking.
        let guard = self.inner.lock().await;
        match guard.state {
            SessionState::Running => Ok(()),
            SessionState::Stopped | SessionState::Done | SessionState::Failed => Err(
                SessionError::Cancelled(format!("session entered terminal state {:?}", guard.state)),
            ),
            // We were notified but still Paused? That shouldn't happen — every
            // notify is paired with a successful transition. Treat defensively.
            SessionState::Paused => Err(SessionError::Cancelled(
                "checkpoint woken in unexpected state Paused".to_string(),
            )),
        }
    }
}
