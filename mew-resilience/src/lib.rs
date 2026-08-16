//! mew v2 — Phase 6: Resilience Core.
//!
//! Background: in real-world production browser agents, "did the happy path
//! finish?" is the *least* interesting question. The interesting question is
//! "what did the agent do when the page didn't cooperate?" The 2026
//! failure-mode literature is consistent (see `docs/phase6-resilience-core.md`):
//! what actually breaks production agents is one of six classes of failure
//! that aren't even tested by happy-path benchmarks:
//!
//! 1. Selector/ref drift      — the `@eN` the model remembered no longer points
//!                                to a live element on the page.
//! 2. Modal interruptions     — a cookie banner / login wall / newsletter popup
//!                                appears on top of the page and silently eats
//!                                the first 1-3 clicks.
//! 3. Login/session loss      — the page that was a dashboard a moment ago is
//!                                now a login form, and the agent flounders.
//! 4. Rate-limit cliffs       — a 429 / Cloudflare interstitial replaces the
//!                                real page; the model treats it as "empty page".
//! 5. Irreversible actions    — the model decides to send the message / pay
//!                                the invoice / delete the file without ever
//!                                pausing to confirm with the user.
//! 6. Vision ambiguity        — a screenshot shows a region the model isn't
//!                                sure about, and it guesses.
//!
//! Each of those gets a module in this crate with a `*_fixture` and unit
//! tests so the behavior is verifiable without a live browser. The
//! integration with the live agent loop is wired in `mew-agent` (a thin
//! adapter in `agent.rs` consumes the public surface here).
//!
//! Design notes (chosen up front, applies to every module):
//!
//! * Pure-Rust where possible. The page state is a `mew_perception::TreeNode`
//!   in memory, so every detector runs in-process with no I/O and no LLM
//!   cost. This is what makes the per-mode test fixtures viable.
//! * Each detector returns a typed `ResilienceFinding` enum so the caller
//!   can branch on the mode in a single match. A free-function
//!   `categorize()` returns the *most severe* finding when several
//!   detectors fire on the same page (severity order is documented on
//!   the enum).
//! * Every detector exposes a `*_fixture()` returning a synthetic
//!   `TreeNode` so tests can drive it without Chrome. The fixture is
//!   `pub(crate)` so external tests reach it through the test API
//!   re-export below.
//! * Each module has its own unit-test mod with at least 4-6 tests
//!   covering the positive case, the negative case, the multi-modal
//!   case, and the degraded case. A separate `tests` integration
//!   test in `mew-agent/examples/` exercises the cross-module
//!   composition with mock fixtures.
//!
//! The crate does NOT depend on the live `Agent` (no circular dep, and the
//! resilience logic is testable without the LLM). The integration is a
//! small `Phase6Hook` block in `mew-agent/src/agent.rs` that calls these
//! pure functions at the right moments.

pub mod captcha;
pub mod irreversible_actions;
pub mod mock_fixtures;
pub mod modal_interrupts;
pub mod rate_limit;
pub mod ref_recovery;
pub mod session_loss;
pub mod stuck_detector;
pub mod vision_confidence;

pub use captcha::{detect as detect_captcha, Challenge, ChallengeKind};
pub use irreversible_actions::{
    classify as classify_irreversible, ActionKind, IrreverisbleVerdict,
};
pub use modal_interrupts::{detect as detect_modal, ModalKind, ModalReport};
pub use rate_limit::{detect as detect_rate_limit, RateLimitSignal};
pub use ref_recovery::{
    attempt_recovery, RefActionKind, RefRecoveryConfig, RefRecoveryInputs, RefRecoveryOutcome,
};
pub use session_loss::{detect as detect_session_loss, SessionLossInputs, SessionLossReport};
pub use stuck_detector::{
    detect_stuck, StuckDetectorConfig, StuckReason, WorkerHealth, WorkerHealthReport,
};
pub use vision_confidence::{score as score_vision, VisionConfidence, VisionVerdict};

/// A page-state finding produced by one of the six failure-mode
/// detectors. The `severity` field is a stable ordering — the agent
/// uses it to decide which finding to act on first when several fire
/// on the same iteration.
///
/// Ordering (highest first, from the perspective of "what blocks the
/// task most aggressively"):
///
///   1. RateLimit        — the server is actively refusing us; nothing
///                         else matters until we back off.
///   2. SessionLoss      — the user is logged out; we cannot proceed
///                         with the original task at all.
///   3. Modal            — there's an overlay eating the next click;
///                         dismiss-or-confirm gate must run first.
///   4. VisionAmbiguity  — we don't know what we're looking at; this
///                         is local (one region) so it's lower-priority
///                         than a page-wide overlay.
///   5. IrreversibleGate — we know what to do, but it would be
///                         irreversible; pause and ask the user.
///   6. RefDrift         — we know what to do and the ref is stale;
///                         auto-recoverable in O(1) iteration.
///   7. Captcha          — Phase 8: a challenge page was detected;
///                         pause the session and message the user
///                         (they can solve it in-window). Higher
///                         urgency than a Modal because the user
///                         *must* be the one to act.
///
/// `IrreversibleGate`, `RefDrift`, and `Captcha` are the three
/// pause-or-no-op modes; the others either auto-recover
/// (`RefDrift`, `VisionAmbiguity`) or surface the problem in the
/// chat for the user to act on (`SessionLoss`, `Modal`,
/// `RateLimit`). The integer codes are stable so callers can
/// log/track them across runs.
///
/// Note: `PartialEq` only — `f32` is not `Eq`, and the
/// `VisionAmbiguity` variant contains a `VisionConfidence { score: f32 }`.
/// Callers that need a Hash/Eq key should use `code()` instead.
#[derive(Debug, Clone, PartialEq)]
pub enum ResilienceFinding {
    /// A stale `@eN` was supplied; the agent should re-snapshot and retry.
    RefDrift(RefDriftDetail),
    /// An overlay (cookie / login wall / newsletter) is on top of the page.
    Modal(ModalReport),
    /// The page that was a dashboard is now a login form.
    SessionLoss(SessionLossReport),
    /// The page is a 429 / Cloudflare interstitial.
    RateLimit(RateLimitSignal),
    /// The next action the model wants to take is irreversible.
    IrreversibleGate(IrreverisbleVerdict),
    /// A vision result is below the confidence threshold.
    VisionAmbiguity(VisionVerdict),
    /// Phase 8: a CAPTCHA / challenge page was detected (Cloudflare
    /// Turnstile, reCAPTCHA v2/v3, hCaptcha). The default response
    /// is to pause the session and message the user so they can
    /// solve it in-window — mew runs a real visible headed
    /// browser and the human is the safest solver.
    Captcha(Challenge),
    /// Phase 26: Worker health report indicating suspected stuck or token limit approaching.
    Stuck(WorkerHealthReport),
    /// Phase 40: the navigate tool's domain allowlist check
    /// rejected the resolved URL. The agent surfaces this as a
    /// chat-visible event (per the spec) so the user can either
    /// (a) edit `config.yaml` to add the domain, or (b) check
    /// the per-vertical `required_domains` to confirm the
    /// domain should have been auto-expanded. The `hint`
    /// field carries the actionable instruction (e.g. "add
    /// 'example.com' to agent.allowed_domains in config.yaml")
    /// and the `path` field is the resolver decision string
    /// (DirectNav / SearchRedirect / SensitiveRedirect /
    /// AuthProbeRedirect / Unknown) so a post-mortem reviewer
    /// can see *why* the resolver sent the agent to a
    /// non-allowlisted domain. The `domain` is the bare
    /// hostname (no scheme, no path) that the allowlist check
    /// rejected — pre-Phase-40 this was dropped silently.
    AllowlistViolation(AllowlistViolationDetail),
}

impl ResilienceFinding {
    /// Stable integer code for each variant. Stable across runs so a
    /// regression dashboard can track "ref_drift count went from 2 to
    /// 17 today" without re-implementing the enum match.
    pub fn code(&self) -> u16 {
        match self {
            ResilienceFinding::RefDrift(_) => 1,
            ResilienceFinding::Modal(_) => 3,
            ResilienceFinding::SessionLoss(_) => 2,
            ResilienceFinding::RateLimit(_) => 0,
            ResilienceFinding::IrreversibleGate(_) => 5,
            ResilienceFinding::VisionAmbiguity(_) => 4,
            ResilienceFinding::Captcha(_) => 6,
            ResilienceFinding::Stuck(_) => 7,
            ResilienceFinding::AllowlistViolation(_) => 8,
        }
    }

    /// Short human-readable name used in transcript / trace events.
    pub fn kind_str(&self) -> &'static str {
        match self {
            ResilienceFinding::RefDrift(_) => "ref_drift",
            ResilienceFinding::Modal(_) => "modal_interruption",
            ResilienceFinding::SessionLoss(_) => "session_loss",
            ResilienceFinding::RateLimit(_) => "rate_limit",
            ResilienceFinding::IrreversibleGate(_) => "irreversible_action",
            ResilienceFinding::VisionAmbiguity(_) => "vision_ambiguity",
            ResilienceFinding::Captcha(_) => "captcha_challenge",
            ResilienceFinding::Stuck(_) => "stuck_worker",
            ResilienceFinding::AllowlistViolation(_) => "allowlist_violation",
        }
    }

    /// One-line summary for the user-facing chat reply (the agent
    /// includes this when a finding becomes the active escalation
    /// path). Kept short so the LLM's chat reply can quote it
    /// verbatim without rewriting.
    pub fn summary(&self) -> String {
        match self {
            ResilienceFinding::RefDrift(d) => {
                format!(
                    "stale ref {} (auto-recovering via fresh snapshot)",
                    d.supplied_ref
                )
            }
            ResilienceFinding::Modal(m) => {
                format!("modal overlay detected ({})", m.kind.as_str())
            }
            ResilienceFinding::SessionLoss(s) => {
                format!("session loss detected: {}", s.reason)
            }
            ResilienceFinding::RateLimit(r) => {
                format!("rate-limit signal: {}", r.label)
            }
            ResilienceFinding::IrreversibleGate(v) => {
                format!(
                    "irreversible action: {} (paused for confirmation)",
                    v.action.as_str()
                )
            }
            ResilienceFinding::VisionAmbiguity(v) => {
                format!("vision confidence too low ({:.2})", v.confidence.score)
            }
            ResilienceFinding::Captcha(c) => {
                format!("challenge detected: {}", c.label)
            }
            ResilienceFinding::Stuck(r) => {
                format!(
                    "worker health signal: suspected_stuck={}, token_limit_approaching={}",
                    r.suspected_stuck, r.token_limit_approaching
                )
            }
            ResilienceFinding::AllowlistViolation(v) => {
                // Compact one-liner: domain + the short
                // "edit config.yaml" hint. The full hint
                // text is available on the
                // `AllowlistViolationDetail` field for the
                // transcript writer; the chat-visible
                // `ProgressLine` uses the same string the
                // `summary()` returns so the user reads
                // one consistent message.
                format!(
                    "domain '{dom}' not in allowlist ({}); edit config.yaml `agent.allowed_domains`",
                    v.path,
                    dom = v.domain,
                )
            }
        }
    }

    /// Whether the finding requires the loop to pause and ask the
    /// user (vs. an automatic recovery). `IrreversibleGate` and
    /// `Captcha` both pause — the former because the action is
    /// one-way, the latter because the challenge cannot be
    /// programmatically solved without a third-party solving
    /// service (an opt-in capability, see `mew_agent::AgentConfig
    /// ::captcha`).
    pub fn requires_pause(&self) -> bool {
        matches!(
            self,
            ResilienceFinding::IrreversibleGate(_) | ResilienceFinding::Captcha(_)
        )
    }
}

/// Severity ordering: lower number = higher urgency. The `categorize`
/// helper uses this to pick the finding the agent should escalate on
/// when several fire at once.
pub fn severity_rank(finding: &ResilienceFinding) -> u8 {
    match finding {
        ResilienceFinding::RateLimit(_) => 0,
        ResilienceFinding::SessionLoss(_) => 1,
        ResilienceFinding::Modal(_) => 2,
        ResilienceFinding::VisionAmbiguity(_) => 3,
        ResilienceFinding::IrreversibleGate(_) => 4,
        ResilienceFinding::Captcha(_) => 5,
        ResilienceFinding::RefDrift(_) => 6,
        ResilienceFinding::Stuck(_) => 7,
        // Phase 40: an allowlist violation is a hard stop
        // (the LLM cannot navigate to a blocked domain no
        // matter what it does), so the severity is on par
        // with the other "this run is blocked" findings
        // (`SessionLoss` / `Captcha`). We slot it
        // between `Stuck` and `RefDrift` so the order is
        // monotonic with the doc comment above — `Stuck`
        // stays a `7`, `AllowlistViolation` takes `8`, and
        // the enum match is still exhaustive without a
        // `_ => unreachable!()` arm.
        ResilienceFinding::AllowlistViolation(_) => 8,
    }
}

/// Pick the most severe finding from a slice. Returns `None` for an
/// empty slice. Stable across the slice (same order of equal-severity
/// items: the first one wins) so a regression test can assert the
/// exact finding for a given page state without worrying about
/// ordering accidents.
pub fn categorize(findings: &[ResilienceFinding]) -> Option<ResilienceFinding> {
    findings.iter().min_by_key(|f| severity_rank(f)).cloned()
}

/// Detail for the `RefDrift` variant. Carries the original ref the
/// model tried to act on so a trace log can connect "ref X was stale"
/// to the exact LLM call that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefDriftDetail {
    pub supplied_ref: String,
    pub reason: &'static str,
}

/// Phase 40: detail for the `AllowlistViolation` variant.
/// Carries the bare hostname the allowlist rejected, the
/// resolver decision string (DirectNav / SearchRedirect /
/// SensitiveRedirect / AuthProbeRedirect / Unknown), and
/// the full actionable hint the navigate handler wrote into
/// the tool result. The hint is the load-bearing field — the
/// chat-visible `ProgressLine` consumes it verbatim so the
/// user sees the same text in the live progress sub-list and
/// in the LLM's next-turn tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowlistViolationDetail {
    /// Bare hostname the allowlist rejected. No scheme, no
    /// port, no path. e.g. `"notallowed.example.com"`.
    pub domain: String,
    /// The resolver decision string — see
    /// `mew_nav::ResolutionPath::as_str()`. Stored as a
    /// `String` (not `&'static str`) so the navigate
    /// handler doesn't have to thread the lifetime of the
    /// `ResolutionPath` enum through the resilience
    /// finding. The variants are a small fixed set, so
    /// the `String` cost is negligible.
    pub path: String,
    /// The full hint string the navigate handler wrote
    /// into the tool result. Consumed verbatim by the
    /// `AgentEvent::ProgressLine` emit. Pre-Phase-40 the
    /// tool result was a generic "Do not attempt this
    /// domain"; the new text is the spec's hardened
    /// error: "add '<domain>' to the
    /// agent.allowed_domains list in config.yaml...".
    pub hint: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_orders_match_spec() {
        // Lock the severity ordering down. The doc comment on
        // `severity_rank` is the source of truth; this test catches
        // accidental re-orderings.
        let r = ResilienceFinding::RateLimit(RateLimitSignal {
            label: "test".into(),
            kind: rate_limit::RateLimitKind::Http429,
        });
        let s = ResilienceFinding::SessionLoss(SessionLossReport {
            reason: "test".into(),
            hint: "test".into(),
        });
        let m = ResilienceFinding::Modal(ModalReport {
            kind: ModalKind::CookieBanner,
            dismiss_ref: None,
        });
        let v = ResilienceFinding::VisionAmbiguity(VisionVerdict {
            confidence: VisionConfidence { score: 0.1 },
            description: "x".into(),
            tighten_crop: None,
        });
        let i = ResilienceFinding::IrreversibleGate(IrreverisbleVerdict {
            action: ActionKind::Send,
            target: "x".into(),
        });
        let c = ResilienceFinding::Captcha(Challenge {
            kind: ChallengeKind::CloudflareTurnstile,
            domain_hint: None,
            label: "test".into(),
        });
        let d = ResilienceFinding::RefDrift(RefDriftDetail {
            supplied_ref: "@e1".into(),
            reason: "stale",
        });
        assert!(severity_rank(&r) < severity_rank(&s));
        assert!(severity_rank(&s) < severity_rank(&m));
        assert!(severity_rank(&m) < severity_rank(&v));
        assert!(severity_rank(&v) < severity_rank(&i));
        assert!(severity_rank(&i) < severity_rank(&c));
        assert!(severity_rank(&c) < severity_rank(&d));
    }

    #[test]
    fn categorize_picks_highest_severity() {
        let findings = vec![
            ResilienceFinding::RefDrift(RefDriftDetail {
                supplied_ref: "@e1".into(),
                reason: "stale",
            }),
            ResilienceFinding::RateLimit(RateLimitSignal {
                label: "x".into(),
                kind: rate_limit::RateLimitKind::Http429,
            }),
            ResilienceFinding::Modal(ModalReport {
                kind: ModalKind::CookieBanner,
                dismiss_ref: None,
            }),
        ];
        let top = categorize(&findings).unwrap();
        assert!(matches!(top, ResilienceFinding::RateLimit(_)));
    }

    #[test]
    fn categorize_empty_returns_none() {
        assert!(categorize(&[]).is_none());
    }

    #[test]
    fn code_and_kind_str_are_stable() {
        // Regression guard: a future refactor of the enum must keep
        // these strings/codes stable so the trace dashboards don't
        // silently break.
        let f = ResilienceFinding::RateLimit(RateLimitSignal {
            label: "x".into(),
            kind: rate_limit::RateLimitKind::Http429,
        });
        assert_eq!(f.code(), 0);
        assert_eq!(f.kind_str(), "rate_limit");
    }

    #[test]
    fn pause_contract_lists_every_pause_variant() {
        // Regression guard: any future variant that *also* requires
        // pause should be added here explicitly so the contract
        // is reviewable. Phase 8: `Captcha` joins `IrreversibleGate`
        // in the pause-required set — the challenge cannot be
        // programmatically solved by the agent without a third-party
        // solving service (an opt-in capability), and the default
        // is to hand off to the human in the visible browser.
        let cases = vec![
            (
                ResilienceFinding::RateLimit(RateLimitSignal {
                    label: "x".into(),
                    kind: rate_limit::RateLimitKind::Http429,
                }),
                false,
            ),
            (
                ResilienceFinding::IrreversibleGate(IrreverisbleVerdict {
                    action: ActionKind::Send,
                    target: "x".into(),
                }),
                true,
            ),
            (
                ResilienceFinding::Captcha(Challenge {
                    kind: ChallengeKind::RecaptchaV2,
                    domain_hint: Some("example.com".into()),
                    label: "x".into(),
                }),
                true,
            ),
        ];
        for (f, expected) in cases {
            assert_eq!(f.requires_pause(), expected, "for {:?}", f);
        }
    }
}
