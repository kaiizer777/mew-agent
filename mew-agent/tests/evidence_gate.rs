// mew v2 — Phase 12: Per-todo evidence gate tests.

use mew_agent::todo::{
    planner_signature, todo_cancelled_event, todo_rejected_event, verify_evidence, MarkTodoOutcome,
    Todo, TodoId, TodoStatus,
};

#[test]
fn test_evidence_gate_positive_match() {
    let mut todo = Todo::new(TodoId::from_slug("navigate instagram", 0), "navigate instagram", None);
    let obs_text = "Accessibility Tree:\n[Button] Log In\n[Input] Search";
    let expected_sig = planner_signature(obs_text);

    let outcome = todo.mark_done(
        &expected_sig,
        obs_text,
        1,
        1700000000,
        3,
    );

    match outcome {
        MarkTodoOutcome::MarkedDone { evidence } => {
            assert_eq!(evidence.worker_signature, expected_sig);
            assert_eq!(evidence.planner_signature, expected_sig);
            assert_eq!(evidence.todo_id, todo.id);
            assert_eq!(evidence.verified_at_secs, 1700000000);
        }
        _ => panic!("Expected MarkedDone, got {:?}", outcome),
    }

    assert_eq!(todo.status, TodoStatus::Done);
    assert!(todo.evidence.is_some());
    assert_eq!(todo.attempts, 1);
}

#[test]
fn test_evidence_gate_signature_mismatch_rejects_and_increments_attempts() {
    let mut todo = Todo::new(TodoId::from_slug("navigate instagram", 0), "navigate instagram", None);
    let obs_text = "Accessibility Tree:\n[Button] Log In";
    let fake_worker_sig = "len:12345678";
    let actual_planner_sig = planner_signature(obs_text);

    assert_ne!(fake_worker_sig, actual_planner_sig);

    let outcome = todo.mark_done(
        fake_worker_sig,
        obs_text,
        1,
        1700000000,
        3,
    );

    match outcome {
        MarkTodoOutcome::StaleEvidence(mismatch) => {
            assert_eq!(mismatch.worker_signature, fake_worker_sig);
            assert_eq!(mismatch.planner_signature, actual_planner_sig);
            assert_eq!(mismatch.reason, "snapshot signature mismatch");
        }
        _ => panic!("Expected StaleEvidence mismatch, got {:?}", outcome),
    }

    assert_eq!(todo.status, TodoStatus::Pending);
    assert!(todo.evidence.is_none());
    assert_eq!(todo.attempts, 1);
}

#[test]
fn test_evidence_gate_already_terminal_after_success() {
    let mut todo = Todo::new(TodoId::from_slug("click search", 0), "click search", None);
    let obs_text = "Accessibility Tree:\n[Button] Search";
    let sig = planner_signature(obs_text);

    // First call succeeds
    let _ = todo.mark_done(&sig, obs_text, 2, 1700000000, 3);
    assert_eq!(todo.status, TodoStatus::Done);
    assert_eq!(todo.last_evidence_iteration, Some(2));

    // Second call gets AlreadyTerminal
    let outcome = todo.mark_done(&sig, obs_text, 1, 1700000001, 3);
    match outcome {
        MarkTodoOutcome::AlreadyTerminal { current } => {
            assert_eq!(current, TodoStatus::Done);
        }
        _ => panic!("Expected AlreadyTerminal, got {:?}", outcome),
    }
}

#[test]
fn test_evidence_gate_stale_iteration_rejection() {
    let mut todo = Todo::new(TodoId::from_slug("click search", 0), "click search", None);
    let obs_text = "Accessibility Tree:\n[Button] Search";
    let sig = planner_signature(obs_text);

    // Manually set field to simulate a previous partial success or state
    todo.last_evidence_iteration = Some(3);

    // Iteration 2 is <= last_evidence_iteration (3) -> rejected as stale
    let outcome = todo.mark_done(
        &sig,
        obs_text,
        2,
        1700000000,
        3,
    );

    match outcome {
        MarkTodoOutcome::StaleEvidence(mismatch) => {
            assert!(mismatch.reason.contains("stale iteration"));
        }
        _ => panic!("Expected StaleEvidence for stale iteration, got {:?}", outcome),
    }

    assert_eq!(todo.status, TodoStatus::Pending);
    assert!(todo.evidence.is_none());
    assert_eq!(todo.attempts, 1);
}

#[test]
fn test_evidence_gate_empty_obs_text_fails_closed() {
    let mut todo = Todo::new(TodoId::from_slug("type text", 0), "type text", None);
    let empty_obs_text = "";
    let worker_sig = planner_signature(empty_obs_text);

    let outcome = todo.mark_done(
        &worker_sig,
        empty_obs_text,
        1,
        1700000000,
        3,
    );

    match outcome {
        MarkTodoOutcome::StaleEvidence(mismatch) => {
            assert_eq!(mismatch.reason, "empty observation text");
        }
        _ => panic!("Expected StaleEvidence for empty obs text, got {:?}", outcome),
    }

    assert_eq!(todo.status, TodoStatus::Pending);
    assert!(todo.evidence.is_none());
    assert_eq!(todo.attempts, 1);
}

#[test]
fn test_verify_evidence_direct_util() {
    let obs = "Some AX tree content";
    let valid_sig = planner_signature(obs);

    assert_eq!(verify_evidence(&valid_sig, obs), Ok(valid_sig.clone()));

    let res = verify_evidence("invalid", obs);
    assert!(res.is_err());
    let err = res.unwrap_err();
    assert_eq!(err.worker_signature, "invalid");
    assert_eq!(err.planner_signature, valid_sig);

    let empty_res = verify_evidence(&planner_signature(""), "");
    assert!(empty_res.is_err());
    assert_eq!(empty_res.unwrap_err().reason, "empty observation text");
}

#[test]
fn test_todo_rejected_event_carries_task_and_signatures() {
    let mut todo = Todo::new(TodoId::from_slug("send hi", 0), "send hi", None);
    let obs_text = "real AX tree content";
    let outcome = todo.mark_done(
        "len:deadbeef",  // fake worker sig
        obs_text,
        1,
        1700000000,
        3,
    );
    let mismatch = match outcome {
        MarkTodoOutcome::StaleEvidence(m) => m,
        _ => panic!("expected StaleEvidence"),
    };
    let event = todo_rejected_event("task-42", &todo.id, &mismatch);
    assert_eq!(event.task_id, "task-42");
    assert_eq!(event.todo_id, todo.id);
    assert!(event.evidence.is_some());
    assert!(event.reason.is_none());
    let event_evidence = event.evidence.expect("evidence populated");
    assert_eq!(event_evidence.worker_signature, "len:deadbeef");
    assert_eq!(event_evidence.planner_signature, planner_signature(obs_text));
}

#[test]
fn test_todo_cancelled_event_carries_reason() {
    let todo = Todo::new(
        TodoId::from_slug("send hi", 0),
        "send hi",
        None,
    );
    let event = todo_cancelled_event("task-99", &todo.id, "user clicked stop");
    assert_eq!(event.task_id, "task-99");
    assert_eq!(event.todo_id, todo.id);
    assert!(event.evidence.is_none());
    assert_eq!(event.reason.as_deref(), Some("user clicked stop"));
}
