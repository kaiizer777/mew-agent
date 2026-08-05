// mew v2 — Phase 13: Supervisor signal model.
//
// Defines the signal types sent by the outer planner to steer or cancel
// a long-lived `BrowserAgentWorker`.

use serde::{Deserialize, Serialize};
use crate::todo::Todo;

/// Signals sent by the planner supervisor to direct or stop worker execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum SupervisorSignal {
    /// Pause execution between tool dispatches.
    Pause,
    /// Resume execution from a paused state.
    Resume,
    /// Immediately cancel execution of the active todo.
    Cancel,
    /// Replace the active / pending todo set with a new list of todos.
    Replan(Vec<Todo>),
}

/// A supervisor signal packaged with a monotonically increasing `signal_id`.
///
/// The worker maintains a watermark `signal_id` and discards any command
/// with `signal_id <= watermark` to prevent race conditions or stale signals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorCommand {
    pub signal_id: u64,
    pub signal: SupervisorSignal,
}

impl SupervisorCommand {
    pub fn new(signal_id: u64, signal: SupervisorSignal) -> Self {
        Self { signal_id, signal }
    }

    /// Check if this command is fresher than the given watermark.
    pub fn is_fresh(&self, watermark: u64) -> bool {
        self.signal_id > watermark
    }
}
