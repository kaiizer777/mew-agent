//! Phase 6 — Resilience core adapter.
//!
//! The agent loop lives in `agent.rs`. The six failure-mode
//! detectors live in `mew_resilience` as pure-Rust, testable
//! functions. This module is the *only* place the agent depends on
//! `mew_resilience` — the rest of the agent sees the
//! `ResilienceHookOutcome` shape defined here, not the raw
//! `mew_resilience::ResilienceFinding` enum.
//!
//! Why the adapter layer: the resilience crate returns findings as
//! data; the agent loop needs *decisions* (sleep this long, pause
//! the session, retry, escalate, etc). The adapter maps findings
//! to actions. If the resilience crate's API changes in the
//! future, only this file changes — the agent loop keeps its
//! contract.
//!
//! The decisions the adapter makes are intentionally narrow:
//!
//!   * `ResilienceHookOutcome::Continue`           — no finding
//!                                                    fired; the
//!                                                    agent proceeds
//!                                                    normally.
//!   * `ResilienceHookOutcome::AutoDismiss`        — a modal was
//!                                                    detected with
//!                                                    a known
//!                                                    `dismiss_ref`.
//!                                                    The agent
//!                                                    clicks the
//!                                                    ref on the
//!                                                    next iteration
//!                                                    instead of
//!                                                    running the
//!                                                    LLM's planned
//!                                                    action.
//!   * `ResilienceHookOutcome::Backoff { secs }`   — a rate-limit
//!                                                    signal fired.
//!                                                    The agent
//!                                                    sleeps before
//!                                                    the next
//!                                                    iteration.
//!   * `ResilienceHookOutcome::PauseForUser`       — an
//!                                                    irreversible
//!                                                    action was
//!                                                    classified.
//!                                                    The session
//!                                                    moves to
//!                                                    `Paused` and
//!                                                    the user is
//!                                                    asked to
//!                                                    confirm.
//!   * `ResilienceHookOutcome::SurfaceAsFinding`   — a finding
//!                                                    the agent
//!                                                    should
//!                                                    surface to
//!                                                    the LLM as a
//!                                                    typed error
//!                                                    (session loss,
//!                                                    ambiguous
//!                                                    modal without
//!                                                    dismiss ref).
//!   * `ResilienceHookOutcome::ForceSnapshot`      — the ref
//!                                                    drift
//!                                                    detector
//!                                                    asked for a
//!                                                    re-snapshot;
//!                                                    the agent
//!                                                    does that on
//!                                                    the next
//!                                                    iteration.
//!
//! Each finding maps to one outcome; when several findings fire
//! the adapter picks the most severe (matching `mew_resilience::
//! categorize`).

use mew_perception::TreeNode;
use mew_resilience::{
    Challenge, categorize as categorize_findings,
    classify_irreversible, detect_captcha, detect_modal, detect_rate_limit,
    detect_session_loss, score_vision, SessionLossInputs, VisionVerdict,
};
use serde_json::Value;

/// The decision the agent loop should take based on the
/// resilience findings. Each variant carries the data the
/// loop needs to act — no extra lookups.
#[derive(Debug, Clone, PartialEq)]
pub enum ResilienceHookOutcome {
    /// No finding fired. The loop continues with the LLM's
    /// planned action.
    Continue,
    /// A modal was detected with a known close button. The
    /// `dismiss_ref` is what the agent should click on the
    /// next iteration *before* running the LLM's planned
    /// action. The LLM gets a "modal dismissed" note on
    /// its next turn.
    AutoDismiss { dismiss_ref: String },
    /// A rate-limit signal fired. The agent should sleep for
    /// `secs` seconds before the next perception cycle.
    Backoff { secs: u64 },
    /// An irreversible action was classified. The agent
    /// should transition the session to `Paused` and surface
    /// a confirmation request to the user. The `target` is
    /// shown in the user-facing chat message.
    PauseForUser { target: String, action_kind: String },
    /// A finding the LLM should see as a typed tool error.
    /// The loop appends the summary to the next user-role
    /// message so the LLM can re-plan.
    SurfaceAsFinding { summary: String, kind: String },
    /// A ref was stale; the loop should re-snapshot on the
    /// next iteration and retry. (Used by the ref-recovery
    /// hook — see `apply_ref_recovery_outcome`.)
    ForceSnapshot { reason: String },
    /// Phase 8: a CAPTCHA / challenge page was detected. The
    /// session is paused (the human is the safest solver in
    /// a real visible browser) and the user is told what
    /// challenge was detected, on what host, and what they
    /// need to do. `Challenge` is the structured finding the
    /// resilience detector produced — the agent uses
    /// `display_name()` for the user-facing label and
    /// `user_action_hint()` for the actionable prompt.
    PauseForCaptcha { challenge: Challenge },
}

/// The aggregated view of every detector on the current
/// iteration. The adapter builds this once per perception
/// cycle and stores the *most severe* outcome on the agent
/// (`Agent::resilience_outcome`) so other call sites can
/// read it cheaply.
#[derive(Debug)]
pub struct ResilienceHookReport {
    pub outcome: ResilienceHookOutcome,
    /// The original findings, for the trace log. Empty when
    /// no finding fired.
    pub findings: Vec<String>,
}

/// Run all the page-state detectors in one pass and return the
/// aggregated decision. The agent loop calls this after the
/// perception step (where the `TreeNode` is fresh) and before
/// dispatching the LLM's chosen tool. `prior_was_dashboard_like`
/// is the agent's memory of what the *previous* page looked
/// like — fed into the session-loss detector so a login form
/// on a page that was a dashboard is a session-loss event but
/// a login form on a fresh navigation to /login is not.
///
/// The function is pure-Rust and synchronous; the agent loop
/// wraps it without `await`.
pub fn evaluate_page(
    tree: &TreeNode,
    prior_was_dashboard_like: bool,
) -> ResilienceHookReport {
    let mut findings = Vec::new();

    // 1. Phase 8: CAPTCHA / challenge page. Runs *first*
    // because challenge pages contain text that trips the
    // other detectors (Cloudflare's "verify you are human"
    // also trips the rate-limit detector, reCAPTCHA's "I am
    // not a robot" is on a `dialog`-role container that
    // trips the modal detector). The challenge is the
    // *dominant* signal on a challenge page; treating it as
    // a "modal that needs the user's attention" is correct,
    // and the right action is `PauseForCaptcha`, not
    // `SurfaceAsFinding` / `Backoff` / `AutoDismiss`.
    // Re-ordering this above the other detectors also
    // means the captcha fixture tree is reliably classified
    // as a captcha even when its visible text overlaps with
    // other patterns.
    if let Some(challenge) = detect_captcha(tree) {
        let label = challenge.label.clone();
        findings.push(format!("{}:{}", "Captcha", label));
        return ResilienceHookReport {
            outcome: ResilienceHookOutcome::PauseForCaptcha { challenge },
            findings,
        };
    }

    // 2. Rate limit. Highest urgency — the server is
    // actively refusing us. Back off regardless of any
    // other signal.
    if let Some(sig) = detect_rate_limit(tree) {
        let secs = sig.kind.default_backoff_secs();
        let summary = format!("rate-limit:{} ({}s backoff)", sig.kind.as_str(), secs);
        findings.push(format!("{}:{}", "RateLimit", summary));
        return ResilienceHookReport {
            outcome: ResilienceHookOutcome::Backoff { secs },
            findings,
        };
    }

    // 3. Session loss. The user has been logged out — the
    // task cannot proceed without their action. Runs *before*
    // the modal detector because a login-wall dialog and a
    // session-loss event are the same page in this case; the
    // user-actionable "you got logged out, sign in again"
    // message is more useful than a generic "dismiss this
    // overlay" prompt.
    let session_inputs = SessionLossInputs {
        tree,
        prior_was_dashboard_like,
    };
    if let Some(report) = detect_session_loss(&session_inputs) {
        let summary = format!("session loss: {} (hint: {})", report.reason, report.hint);
        findings.push(format!("{}:{}", "SessionLoss", summary));
        return ResilienceHookReport {
            outcome: ResilienceHookOutcome::SurfaceAsFinding {
                summary: report.hint,
                kind: "session_loss".to_string(),
            },
            findings,
        };
    }

    // 4. Modal interruption. The remaining overlay cases
    // (cookie banner, newsletter, age gate) where there is
    // no session-loss signal. A cookie banner with a
    // known close button can be auto-dismissed; everything
    // else gets surfaced as a typed finding.
    if let Some(report) = detect_modal(tree) {
        let kind_str = report.kind.as_str().to_string();
        let summary = format!("modal overlay ({})", kind_str);
        if let Some(dismiss) = report.dismiss_ref {
            if report.kind.can_auto_dismiss() {
                findings.push(format!("{}:{}", "AutoDismiss", summary));
                return ResilienceHookReport {
                    outcome: ResilienceHookOutcome::AutoDismiss { dismiss_ref: dismiss },
                    findings,
                };
            }
        }
        // Auto-dismiss not possible (no ref, or non-dismissable
        // kind) -> surface to the LLM.
        findings.push(format!("{}:{}", "Modal", summary));
        return ResilienceHookReport {
            outcome: ResilienceHookOutcome::SurfaceAsFinding {
                summary: format!(
                    "A {} overlay is on the page. Look at the snapshot to decide whether to dismiss it or report it to the user.",
                    kind_str
                ),
                kind: "modal".to_string(),
            },
            findings,
        };
    }

    // 5. Categorize remaining findings (none in this code path;
    // the function is shaped so future detectors can be added
    // here without changing the contract).
    let _ = categorize_findings(&[]);

    // Default: no finding, continue.
    ResilienceHookReport {
        outcome: ResilienceHookOutcome::Continue,
        findings,
    }
}

/// Classify a tool call before dispatch. Returns
/// `Some(outcome)` if the action is irreversible and the loop
/// should pause; `None` if the action is safe to execute.
///
/// This is a thin wrapper around `mew_resilience::classify_irreversible`
/// that maps the verdict to `ResilienceHookOutcome::PauseForUser`.
/// Kept as a separate function from `evaluate_page` so the agent
/// can call it at the *pre-dispatch* site (after the LLM picks
/// a tool, before execution) without re-walking the tree.
pub fn evaluate_dispatch(tool_name: &str, args: &Value) -> Option<ResilienceHookOutcome> {
    let verdict = classify_irreversible(tool_name, args)?;
    Some(ResilienceHookOutcome::PauseForUser {
        target: verdict.target,
        action_kind: verdict.action.as_str().to_string(),
    })
}

/// Score a vision result. The `description` is the LLM's
/// response, `original_box` is the bounding box the screenshot
/// covered. Returns the typed verdict the agent uses to decide
/// whether to act, re-shoot with a tighter crop, or surface a
/// "I'm not sure" message to the user.
pub fn evaluate_vision(description: &str, original_box: Option<(f64, f64, f64, f64)>) -> VisionVerdict {
    score_vision(description, original_box)
}

/// Helper: a `TreeNode` looks "dashboard-like" if it has at
/// least one `row` or `table` role (data-heavy) OR a `heading`
/// containing the word "dashboard" / "welcome". The agent
/// calls this at the end of every perception cycle to update
/// `self.prior_was_dashboard_like` for the *next* iteration's
/// session-loss check.
pub fn page_looks_dashboard_like(tree: &TreeNode) -> bool {
    fn walk(n: &TreeNode, out: &mut bool) {
        if *out {
            return;
        }
        if n.role.eq_ignore_ascii_case("row")
            || n.role.eq_ignore_ascii_case("table")
            || n.role.eq_ignore_ascii_case("grid")
        {
            *out = true;
            return;
        }
        if n.role.eq_ignore_ascii_case("heading") {
            let lower = n.name.to_lowercase();
            if lower.contains("dashboard")
                || lower.contains("welcome back")
                || lower.contains("recent activity")
            {
                *out = true;
                return;
            }
        }
        for child in &n.children {
            walk(child, out);
            if *out {
                return;
            }
        }
    }
    let mut out = false;
    walk(tree, &mut out);
    out
}

/// Convenience: log a single finding to the transcript in the
/// standard `[ts] [session] KIND: ...` shape. The agent
/// uses this from the hook so a reviewer can grep the
/// transcript for `RESILIENCE:` and see every failure-mode
/// decision in chronological order.
pub fn log_resilience_event(
    file: Option<&std::fs::File>,
    session_id: &str,
    kind: &str,
    detail: &str,
) {
    if let Some(mut f) = file {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let line = format!(
            "[{}] [{}] RESILIENCE: kind={} detail=\"{}\"\n\n",
            ts, session_id, kind, detail
        );
        let _ = std::io::Write::write_all(&mut f, line.as_bytes());
    }
    tracing::info!(
        event = "resilience_finding",
        kind = %kind,
        detail = %detail,
        "resilience finding fired"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use mew_resilience::mock_fixtures;

    #[test]
    fn clean_homepage_continues() {
        let report = evaluate_page(&mock_fixtures::clean_homepage(), false);
        assert_eq!(report.outcome, ResilienceHookOutcome::Continue);
    }

    #[test]
    fn cookie_banner_autodismisses() {
        let report = evaluate_page(&mock_fixtures::cookie_banner_page(), false);
        match report.outcome {
            ResilienceHookOutcome::AutoDismiss { dismiss_ref } => {
                assert_eq!(dismiss_ref, "@e1");
            }
            other => panic!("expected AutoDismiss, got {:?}", other),
        }
    }

    #[test]
    fn http_429_triggers_backoff() {
        let report = evaluate_page(&mock_fixtures::http_429_page(), false);
        match report.outcome {
            ResilienceHookOutcome::Backoff { secs } => {
                assert_eq!(secs, 30);
            }
            other => panic!("expected Backoff, got {:?}", other),
        }
    }

    #[test]
    fn cloudflare_triggers_backoff() {
        let report = evaluate_page(&mock_fixtures::cloudflare_page(), false);
        match report.outcome {
            ResilienceHookOutcome::Backoff { secs } => {
                assert_eq!(secs, 15);
            }
            other => panic!("expected Backoff, got {:?}", other),
        }
    }

    #[test]
    fn login_wall_after_dashboard_surfaces_finding() {
        // The dashboard->login transition: prior was
        // dashboard-like, current is a login wall.
        let report = evaluate_page(
            &mock_fixtures::login_wall_page(),
            true,
        );
        match report.outcome {
            ResilienceHookOutcome::SurfaceAsFinding { kind, .. } => {
                assert_eq!(kind, "session_loss");
            }
            other => panic!("expected SurfaceAsFinding, got {:?}", other),
        }
    }

    #[test]
    fn login_wall_with_password_strong_signal_fires_session_loss() {
        // The login_wall_page fixture has BOTH a password
        // field AND a sign-in prompt — the *strong* signal
        // in the session-loss detector fires regardless of
        // `prior_was_dashboard_like`. This is the right
        // behavior: a login form with a password field is
        // a session-loss event whether or not we know the
        // prior was a dashboard, because the user-actionable
        // hint ("Sign in to continue") is the same in both
        // cases. The LLM gets a session_loss finding and
        // decides whether to fill the form.
        let report = evaluate_page(&mock_fixtures::login_wall_page(), false);
        match report.outcome {
            ResilienceHookOutcome::SurfaceAsFinding { kind, .. } => {
                assert_eq!(kind, "session_loss");
            }
            other => panic!("expected SurfaceAsFinding (session_loss), got {:?}", other),
        }
    }

    #[test]
    fn irreversible_send_pauses() {
        let args = serde_json::json!({ "to": "@alice", "text": "hi" });
        let outcome = evaluate_dispatch("send_message", &args).unwrap();
        match outcome {
            ResilienceHookOutcome::PauseForUser { action_kind, target } => {
                assert_eq!(action_kind, "send");
                assert!(target.contains("@alice"));
            }
            other => panic!("expected PauseForUser, got {:?}", other),
        }
    }

    #[test]
    fn click_does_not_pause() {
        let args = serde_json::json!({ "ref": "@e1" });
        assert!(evaluate_dispatch("click", &args).is_none());
    }

    #[test]
    fn dashboard_like_detector_finds_dashboard() {
        assert!(page_looks_dashboard_like(&mock_fixtures::dashboard_page()));
    }

    #[test]
    fn dashboard_like_detector_rejects_clean_homepage() {
        assert!(!page_looks_dashboard_like(&mock_fixtures::clean_homepage()));
    }

    #[test]
    fn evaluate_vision_returns_verdict() {
        let v = evaluate_vision("I think this is a button", None);
        assert!(v.confidence.score < 0.5);
    }

    // -----------------------------------------------------------------
    // Phase 8: CAPTCHA / challenge tests
    // -----------------------------------------------------------------
    //
    // The captcha detector runs *after* rate-limit / session-loss /
    // modal, but the four captcha fixtures are clean challenge
    // pages that do not contain the strings the earlier detectors
    // key on. The tests below lock the wiring down so a future
    // detector re-ordering that re-introduces a "captcha page is
    // just another modal" bug is caught.

    #[test]
    fn cloudflare_turnstile_triggers_pause_for_captcha() {
        let report = evaluate_page(&mock_fixtures::cloudflare_turnstile_page(), false);
        match report.outcome {
            ResilienceHookOutcome::PauseForCaptcha { challenge } => {
                assert_eq!(challenge.kind, mew_resilience::ChallengeKind::CloudflareTurnstile);
                assert_eq!(challenge.domain_hint.as_deref(), Some("instagram.com"));
                assert!(challenge.label.contains("Cloudflare"));
                assert!(!challenge.kind.user_action_hint().is_empty());
            }
            other => panic!("expected PauseForCaptcha, got {:?}", other),
        }
    }

    #[test]
    fn recaptcha_v2_triggers_pause_for_captcha() {
        let report = evaluate_page(&mock_fixtures::recaptcha_v2_page(), false);
        match report.outcome {
            ResilienceHookOutcome::PauseForCaptcha { challenge } => {
                assert_eq!(challenge.kind, mew_resilience::ChallengeKind::RecaptchaV2);
            }
            other => panic!("expected PauseForCaptcha, got {:?}", other),
        }
    }

    #[test]
    fn recaptcha_v3_triggers_pause_for_captcha() {
        let report = evaluate_page(&mock_fixtures::recaptcha_v3_page(), false);
        match report.outcome {
            ResilienceHookOutcome::PauseForCaptcha { challenge } => {
                assert_eq!(challenge.kind, mew_resilience::ChallengeKind::RecaptchaV3);
            }
            other => panic!("expected PauseForCaptcha, got {:?}", other),
        }
    }

    #[test]
    fn hcaptcha_triggers_pause_for_captcha() {
        let report = evaluate_page(&mock_fixtures::hcaptcha_page(), false);
        match report.outcome {
            ResilienceHookOutcome::PauseForCaptcha { challenge } => {
                assert_eq!(challenge.kind, mew_resilience::ChallengeKind::Hcaptcha);
            }
            other => panic!("expected PauseForCaptcha, got {:?}", other),
        }
    }

    #[test]
    fn clean_homepage_does_not_trigger_captcha() {
        // Negative case: a normal homepage must NOT be classified
        // as a captcha. Otherwise every innocent "Verify you are
        // a human" copy in marketing pages would pause the agent.
        let report = evaluate_page(&mock_fixtures::clean_homepage(), false);
        assert!(matches!(report.outcome, ResilienceHookOutcome::Continue));
    }

    #[test]
    fn dashboard_page_does_not_trigger_captcha() {
        let report = evaluate_page(&mock_fixtures::dashboard_page(), false);
        assert!(matches!(report.outcome, ResilienceHookOutcome::Continue));
    }

    #[test]
    fn captcha_pause_outcome_carries_actionable_hint() {
        // Lock down: the PauseForCaptcha outcome's
        // `Challenge::user_action_hint` is non-empty for every
        // kind. The orchestrator hands this string to the user
        // when it pauses; an empty hint would leave the user
        // staring at a paused session with no next step.
        for fixture in [
            mock_fixtures::cloudflare_turnstile_page(),
            mock_fixtures::recaptcha_v2_page(),
            mock_fixtures::recaptcha_v3_page(),
            mock_fixtures::hcaptcha_page(),
        ] {
            let report = evaluate_page(&fixture, false);
            match report.outcome {
                ResilienceHookOutcome::PauseForCaptcha { challenge } => {
                    assert!(
                        !challenge.kind.user_action_hint().trim().is_empty(),
                        "empty hint for kind {:?}",
                        challenge.kind
                    );
                }
                _ => panic!("expected PauseForCaptcha"),
            }
        }
    }
}
