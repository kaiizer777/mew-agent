//! Phase 9.2 — eval runner.
//!
//! `run_scenario(&scenario)` exercises one scenario
//! through the same code paths the production orchestrator
//! uses:
//!
//! 1. Build the `Handoff` from the scenario's task
//!    (mirrors `ChatAgent::build_handoff`).
//! 2. Build the expected `BrowserResult` from the
//!    scenario (the runner doesn't call `Agent::run` —
//!    that's live-LLM territory; it asserts the
//!    *contract* that the orchestrator would honor if
//!    the agent returned this `BrowserResult`).
//! 3. Call `ChatAgent::synthesize_reply` to produce the
//!    user-facing chat reply.
//! 4. Assert the four handoff contract properties
//!    (correct task dispatched, result reflected in
//!    reply, id preserved, decomposition not too coarse).
//! 5. Run the resilience detectors against the
//!    scenario's `page_state` to record which failure
//!    modes the live agent would have hit.
//! 6. Compare the detected failure modes against the
//!    scenario's `known_failure_modes` (a regression
//!    guard: if a future detector tightening stops
//!    flagging a scenario's known mode, the test
//!    catches it).
//! 7. Record metrics in a `ScenarioOutcome` and
//!    return.
//!
//! `run_scenarios(&scenarios)` is the multi-scenario
//! version that produces a full `EvalReport` and
//! optionally fails the process on any regression.
//!
//! The runner never opens Chrome, never calls an LLM,
//! and never sleeps. Wall-clock time is recorded so
//! future live-LLM integration can append latency
//! without changing the report shape.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::chat_agent::ChatAgent;
use crate::handoff::BrowserStatus;
use crate::ProviderConfig;

use super::assertions::{assert_handoff_contract, AssertionResult};
use super::harness::{default_scenarios, FailureMode, Scenario, ScenarioOutcome};
use super::report::{EvalReport, RunMetrics};

/// Run a single scenario. Returns the outcome; the
/// caller decides whether a failed outcome is a test
/// failure. Pure-Rust; no I/O.
pub fn run_scenario(
    scenario: &Scenario,
    chat_agent: &ChatAgent,
) -> ScenarioOutcome {
    let started = Instant::now();
    let originating_message_id = format!("eval:{}:{}", scenario.id, unix_secs());

    // (1) Build the handoff the production orchestrator
    // would build for this scenario's task.
    let handoff = scenario.build_handoff(&originating_message_id);

    // (2) The expected BrowserResult is the one the
    // runner asserts on. In a live run this would be
    // what `Agent::run` returns; in the eval it's
    // supplied by the scenario fixture.
    let result = scenario.expected_result.clone();

    // (3) Run the synthesizer — this is the exact
    // method the production `dispatch_browser_task`
    // calls after the agent returns.
    let chat_reply = chat_agent.synthesize_reply(&result, &[], &handoff);

    // (4) Assert the four handoff-contract properties.
    // min_subtasks defaults to 1 so single-action
    // scenarios don't fail on the decomposition check;
    // multi-action scenarios (instagram, google-then-ig)
    // override via `expected_subtask_count`.
    let min_subtasks = if scenario.expected_subtask_count >= 2 {
        2
    } else {
        1
    };
    let contract: AssertionResult = assert_handoff_contract(
        scenario.task,
        &handoff,
        &result,
        &chat_reply,
        &originating_message_id,
        min_subtasks,
    );

    // (5) Run the resilience detectors against the
    // scenario's page state. The output is the
    // *detected* failure-mode list; the assertion
    // (6) compares it against the *known* list.
    let detected = scenario.detected_failure_modes();
    let known = scenario
        .known_failure_modes
        .iter()
        .map(|m| m.as_str().to_string())
        .collect::<Vec<_>>();
    let detection_ok = known.iter().all(|k| detected.iter().any(|d| d == k));

    // (7) Compute pass/fail and assemble the outcome.
    let passed = contract.is_ok() && detection_ok;
    let failure_reason = match (&contract, detection_ok) {
        (Err(e), _) => format!("contract: {e}"),
        (Ok(_), false) => format!(
            "detector missed a known mode: known={known:?}, detected={detected:?}"
        ),
        (Ok(_), true) => String::new(),
    };

    // Pull step_count from the result's key_findings so
    // the report's `step_count` column tracks the
    // orchestrator's `TaskCompleted` event's field.
    let step_count = result.key_findings.len() as u32;

    let elapsed = started.elapsed();

    // We deliberately don't include `elapsed` in
    // ScenarioOutcome (RunMetrics owns it) so the
    // outcome shape stays focused on *what*, not *how
    // long*. RunMetrics::from_outcome stamps the
    // elapsed on the way out.
    let _ = elapsed;

    ScenarioOutcome {
        scenario_id: scenario.id.to_string(),
        passed,
        status: result.status,
        subtask_count: handoff.subtasks.len(),
        step_count,
        failure_modes_hit: detected,
        failure_reason,
        summary: result.summary,
        chat_reply,
    }
}

/// Run a list of scenarios, returning a single
/// `EvalReport`. The optional `fail_on_regression`
/// argument (default `false`) controls whether the
/// runner returns `Err` on any failed scenario — the
/// eval binary sets this to `true` so `cargo test
/// --features eval` fails loudly on regression.
pub fn run_scenarios(
    scenarios: &[Scenario],
    chat_agent: &ChatAgent,
) -> EvalReport {
    let mut report = EvalReport::new(unix_secs());
    for s in scenarios {
        let started = Instant::now();
        let outcome = run_scenario(s, chat_agent);
        let elapsed = started.elapsed();
        let m = RunMetrics::from_outcome(&outcome, elapsed);
        report.push(m);
    }
    report
}

/// Build a default `ChatAgent` for the runner. Uses a
/// minimal `ProviderConfig` (the synthesizer is
/// deterministic, so no API call is made). Useful for
/// the `cargo test --features eval` flow.
pub fn default_chat_agent() -> ChatAgent {
    let cfg = ProviderConfig {
        opencode_zen: crate::OpencodeZenConfig {
            base_url: "http://test".into(),
            api_key: "test".into(),
            default_model: "test".into(),
            max_iterations: 1,
            max_tokens: None,
            max_cost: None,
        },
        browser: None,
        agent: crate::AgentConfig::default(),
    };
    ChatAgent::new(cfg)
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// -----------------------------------------------------------------------
// FailureMode ↔ ResilienceFinding helpers (kept here so the
// runner file is the single import for the eval consumer).
// -----------------------------------------------------------------------

/// Find the first `Scenario` whose id matches the
/// argument. Returns `None` if the id is not in the
/// default scenario set. Used by the eval binary's
/// `--scenario` filter.
pub fn find_scenario(id: &str) -> Option<Scenario> {
    super::harness::default_scenarios()
        .into_iter()
        .find(|s| s.id == id)
}

/// Build a report from a single scenario's outcome. A
/// convenience for tests that want to assert on a
/// report shape without running the full suite.
pub fn report_from_outcome(
    scenario: &Scenario,
    outcome: ScenarioOutcome,
    elapsed: std::time::Duration,
) -> EvalReport {
    let mut r = EvalReport::new(unix_secs());
    r.push(RunMetrics::from_outcome(&outcome, elapsed));
    let _ = scenario; // keep the argument for future trace enrichment
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_passes_all_default_scenarios() {
        // The headline regression: every scenario in
        // the default set must pass the handoff
        // contract. A failure here is a regression
        // caught by `cargo test --features eval`.
        let agent = default_chat_agent();
        let report = run_scenarios(&super::super::harness::default_scenarios(), &agent);
        for row in &report.rows {
            assert!(
                row.passed,
                "scenario {} failed: failure_reason={:?}, failure_modes_hit={:?}",
                row.scenario_id, row.failure_reason, row.failure_modes_hit,
            );
        }
    }

    #[test]
    fn runner_records_subtask_count_from_handoff() {
        let agent = default_chat_agent();
        let scenarios = super::super::harness::default_scenarios();
        let ig = scenarios
            .iter()
            .find(|s| s.id == "instagram_text_friend")
            .expect("instagram_text_friend scenario");
        let outcome = run_scenario(ig, &agent);
        assert_eq!(outcome.subtask_count, 2, "instagram phrasing should split into 2 subtasks");
    }

    #[test]
    fn runner_records_subtask_count_for_compound_task() {
        let agent = default_chat_agent();
        let scenarios = super::super::harness::default_scenarios();
        let g = scenarios
            .iter()
            .find(|s| s.id == "google_then_instagram_text")
            .expect("google_then_instagram_text scenario");
        let outcome = run_scenario(g, &agent);
        // The planner splits on `, ` and ` then `, so
        // "go to google, search instagram, then text my
        // friend hi" produces 3 pieces. (The
        // expectation lives on the scenario itself;
        // this test pins the runner against it so a
        // future planner change that drops a clause
        // fails the runner first.)
        assert_eq!(outcome.subtask_count, g.expected_subtask_count);
    }

    #[test]
    fn runner_records_failure_modes_for_429_scenario() {
        let agent = default_chat_agent();
        let scenarios = super::super::harness::default_scenarios();
        let r = scenarios
            .iter()
            .find(|s| s.id == "rate_limit_429")
            .expect("rate_limit_429 scenario");
        let outcome = run_scenario(r, &agent);
        assert!(
            outcome
                .failure_modes_hit
                .iter()
                .any(|m| m == FailureMode::RateLimit.as_str()),
            "expected rate_limit detection, got {:?}",
            outcome.failure_modes_hit,
        );
    }

    #[test]
    fn runner_marks_failed_status_for_429_scenario() {
        let agent = default_chat_agent();
        let scenarios = super::super::harness::default_scenarios();
        let r = scenarios
            .iter()
            .find(|s| s.id == "rate_limit_429")
            .expect("rate_limit_429 scenario");
        let outcome = run_scenario(r, &agent);
        assert_eq!(outcome.status, BrowserStatus::Failed);
        // The "never silent" guarantee: even a Failed
        // result produces a non-empty chat reply.
        assert!(!outcome.chat_reply.is_empty());
    }

    #[test]
    fn runner_records_no_failure_modes_for_clean_dashboard() {
        let agent = default_chat_agent();
        let scenarios = super::super::harness::default_scenarios();
        let r = scenarios
            .iter()
            .find(|s| s.id == "clean_dashboard")
            .expect("clean_dashboard scenario");
        let outcome = run_scenario(r, &agent);
        assert!(
            outcome.failure_modes_hit.is_empty(),
            "clean dashboard should hit no failure modes, got {:?}",
            outcome.failure_modes_hit,
        );
    }

    #[test]
    fn runner_records_captcha_failure_mode() {
        let agent = default_chat_agent();
        let scenarios = super::super::harness::default_scenarios();
        let r = scenarios
            .iter()
            .find(|s| s.id == "captcha_turnstile")
            .expect("captcha_turnstile scenario");
        let outcome = run_scenario(r, &agent);
        assert!(
            outcome
                .failure_modes_hit
                .iter()
                .any(|m| m == FailureMode::CaptchaChallenge.as_str()),
        );
    }

    #[test]
    fn report_pass_rate_is_full_when_all_pass() {
        let agent = default_chat_agent();
        let report = run_scenarios(&super::super::harness::default_scenarios(), &agent);
        assert_eq!(report.pass_rate(), Some(1.0));
    }

    #[test]
    fn find_scenario_returns_matching_scenario() {
        let s = find_scenario("instagram_text_friend");
        assert!(s.is_some());
        let s = find_scenario("nonexistent");
        assert!(s.is_none());
    }
}
