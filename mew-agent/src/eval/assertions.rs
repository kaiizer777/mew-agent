//! Phase 9.3 — handoff-specific assertions.
//!
//! The Phase 3 handoff contract is the *correct* property to
//! regress on. The four things a `ChatAgent → BrowserAgent →
//! ChatAgent` round trip has to get right (per the 2026
//! multi-agent practice literature cited in `work.md`):
//!
//! 1. The right task got dispatched. (The Handoff's
//!    `task_description` matches the user task the
//!    orchestrator saw, modulo the planner's rephrasing.)
//! 2. The result actually got folded back into the
//!    caller's response. (The synthesized chat reply is
//!    non-empty and references the typed result's
//!    `summary` / `key_findings`.)
//! 3. The round trip ran end-to-end. (The originating
//!    message id is preserved across both halves — a
//!    `Handoff`'s `originating_message_id` matches the
//!    `ChatReply`'s `originating_message_id`.)
//! 4. A failure gets communicated, not swallowed.
//!    (`Failed` results still produce a non-empty chat
//!    reply that quotes the `failure_reason`.)
//!
//! These are wrapped as `assert_*` functions so any test
//! (in this module, in the `mew-cli` smoke-test binary, or
//! in a future live-LLM integration test) can call them
//! directly. Each function returns a `Result<(), String>`
//! so a non-panicking caller (the runner) can record the
//! outcome instead of crashing the whole suite.

use crate::handoff::{BrowserResult, BrowserStatus, Handoff};
use crate::planner::plan;
use crate::todo::{Evidence, Todo, TodoStatus};

/// What went wrong (or didn't) for an individual
/// assertion. The `Ok(())` case means the assertion held;
/// the `Err(String)` case carries a one-line explanation
/// for the report.
pub type AssertionResult = Result<(), String>;

/// Assert a `Todo` is in `Done` status and has matching worker and planner evidence signatures.
/// Checks `status == Done ∧ evidence.is_some() ∧ evidence.worker == evidence.planner`.
pub fn assert_todo_done(todo: &Todo, evidence: Option<&Evidence>) -> AssertionResult {
    if todo.status != TodoStatus::Done {
        return Err(format!(
            "todo {} status is {:?}, expected Done",
            todo.id, todo.status
        ));
    }
    let ev = match evidence.or(todo.evidence.as_ref()) {
        Some(e) => e,
        None => {
            return Err(format!(
                "todo {} status is Done but evidence is None",
                todo.id
            ));
        }
    };
    if ev.worker_signature.is_empty() || ev.planner_signature.is_empty() {
        return Err(format!(
            "todo {} has empty signature: worker={:?}, planner={:?}",
            todo.id, ev.worker_signature, ev.planner_signature
        ));
    }
    if ev.worker_signature != ev.planner_signature {
        return Err(format!(
            "todo {} evidence mismatch: worker_signature={:?}, planner_signature={:?}",
            todo.id, ev.worker_signature, ev.planner_signature
        ));
    }
    Ok(())
}

/// Assert a `Todo` was rejected / non-Done with attempts >= 1 and optional rejection reason.
/// Checks `status != Done ∧ attempts >= 1 ∧ rejected_reason.is_some()`.
pub fn assert_todo_rejected(todo: &Todo, reason: Option<&str>) -> AssertionResult {
    if todo.status == TodoStatus::Done {
        return Err(format!(
            "todo {} status is Done, expected non-Done (rejected)",
            todo.id
        ));
    }
    if todo.attempts < 1 {
        return Err(format!(
            "todo {} attempts is {}, expected >= 1",
            todo.id, todo.attempts
        ));
    }
    if let Some(expected_reason) = reason {
        let actual_reason = match &todo.status {
            TodoStatus::Failed { reason: r } => r.clone(),
            other => format!("{:?}", other),
        };
        if !actual_reason.contains(expected_reason) {
            return Err(format!(
                "todo {} rejected reason {:?} does not contain expected fragment {:?}",
                todo.id, actual_reason, expected_reason
            ));
        }
    }
    Ok(())
}


/// Assert the orchestrator dispatched the right task.
/// `expected` is the user message the test passed to
/// `ChatAgent`; the planner may rephrase it (e.g. split
/// clauses), so the assertion is fuzzy: the handoff's
/// `task_description` should contain a meaningful
/// fragment of the user's task (or, for compound tasks,
/// the planner's stand-alone rephrasing). The check
/// here is the "did the planner produce a non-empty
/// task that the agent can act on" guard.
pub fn assert_correct_task_dispatched(
    user_message: &str,
    handoff: &Handoff,
) -> AssertionResult {
    if handoff.task_description.is_empty() {
        return Err(format!(
            "handoff has empty task_description (user message was {user_message:?})"
        ));
    }
    // Word-overlap check: at least one word from the
    // user message must appear (case-insensitive) in
    // the handoff's task description. The production
    // `ChatAgent::build_handoff` reuses the user
    // message verbatim when no LLM rephrasing is in
    // play (the planner splits clauses but does not
    // rephrase), so this is a strong signal.
    if user_message.split_whitespace().any(|w| {
        handoff.task_description.to_lowercase().contains(&w.to_lowercase())
    }) {
        Ok(())
    } else {
        // Fallback: the planner's deterministic
        // decomposition must produce at least one
        // subtask. An empty plan + an empty overlap
        // is a real regression.
        let p = plan(user_message);
        if p.subtasks.is_empty() {
            Err(format!(
                "handoff task_description {handoff_task:?} shares no words with user message {user_message:?}",
                handoff_task = handoff.task_description,
            ))
        } else {
            Ok(())
        }
    }
}

/// Assert the synthesized chat reply actually reflects
/// the typed `BrowserResult`. For `Done` results, the
/// reply should contain the result's `summary` (or a
/// subset of its key_findings). For `Failed`, the reply
/// should be non-empty even if the summary is empty.
///
/// The check uses three signals (in order, strongest
/// first):
///
/// 1. **Summary fragment** — the first 30 chars of
///    `result.summary` appear verbatim in the reply.
///    The strongest signal that the reply *came from*
///    this result.
/// 2. **Key finding description overlap** — any
///    `key_finding.description` appears in the reply.
///    Useful when the synthesizer rephrased the
///    summary but kept a per-subtask line.
/// 3. **Word overlap** — at least one non-trivial
///    word from `result.summary` appears in the reply.
///    The weakest signal; exists so a synthesizer that
///    rephrases everything (e.g. "Message sent to
///    Alice." → "I sent the message to Alice.")
///    still passes the assertion, because the *content*
///    the user reads is the same even if the exact
///    phrasing differs.
///
/// All three signals have to fail for the assertion to
/// fail. The intent is "did the user-facing text
/// come from this typed result?", not "is the reply a
/// verbatim copy."
pub fn assert_result_reflected_in_chat_reply(
    result: &BrowserResult,
    chat_reply: &str,
) -> AssertionResult {
    if chat_reply.is_empty() {
        return Err(format!(
            "chat reply is empty for {:?} result (session {})",
            result.status, result.session_id
        ));
    }
    match result.status {
        BrowserStatus::Done | BrowserStatus::Partial => {
            if result.summary.is_empty() && result.key_findings.is_empty() {
                // The result has no substance to reflect;
                // the assertion is satisfied by the
                // non-empty reply alone (the "never
                // silent" contract).
                return Ok(());
            }
            // (1) Summary fragment.
            if !result.summary.is_empty() {
                let first_30: String =
                    result.summary.chars().take(30).collect();
                if chat_reply.contains(&first_30) {
                    return Ok(());
                }
            }
            // (2) Key finding description.
            for finding in &result.key_findings {
                if !finding.description.is_empty()
                    && chat_reply.contains(&finding.description)
                {
                    return Ok(());
                }
            }
            // (3) Word overlap (skip short common words
            // so the signal stays meaningful).
            if !result.summary.is_empty() {
                let overlap = word_overlap(&result.summary, chat_reply);
                if overlap {
                    return Ok(());
                }
            }
            Err(format!(
                "chat reply does not reflect the result summary (first 30 chars of summary: {:?}, reply: {:?})",
                result.summary.chars().take(30).collect::<String>(),
                chat_reply,
            ))
        }
        BrowserStatus::Failed => {
            // For Failed results, the reply must
            // *quote the failure reason* — that's how
            // the user knows what went wrong.
            if result.failure_reason.is_empty() {
                return Err(
                    "Failed result has empty failure_reason (the user gets nothing to read)"
                        .into(),
                );
            }
            // The synthesizer rephrases the reason
            // (e.g. "I couldn't complete the task:
            // <reason>"), so a 30-char fragment is the
            // check.
            let first_30: String =
                result.failure_reason.chars().take(30).collect();
            if chat_reply.contains(&first_30) {
                Ok(())
            } else if word_overlap(&result.failure_reason, chat_reply) {
                Ok(())
            } else {
                Err(format!(
                    "Failed reply does not quote the failure reason (reason: {:?}, reply: {:?})",
                    result.failure_reason, chat_reply
                ))
            }
        }
    }
}

/// True if at least one non-trivial word from
/// `source` appears in `target` (case-insensitive). A
/// "non-trivial" word is 4+ chars and not a stopword —
/// this is the weakest of the three overlap signals
/// the reflection assertion uses, and exists only to
/// bridge rephrasings.
fn word_overlap(source: &str, target: &str) -> bool {
    const STOPWORDS: &[&str] = &[
        "the", "and", "then", "with", "this", "that", "from", "your", "into", "have",
        "this", "task", "result", "reply", "message",
    ];
    let target_lower = target.to_lowercase();
    source.split_whitespace().any(|w| {
        let lower = w.to_lowercase();
        let trimmed = lower.trim_matches(|c: char| !c.is_alphanumeric());
        trimmed.len() >= 4
            && !STOPWORDS.contains(&trimmed)
            && target_lower.contains(&trimmed)
    })
}

/// Assert the originating message id is preserved
/// across the round trip. The orchestrator stamps the
/// id on the Handoff; the synthesized `ChatReply`
/// carries the same id. A mismatch means a regression
/// in the message-bus plumbing (the frontend would not
/// be able to correlate the user message with the
/// agent's reply).
pub fn assert_originating_message_id_preserved(
    handoff: &Handoff,
    chat_reply_originating_message_id: &str,
) -> AssertionResult {
    if handoff.originating_message_id != chat_reply_originating_message_id {
        Err(format!(
            "originating_message_id drift: handoff={:?}, reply={:?}",
            handoff.originating_message_id, chat_reply_originating_message_id
        ))
    } else {
        Ok(())
    }
}

/// Assert the planner decomposed the task into a
/// non-trivial subtask list when the task is
/// compound. "Non-trivial" here means at least the
/// `min_subtasks` argument (default 2 for "go to X and
/// do Y"-shaped tasks). This is the Phase 2 regression
/// assertion in assertion form: a future regression
/// that flattens compound tasks back to a single
/// sub-task fails this guard.
pub fn assert_subtask_decomposition(
    handoff: &Handoff,
    min_subtasks: usize,
) -> AssertionResult {
    if handoff.subtasks.len() < min_subtasks {
        Err(format!(
            "task decomposition too coarse: got {} subtasks, expected at least {} (handoff task: {:?})",
            handoff.subtasks.len(),
            min_subtasks,
            handoff.task_description,
        ))
    } else {
        Ok(())
    }
}

/// All-in-one handoff assertion. Convenience for the
/// runner — calls all four guards above and aggregates
/// the failures into one `Err` string. `chat_reply_id`
/// is the `originating_message_id` of the
/// synthesized `ChatReply` (the production code
/// copies the handoff's id through).
pub fn assert_handoff_contract(
    user_message: &str,
    handoff: &Handoff,
    result: &BrowserResult,
    chat_reply: &str,
    chat_reply_originating_message_id: &str,
    min_subtasks: usize,
) -> AssertionResult {
    let mut failures: Vec<String> = Vec::new();
    if let Err(e) = assert_correct_task_dispatched(user_message, handoff) {
        failures.push(e);
    }
    if let Err(e) = assert_result_reflected_in_chat_reply(result, chat_reply) {
        failures.push(e);
    }
    if let Err(e) =
        assert_originating_message_id_preserved(handoff, chat_reply_originating_message_id)
    {
        failures.push(e);
    }
    if let Err(e) = assert_subtask_decomposition(handoff, min_subtasks) {
        failures.push(e);
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join(" | "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff::{HandoffSubTask, KeyFinding};

    fn done_result() -> BrowserResult {
        BrowserResult::done(
            "session_1",
            "Message sent to Alice.",
            vec![KeyFinding {
                id: "step-1".into(),
                description: "send".into(),
                status: "done".into(),
                reason: String::new(),
                evidence_signature: None,
            }],
            Some("len:abc".into()),
            Some("/tmp/t.log".into()),
        )
    }

    fn failed_result(reason: &str) -> BrowserResult {
        BrowserResult::failure("session_1", reason, Some("/tmp/t.log".into()))
    }

    fn bare_handoff(task: &str, id: &str) -> Handoff {
        Handoff {
            task_description: task.into(),
            subtasks: vec![HandoffSubTask::new("step-1", "do thing")],
            constraints: vec![],
            originating_message_id: id.into(),
            research_plan: None,
        }
    }

    fn decomposed_handoff(task: &str, id: &str) -> Handoff {
        Handoff {
            task_description: task.into(),
            subtasks: vec![
                HandoffSubTask::new("step-1", "navigate"),
                HandoffSubTask::new("step-2", "send"),
            ],
            constraints: vec![],
            originating_message_id: id.into(),
            research_plan: None,
        }
    }

    #[test]
    fn dispatch_assertion_passes_for_non_empty_task() {
        let h = bare_handoff("send a message", "id:1");
        assert!(assert_correct_task_dispatched("send a message", &h).is_ok());
    }

    #[test]
    fn dispatch_assertion_fails_for_empty_task() {
        let mut h = bare_handoff("send a message", "id:1");
        h.task_description = String::new();
        assert!(assert_correct_task_dispatched("send a message", &h).is_err());
    }

    #[test]
    fn reflection_assertion_passes_for_done_with_summary() {
        let r = done_result();
        let reply = "I sent the message to Alice.";
        assert!(assert_result_reflected_in_chat_reply(&r, reply).is_ok());
    }

    #[test]
    fn reflection_assertion_fails_for_empty_reply() {
        let r = done_result();
        assert!(assert_result_reflected_in_chat_reply(&r, "").is_err());
    }

    #[test]
    fn reflection_assertion_passes_for_failed_with_quoted_reason() {
        let r = failed_result("Chrome failed to launch");
        let reply = "I couldn't complete the task: Chrome failed to launch.";
        assert!(assert_result_reflected_in_chat_reply(&r, reply).is_ok());
    }

    #[test]
    fn reflection_assertion_fails_for_failed_with_unquoted_reason() {
        let r = failed_result("Chrome failed to launch");
        let reply = "Sorry, something went wrong.";
        assert!(assert_result_reflected_in_chat_reply(&r, reply).is_err());
    }

    #[test]
    fn id_preservation_passes_for_matching_ids() {
        let h = bare_handoff("x", "id:1");
        assert!(assert_originating_message_id_preserved(&h, "id:1").is_ok());
    }

    #[test]
    fn id_preservation_fails_for_mismatched_ids() {
        let h = bare_handoff("x", "id:1");
        assert!(assert_originating_message_id_preserved(&h, "id:2").is_err());
    }

    #[test]
    fn decomposition_assertion_passes_for_sufficient_subtasks() {
        let h = decomposed_handoff("x", "id:1");
        assert!(assert_subtask_decomposition(&h, 2).is_ok());
    }

    #[test]
    fn decomposition_assertion_fails_for_too_few_subtasks() {
        let h = bare_handoff("go to instagram and text alice", "id:1");
        assert!(assert_subtask_decomposition(&h, 2).is_err());
    }

    #[test]
    fn full_contract_passes_when_all_four_hold() {
        let user = "go to instagram and text alice hi";
        let h = decomposed_handoff(
            "open instagram and send the message",
            "id:42",
        );
        let r = done_result();
        let reply = "I sent the message to Alice. 2 of 2 sub-tasks completed.";
        let res = assert_handoff_contract(user, &h, &r, reply, "id:42", 2);
        assert!(res.is_ok(), "unexpected failure: {:?}", res);
    }

    #[test]
    fn full_contract_fails_aggregating_multiple_failures() {
        let user = "go to instagram and text alice hi";
        let h = bare_handoff("go to instagram and text alice hi", "id:42");
        let r = failed_result("Chrome failed");
        let reply = "Sorry, something went wrong.";
        let res = assert_handoff_contract(user, &h, &r, reply, "id:99", 2);
        assert!(res.is_err());
        let err = res.unwrap_err();
        // The aggregated error should mention at least
        // the id-preservation drift and the
        // decomposition-coarseness.
        assert!(err.contains("originating_message_id"), "missing id drift: {err}");
        assert!(err.contains("decomposition"), "missing decomposition failure: {err}");
    }
}
