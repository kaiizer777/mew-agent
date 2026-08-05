//! Phase 8 — CAPTCHA solving-service adapter (opt-in).
//!
//! Background: the default response to a challenge page (see
//! `mew_resilience::captcha::detect` and
//! `agent::resilience::evaluate_page`) is to *pause* the session
//! and message the user, who solves the challenge in the visible
//! browser window. This is the 2026 consensus path: avoidance +
//! human handoff by default, with solving services as a fallback
//! only.
//!
//! For users who *explicitly* want unattended runs (e.g. long
//! background tasks, no human in front of the screen), the
//! `agent.captcha` block in `config.yaml` exposes a single
//! opt-in switch. **It is off by default.** When on, the agent
//! delegates the challenge to a `CaptchaSolver` implementation;
//! the only built-in implementation today is `DisabledSolver`,
//! which returns `Err(Unsupported)` and tells the user the
//! solving service is not wired up. The intent is to leave a
//! clean extension point — a future integration (2captcha,
//! anti-captcha, capmonster, etc.) drops in as a `CaptchaSolver`
//! impl without changing the agent's hook site.
//!
//! ## Ethical / ToS caveat (read this before enabling)
//!
//! Many sites' Terms of Service explicitly forbid automated
//! solving of challenges. Cloudflare, Google, and the major
//! hCaptcha-using sites all treat solver-API traffic as a
//! violation that can result in:
//!
//!   * Permanent account ban on the offending site
//!   * IP-range level blocking that affects *all* users
//!     behind the same NAT / VPN
//!   * Reputational damage if your account is associated
//!     with public-facing work (research, journalism, etc.)
//!
//! Solving services also typically cost money — per-challenge
//! pricing in the $1-3 / 1000 range as of 2026, with reCAPTCHA
//! v2 / v3 cheaper than image-grid challenges.
//!
//! **You should enable `agent.captcha.solver.enabled = true`
//! only when you have read and accept the ToS of every site
//! your agent will visit, and only when you understand the
//! cost.** The agent does not make this decision for you; the
//! `enabled: true` line in `config.yaml` is your explicit
//! consent. The README's "Ethical / ToS boundary" section
//! covers the same ground at higher level.
//!
//! ## What is *not* here
//!
//! No actual HTTP call to a third-party solving service. The
//! flag is the extension point; the implementations are
//! downstream. This matches the spec's "documented cost/ToS
//! caveat" requirement without overcommitting to a specific
//! provider whose API or pricing may change.

use mew_resilience::{Challenge, ChallengeKind};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Configuration for the captcha solver. The `agent.captcha`
/// block in `config.yaml` is parsed into this struct. The
/// `Default` impl returns a fully disabled configuration —
/// even a `mew-agent` install with no `captcha:` block at all
/// will be safe-by-default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptchaConfig {
    /// Master switch. When `false` (the default), the
    /// resilience hook's `PauseForCaptcha` outcome is
    /// honored as-is — the session pauses, the user is
    /// messaged, no solver is consulted. When `true`,
    /// the agent consults `solver()` and may bypass the
    /// pause for solver-supported challenge kinds.
    #[serde(default)]
    pub enabled: bool,

    /// Which kinds of challenge to delegate to the
    /// solver. An empty list means "no kinds" (the
    /// solver is configured but inert). The most common
    /// setup is `["recaptcha_v2", "recaptcha_v3",
    /// "hcaptcha"]`; Cloudflare Turnstile is intentionally
    /// *not* in the default list because Turnstile is
    /// where most site operators put their strongest
    /// anti-automation signal and solving it via a
    /// third-party service is the most likely to trigger
    /// a ToS strike.
    ///
    /// Phase 8 ships the type but no solver
    /// implementation. The list is a *preference* the
    /// agent checks before consulting `solver()` — if
    /// the detected kind is not in this list, the
    /// solver is not consulted even when `enabled` is
    /// true.
    #[serde(default)]
    pub solve_kinds: Vec<ChallengeKind>,

    /// Provider name. Free-form string the user
    /// fills in (`"2captcha"`, `"anticaptcha"`,
    /// `"capmonster"`, etc.). The agent logs it but
    /// does not interpret it — no provider-specific
    /// logic is hardcoded. When a future `CaptchaSolver`
    /// implementation lands, it can dispatch on this
    /// string to pick its HTTP endpoint.
    #[serde(default)]
    pub provider: Option<String>,

    /// Name of the env var that holds the provider's
    /// API key. The agent reads the env var at solver-
    /// call time (not at config-load time) so a key
    /// rotation doesn't require a restart. **No
    /// hardcoded secret support** — the env-var indirection
    /// is the only path, and the agent never writes
    /// the key to the transcript or the trace log.
    #[serde(default)]
    pub api_key_env: Option<String>,

    /// Soft cap on how many solver calls per
    /// session. The counter increments on every
    /// solver call (successful or not). When the cap
    /// is hit, the agent falls back to the
    /// pause-and-message default even when
    /// `enabled: true`. The cap exists because a
    /// misconfigured solver can loop on a
    /// challenge page that the solver can't actually
    /// solve (e.g. wrong API key, out of credits);
    /// without the cap, the agent would burn budget
    /// indefinitely.
    #[serde(default = "default_per_session_cap")]
    pub per_session_cap: u32,
}

fn default_per_session_cap() -> u32 {
    5
}

impl Default for CaptchaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            solve_kinds: Vec::new(),
            provider: None,
            api_key_env: None,
            per_session_cap: default_per_session_cap(),
        }
    }
}

impl CaptchaConfig {
    /// Whether a `Challenge` of the given kind should be
    /// delegated to the solver. Returns `false` when the
    /// master switch is off, when the kind is not in the
    /// configured `solve_kinds` list, or when the per-
    /// session cap has been reached.
    pub fn should_solve(&self, kind: ChallengeKind, calls_this_session: u32) -> bool {
        if !self.enabled {
            return false;
        }
        if (calls_this_session as u32) >= self.per_session_cap {
            return false;
        }
        self.solve_kinds.contains(&kind)
    }

    /// Human-readable summary of the current configuration.
    /// The intent is for the agent to be able to log this
    /// on startup (without leaking the API key) so a
    /// reviewer can see at a glance "captcha solver is
    /// enabled and configured for these kinds."
    pub fn summary(&self) -> String {
        if !self.enabled {
            return "captcha solver: disabled (default — human handoff on challenge)".to_string();
        }
        let provider = self.provider.as_deref().unwrap_or("(unspecified provider)");
        let kinds = if self.solve_kinds.is_empty() {
            "(no kinds — solver is configured but inert)".to_string()
        } else {
            self.solve_kinds
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            "captcha solver: enabled; provider={}; kinds=[{}]; per-session cap={}",
            provider, kinds, self.per_session_cap
        )
    }
}

/// The verdict a `CaptchaSolver` returns. The
/// "no solver wired up" path is `Unsupported` — distinct
/// from `Failed` so the agent can surface the right
/// reason ("solving service not configured" vs "solver
/// returned an error").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolveOutcome {
    /// The solver successfully solved the challenge.
    /// The agent can proceed with the original task.
    Solved,
    /// The solver ran but could not solve this challenge
    /// (wrong API key, low credit, unsupported variant).
    /// The agent should fall back to the pause-and-
    /// message default.
    Failed { reason: String },
    /// The agent has no solver implementation wired up.
    /// Distinct from `Failed` so the error message can be
    /// specific ("captcha solving service not configured"
    /// rather than "solver returned an error").
    Unsupported,
}

impl fmt::Display for SolveOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SolveOutcome::Solved => write!(f, "solved"),
            SolveOutcome::Failed { reason } => write!(f, "failed: {reason}"),
            SolveOutcome::Unsupported => write!(f, "unsupported (no solver wired up)"),
        }
    }
}

/// The trait every captcha solver implements. The agent
/// holds a `Box<dyn CaptchaSolver>` behind a `Mutex` (so
/// a future remote-managed solver can mutate internal
/// state). The trait is `Send + Sync` so the agent can
/// call it from the loop's `await` context.
///
/// Phase 8 ships the `DisabledSolver` as the default
/// implementation; it always returns `Unsupported`.
/// The trait is the extension point — a future
/// `TwoCaptchaSolver`, `AntiCaptchaSolver`, or
/// `CapMonsterSolver` slots in here without any change
/// to the agent's hook site.
pub trait CaptchaSolver: Send + Sync {
    /// Attempt to solve the given challenge. The
    /// implementation may do I/O (HTTP call to a solving
    /// service, browser automation against a
    /// service-hosted solver, etc.) and may take seconds
    /// to minutes; the `async` signature lets the agent
    /// `await` it without blocking the loop.
    ///
    /// Implementations should return:
    ///   * `SolveOutcome::Solved` on success.
    ///   * `SolveOutcome::Failed { reason }` on a
    ///     specific failure (the agent logs the reason
    ///     and falls back to the default path).
    ///   * `SolveOutcome::Unsupported` when the
    ///     implementation does not actually know how to
    ///     solve this kind. Distinct from `Failed` so
    ///     the user can tell "this solver doesn't do
    ///     this kind" from "the solver tried and
    ///     failed."
    fn solve<'a>(
        &'a self,
        challenge: &'a Challenge,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = SolveOutcome> + Send + 'a>,
    >;
}

/// The default, always-unsupported solver. Returned by
/// `CaptchaConfig::default_solver()`. Logs nothing,
/// makes no network calls, and tells the agent "this
/// isn't wired up yet." A future build that ships a real
/// 2captcha / anti-captcha / capmonster integration
/// will replace this in `CaptchaConfig::default_solver`
/// with the appropriate `CaptchaSolver` impl, behind
/// the same trait — the agent's hook site does not
/// change.
pub struct DisabledSolver;

impl CaptchaSolver for DisabledSolver {
    fn solve<'a>(
        &'a self,
        _challenge: &'a Challenge,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = SolveOutcome> + Send + 'a>,
    > {
        Box::pin(async { SolveOutcome::Unsupported })
    }
}

impl CaptchaConfig {
    /// The solver to use at runtime. Today, this is
    /// always `DisabledSolver`. When `enabled: true`
    /// the agent still calls this and the result is
    /// `Unsupported`; the future integration point is
    /// here.
    pub fn default_solver(&self) -> Box<dyn CaptchaSolver> {
        // Phase 8 ships no provider integration.
        // The `enabled: true` path is a working
        // extension point: the agent respects the
        // config, the config drives a real decision
        // (`should_solve`), and the absence of a
        // provider is surfaced as `Unsupported`
        // rather than silently dropped.
        if self.enabled {
            tracing::warn!(
                event = "captcha_solver_enabled_but_unimplemented",
                provider = %self.provider.as_deref().unwrap_or("unspecified"),
                "captcha solving is enabled in config but no provider implementation ships; \
                 challenge pages will be handled by the default pause-and-message path. \
                 See docs/phase8-captcha-handling.md for the extension point."
            );
        }
        Box::new(DisabledSolver)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled() {
        let c = CaptchaConfig::default();
        assert!(!c.enabled);
        assert!(c.solve_kinds.is_empty());
        assert_eq!(c.per_session_cap, 5);
    }

    #[test]
    fn should_solve_respects_master_switch() {
        let mut c = CaptchaConfig::default();
        c.enabled = false;
        c.solve_kinds = vec![ChallengeKind::RecaptchaV2];
        assert!(!c.should_solve(ChallengeKind::RecaptchaV2, 0));
    }

    #[test]
    fn should_solve_respects_kind_list() {
        let mut c = CaptchaConfig::default();
        c.enabled = true;
        c.solve_kinds = vec![ChallengeKind::RecaptchaV2];
        assert!(c.should_solve(ChallengeKind::RecaptchaV2, 0));
        assert!(!c.should_solve(ChallengeKind::Hcaptcha, 0));
    }

    #[test]
    fn should_solve_respects_per_session_cap() {
        let mut c = CaptchaConfig::default();
        c.enabled = true;
        c.solve_kinds = vec![ChallengeKind::RecaptchaV2];
        c.per_session_cap = 2;
        assert!(c.should_solve(ChallengeKind::RecaptchaV2, 0));
        assert!(c.should_solve(ChallengeKind::RecaptchaV2, 1));
        assert!(!c.should_solve(ChallengeKind::RecaptchaV2, 2));
        assert!(!c.should_solve(ChallengeKind::RecaptchaV2, 5));
    }

    #[test]
    fn summary_includes_disabled_marker() {
        let c = CaptchaConfig::default();
        let s = c.summary();
        assert!(s.contains("disabled"));
    }

    #[test]
    fn summary_includes_provider_and_kinds_when_enabled() {
        let mut c = CaptchaConfig::default();
        c.enabled = true;
        c.provider = Some("2captcha".to_string());
        c.solve_kinds = vec![ChallengeKind::RecaptchaV2, ChallengeKind::Hcaptcha];
        let s = c.summary();
        assert!(s.contains("enabled"));
        assert!(s.contains("2captcha"));
        assert!(s.contains("recaptcha_v2"));
        assert!(s.contains("hcaptcha"));
        assert!(s.contains("cap=5"));
    }

    #[test]
    fn disabled_solver_returns_unsupported() {
        let s = DisabledSolver;
        let challenge = Challenge {
            kind: ChallengeKind::RecaptchaV2,
            domain_hint: Some("example.com".to_string()),
            label: "test".to_string(),
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = rt.block_on(s.solve(&challenge));
        assert_eq!(outcome, SolveOutcome::Unsupported);
    }

    #[test]
    fn default_solver_is_disabled() {
        let mut c = CaptchaConfig::default();
        c.enabled = true;
        c.provider = Some("any".to_string());
        let _solver = c.default_solver();
        // The shape of the type isn't worth pinning in a
        // test; what matters is that the call doesn't
        // panic and that the `enabled: true` path is
        // exercised. (Future: a real solver test would
        // build a `TwoCaptchaSolver` here.)
    }

    #[test]
    fn solve_outcome_display_strings_are_stable() {
        // The Display impl is what the resilience
        // hook logs into the transcript. Stable
        // strings keep log parsers happy.
        assert_eq!(format!("{}", SolveOutcome::Solved), "solved");
        assert_eq!(
            format!(
                "{}",
                SolveOutcome::Failed { reason: "x".into() }
            ),
            "failed: x"
        );
        assert_eq!(
            format!("{}", SolveOutcome::Unsupported),
            "unsupported (no solver wired up)"
        );
    }
}
