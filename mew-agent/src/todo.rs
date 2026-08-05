// mew v2 — Phase 11: Todo schema and decomposition contract.
//
// Pure data + contract for worker execution of planned subtasks.

use std::fmt;
use std::ops::Deref;
use serde::{Deserialize, Serialize};

pub use crate::completeness::SubTaskStatus as TodoStatus;

/// Unique identifier for a `Todo` item.
/// Wraps a slugified string (e.g. "navigate-instagram-1").
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TodoId(pub String);

impl TodoId {
    /// Create a `TodoId` by slugifying the description and appending `-N`.
    /// Thin wrapper over `mew_agent::planner::slugify`.
    pub fn from_slug(description: &str, index: usize) -> Self {
        Self(crate::planner::slugify(description, index))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Deref for TodoId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<&str> for TodoId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for TodoId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl fmt::Display for TodoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for TodoId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// The kind of acceptance criterion expected for a `Todo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AcceptanceKind {
    /// Planner expects page URL to equal `value`
    UrlAt,
    /// Planner expects AX-tree text to contain `value`
    TextInSnapshot,
    /// Planner expects an interactive element with `value` as label
    ElementPresent,
    /// Planner only requires a fresh snapshot, no semantic check
    AnySnapshot,
}

/// Criterion specifying what "done" means for a `Todo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceCriterion {
    pub kind: AcceptanceKind,
    pub value: String,
}

impl AcceptanceCriterion {
    pub fn new(kind: AcceptanceKind, value: impl Into<String>) -> Self {
        Self {
            kind,
            value: value.into(),
        }
    }
}

/// Evidence recorded when a `Todo` reaches a terminal state (`Done`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub todo_id: TodoId,
    pub worker_signature: String,
    pub planner_signature: String,
    pub verified_at_secs: u64,
}

/// Resource budget (step and time caps) for executing a single `Todo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoBudget {
    pub max_steps: u32,
    pub max_seconds: u64,
}

impl Default for TodoBudget {
    fn default() -> Self {
        Self {
            max_steps: 10,
            max_seconds: 60,
        }
    }
}

/// Representation of a planned todo item for worker execution.
///
/// Invariant: `status == TodoStatus::Done ⇒ evidence.is_some() ∧ evidence.planner_signature == evidence.worker_signature`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub id: TodoId,
    pub intent: String,
    pub acceptance: Option<AcceptanceCriterion>,
    pub depends_on: Vec<TodoId>,
    pub status: TodoStatus,
    pub evidence: Option<Evidence>,
    pub attempts: u8,
    pub budget: TodoBudget,
    /// The iteration number of the last `mark_done` call that succeeded.
    /// `None` until the todo is terminal-`Done`. Used by Phase 13's
    /// worker to detect the "model re-uses old evidence" shortcut
    /// without the caller having to thread the value through.
    pub last_evidence_iteration: Option<usize>,
}

impl Todo {
    pub fn new(id: TodoId, intent: impl Into<String>, acceptance: Option<AcceptanceCriterion>) -> Self {
        Self {
            id,
            intent: intent.into(),
            acceptance,
            depends_on: Vec::new(),
            status: TodoStatus::Pending,
            evidence: None,
            attempts: 0,
            budget: TodoBudget::default(),
            last_evidence_iteration: None,
        }
    }

    /// Attempt to mark the todo as done with the worker's reported signature and observation text.
    /// Enforces iteration freshness and evidence verification.
    pub fn mark_done(
        &mut self,
        worker_sig: &str,
        obs_text: &str,
        evidence_iteration: usize,
        verified_at_secs: u64,
        max_attempts: u8,
    ) -> MarkTodoOutcome {
        if self.status.is_terminal() || self.evidence.is_some() {
            return MarkTodoOutcome::AlreadyTerminal {
                current: self.status.clone(),
            };
        }

        if let Some(last_iter) = self.last_evidence_iteration {
            if evidence_iteration <= last_iter {
                self.attempts = self.attempts.saturating_add(1);
                let mismatch = EvidenceMismatch {
                    worker_signature: worker_sig.to_string(),
                    planner_signature: planner_signature(obs_text),
                    reason: format!(
                        "stale iteration {} <= last marked iteration {}",
                        evidence_iteration, last_iter
                    ),
                };
                if self.attempts >= max_attempts {
                    self.status = TodoStatus::Failed {
                        reason: mismatch.reason.clone(),
                    };
                }
                return MarkTodoOutcome::StaleEvidence(mismatch);
            }
        }

        match verify_evidence(worker_sig, obs_text) {
            Ok(planner_sig) => {
                self.attempts = self.attempts.saturating_add(1);
                let evidence = Evidence {
                    todo_id: self.id.clone(),
                    worker_signature: worker_sig.to_string(),
                    planner_signature: planner_sig,
                    verified_at_secs,
                };
                self.evidence = Some(evidence.clone());
                self.status = TodoStatus::Done;
                self.last_evidence_iteration = Some(evidence_iteration);
                MarkTodoOutcome::MarkedDone { evidence }
            }
            Err(mismatch) => {
                self.attempts = self.attempts.saturating_add(1);
                if self.attempts >= max_attempts {
                    self.status = TodoStatus::Failed {
                        reason: mismatch.reason.clone(),
                    };
                }
                MarkTodoOutcome::StaleEvidence(mismatch)
            }
        }
    }
}

/// Compute planner signature for given AX-tree text using canonical `len:{:08x}` DefaultHasher algorithm.
pub fn planner_signature(obs_text: &str) -> String {
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
}

/// Error type returned when evidence verification fails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceMismatch {
    pub worker_signature: String,
    pub planner_signature: String,
    pub reason: String,
}

impl fmt::Display for EvidenceMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Evidence mismatch: worker={}, planner={}, reason={}",
            self.worker_signature, self.planner_signature, self.reason
        )
    }
}

impl std::error::Error for EvidenceMismatch {}

/// Outcome of attempting to mark a `Todo` as done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkTodoOutcome {
    MarkedDone { evidence: Evidence },
    StaleEvidence(EvidenceMismatch),
    AlreadyTerminal { current: TodoStatus },
}

/// Verify worker evidence signature against raw observation text.
/// Fails closed on empty observation text or signature mismatch.
pub fn verify_evidence(worker_sig: &str, obs_text: &str) -> Result<String, EvidenceMismatch> {
    if obs_text.is_empty() {
        return Err(EvidenceMismatch {
            worker_signature: worker_sig.to_string(),
            planner_signature: planner_signature(obs_text),
            reason: "empty observation text".to_string(),
        });
    }
    let planner_sig = planner_signature(obs_text);
    if worker_sig != planner_sig {
        return Err(EvidenceMismatch {
            worker_signature: worker_sig.to_string(),
            planner_signature: planner_sig,
            reason: "snapshot signature mismatch".to_string(),
        });
    }
    Ok(planner_sig)
}

/// Convert a `MarkTodoOutcome` rejection into the typed
/// event the orchestrator emits to the sink. Pure data transform —
/// does not touch the sink directly, so tests can call it
/// without a `TurnSink` mock.
///
/// `task_id` is injected at the call site (the planner knows
/// the task; `mark_done` does not). The returned event is what
/// `OrchestratorEvent::TodoRejected` would carry.
pub fn todo_rejected_event(
    task_id: &str,
    todo_id: &TodoId,
    mismatch: &EvidenceMismatch,
) -> TodoRejectedEvent {
    TodoRejectedEvent {
        task_id: task_id.to_string(),
        todo_id: todo_id.clone(),
        evidence: Some(mismatch.clone()),
        reason: None,
    }
}

/// Build a `TodoRejectedEvent` for the user-cancel path. Phase 14
/// `cancel_todo` calls this to construct the event payload before
/// passing it to `OrchestratorEvent::TodoRejected`.
pub fn todo_cancelled_event(
    task_id: &str,
    todo_id: &TodoId,
    reason: impl Into<String>,
) -> TodoRejectedEvent {
    TodoRejectedEvent {
        task_id: task_id.to_string(),
        todo_id: todo_id.clone(),
        evidence: None,
        reason: Some(reason.into()),
    }
}

/// Typed view of `OrchestratorEvent::TodoRejected` that lives in
/// `todo.rs` so the data side can be tested without the orchestrator's
/// event sink. Mirrors the variant shape in `orchestrator.rs` —
/// exactly one of `evidence` and `reason` is `Some`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoRejectedEvent {
    pub task_id: String,
    pub todo_id: TodoId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<EvidenceMismatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

