//! Phase 9.1 — synthetic mock-site harness.
//!
//! Each scenario bundles:
//!
//! * the user task string the runner feeds into `ChatAgent`,
//! * a deterministic pre-flight plan (the `planner::plan`
//!   decomposition the orchestrator would run before the LLM
//!   call),
//! * the page-state tree the agent would see on arrival
//!   (built from the `mew_resilience::mock_fixtures` page
//!   shapes so we share one source of truth for what an
//!   "instagram search page" looks like),
//! * the *expected* terminal `BrowserResult` the runner asserts
//!   on (status, summary, key_findings),
//! * and a list of failure modes the scenario is *known* to
//!   trip (Phase 6's six + Phase 8 captcha). The runner records
//!   these in the report so a regression dashboard can show
//!   "RefDrift count went from 2 to 17 today."
//!
//! Scenarios are pure data: no I/O, no LLM, no time-of-day
//! dependence. The only state the runner needs is the scenario
//! itself. That is what makes the suite re-runnable in CI
//! without flakes.
//!
//! A new scenario is a single `Scenario::new(...)` call in
//! `default_scenarios()`. The runner picks them up
//! automatically.

use crate::completeness::DeclareItem;
use crate::handoff::{BrowserResult, BrowserStatus, Handoff, HandoffSubTask, KeyFinding};
use crate::planner::{plan, Plan};
use mew_perception::TreeNode;
use mew_resilience::mock_fixtures;
use mew_resilience::SessionLossInputs;

/// A single failure mode the scenario is known to trip.
///
/// Stored as a stable string (mirroring
/// `ResilienceFinding::kind_str`) so a report can be serialized
/// to JSON / CSV / Markdown without re-implementing the enum
/// match. The integer code in `ResilienceFinding::code` is the
/// canonical sort key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureMode {
    RefDrift,
    ModalInterruption,
    SessionLoss,
    RateLimit,
    IrreversibleAction,
    VisionAmbiguity,
    CaptchaChallenge,
}

impl FailureMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            FailureMode::RefDrift => "ref_drift",
            FailureMode::ModalInterruption => "modal_interruption",
            FailureMode::SessionLoss => "session_loss",
            FailureMode::RateLimit => "rate_limit",
            FailureMode::IrreversibleAction => "irreversible_action",
            FailureMode::VisionAmbiguity => "vision_ambiguity",
            FailureMode::CaptchaChallenge => "captcha_challenge",
        }
    }
}

impl From<FailureMode> for &'static str {
    fn from(m: FailureMode) -> &'static str {
        m.as_str()
    }
}

/// A single eval scenario. The runner calls
/// `run_scenario(&scenario)` and records the metrics; tests
/// assert on the recorded metrics.
#[derive(Debug, Clone)]
pub struct Scenario {
    /// Short, stable id (e.g. `"instagram_text_friend_via_search"`).
    /// Used as the report row's primary key.
    pub id: &'static str,
    /// Free-text description for the human reader.
    pub description: &'static str,
    /// The user task string fed into the orchestrator.
    pub task: &'static str,
    /// The expected pre-flight decomposition. The runner
    /// asserts the actual planner output matches this in
    /// `subtask_count` and (for the regression-suite scenarios)
    /// in `subtask_ids`. The phase-9 caller-supplied
    /// `expected_subtask_count` keeps the assertion cheap
    /// without entangling the test in the planner's exact
    /// phrasing.
    pub expected_subtask_count: usize,
    /// The page state the agent would land on after the
    /// "navigate" step. The runner feeds it to the
    /// resilience detectors to compute the failure-mode hits
    /// for the report.
    pub page_state: TreeNode,
    /// The expected `BrowserResult` after a successful
    /// round-trip. The runner asserts `status` and that the
    /// synthesized chat reply is non-empty.
    pub expected_result: BrowserResult,
    /// Failure modes this scenario is *known* to trip (if
    /// any). The runner asserts the resilience detector
    /// report includes each one so a future detector
    /// tightening is caught by a test.
    pub known_failure_modes: Vec<FailureMode>,
}

/// The expected outcome of a single scenario run. Pure data
/// so tests can assert on individual fields without a complex
/// matcher. The runner returns one of these per scenario.
#[derive(Debug, Clone)]
pub struct ScenarioOutcome {
    pub scenario_id: String,
    pub passed: bool,
    pub status: BrowserStatus,
    pub subtask_count: usize,
    pub step_count: u32,
    pub failure_modes_hit: Vec<String>,
    pub failure_reason: String,
    pub summary: String,
    pub chat_reply: String,
}

impl Scenario {
    /// Build a scenario with the standard shape. The
    /// `expected_subtask_count` is what the runner asserts;
    /// `page_state` is fed to the resilience detectors;
    /// `expected_result` is what the runner expects the
    /// `ChatAgent` round-trip to produce; `known_failure_modes`
    /// is the tripwire list.
    pub fn new(
        id: &'static str,
        description: &'static str,
        task: &'static str,
        expected_subtask_count: usize,
        page_state: TreeNode,
        expected_result: BrowserResult,
        known_failure_modes: Vec<FailureMode>,
    ) -> Self {
        Self {
            id,
            description,
            task,
            expected_subtask_count,
            page_state,
            expected_result,
            known_failure_modes,
        }
    }

    /// Build the `Handoff` the orchestrator would build for
    /// this scenario. The runner uses this to feed
    /// `ChatAgent::synthesize_reply` so the test exercises
    /// the same handoff path the production orchestrator
    /// runs.
    pub fn build_handoff(&self, originating_message_id: &str) -> Handoff {
        let preflight = plan(self.task);
        // The planner's `Plan` doesn't carry the original
        // task string back (the orchestrator passes it
        // through to `Handoff::task_description`). For the
        // eval harness the scenario's `task` is the
        // canonical input; the Handoff's `task_description`
        // mirrors it.
        let subtasks: Vec<HandoffSubTask> = preflight
            .subtasks
            .iter()
            .map(|s: &DeclareItem| HandoffSubTask {
                id: s.id.clone(),
                description: s.description.clone(),
            })
            .collect();
        Handoff {
            task_description: self.task.to_string(),
            subtasks,
            constraints: Vec::new(),
            originating_message_id: originating_message_id.to_string(),
            research_plan: None,
        }
    }

    /// Run the resilience detectors against `page_state` and
    /// return the list of failure-mode strings the
    /// detectors found. Used by the runner to populate
    /// `failure_modes_hit` on the report.
    pub fn detected_failure_modes(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Some(f) = mew_resilience::detect_modal(&self.page_state) {
            out.push(FailureMode::ModalInterruption.as_str().to_string());
            // Silence unused warning on `f` while keeping the
            // pattern in the code so a future reader sees the
            // detector return shape.
            let _ = f;
        }
        if let Some(f) = mew_resilience::detect_rate_limit(&self.page_state) {
            out.push(FailureMode::RateLimit.as_str().to_string());
            let _ = f;
        }
        // The session-loss detector needs the prior page
        // (so it can tell "we were on a dashboard, now
        // we're on a login form" apart from "we are
        // navigating to the login page on purpose"). The
        // harness doesn't keep the prior here; the
        // detection only fires when the scenario's
        // page_state *is* a login form, which is the same
        // shape the live agent sees. We pass an empty
        // prior — the detector still flags strong
        // (password + sign-in-text) login forms even
        // without the prior.
        let inputs = SessionLossInputs {
            tree: &self.page_state,
            prior_was_dashboard_like: false,
        };
        if let Some(f) = mew_resilience::detect_session_loss(&inputs) {
            out.push(FailureMode::SessionLoss.as_str().to_string());
            let _ = f;
        }
        if let Some(f) = mew_resilience::detect_captcha(&self.page_state) {
            out.push(FailureMode::CaptchaChallenge.as_str().to_string());
            let _ = f;
        }
        out
    }
}

/// The fixed scenario set the regression suite replays.
///
/// Adding a new scenario here is the canonical way to grow
/// the eval set. The runner is data-driven — it doesn't know
/// what scenarios exist, it just runs whatever is in this
/// list.
pub fn default_scenarios() -> Vec<Scenario> {
    vec![
        // 1. The canonical "go to instagram and text my friend"
        //    phrasing. Phase 2's regression-suite scenario
        //    asserts the planner splits it into navigate + send;
        //    the eval scenario asserts the round trip produces
        //    a Done result with both subtasks marked done.
        instagram_text_friend(),
        // 2. The search-first variant: "go to google, search
        //    instagram, then text my friend." The Phase 2
        //    spec says this is the *working* phrasing pre-fix.
        //    The eval asserts the planner decomposes it the
        //    same way as #1 (3+ subtasks: google, instagram,
        //    message).
        google_then_instagram_text(),
        // 3. Cookie banner: agent lands on a page with a
        //    consent dialog. The scenario asserts the modal
        //    detector trips and the round trip still produces
        //    a non-empty chat reply (the resilience layer
        //    handles the dismiss in the live loop; the eval
        //    only checks the *detection* surfaces and the
        //    contract that the user still sees a reply).
        cookie_banner_arrival(),
        // 4. Login wall: agent lands on a "sign in to
        //    continue" dialog. Same surface as #3 but the
        //    expected finding is the login modal kind.
        login_wall_arrival(),
        // 5. 429 rate limit: server returns a 429 page. The
        //    rate-limit detector trips; the round trip's
        //    expected result is a `Failed` with the
        //    rate-limit reason echoed in the chat reply.
        rate_limit_429(),
        // 6. Session loss: the agent was on a dashboard and
        //    is now on a login form. The session-loss
        //    detector trips; the expected result is a
        //    `Failed` with the session-loss reason.
        session_loss_after_dashboard(),
        // 7. Captcha: a Turnstile interstitial. Captcha
        //    detector trips; the expected result is a
        //    `Failed` with the captcha reason.
        captcha_turnstile(),
        // 8. Multi-modal: two overlays on the same page.
        //    Asserts the runner doesn't crash on multiple
        //    findings and the round trip still produces a
        //    non-empty reply.
        multi_modal_arrival(),
        // 9. Clean dashboard: the negative case. No failure
        //    modes hit; the round trip produces a Done
        //    result with a non-empty summary.
        clean_dashboard(),
        // 10. Ref drift: a page where the @e1 the LLM
        //     remembered no longer exists. The page tree
        //     intentionally has a high-ref interactive
        //     element but the scenario's `known_failure_modes`
        //     does not include RefDrift — the eval exercises
        //     the detector surface; RefDrift is the
        //     ref-recovery detector's job, which is in the
        //     unit test layer (Phase 6's
        //     `ref_recovery::attempt_recovery`).
        ref_drift_arrival(),
    ]
}

// -----------------------------------------------------------------------
// Concrete scenarios
// -----------------------------------------------------------------------

fn instagram_text_friend() -> Scenario {
    let page = mock_fixtures::clean_homepage();
    let expected = BrowserResult::done(
        "session_instagram_text",
        "Opened instagram and sent the message to your friend.",
        vec![
            KeyFinding {
                id: "step-1".into(),
                description: "Open instagram.com".into(),
                status: "done".into(),
                reason: String::new(),
                evidence_signature: None,
            },
            KeyFinding {
                id: "step-2".into(),
                description: "Send the message".into(),
                status: "done".into(),
                reason: String::new(),
                evidence_signature: None,
            },
        ],
        Some("len:abcd1234".into()),
        Some("/tmp/transcript_instagram_text.log".into()),
    );
    Scenario::new(
        "instagram_text_friend",
        "Plain 'go to instagram and text my friend' phrasing — Phase 2 regression case.",
        "go to instagram and text my friend hi",
        2,
        page,
        expected,
        vec![],
    )
}

fn google_then_instagram_text() -> Scenario {
    let page = mock_fixtures::clean_homepage();
    let expected = BrowserResult::done(
        "session_google_then_instagram",
        "Searched for instagram on Google, opened it, and sent the message.",
        vec![
            KeyFinding {
                id: "step-1".into(),
                description: "Go to google.com".into(),
                status: "done".into(),
                reason: String::new(),
                evidence_signature: None,
            },
            KeyFinding {
                id: "step-2".into(),
                description: "Search for instagram on google".into(),
                status: "done".into(),
                reason: String::new(),
                evidence_signature: None,
            },
            KeyFinding {
                id: "step-3".into(),
                description: "text my friend hi".into(),
                status: "done".into(),
                reason: String::new(),
                evidence_signature: None,
            },
        ],
        Some("len:deadbeef".into()),
        Some("/tmp/transcript_google_instagram.log".into()),
    );
    // The deterministic planner splits "go to google,
    // search instagram, then text my friend hi" on
    // comma + " then " into 3 pieces (go to google /
    // search instagram / text my friend hi). The
    // expected_subtask_count tracks that. A future
    // planner that *merges* these into 1 piece (e.g. by
    // joining "search X" with "open X") would be a
    // regression on the Phase 2 spec and is caught by
    // this assertion.
    Scenario::new(
        "google_then_instagram_text",
        "Compound phrasing with explicit via-search path — Phase 2 working case.",
        "go to google, search instagram, then text my friend hi",
        3,
        page,
        expected,
        vec![],
    )
}

fn cookie_banner_arrival() -> Scenario {
    let page = mock_fixtures::cookie_banner_page();
    let expected = BrowserResult::done(
        "session_cookie",
        "Dismissed the cookie banner and continued.",
        vec![KeyFinding {
            id: "step-1".into(),
            description: "Dismiss cookie banner".into(),
            status: "done".into(),
            reason: String::new(),
            evidence_signature: None,
        }],
        Some("len:cookie".into()),
        Some("/tmp/transcript_cookie.log".into()),
    );
    // The planner splits "open the homepage and
    // dismiss the cookie banner" on ` and ` into 2
    // pieces (open the homepage / dismiss the cookie
    // banner). The expected count tracks that.
    Scenario::new(
        "cookie_banner_arrival",
        "Cookie consent dialog appears on landing. Asserts modal detector trips.",
        "open the homepage and dismiss the cookie banner",
        2,
        page,
        expected,
        vec![FailureMode::ModalInterruption],
    )
}

fn login_wall_arrival() -> Scenario {
    let page = mock_fixtures::login_wall_page();
    let expected = BrowserResult::done(
        "session_login",
        "Signed in and continued.",
        vec![KeyFinding {
            id: "step-1".into(),
            description: "Sign in".into(),
            status: "done".into(),
            reason: String::new(),
            evidence_signature: None,
        }],
        Some("len:login".into()),
        Some("/tmp/transcript_login.log".into()),
    );
    Scenario::new(
        "login_wall_arrival",
        "Login wall on landing. Asserts modal detector trips with the login kind.",
        "open the dashboard and sign in",
        2,
        page,
        expected,
        vec![FailureMode::ModalInterruption],
    )
}

fn rate_limit_429() -> Scenario {
    let page = mock_fixtures::http_429_page();
    let expected = BrowserResult::failure(
        "session_429",
        "rate-limit signal: HTTP 429 page (back off and retry)",
        Some("/tmp/transcript_429.log".into()),
    );
    Scenario::new(
        "rate_limit_429",
        "Server returns a 429 page. Asserts rate-limit detector trips and round trip is Failed.",
        "scrape the listings page",
        1,
        page,
        expected,
        vec![FailureMode::RateLimit],
    )
}

fn session_loss_after_dashboard() -> Scenario {
    // The session-loss detector requires the *prior* tree as
    // a second argument; the harness here uses the dashboard
    // fixture as the prior so the comparison the detector
    // makes ("we were on a dashboard, now we're on a login
    // form") triggers.
    let page = mock_fixtures::login_wall_page();
    let expected = BrowserResult::failure(
        "session_session_loss",
        "session loss detected: dashboard replaced by login form",
        Some("/tmp/transcript_session_loss.log".into()),
    );
    Scenario::new(
        "session_loss_after_dashboard",
        "Was on a dashboard; page now shows a login form. Asserts session-loss detector trips.",
        "continue with the dashboard task",
        1,
        page,
        expected,
        vec![FailureMode::SessionLoss, FailureMode::ModalInterruption],
    )
}

fn captcha_turnstile() -> Scenario {
    let page = mock_fixtures::cloudflare_turnstile_page();
    let expected = BrowserResult::failure(
        "session_captcha",
        "challenge detected: Cloudflare Turnstile — paused for human handoff",
        Some("/tmp/transcript_captcha.log".into()),
    );
    Scenario::new(
        "captcha_turnstile",
        "Cloudflare Turnstile interstitial. Asserts captcha detector trips and round trip is Failed.",
        "open instagram and view the profile",
        2,
        page,
        expected,
        vec![FailureMode::CaptchaChallenge],
    )
}

fn multi_modal_arrival() -> Scenario {
    let page = mock_fixtures::multi_modal_page();
    let expected = BrowserResult::done(
        "session_multi_modal",
        "Dismissed the cookie banner and the newsletter popup.",
        vec![KeyFinding {
            id: "step-1".into(),
            description: "Dismiss both modals".into(),
            status: "done".into(),
            reason: String::new(),
            evidence_signature: None,
        }],
        Some("len:multi".into()),
        Some("/tmp/transcript_multi_modal.log".into()),
    );
    Scenario::new(
        "multi_modal_arrival",
        "Two overlays on the same page. Asserts runner doesn't crash on multi-finding and reply is non-empty.",
        "open the homepage and dismiss both popups",
        2,
        page,
        expected,
        vec![FailureMode::ModalInterruption],
    )
}

fn clean_dashboard() -> Scenario {
    let page = mock_fixtures::dashboard_page();
    let expected = BrowserResult::done(
        "session_dashboard",
        "Dashboard opened. Welcome back, Alice.",
        vec![],
        Some("len:dash".into()),
        Some("/tmp/transcript_dashboard.log".into()),
    );
    Scenario::new(
        "clean_dashboard",
        "Negative case: a clean dashboard. No failure modes should fire.",
        "open the dashboard",
        1,
        page,
        expected,
        vec![],
    )
}

fn ref_drift_arrival() -> Scenario {
    // A page with stale refs — the @e1 the LLM might
    // remember no longer points to a live element. The eval
    // surface treats this as a clean page (no detector
    // trips); ref recovery is the unit test's job. The
    // scenario exists so the runner's page-state handling
    // has a tree that doesn't trip any of the four
    // detectors, mirroring the real-world case the agent
    // sees most often.
    let page = mock_fixtures::clean_homepage();
    let expected = BrowserResult::done(
        "session_ref_drift",
        "Continued past the stale ref.",
        vec![],
        Some("len:drift".into()),
        Some("/tmp/transcript_ref_drift.log".into()),
    );
    Scenario::new(
        "ref_drift_arrival",
        "Clean page used to exercise the runner with a non-finding path.",
        "open the homepage",
        1,
        page,
        expected,
        vec![],
    )
}

// -----------------------------------------------------------------------
// Plan-shape assertion helpers
// -----------------------------------------------------------------------

/// Assert the planner's actual decomposition matches the
/// scenario's `expected_subtask_count`. Returns the actual
/// `Plan` so a caller can chain more assertions without
/// running the planner twice. Public so the `assertions`
/// module can re-export it under a friendlier name.
pub fn assert_plan_subtask_count(scenario: &Scenario) -> Plan {
    let p = plan(scenario.task);
    assert_eq!(
        p.subtasks.len(),
        scenario.expected_subtask_count,
        "scenario {}: expected {} subtasks from planner, got {} (subtasks: {:?})",
        scenario.id,
        scenario.expected_subtask_count,
        p.subtasks.len(),
        p.subtasks,
    );
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use mew_resilience::ResilienceFinding;

    #[test]
    fn default_scenarios_has_ten_rows() {
        // The regression suite must not silently shrink. A
        // future scenario-removal that drops the count
        // below 10 is caught here.
        let set = default_scenarios();
        assert_eq!(set.len(), 10, "expected 10 default scenarios, got {}", set.len());
    }

    #[test]
    fn every_scenario_has_unique_id() {
        let set = default_scenarios();
        let mut ids: Vec<&str> = set.iter().map(|s| s.id).collect();
        ids.sort();
        let original_len = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), original_len, "duplicate scenario id in default_scenarios()");
    }

    #[test]
    fn every_scenario_task_is_non_empty() {
        let set = default_scenarios();
        for s in &set {
            assert!(!s.task.is_empty(), "scenario {} has empty task", s.id);
        }
    }

    #[test]
    fn every_scenario_builds_a_handoff() {
        // Smoke test: every scenario's `build_handoff` runs
        // without panicking and produces a non-empty
        // `task_description`.
        for s in default_scenarios() {
            let h = s.build_handoff("test:0");
            assert!(!h.task_description.is_empty(), "scenario {}: empty handoff task", s.id);
            // The planner's decomposition is reflected in
            // the handoff's subtask list.
            assert_eq!(
                h.subtasks.len(),
                s.expected_subtask_count,
                "scenario {}: planner subtasks ({}) don't match expected ({})",
                s.id,
                h.subtasks.len(),
                s.expected_subtask_count,
            );
        }
    }

    #[test]
    fn failure_mode_strings_are_stable() {
        // Lock the strings down: the report's column for
        // failure modes is keyed on these. Changing one is a
        // wire-format break.
        assert_eq!(FailureMode::RefDrift.as_str(), "ref_drift");
        assert_eq!(FailureMode::ModalInterruption.as_str(), "modal_interruption");
        assert_eq!(FailureMode::SessionLoss.as_str(), "session_loss");
        assert_eq!(FailureMode::RateLimit.as_str(), "rate_limit");
        assert_eq!(FailureMode::IrreversibleAction.as_str(), "irreversible_action");
        assert_eq!(FailureMode::VisionAmbiguity.as_str(), "vision_ambiguity");
        assert_eq!(FailureMode::CaptchaChallenge.as_str(), "captcha_challenge");
    }

    #[test]
    fn resilience_finding_kind_str_matches_failure_mode() {
        // The harness's `FailureMode` strings and the
        // resilience crate's `ResilienceFinding::kind_str`
        // must agree so a report column using the harness
        // strings can be cross-referenced against the trace
        // layer's findings.
        let page = mock_fixtures::cookie_banner_page();
        let f = mew_resilience::detect_modal(&page).expect("modal");
        let r = ResilienceFinding::Modal(f);
        assert_eq!(r.kind_str(), FailureMode::ModalInterruption.as_str());
    }
}
