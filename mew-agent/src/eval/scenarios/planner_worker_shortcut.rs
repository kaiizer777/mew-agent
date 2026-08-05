//! Phase 17 — Evaluation harness scenarios for planner-worker contract.
//!
//! Re-enforces the "no shortcut" claim:
//! 1. Happy path: worker signature matches planner signature -> MarkedDone, attempts == 1, evidence populated.
//! 2. Worker shortcut: worker supplies fake signature -> rejected as StaleEvidence, attempts == 2 after retry, status Failed.
//! 3. Stale evidence: worker reuses signature from a previous iteration -> rejected as StaleEvidence, status stays Pending.

use crate::eval::assertions::{assert_todo_done, assert_todo_rejected, AssertionResult};
use crate::todo::{
    planner_signature, MarkTodoOutcome, Todo, TodoId, TodoStatus,
};

#[derive(Clone)]
pub struct PlannerWorkerScenario {
    pub id: &'static str,
    pub description: &'static str,
    pub run_fn: fn() -> AssertionResult,
}

impl PlannerWorkerScenario {
    pub fn new(
        id: &'static str,
        description: &'static str,
        run_fn: fn() -> AssertionResult,
    ) -> Self {
        Self { id, description, run_fn }
    }

    pub fn run(&self) -> AssertionResult {
        (self.run_fn)()
    }
}

/// Scenario 1: Happy path — worker reports Done with signature matching planner's.
/// Assert: todo transitions to Done, evidence populated, attempts == 1.
pub fn accept_on_match() -> AssertionResult {
    let mut todo = Todo::new(
        TodoId::from_slug("navigate-instagram", 0),
        "navigate instagram",
        None,
    );
    let obs_text = "<html><head><title>Instagram</title></head><body><div role=\"main\">Instagram feed</div></body></html>";
    let worker_sig = planner_signature(obs_text);
    let now = 1700000000;

    let outcome = todo.mark_done(&worker_sig, obs_text, 1, now, 3);
    match outcome {
        MarkTodoOutcome::MarkedDone { evidence } => {
            if todo.attempts != 1 {
                return Err(format!("expected attempts == 1, got {}", todo.attempts));
            }
            assert_todo_done(&todo, Some(&evidence))?;
            Ok(())
        }
        other => Err(format!("expected MarkedDone, got {:?}", other)),
    }
}

/// Scenario 2: Worker shortcut — worker reports Done with a fake signature.
/// Assert: todo stays Pending on 1st attempt, attempts == 2 after retry, eventual Failed on second mismatch.
pub fn reject_on_mismatch() -> AssertionResult {
    let mut todo = Todo::new(
        TodoId::from_slug("navigate-instagram", 0),
        "navigate instagram",
        None,
    );
    let real_obs_text = "<html><body>Real Instagram Content</body></html>";
    let fake_worker_sig = "len:00000000";
    let now = 1700000000;

    // Attempt 1: 1st fake signature attempt, max_attempts = 2
    let outcome1 = todo.mark_done(fake_worker_sig, real_obs_text, 1, now, 2);
    match outcome1 {
        MarkTodoOutcome::StaleEvidence(mismatch) => {
            if mismatch.worker_signature != fake_worker_sig {
                return Err(format!(
                    "expected worker sig {}, got {}",
                    fake_worker_sig, mismatch.worker_signature
                ));
            }
            if todo.status != TodoStatus::Pending {
                return Err(format!(
                    "expected todo status Pending after 1st failed attempt, got {:?}",
                    todo.status
                ));
            }
        }
        other => return Err(format!("expected StaleEvidence on 1st attempt, got {:?}", other)),
    }

    // Attempt 2: retry with fake signature, hitting max_attempts = 2
    let outcome2 = todo.mark_done(fake_worker_sig, real_obs_text, 2, now + 1, 2);
    match outcome2 {
        MarkTodoOutcome::StaleEvidence(_) => {
            if todo.attempts != 2 {
                return Err(format!("expected attempts == 2, got {}", todo.attempts));
            }
            assert_todo_rejected(&todo, Some("snapshot signature mismatch"))?;
            Ok(())
        }
        other => Err(format!("expected StaleEvidence on 2nd attempt, got {:?}", other)),
    }
}

/// Scenario 3: Stale evidence — worker re-uses evidence from a previous iteration.
/// Assert: rejected as StaleEvidence, todo stays Pending.
pub fn retry_on_stale_evidence() -> AssertionResult {
    let mut todo = Todo::new(
        TodoId::from_slug("send-message", 1),
        "send message",
        None,
    );
    // Simulate that the todo was already evaluated at iteration 2
    todo.last_evidence_iteration = Some(2);
    todo.attempts = 1;

    let obs_text = "<html><body>Message sent page</body></html>";
    let valid_sig = planner_signature(obs_text);
    let now = 1700000000;

    // Worker attempts to submit evidence using stale iteration 1 (<= last_evidence_iteration 2)
    let outcome = todo.mark_done(&valid_sig, obs_text, 1, now, 3);
    match outcome {
        MarkTodoOutcome::StaleEvidence(mismatch) => {
            if !mismatch.reason.contains("stale iteration") {
                return Err(format!(
                    "expected mismatch reason to mention stale iteration, got {:?}",
                    mismatch.reason
                ));
            }
            if todo.status != TodoStatus::Pending {
                return Err(format!(
                    "expected todo status to remain Pending, got {:?}",
                    todo.status
                ));
            }
            if todo.attempts != 2 {
                return Err(format!("expected attempts incremented to 2, got {}", todo.attempts));
            }
            assert_todo_rejected(&todo, None)?;
            Ok(())
        }
        other => Err(format!("expected StaleEvidence outcome for stale iteration, got {:?}", other)),
    }
}

pub fn all_planner_shortcut_scenarios() -> Vec<PlannerWorkerScenario> {
    vec![
        PlannerWorkerScenario::new(
            "planner_accept_on_match",
            "Happy path: worker evidence matches planner signature -> MarkedDone",
            accept_on_match,
        ),
        PlannerWorkerScenario::new(
            "planner_reject_on_mismatch",
            "Worker shortcut: fake signature rejected -> Pending then Failed",
            reject_on_mismatch,
        ),
        PlannerWorkerScenario::new(
            "planner_retry_on_stale_evidence",
            "Stale evidence: re-used iteration signature rejected -> Pending",
            retry_on_stale_evidence,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_on_match_scenario_passes() {
        assert!(accept_on_match().is_ok());
    }

    #[test]
    fn reject_on_mismatch_scenario_passes() {
        assert!(reject_on_mismatch().is_ok());
    }

    #[test]
    fn retry_on_stale_evidence_scenario_passes() {
        assert!(retry_on_stale_evidence().is_ok());
    }
}
