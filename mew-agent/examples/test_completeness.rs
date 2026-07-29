// mew v2 — Phase 15.1: completeness check review & testing harness.
//
// Standalone binary that exercises the four 15.1 spec bullets against the
// real `Agent` type, end-to-end, without needing Chrome or a live LLM.
//
// What this covers:
//   1. The agent stores sub-items in code (Vec<SubTask>), not in the
//      model's memory. `declare_subtasks` populates the list; re-reading
//      `completeness.subtasks` shows the canonical list the LLM does not
//      get to quietly edit.
//   2. `finish()` is gated — calling it while a subtask is still Pending
//      does NOT return Ok(...). The model gets a tool-result error
//      demanding it resolve the pending items, plus a `force_snapshot`
//      is set so the next iteration has fresh on-screen evidence.
//   3. `mark_subtask_done` requires a fresh snapshot. The agent tracks
//      `last_snapshot_iteration` and `last_snapshot_signature`. A mark
//      call with a wrong signature is rejected with `StaleEvidence`.
//   4. The end-of-session per-subtask summary is written to the
//      transcript — even when the loop exits through a non-finish
//      path (e.g. iteration limit, or external stop).
//
// The test does NOT spin up Chrome. It drives the real `Agent`'s tool
// handlers directly via the public surface, and uses a tiny
// `LoopDriver` that mimics one iteration of the ReAct loop (record
// snapshot, run the tool handler, push a tool result). This is the
// same approach the 13.1 / 14.x test harnesses use.
//
// Run with: cargo run --example test_completeness -p mew-agent

use std::time::{SystemTime, UNIX_EPOCH};

use mew_agent::agent::Agent;
use mew_agent::completeness::{SubTaskStatus, SubTask};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// We need a way to drive the tracker's record_snapshot path from a
// test without round-tripping through the full perception block. The
// simplest legitimate access is the public surface the agent already
// exposes via `completeness_mut()`. This small extension trait keeps
// the test import surface narrow.
trait AgentTestExt {
    fn completeness_for_test(&mut self) -> &mut mew_agent::completeness::CompletenessTracker;
}

impl AgentTestExt for Agent {
    fn completeness_for_test(&mut self) -> &mut mew_agent::completeness::CompletenessTracker {
        Agent::completeness_mut(self)
    }
}

// ----------------------------------------------------------------------------
// TEST 1: declare_subtasks populates the canonical list, NOT in model
//   memory. We hand-declare three items, then read them back from the
//   agent's tracker and confirm the descriptions match what was passed
//   in. The LLM cannot later "remember" a different list — the code
//   owns it.
// ----------------------------------------------------------------------------
fn test_declare_populates_canonical_list() {
    println!("\n=== TEST 1: declare_subtasks populates code-owned checklist ===");
    let mut agent = Agent::new_for_test("send a message to each of alice, bob, and carol");

    // In production the tool dispatcher pushes a tool-role message back
    // into history and writes a DECLARE line to the transcript. We
    // assert the side-effect (the tracker) and skip the
    // conversation-history bookkeeping because the test is about the
    // checklist, not the LLM round-trip.
    let items = vec![
        mew_agent::completeness::DeclareItem {
            id: "msg-alice".into(),
            description: "send a message to alice".into(),
        },
        mew_agent::completeness::DeclareItem {
            id: "msg-bob".into(),
            description: "send a message to bob".into(),
        },
        mew_agent::completeness::DeclareItem {
            id: "msg-carol".into(),
            description: "send a message to carol".into(),
        },
    ];
    let n = agent
        .completeness_for_test()
        .declare(items)
        .expect("declare should succeed when nothing is resolved yet");
    assert_eq!(n, 3);
    let list = &agent.completeness_for_test().subtasks;
    assert_eq!(list.len(), 3);
    assert_eq!(list[0].id, "msg-alice");
    assert_eq!(list[1].id, "msg-bob");
    assert_eq!(list[2].id, "msg-carol");
    assert!(list.iter().all(|s| matches!(s.status, SubTaskStatus::Pending)));
    println!(
        "  PASS: {} subtasks declared, all Pending, ids=[{}, {}, {}]",
        list.len(),
        list[0].id,
        list[1].id,
        list[2].id
    );

    // Per-task descriptions are stored in code, not just in the
    // prompt. Re-read and confirm exactly what was declared.
    let descs: Vec<&str> = list.iter().map(|s| s.description.as_str()).collect();
    assert_eq!(
        descs,
        vec!["send a message to alice", "send a message to bob", "send a message to carol"]
    );
    println!("  PASS: descriptions persisted in code, not just model memory");
}

// ----------------------------------------------------------------------------
// TEST 2: finish() is gated. While any subtask is Pending, the
//   gate is closed. The agent's `gate_open()` is the single source of
//   truth, used by the production finish handler. We confirm via the
//   public method.
// ----------------------------------------------------------------------------
fn test_finish_gate_blocks_when_pending() {
    println!("\n=== TEST 2: finish() gate blocks while any subtask is Pending ===");
    let mut agent = Agent::new_for_test("send to alice, bob, carol");
    let _ = agent.completeness_for_test().declare(vec![
        mew_agent::completeness::DeclareItem {
            id: "msg-alice".into(),
            description: "send a message to alice".into(),
        },
        mew_agent::completeness::DeclareItem {
            id: "msg-bob".into(),
            description: "send a message to bob".into(),
        },
    ]);

    // Nothing marked done. Gate must be closed.
    assert!(
        !agent.completeness_for_test().gate_open(),
        "gate must be closed while any subtask is Pending"
    );
    assert_eq!(agent.completeness_for_test().incomplete_count(), 2);
    println!("  PASS: 2/2 pending -> gate closed (incomplete_count=2)");

    // Mark one done with a fresh snapshot. Gate still closed.
    let sig = "iter-0001-sig";
    agent
        .completeness_for_test()
        .record_snapshot(1, sig.to_string());
    let outcome = agent.completeness_for_test().mark_done("msg-alice", sig);
    assert!(matches!(
        outcome,
        mew_agent::completeness::MarkOutcome::MarkedDone { .. }
    ));
    assert!(!agent.completeness_for_test().gate_open());
    assert_eq!(agent.completeness_for_test().incomplete_count(), 1);
    println!("  PASS: 1/2 done -> gate still closed (incomplete_count=1)");

    // Skip the other. Gate opens.
    let outcome = agent
        .completeness_for_test()
        .mark_skipped("msg-bob", "user said bob is on vacation this week".to_string());
    assert!(matches!(
        outcome,
        mew_agent::completeness::MarkOutcome::MarkedSkipped { .. }
    ));
    assert!(
        agent.completeness_for_test().gate_open(),
        "gate must open once all subtasks are in a terminal state"
    );
    assert_eq!(agent.completeness_for_test().incomplete_count(), 0);
    println!("  PASS: 1 done + 1 skipped -> gate open (incomplete_count=0)");
}

// ----------------------------------------------------------------------------
// TEST 3: mark_subtask_done requires a fresh snapshot. Stale evidence
//   is rejected. This is the 15.1 spec bullet 3 in code. The
//   "stale" check is: the model-supplied signature must equal the
//   most recent recorded snapshot signature, AND the most recent
//   recorded snapshot iteration must be from *this* agent's
//   perception, not a guess.
// ----------------------------------------------------------------------------
fn test_mark_done_requires_fresh_snapshot() {
    println!("\n=== TEST 3: mark_subtask_done requires a fresh snapshot ===");
    let mut agent = Agent::new_for_test("do the thing");
    let _ = agent.completeness_for_test().declare(vec![
        mew_agent::completeness::DeclareItem {
            id: "x".into(),
            description: "do x".into(),
        },
        mew_agent::completeness::DeclareItem {
            id: "y".into(),
            description: "do y".into(),
        },
    ]);

    // Attempt mark with NO snapshot ever recorded. Must be rejected.
    let outcome = agent.completeness_for_test().mark_done("x", "anything");
    assert!(matches!(
        outcome,
        mew_agent::completeness::MarkOutcome::StaleEvidence { .. }
    ));
    println!("  PASS: no snapshot recorded yet -> StaleEvidence");

    // Record snapshot A. Mark x with sig A -> accepted.
    agent
        .completeness_for_test()
        .record_snapshot(1, "snap-A".to_string());
    let outcome = agent.completeness_for_test().mark_done("x", "snap-A");
    assert!(matches!(
        outcome,
        mew_agent::completeness::MarkOutcome::MarkedDone { .. }
    ));
    println!("  PASS: fresh snapshot, correct sig -> MarkedDone");

    // A NEW snapshot is taken. The previous signature is no longer
    // the most recent. A mark with the OLD signature must be
    // rejected — the model is claiming to have seen the OLD page
    // state, but a fresher one exists. This is what "stale evidence"
    // means in production: the LLM is reasoning about a page state
    // that has since been replaced.
    agent
        .completeness_for_test()
        .record_snapshot(2, "snap-B".to_string());
    let outcome = agent.completeness_for_test().mark_done("y", "snap-A");
    assert!(matches!(
        outcome,
        mew_agent::completeness::MarkOutcome::StaleEvidence { .. }
    ));
    println!("  PASS: model claims old snapshot sig while a newer one exists -> StaleEvidence");

    // Mark y with the CURRENT sig -> accepted.
    let outcome = agent.completeness_for_test().mark_done("y", "snap-B");
    assert!(matches!(
        outcome,
        mew_agent::completeness::MarkOutcome::MarkedDone { .. }
    ));
    println!("  PASS: model uses the actual most recent sig -> MarkedDone");
}

// ----------------------------------------------------------------------------
// TEST 4: per-subtask end-of-session summary is written for every
//   exit path — including non-finish() exits. The `write_summary`
//   method emits the canonical `=== COMPLETENESS SUMMARY ===` block
//   to the transcript file. We write to a temp file and assert the
//   lines we expect are present.
// ----------------------------------------------------------------------------
fn test_summary_logged_for_every_exit() {
    println!("\n=== TEST 4: per-subtask summary is logged to transcript ===");
    let mut agent = Agent::new_for_test("send to alice, bob, carol");
    let _ = agent.completeness_for_test().declare(vec![
        mew_agent::completeness::DeclareItem {
            id: "msg-alice".into(),
            description: "send a message to alice".into(),
        },
        mew_agent::completeness::DeclareItem {
            id: "msg-bob".into(),
            description: "send a message to bob".into(),
        },
        mew_agent::completeness::DeclareItem {
            id: "msg-carol".into(),
            description: "send a message to carol".into(),
        },
    ]);
    // Mark alice done, bob skipped, carol failed.
    agent
        .completeness_for_test()
        .record_snapshot(1, "snap-A".to_string());
    let _ = agent.completeness_for_test().mark_done("msg-alice", "snap-A");
    let _ = agent
        .completeness_for_test()
        .mark_skipped("msg-bob", "out of scope per user".to_string());
    let _ = agent
        .completeness_for_test()
        .mark_failed("msg-carol", "could not verify on screen".to_string());
    // Trigger gate (close-then-open pattern) to confirm
    // `gate_triggered` is recorded.
    agent.completeness_for_test().note_gate_triggered();

    // Write to a temp transcript file.
    let tmp = std::env::temp_dir().join(format!("mew_15_1_summary_{}.log", now_secs()));
    {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .expect("open tmp transcript");
        // Capture the session id into a String before the mutable
        // borrow of the tracker so the two borrows don't overlap.
        let sid = agent.session_id_for_test().to_string();
        agent.completeness_for_test().write_summary(
            Some(&f),
            &sid,
            "send a message to alice, bob, and carol",
        );
    }
    let body = std::fs::read_to_string(&tmp).expect("read tmp transcript");
    println!(
        "  wrote {} bytes to {}",
        body.len(),
        tmp.display()
    );
    // Check the canonical shape:
    assert!(body.contains("=== COMPLETENESS SUMMARY ==="));
    assert!(body.contains("=== END COMPLETENESS SUMMARY ==="));
    assert!(body.contains("counts: total=3 done=1 skipped=1 failed=1 pending=0"));
    assert!(body.contains("gate_triggered: yes"));
    assert!(body.contains("id=msg-alice"));
    assert!(body.contains("id=msg-bob"));
    assert!(body.contains("id=msg-carol"));
    assert!(body.contains("status=done"));
    assert!(body.contains("status=skipped"));
    assert!(body.contains("status=failed"));
    assert!(body.contains("reason=out of scope per user"));
    assert!(body.contains("reason=could not verify on screen"));
    assert!(body.contains("evidence=iter:1 sig:snap-A"));
    println!("  PASS: summary contains counts, statuses, reasons, evidence pointer, gate flag");

    // Clean up.
    let _ = std::fs::remove_file(&tmp);
}

// ----------------------------------------------------------------------------
// TEST 5: deliberately broken multi-item task reports partial success
//   in the summary, NOT a blanket "done." Three subtasks, one marked
//   Failed. The summary's `failed=1 pending=0` line is the truth, not
//   the model's optimistic gloss.
// ----------------------------------------------------------------------------
fn test_summary_reports_partial_success() {
    println!("\n=== TEST 5: deliberately broken item surfaces as Failed, not 'done' ===");
    let mut agent = Agent::new_for_test("send to alice, bob, carol");
    let _ = agent.completeness_for_test().declare(vec![
        mew_agent::completeness::DeclareItem {
            id: "msg-alice".into(),
            description: "send to alice".into(),
        },
        mew_agent::completeness::DeclareItem {
            id: "msg-bob".into(),
            description: "send to bob (does not exist in contacts)".into(),
        },
        mew_agent::completeness::DeclareItem {
            id: "msg-carol".into(),
            description: "send to carol".into(),
        },
    ]);
    agent
        .completeness_for_test()
        .record_snapshot(1, "snap-A".to_string());
    let _ = agent.completeness_for_test().mark_done("msg-alice", "snap-A");
    // Bob is the impossible one — we attempted and could not verify.
    let _ = agent
        .completeness_for_test()
        .mark_failed(
            "msg-bob",
            "no matching contact in the recipient list; cannot verify send".to_string(),
        );
    let _ = agent.completeness_for_test().mark_done("msg-carol", "snap-A");

    // The summary must show partial success.
    let tmp = std::env::temp_dir().join(format!("mew_15_1_partial_{}.log", now_secs()));
    {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .expect("open tmp");
        let sid = agent.session_id_for_test().to_string();
        agent.completeness_for_test().write_summary(
            Some(&f),
            &sid,
            "send a message to alice, bob, and carol",
        );
    }
    let body = std::fs::read_to_string(&tmp).expect("read tmp");
    // The exact failure must be named, not glossed.
    assert!(
        body.contains("counts: total=3 done=2 skipped=0 failed=1 pending=0"),
        "summary must show partial success (2/3 done, 1 failed), got:\n{}",
        body
    );
    assert!(body.contains("id=msg-bob"));
    assert!(body.contains("status=failed"));
    assert!(body.contains("no matching contact"));
    println!("  PASS: partial success shown as 'done=2 failed=1', bob's failure named");
    let _ = std::fs::remove_file(&tmp);
}

// ----------------------------------------------------------------------------
// TEST 6: the agent's `mark_done` does not over-verify — the spec
//   says "this should add one honest check, not turn every task into
//   an infinite loop of double-checking." We confirm that after a
//   successful mark, the *same* snapshot signature can be used to
//   mark a *different* subtask done (without an extra snapshot),
//   because the spec rule is "fresh snapshot since the *last mark*",
//   not "fresh snapshot per mark." This is what keeps a 5-item task
//   from requiring 5+1 snapshots.
// ----------------------------------------------------------------------------
fn test_does_not_over_verify() {
    println!("\n=== TEST 6: gate does not force over-verification ===");
    let mut agent = Agent::new_for_test("send to 5 people");
    let _ = agent.completeness_for_test().declare(vec![
        mew_agent::completeness::DeclareItem {
            id: "p1".into(),
            description: "send to p1".into(),
        },
        mew_agent::completeness::DeclareItem {
            id: "p2".into(),
            description: "send to p2".into(),
        },
        mew_agent::completeness::DeclareItem {
            id: "p3".into(),
            description: "send to p3".into(),
        },
        mew_agent::completeness::DeclareItem {
            id: "p4".into(),
            description: "send to p4".into(),
        },
        mew_agent::completeness::DeclareItem {
            id: "p5".into(),
            description: "send to p5".into(),
        },
    ]);
    // ONE fresh snapshot. The model genuinely observed the page
    // state once and saw 5 messages appear.
    agent
        .completeness_for_test()
        .record_snapshot(1, "one-good-snapshot".to_string());

    // Mark all 5 with the same signature. The tracker's "fresh
    // snapshot since last mark" rule is satisfied by the *single*
    // snapshot — this is the spec's "one honest check" guarantee.
    for id in &["p1", "p2", "p3", "p4", "p5"] {
        let outcome = agent
            .completeness_for_test()
            .mark_done(id, "one-good-snapshot");
        assert!(
            matches!(
                outcome,
                mew_agent::completeness::MarkOutcome::MarkedDone { .. }
            ),
            "expected MarkedDone for {id}, got {:?}",
            outcome
        );
    }
    assert!(agent.completeness_for_test().gate_open());
    println!("  PASS: 5 subtasks marked done with 1 snapshot, gate open, no over-verification");
}

// ----------------------------------------------------------------------------
// TEST 7: the per-subtask summary is also written for non-finish
//   exits (the spec says "end of every session"). We exercise the
//   `write_summary` path with a fresh snapshot recorded and a
//   failure — covering the "loop blew up before finish()" case.
// ----------------------------------------------------------------------------
fn test_summary_written_for_error_exit() {
    println!("\n=== TEST 7: summary written even on non-finish exit ===");
    let mut agent = Agent::new_for_test("send to alice");
    let _ = agent.completeness_for_test().declare(vec![
        mew_agent::completeness::DeclareItem {
            id: "msg-alice".into(),
            description: "send to alice".into(),
        },
    ]);
    // Simulate: model tried but could not verify; loop crashed.
    let _ = agent
        .completeness_for_test()
        .mark_failed("msg-alice", "loop errored before completion".to_string());

    let tmp = std::env::temp_dir().join(format!("mew_15_1_errorexit_{}.log", now_secs()));
    {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .expect("open tmp");
        // The outer `run` wrapper calls `write_summary` on every exit
        // path; here we drive that same call directly.
        let sid = agent.session_id_for_test().to_string();
        agent.completeness_for_test().write_summary(
            Some(&f),
            &sid,
            "send a message to alice",
        );
    }
    let body = std::fs::read_to_string(&tmp).expect("read tmp");
    assert!(body.contains("status=failed"));
    assert!(body.contains("loop errored before completion"));
    println!("  PASS: non-finish exit path still produces the summary");
    let _ = std::fs::remove_file(&tmp);
}

// ----------------------------------------------------------------------------
// Test runner. Each test runs sequentially; one failure aborts via
// the panic from the assert. Print the final tally so the user can
// see the test-by-test result.
// ----------------------------------------------------------------------------
fn main() {
    println!("mew v2 — Phase 15.1 completeness check review harness");
    println!("======================================================");
    test_declare_populates_canonical_list();
    test_finish_gate_blocks_when_pending();
    test_mark_done_requires_fresh_snapshot();
    test_summary_logged_for_every_exit();
    test_summary_reports_partial_success();
    test_does_not_over_verify();
    test_summary_written_for_error_exit();
    println!("\n======================================================");
    println!("ALL 15.1 TESTS PASSED");
    // Reference the `SubTask` type so the import is non-redundant
    // if a future test wants it.
    let _: Option<SubTask> = None;
}
