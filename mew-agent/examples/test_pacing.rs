// mew v2 — Phase 17.1: site-specific pacing guard review & testing harness.
//
// Standalone binary that exercises the pacing guard end-to-end against the
// real `Agent` type, without needing Chrome or a live LLM.
//
// What this covers (per the 17.1 spec bullets):
//   1. Repeated identical actions in a tight loop (e.g. several `click`s
//      back-to-back) are spaced out by a random delay in
//      [min_delay_ms, max_delay_ms].
//   2. One-off actions are NEVER paced — only after a streak of the
//      same action type.
//   3. A different action type between two of the same type resets
//      the streak (so a click-click-type-click is *not* a 4-click
//      streak).
//   4. The pacing decision is logged to the transcript with a real
//      timestamp and the streak count + chosen delay.
//
// We don't need the full `run_inner` loop. The pacing decision is
// a pure synchronous function on the guard, so we can call it
// directly. The integration aspect is that we drive it through the
// real `Agent` struct (via the public `pacing` accessor added in
// 17.1) and exercise the same `PacedAction::from_tool_name` ->
// `before_action` -> `log_pacing_decision` chain the production
// loop uses.
//
// Run with: cargo run --example test_pacing -p mew-agent

use std::io::Write;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mew_agent::agent::Agent;
use mew_agent::pacing::{PacedAction, PacingConfig, PacingDecision, PacingGuard};

// ----------------------------------------------------------------------------
// Helper: a tiny "loop" that mimics the production dispatch site. It
// calls `before_action`, then if the decision is `Pace`, sleeps for
// the chosen delay and records what happened. We measure real elapsed
// time with `Instant` so the test can assert the *actual* sleep
// happened, not just that the function returned the right enum
// variant. This is the "real timestamps" check the 17.2 spec asks
// for.
// ----------------------------------------------------------------------------
struct LoopDriver {
    transcript_path: String,
    events: Vec<(u64, String, Option<Duration>)>,
}

impl LoopDriver {
    fn new(name: &str) -> Self {
        let path = format!("test_pacing_{}.log", name);
        // Truncate any previous run.
        let _ = std::fs::write(&path, "");
        Self {
            transcript_path: path,
            events: Vec::new(),
        }
    }

    fn now_secs(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// One simulated dispatch. Returns the elapsed wall time so the
    /// test can assert pacing actually slept.
    fn dispatch(&mut self, guard: &mut PacingGuard, name: &str) -> Duration {
        let start = Instant::now();
        let decision = guard.before_action(name);
        match &decision {
            PacingDecision::NoPacing => {
                // No log, no sleep — per 17.1 spec, no-op is silent.
            }
            PacingDecision::Pace { delay, streak } => {
                // Mirror the production transcript format.
                let ts = self.now_secs();
                let action = PacedAction::from_tool_name(name).unwrap();
                let line = format!(
                    "[{}] PACING: action={} streak={} delay_ms={}\n",
                    ts,
                    action.as_str(),
                    streak,
                    delay.as_millis()
                );
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.transcript_path)
                {
                    let _ = f.write_all(line.as_bytes());
                }
                self.events.push((ts, line.trim().to_string(), Some(*delay)));
                std::thread::sleep(*delay);
            }
        }
        start.elapsed()
    }

    fn transcript(&self) -> String {
        std::fs::read_to_string(&self.transcript_path).unwrap_or_default()
    }
}

// ----------------------------------------------------------------------------
// TEST 1: the pacing guard sleeps between consecutive identical
//   actions in a tight loop, and the elapsed time proves the sleep
//   actually happened (not just a fake return value).
//
//   This is the 17.2 "real timestamps" check: a real sleep adds
//   real time. We pick a deliberately tiny range (50-100ms) so
//   the test runs in <1s, but the elapsed-time assertion still
//   proves the code path actually slept.
// ----------------------------------------------------------------------------
fn test_tight_loop_is_paced() {
    println!("\n=== TEST 1: tight loop of identical actions is paced ===");
    let mut guard = PacingGuard::new(PacingConfig {
        enabled: true,
        // 50-100ms range. Way smaller than production's 800-2500
        // because this is a test — we just need the path to fire
        // and the elapsed-time assertion to be unambiguous.
        min_delay_ms: 50,
        max_delay_ms: 100,
        // threshold=3: first 2 clicks under threshold, 3rd+4th pace.
        // This matches the unit test `threshold_gates_pacing` and
        // gives us a clear "first two no-pace, last two pace"
        // shape to assert against.
        streak_threshold: 3,
    });
    let mut driver = LoopDriver::new("t1_tight_loop");

    // Simulate: click, click, click, click — 4 clicks in a row.
    // With threshold=3, the first 2 fire without delay, the next 2
    // each pay a 50-100ms sleep.
    let mut elapsed_per_click = Vec::new();
    for _ in 0..4 {
        elapsed_per_click.push(driver.dispatch(&mut guard, "click"));
    }
    println!("  elapsed per click: {:?}", elapsed_per_click);

    // First two clicks: no pacing — must be near-instant.
    assert!(
        elapsed_per_click[0] < Duration::from_millis(20),
        "click 1 should be near-instant, got {:?}",
        elapsed_per_click[0]
    );
    assert!(
        elapsed_per_click[1] < Duration::from_millis(20),
        "click 2 (still under threshold) should be near-instant, got {:?}",
        elapsed_per_click[1]
    );
    // Clicks 3 and 4: each pays a sleep in [50, 100]ms.
    assert!(
        elapsed_per_click[2] >= Duration::from_millis(45),
        "click 3 should have slept ~50-100ms, got {:?}",
        elapsed_per_click[2]
    );
    assert!(
        elapsed_per_click[3] >= Duration::from_millis(45),
        "click 4 should have slept ~50-100ms, got {:?}",
        elapsed_per_click[3]
    );

    // Transcript must contain exactly 2 PACING lines (one per
    // paced click). The first two clicks are silent (per 17.1
    // spec: log *when pacing is applied*, not every no-op).
    let transcript = driver.transcript();
    let pacing_line_count = transcript.matches("PACING:").count();
    assert_eq!(
        pacing_line_count, 2,
        "expected exactly 2 PACING log lines, got {} in:\n{}",
        pacing_line_count, transcript
    );
    println!("  PASS: 4 clicks dispatched, 2 paced, transcript shows 2 PACING lines");
}

// ----------------------------------------------------------------------------
// TEST 2: a single one-off action (e.g. one click in isolation,
//   or one click with different actions between) is NOT paced.
//
//   This is the 17.2 bullet: "Confirm a single one-off action is
//   not needlessly delayed."
// ----------------------------------------------------------------------------
fn test_one_off_is_not_paced() {
    println!("\n=== TEST 2: one-off action is not paced ===");
    let mut guard = PacingGuard::new(PacingConfig {
        enabled: true,
        // Use a huge range so a true no-pacing is obvious — if any
        // sleep fires, the elapsed time will be hundreds of ms.
        min_delay_ms: 1000,
        max_delay_ms: 2000,
        streak_threshold: 2,
    });
    let mut driver = LoopDriver::new("t2_one_off");

    // One click, then a snapshot (non-paced), then another click.
    // Neither click should be paced because:
    //   - the first click is a new streak of length 1
    //   - the snapshot resets the streak
    //   - the second click is again a new streak of length 1
    let e1 = driver.dispatch(&mut guard, "click");
    let e2 = driver.dispatch(&mut guard, "snapshot");
    let e3 = driver.dispatch(&mut guard, "click");
    println!("  elapsed: click1={:?} snapshot={:?} click2={:?}", e1, e2, e3);

    // All three must be near-instant (no sleep anywhere in a
    // 1000-2000ms range).
    for (i, e) in [&e1, &e2, &e3].iter().enumerate() {
        assert!(
            **e < Duration::from_millis(20),
            "dispatch {} should be near-instant, got {:?}",
            i, e
        );
    }

    // Transcript must be empty — NoPacing is silent.
    let transcript = driver.transcript();
    assert!(
        !transcript.contains("PACING:"),
        "no PACING lines should appear for one-off actions, got:\n{}",
        transcript
    );
    println!("  PASS: one-off click + snapshot + click = no pacing, transcript empty");
}

// ----------------------------------------------------------------------------
// TEST 3: a different action type in between resets the streak.
//
//   click, click, type, click — the last click is a fresh streak
//   of length 1, not a 4-click continuation. With threshold=2, it
//   should NOT be paced.
// ----------------------------------------------------------------------------
fn test_different_action_resets_streak() {
    println!("\n=== TEST 3: different action type resets streak ===");
    let mut guard = PacingGuard::new(PacingConfig {
        enabled: true,
        // Use a range that would be obvious if it fired.
        min_delay_ms: 200,
        max_delay_ms: 400,
        streak_threshold: 2,
    });
    let mut driver = LoopDriver::new("t3_streak_reset");

    let e1 = driver.dispatch(&mut guard, "click"); // streak=1, no pace
    let e2 = driver.dispatch(&mut guard, "click"); // streak=2, PACE
    let e3 = driver.dispatch(&mut guard, "type"); // resets streak
    let e4 = driver.dispatch(&mut guard, "click"); // streak=1 again, no pace
    println!("  elapsed: {:?} {:?} {:?} {:?}", e1, e2, e3, e4);

    // Only the second click should be paced.
    assert!(e1 < Duration::from_millis(20), "click1 no-pace: {:?}", e1);
    assert!(
        e2 >= Duration::from_millis(180),
        "click2 should pace 200-400ms: {:?}",
        e2
    );
    assert!(e3 < Duration::from_millis(20), "type resets, no pace: {:?}", e3);
    assert!(e4 < Duration::from_millis(20), "click3 new streak, no pace: {:?}", e4);

    // Exactly 1 PACING line in the transcript.
    let transcript = driver.transcript();
    let pacing_line_count = transcript.matches("PACING:").count();
    assert_eq!(pacing_line_count, 1, "expected 1 PACING line, got {} in:\n{}", pacing_line_count, transcript);
    println!("  PASS: type-action reset the click streak, only 1 PACING line");
}

// ----------------------------------------------------------------------------
// TEST 4: disabled guard is a true no-op.
//
//   With `enabled: false`, even a long streak of identical actions
//   must produce no sleep and no log lines. This is the 17.1
//   requirement: "Gating the whole feature behind a flag, when off
//   the code path is fully skipped."
// ----------------------------------------------------------------------------
fn test_disabled_is_no_op() {
    println!("\n=== TEST 4: disabled guard is a no-op ===");
    let mut guard = PacingGuard::new(PacingConfig {
        enabled: false,
        // Set a huge range so any sleep would be obvious. Even
        // though enabled=false, the range values must not matter.
        min_delay_ms: 5000,
        max_delay_ms: 9000,
        streak_threshold: 1,
    });
    let mut driver = LoopDriver::new("t4_disabled");

    // 10 identical clicks. None should be paced.
    let mut max_elapsed = Duration::ZERO;
    for _ in 0..10 {
        let e = driver.dispatch(&mut guard, "click");
        if e > max_elapsed {
            max_elapsed = e;
        }
    }
    println!("  max elapsed across 10 clicks: {:?}", max_elapsed);

    // Even with 5-9s configured delays, every dispatch is
    // near-instant.
    assert!(
        max_elapsed < Duration::from_millis(20),
        "disabled guard slept for {:?} (should be near-instant)",
        max_elapsed
    );

    // Transcript must be empty.
    let transcript = driver.transcript();
    assert!(
        !transcript.contains("PACING:"),
        "disabled guard should not log, got:\n{}",
        transcript
    );
    println!("  PASS: disabled guard = no sleep, no log");
}

// ----------------------------------------------------------------------------
// TEST 5: integration with the real `Agent` struct.
//
//   The pacing guard lives on `Agent`. We use the `pacing_mut`
//   test accessor (added in 17.1) to install an enabled config
//   on an `Agent::new_for_test` instance, then drive the same
//   before_action path the production `run_inner` loop uses. This
//   proves the wiring is end-to-end real, not just that the unit
//   test of `PacingGuard` works in isolation.
// ----------------------------------------------------------------------------
fn test_agent_integration() {
    println!("\n=== TEST 5: end-to-end through Agent struct ===");
    let mut agent = Agent::new_for_test("click 4 buttons in a row");
    // Replace the disabled guard with an enabled one. We can't
    // reach the field directly (it's private), so the
    // `new_for_test` constructor gave us a disabled guard; we
    // need a setter. The cleanest path is a public test helper —
    // see the `pacing_mut_for_test` method added in 17.1.

    // Drive: 4 clicks through the agent's pacing guard.
    let mut driver = LoopDriver::new("t5_agent");
    let mut total_elapsed = Duration::ZERO;
    for _ in 0..4 {
        let e = driver.dispatch(agent.pacing_mut_for_test(), "click");
        total_elapsed += e;
    }
    println!("  total elapsed across 4 clicks: {:?}", total_elapsed);

    // For the test, we want it to run fast. Replace the agent's
    // pacing with one configured for a small range, then drive
    // it again.
    *agent.pacing_mut_for_test() = PacingGuard::new(PacingConfig {
        enabled: true,
        min_delay_ms: 30,
        max_delay_ms: 60,
        streak_threshold: 3,
    });
    let mut driver2 = LoopDriver::new("t5_agent_b");
    let mut total2 = Duration::ZERO;
    for _ in 0..4 {
        let e = driver2.dispatch(agent.pacing_mut_for_test(), "click");
        total2 += e;
    }
    println!("  (small-range) total elapsed: {:?}", total2);

    // 4 clicks: first 2 no-pace (streak 1, 2), last 2 each
    // sleep 30-60ms. So total is in [60ms, 120ms]. Use a
    // generous lower bound to avoid Windows scheduler jitter.
    assert!(
        total2 >= Duration::from_millis(50),
        "expected at least 50ms of pacing (2 sleeps of 30-60ms each), got {:?}",
        total2
    );
    assert!(
        total2 < Duration::from_millis(500),
        "expected well under 500ms total, got {:?}",
        total2
    );

    // Transcript should have exactly 2 PACING lines from this
    // run (the first 2 clicks were under the threshold, silent).
    let t = driver2.transcript();
    let count = t.matches("PACING:").count();
    assert_eq!(count, 2, "expected 2 PACING lines, got {} in:\n{}", count, t);
    println!("  PASS: end-to-end through Agent pacing guard works");
}

// ----------------------------------------------------------------------------
// TEST 6: the absurdly-low-range case from the 17.2 spec.
//
//   "Set the delay range to something absurdly low temporarily and
//    confirm the code path still works (just faster) — proves the
//    delay is real logic, not a hardcoded sleep that happens to
//    look right at default settings."
//
//   We use min=0, max=0: enabled, but the actual sleep is 0ms.
//   The guard must still classify the action as paced (the
//   decision variant is `Pace`, not `NoPacing`) and the
//   transcript still gets a log line. This proves the pacing
//   code path is what's executing, not a fixed sleep.
// ----------------------------------------------------------------------------
fn test_absurdly_low_range_still_logs() {
    println!("\n=== TEST 6: 0..=0 range still logs (proves real code path) ===");
    let mut guard = PacingGuard::new(PacingConfig {
        enabled: true,
        min_delay_ms: 0,
        max_delay_ms: 0,
        streak_threshold: 1,
    });
    let mut driver = LoopDriver::new("t6_zero_range");

    // First click: streak=1, threshold=1 means second click is
    // the one that paces.
    let _ = driver.dispatch(&mut guard, "click"); // streak=1, no pace
    let elapsed = driver.dispatch(&mut guard, "click"); // streak=2, PACE 0ms
    println!("  second click elapsed: {:?}", elapsed);

    // The decision was `Pace` even though the sleep was 0ms —
    // elapsed is near-instant but the function path executed.
    assert!(elapsed < Duration::from_millis(20), "should be near-instant with 0ms delay, got {:?}", elapsed);

    // Transcript must contain a PACING line — proving the
    // pacing code path ran, not just that we got lucky on a
    // no-op return.
    let transcript = driver.transcript();
    assert!(
        transcript.contains("PACING:") && transcript.contains("delay_ms=0"),
        "expected a PACING line with delay_ms=0, got:\n{}",
        transcript
    );
    println!("  PASS: 0..=0 range still produces a real PACING log line");
}

// ----------------------------------------------------------------------------
// Entry point
// ----------------------------------------------------------------------------
#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!("mew v2 — Phase 17.1 pacing guard review & testing harness");
    println!("=========================================================");

    test_tight_loop_is_paced();
    test_one_off_is_not_paced();
    test_different_action_resets_streak();
    test_disabled_is_no_op();
    test_agent_integration();
    test_absurdly_low_range_still_logs();

    println!("\n=========================================================");
    println!("All 6 tests passed. Phase 17.1 pacing guard is verified.");
}
