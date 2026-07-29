// mew v2 — Phase 12.2 review & testing harness.
//
// Standalone binary that exercises SessionHandle the same way the real ReAct
// loop will in production: a "loop" task that calls checkpoint() between
// iterations, and a "controller" task that drives pause/resume/stop from
// outside. No Chrome, no LLM — just the state machine.
//
// Run with: cargo run --example test_state_machine -p mew-agent
//
// Each test prints a banner, the actions it took, and the result. Reading the
// raw output is the "eyes-on" check 12.2 requires.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use mew_agent::session::{SessionError, SessionHandle, SessionState};

// Stand-in for the real transcript file. Holds a transcript path we append to
// and a queue of every transition the test loop observed, with timestamps.
struct TestHarness {
    session: SessionHandle,
    transcript_path: String,
    // (timestamp_secs, label) — appended by the simulated loop
    loop_events: Arc<Mutex<Vec<(u64, String)>>>,
}

impl TestHarness {
    fn new(session_id: &str) -> Self {
        // Transcripts live under tests-output/test_state_machine/ so the
        // project root stays clean. The folder is gitignored.
        let _ = std::fs::create_dir_all("tests-output/test_state_machine");
        let transcript_path = format!(
            "tests-output/test_state_machine/test_state_machine_{}.log",
            session_id
        );
        // Truncate any previous run so we always read a clean transcript.
        let _ = std::fs::write(&transcript_path, "");
        Self {
            session: SessionHandle::new(session_id.to_string()),
            transcript_path,
            loop_events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn record(&self, label: &str) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut events = self.loop_events.lock().await;
        events.push((now, label.to_string()));
        // Mirror to a transcript file in the same shape the real loop uses.
        let line = format!(
            "[{}] [{}] LOOP: {}\n",
            now,
            self.session.session_id(),
            label
        );
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.transcript_path)
        {
            let _ = f.write_all(line.as_bytes());
        }
        now
    }

    /// Simulate one ReAct-loop iteration: checkpoint, then "do work", then
    /// log what happened. Mirrors the structure in `agent.rs` so any bug in
    /// checkpoint() / pause / resume / stop surfaces here too.
    async fn step(&self, iter: usize) -> Result<(), String> {
        let pre = self.record(&format!("iter {iter}: pre-checkpoint")).await;
        self.session
            .checkpoint()
            .await
            .map_err(|e| format!("iter {iter}: checkpoint returned error: {e}"))?;
        let post = self.record(&format!("iter {iter}: post-checkpoint")).await;
        // Sanity: a no-op checkpoint (state already Running) should be
        // near-instant. A real pause should leave a visible gap between
        // pre and post.
        let gap_ms = post.saturating_sub(pre) * 1000;
        println!(
            "  iter {iter}: pre={pre} post={post} gap={gap_ms}ms state={:?}",
            self.session.state().await
        );
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// Test 1: terminal state genuinely rejects further transitions.
//   resume() after stop() must return TerminalState, not silently no-op.
//   This is the invariant the spec calls out: "try to call resume() after
//   stop() in a quick test and confirm it returns an error, not a silent
//   no-op."
// ----------------------------------------------------------------------------
async fn test_terminal_rejects_resume() {
    println!("\n=== TEST 1: terminal state rejects resume() ===");
    let h = TestHarness::new("t1_terminal");

    let s = h.session.stop("user asked".to_string()).await;
    println!("  stop() -> {:?}", s);
    assert!(matches!(s, Ok(SessionState::Stopped)), "stop should succeed");

    let r = h.session.resume(None).await;
    println!("  resume() after stop -> {:?}", r);
    match r {
        Err(SessionError::TerminalState(SessionState::Stopped)) => {
            println!("  PASS: terminal state correctly rejected resume()");
        }
        other => panic!("expected TerminalState(Stopped) error, got {other:?}"),
    }

    // Also: try to pause a stopped session. Must be rejected, not silently
    // accepted, not a panic.
    let p = h.session.pause(Some("should fail".into())).await;
    println!("  pause() after stop -> {:?}", p);
    assert!(
        matches!(p, Err(SessionError::TerminalState(_))),
        "pause after stop must be rejected"
    );

    // And: try to stop again. Same rule.
    let s2 = h.session.stop("again".to_string()).await;
    println!("  stop() after stop -> {:?}", s2);
    assert!(
        matches!(s2, Err(SessionError::TerminalState(_))),
        "double-stop must be rejected"
    );
}

// ----------------------------------------------------------------------------
// Test 2: pause() from another task genuinely blocks the loop. We spawn a
//   loop task that calls step() 5 times with a small sleep between. After
//   the second step, the controller calls pause(). The loop's third
//   step() must block at checkpoint() until resume() is called. We
//   measure the gap between pre-checkpoint and post-checkpoint to prove
//   the block is real, not a tight loop.
// ----------------------------------------------------------------------------
async fn test_pause_blocks_and_resume_continues() {
    println!("\n=== TEST 2: pause blocks; resume continues from same point ===");
    let h = Arc::new(TestHarness::new("t2_pause_block"));
    let h2 = h.clone();

    // Loop task: 5 iterations, 200ms work between each, then checkpoint.
    let loop_task = tokio::spawn(async move {
        for i in 1..=5 {
            // Simulate a small chunk of "work" so the gap from a pause is
            // distinguishable from the natural ~ms cost of a no-op
            // checkpoint.
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = h2.record(&format!("iter {i}: about to checkpoint")).await;
            match h2.step(i).await {
                Ok(()) => {}
                Err(e) => {
                    println!("  loop stopping: {e}");
                    return;
                }
            }
        }
        println!("  loop: all 5 iterations finished");
    });

    // Controller: let the loop get through 2 iterations, then pause.
    // Then sleep 1.5s while paused (the loop's iter 3 checkpoint should be
    // parked for this entire window). Then resume and let it finish.
    tokio::time::sleep(Duration::from_millis(550)).await; // ~2 iterations
    let pause_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    println!("  controller: calling pause() at t={pause_at}");
    h.session
        .pause(Some("stepping in".into()))
        .await
        .expect("pause should succeed");
    println!("  controller: paused. Sleeping 1.5s while loop is parked.");

    tokio::time::sleep(Duration::from_millis(1500)).await;

    let resume_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    println!("  controller: calling resume() at t={resume_at}");
    h.session.resume(None).await.expect("resume should succeed");
    println!("  controller: resumed.");

    // Wait for the loop to finish naturally.
    let _ = loop_task.await;

    // Inspect the recorded events. After the pause, the next pre-checkpoint
    // line should be at t=pause_at (or just after) and the matching
    // post-checkpoint line should be ~1.5s later (i.e. AFTER the resume).
    let events = h.loop_events.lock().await;
    println!("  recorded loop events:");
    for (t, label) in events.iter() {
        println!("    t={t}  {label}");
    }

    // Find the pre-checkpoint for iter 3 (the one expected to be parked).
    let pre_iter3 = events
        .iter()
        .find(|(_, l)| l.contains("iter 3: pre-checkpoint"))
        .map(|(t, _)| *t)
        .expect("should have iter 3 pre-checkpoint");
    let post_iter3 = events
        .iter()
        .find(|(_, l)| l.contains("iter 3: post-checkpoint"))
        .map(|(t, _)| *t)
        .expect("should have iter 3 post-checkpoint");

    let block_secs = post_iter3.saturating_sub(pre_iter3);
    println!("  iter 3 was blocked for {block_secs}s (pre={pre_iter3}, post={post_iter3})");
    assert!(
        block_secs >= 1,
        "iter 3 should have been parked for at least 1s, got {block_secs}s"
    );
    println!("  PASS: pause genuinely blocked the loop for {block_secs}s");

    // And: the loop should have run iter 4 and 5 after the resume, not
    // restarted. This is the "doesn't restart" guarantee.
    let iter4 = events
        .iter()
        .any(|(_, l)| l.contains("iter 4: pre-checkpoint"));
    let iter5 = events
        .iter()
        .any(|(_, l)| l.contains("iter 5: pre-checkpoint"));
    assert!(iter4, "iter 4 must have happened after resume");
    assert!(iter5, "iter 5 must have happened after resume");
    println!("  PASS: loop continued to iter 4 and iter 5 (did not restart)");
}

// ----------------------------------------------------------------------------
// Test 3: idempotency edge cases. Double-pause must error cleanly without
//   deadlocking. Resume on a non-paused session must error cleanly.
// ----------------------------------------------------------------------------
async fn test_double_pause_and_resume_idle() {
    println!("\n=== TEST 3: double-pause and resume-without-pause are clean ===");
    let h = TestHarness::new("t3_idempotent");

    // State is Running. Resume on Running is a real error, not a no-op.
    let r1 = h.session.resume(None).await;
    println!("  resume() on Running -> {:?}", r1);
    assert!(
        matches!(r1, Err(SessionError::InvalidTransition { .. })),
        "resume on Running must be InvalidTransition"
    );

    // Pause once -> ok.
    h.session.pause(None).await.expect("first pause should work");

    // Pause twice -> InvalidTransition (Paused -> Paused is not allowed).
    let r2 = h.session.pause(None).await;
    println!("  pause() on Paused -> {:?}", r2);
    assert!(
        matches!(r2, Err(SessionError::InvalidTransition { .. })),
        "double pause must be InvalidTransition"
    );

    // Resume once -> ok.
    h.session.resume(None).await.expect("resume from Paused should work");

    // Resume again on Running -> InvalidTransition.
    let r3 = h.session.resume(None).await;
    println!("  resume() on Running (after one pause/resume cycle) -> {:?}", r3);
    assert!(
        matches!(r3, Err(SessionError::InvalidTransition { .. })),
        "second resume on Running must be InvalidTransition"
    );

    // After all of that the session must still be functional: pause +
    // resume one more time to confirm no deadlock / no poisoned state.
    h.session.pause(None).await.expect("pause still works");
    h.session.resume(None).await.expect("resume still works");
    println!("  PASS: all idempotency cases handled cleanly, no panic, no deadlock");

    // And: history should record every successful transition with a
    // real timestamp.
    let history = h.session.history().await;
    println!("  history ({} entries):", history.len());
    for r in &history {
        println!(
            "    t={}  {} -> {} ({}) reason={:?}",
            r.timestamp_secs,
            r.from.as_str(),
            r.to.as_str(),
            r.kind.as_str(),
            r.reason
        );
        // Sanity: timestamp must be > 0 (real Unix time, not 1970).
        assert!(r.timestamp_secs > 1_700_000_000, "timestamp looks fake: {}", r.timestamp_secs);
    }
    assert!(history.len() >= 5, "should have start + 3 pauses + 3 resumes at minimum");
    println!("  PASS: history has real timestamps (Unix epoch seconds, not 0)");
}

// ----------------------------------------------------------------------------
// Test 4: stop() while loop is parked. The loop's checkpoint() must return
//   a Cancelled error, not hang. This mirrors what the real ReAct loop
//   will see when a user types "stop" during a manual pause.
//
//   Sequence: pause() -> loop is parked -> stop() -> loop wakes with
//   Cancelled. The previous version of this test was racy because the
//   loop task could finish before the controller's stop() arrived; now
//   the controller explicitly pauses first, so the checkpoint() is
//   guaranteed to be waiting on the notify when stop() fires.
// ----------------------------------------------------------------------------
async fn test_stop_while_parked() {
    println!("\n=== TEST 4: stop() unparks a waiting checkpoint ===");
    let h = Arc::new(TestHarness::new("t4_stop_while_parked"));
    let h2 = h.clone();

    // First park the session so the loop's checkpoint will actually block.
    h.session
        .pause(Some("setting up parked state".into()))
        .await
        .expect("setup pause should succeed");

    let loop_task = tokio::spawn(async move {
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let _ = h2.record(&format!("about to call checkpoint (t={started})")).await;
        let res = h2.session.checkpoint().await;
        let ended = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let parked_secs = ended.saturating_sub(started);
        println!("  loop: checkpoint returned at t={ended} (started at t={started}, parked {parked_secs}s)");
        res
    });

    // Give the loop a moment to enter the notified().await branch.
    tokio::time::sleep(Duration::from_millis(150)).await;
    println!("  controller: calling stop() while loop is parked");
    h.session
        .stop("tear it down".to_string())
        .await
        .expect("stop should succeed");

    // The loop's checkpoint should return Cancelled, not hang.
    let result = tokio::time::timeout(Duration::from_secs(2), loop_task)
        .await
        .expect("loop task should not hang after stop()")
        .expect("loop task should not panic");
    println!("  loop: checkpoint result -> {:?}", result);
    match result {
        Err(SessionError::Cancelled(msg)) => {
            assert!(
                msg.contains("terminal") || msg.contains("Stopped"),
                "expected terminal-state message, got: {msg}"
            );
            println!("  PASS: stop() while parked unblocks checkpoint with Cancelled error");
        }
        other => panic!("expected Cancelled error, got {other:?}"),
    }
}

// ----------------------------------------------------------------------------
// Test 5: transcript file actually has real timestamped entries for every
//   transition, with no placeholders. We drive a small set of transitions
//   and, after each one, write the *handle's* formatted line to the
//   transcript file using the same `format_transition_line` helper the
//   production agent uses. Then we read the file back and validate every
//   line.
// ----------------------------------------------------------------------------
async fn test_transcript_has_real_timestamps() {
    println!("\n=== TEST 5: transcript log has real timestamped transitions ===");
    let session_id = "t5_transcript";
    let h = TestHarness::new(session_id);

    // The session was already created with a Start record in its history.
    // Write that to the transcript first so we have a baseline.
    {
        let history = h.session.history().await;
        let last = history.last().expect("history should not be empty").clone();
        let line = h.session.format_transition_line(&last);
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&h.transcript_path)
        {
            let _ = f.write_all(line.as_bytes());
        }
        println!("  wrote start -> {}", line.trim_end());
    }

    h.session.pause(Some("first".into())).await.expect("pause");
    write_latest(&h, "after first pause").await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    h.session.resume(Some("back".into())).await.expect("resume");
    write_latest(&h, "after resume").await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    h.session.pause(Some("again".into())).await.expect("pause 2");
    write_latest(&h, "after second pause").await;

    h.session.stop("done".into()).await.expect("stop");
    write_latest(&h, "after stop").await;

    let content = std::fs::read_to_string(&h.transcript_path).expect("transcript should exist");
    println!("\n  transcript contents ({} bytes):", content.len());
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        println!("    {line}");
    }

    // Each STATE: line should start with [seconds] and have a valid Unix time.
    let state_lines: Vec<&str> = content
        .lines()
        .filter(|l| l.contains("STATE:"))
        .collect();
    assert!(
        state_lines.len() >= 5,
        "expected at least 5 STATE lines (start + 2 pauses + 1 resume + stop), got {}",
        state_lines.len()
    );
    for line in &state_lines {
        // Format: [<ts>] [<sid>] STATE: <from> -> <to> (<kind>)[ reason=...]
        assert!(line.starts_with('['), "line should start with timestamp: {line}");
        let close = line.find(']').expect("malformed line");
        let ts: u64 = line[1..close]
            .parse()
            .unwrap_or_else(|_| panic!("timestamp not a number: {line}"));
        assert!(
            ts > 1_700_000_000,
            "timestamp looks fake (< 2023): {ts} in {line}"
        );
    }
    println!(
        "  PASS: all {} STATE lines have real Unix timestamps",
        state_lines.len()
    );
}

// Helper for test 5: append the latest history entry to the transcript
// using the handle's formatter. Mirrors what the real agent does after
// each transition.
async fn write_latest(h: &TestHarness, label: &str) {
    let history = h.session.history().await;
    let last = history.last().expect("history should not be empty").clone();
    let line = h.session.format_transition_line(&last);
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&h.transcript_path)
    {
        let _ = f.write_all(line.as_bytes());
    }
    println!("  wrote {} -> {}", label, line.trim_end());
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    println!("mew v2 — Phase 12.2: state machine review & testing");
    println!("====================================================");

    test_terminal_rejects_resume().await;
    test_pause_blocks_and_resume_continues().await;
    test_double_pause_and_resume_idle().await;
    test_stop_while_parked().await;
    test_transcript_has_real_timestamps().await;

    println!("\n====================================================");
    println!("All 12.2 tests passed.");
}
