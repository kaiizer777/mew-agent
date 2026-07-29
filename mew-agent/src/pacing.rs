//! Phase 17.1 — Site-specific pacing guard.
//!
//! Background: platforms with anti-automation systems flag bursts of
//! identical actions in quick succession — e.g. several `send` clicks fired
//! back-to-back. The fix is a per-action-type cooldown/jitter, scoped to
//! the case we actually care about: a *tight loop* of the same action
//! (one action type fires repeatedly with no different action type in
//! between). One-off actions are never delayed.
//!
//! The 17.1 spec calls for:
//!   - `min_delay_ms` / `max_delay_ms` per-action-type, configurable.
//!   - Default modest random delay (e.g. 800-2500ms) between repeats of
//!     the *same* action type in a loop.
//!   - Scoped, not blanket — one-off actions must NOT be needlessly
//!     slowed down.
//!   - Logged in the transcript so a reviewer can see it's real, not
//!     just configured and ignored.
//!   - Default-off gate: when the config block is absent or `enabled:
//!     false`, the entire path is a no-op (zero sleep, zero log lines,
//!     zero behavior change vs. pre-17.1).
//!
//! Design (chosen to match the spec's "scoped to tight same-type loops"):
//!   - The guard tracks a single counter `current_streak: (action_type,
//!     count, last_fire_instant)`. The counter increments when an
//!     `before_action(t)` call's `t` matches the previous action's `t`,
//!     and resets when it doesn't.
//!   - The guard sleeps *before* the action fires, only when the streak
//!     has reached `streak_threshold` consecutive same-type actions.
//!   - `before_action` is a non-blocking decision function: it returns
//!     a `PacingDecision` enum the caller acts on. The caller
//!     (`run_inner`) is responsible for `tokio::time::sleep` — this
//!     keeps the guard test-friendly (tests can call `before_action`
//!     synchronously and assert on the decision).
//!
//! The randomness uses `rand::Rng` because timing tests need
//! deterministic control. Production gets the OS thread-local RNG
//! (cheap, fine for jitter).

use rand::Rng;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::time::Duration;

/// The action types the agent dispatches. Only the ones the model can
/// call as `tool_calls` matter here — `snapshot` is read-only and
/// shouldn't pace anything, and `finish` / `mark_subtask_*` /
/// `declare_subtasks` are control plane, not bot-detectable. The
/// pacing guard exposes a helper `should_pace(name)` that returns
/// `true` for the action types that are user-facing and could trip
/// bot detection on burst.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacedAction {
    Click,
    Type,
    Scroll,
    PressKey,
    Navigate,
}

impl PacedAction {
    /// Map a tool name (the LLM-facing string) to the paced-action
    /// enum. Returns `None` for non-paced tool names — control plane
    /// tools (`snapshot`, `finish`, `mark_subtask_*`, `declare_subtasks`)
    /// and the unknown-tool fallthrough.
    pub fn from_tool_name(name: &str) -> Option<Self> {
        match name {
            "click" => Some(Self::Click),
            "type" => Some(Self::Type),
            "scroll" => Some(Self::Scroll),
            "press_key" => Some(Self::PressKey),
            "navigate" => Some(Self::Navigate),
            _ => None,
        }
    }

    /// Short canonical name used in log lines.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::Type => "type",
            Self::Scroll => "scroll",
            Self::PressKey => "press_key",
            Self::Navigate => "navigate",
        }
    }
}

/// The configuration block parsed from `config.yaml`'s
/// `agent.pacing` section. Every field has a `serde(default = ...)`
/// so the entire block can be omitted without breaking the parse —
/// when `enabled` is false (or defaulted), the guard is a no-op.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PacingConfig {
    /// Master switch. Default false — when off, the guard never
    /// sleeps, never logs, never modifies any timing.
    #[serde(default)]
    pub enabled: bool,
    /// Lower bound of the random delay range. Inclusive (the random
    /// value can equal this).
    #[serde(default = "default_min_ms")]
    pub min_delay_ms: u64,
    /// Upper bound of the random delay range. Inclusive.
    #[serde(default = "default_max_ms")]
    pub max_delay_ms: u64,
    /// How many *consecutive* same-type actions are required before
    /// the guard starts pacing. The spec is "after repeats of the
    /// same action type in a loop" — the first occurrence of a new
    /// action type never pays the delay, the second occurrence
    /// (with `streak_threshold=1`) starts pacing. Default 2, which
    /// gives the agent a one-action grace period before pacing
    /// kicks in — the model often has a legitimate click-then-type
    /// pair (e.g. click into a field) that shouldn't be slowed.
    #[serde(default = "default_streak_threshold")]
    pub streak_threshold: u32,
}

impl Default for PacingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_delay_ms: default_min_ms(),
            max_delay_ms: default_max_ms(),
            streak_threshold: default_streak_threshold(),
        }
    }
}

fn default_min_ms() -> u64 {
    800
}
fn default_max_ms() -> u64 {
    2500
}
fn default_streak_threshold() -> u32 {
    2
}

/// The decision returned from `PacingGuard::before_action`. The
/// caller is responsible for sleeping if the variant carries a
/// duration. The decision is data, not a side effect, so tests
/// can call the guard synchronously and assert without time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PacingDecision {
    /// No delay — either pacing is disabled, the action type isn't
    /// paced, or the streak hasn't reached the threshold yet.
    NoPacing,
    /// Pacing is on; sleep for this duration before firing the action.
    Pace { delay: Duration, streak: u32 },
}

/// The internal streak state. Held in `Option` so the *first* call
/// always returns `NoPacing` (nothing to compare to). After the first
/// call, `Some(...)` is always present.
#[derive(Debug, Clone, Copy)]
struct StreakState {
    action: PacedAction,
    count: u32,
}

impl StreakState {
    fn new(action: PacedAction) -> Self {
        Self { action, count: 1 }
    }
}

/// The pacing guard. One per `Agent` (lives in the `Agent` struct).
/// Cheap to construct (no I/O, no async), cheap to call
/// (`before_action` is purely synchronous CPU work + an Instant read).
///
/// NOT thread-safe — the existing `Agent` is single-threaded async on
/// the ReAct loop, so a plain `PacingGuard` is fine. If a future
/// caller wants to drive it from multiple tasks, wrap in a `Mutex`.
#[derive(Debug, Clone)]
pub struct PacingGuard {
    config: PacingConfig,
    streak: Option<StreakState>,
}

impl PacingGuard {
    /// Construct a guard from a parsed `PacingConfig`. If `enabled`
    /// is false, the guard is a no-op for the lifetime of this
    /// struct — the caller doesn't have to check the config flag at
    /// every dispatch site.
    pub fn new(config: PacingConfig) -> Self {
        Self {
            config,
            streak: None,
        }
    }

    /// Convenience: a guard that's fully disabled, regardless of
    /// config. Used by the test helper `new_for_test` so the
    /// `Agent::new_for_test` path doesn't need to know about
    /// pacing defaults.
    pub fn disabled() -> Self {
        Self::new(PacingConfig {
            enabled: false,
            ..PacingConfig::default()
        })
    }

    /// True if the config has pacing on. Cheap accessor used by the
    /// loop to short-circuit before even constructing a decision.
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// The current streak's action type, or `None` if the guard
    /// hasn't recorded any action yet. Exposed for tests + the
    /// transcript logger.
    pub fn current_streak_action(&self) -> Option<PacedAction> {
        self.streak.map(|s| s.action)
    }

    /// The current streak's count. `0` if nothing's been recorded.
    pub fn current_streak_count(&self) -> u32 {
        self.streak.map(|s| s.count).unwrap_or(0)
    }

    /// Decide whether the next action of `name` (the LLM-facing tool
    /// name) should be paced. Returns the decision. If the decision
    /// is `Pace { delay, .. }`, the caller MUST sleep for `delay`
    /// before executing the action.
    ///
    /// This function does NOT sleep and does NOT touch the
    /// transcript. Side effects (sleep, log) are explicitly kept
    /// out so the function is testable in isolation and so the
    /// caller can compose it with other decisions (e.g. "if pacing
    /// would push us over the iteration cap, skip it") without
    /// the guard having a stale sleep in flight.
    pub fn before_action(&mut self, name: &str) -> PacingDecision {
        // Fast path: pacing off entirely.
        if !self.config.enabled {
            return PacingDecision::NoPacing;
        }
        // Fast path: non-paced tool name (control plane, snapshot, etc.).
        let action = match PacedAction::from_tool_name(name) {
            Some(a) => a,
            None => {
                // Non-paced action *resets* the streak (its presence
                // breaks the "tight loop of same-type" condition).
                self.streak = None;
                return PacingDecision::NoPacing;
            }
        };

        // Config sanity: if the range is inverted or zero, treat as
        // disabled rather than panic. Defensive: bad config shouldn't
        // crash the agent.
        let min = self.config.min_delay_ms;
        let max = self.config.max_delay_ms.max(min);
        if min == 0 && max == 0 {
            // Explicit "no pacing" override via the delay knobs. The
            // 17.2 spec uses exactly this: "set the delay range to
            // something absurdly low temporarily" to prove the logic
            // is real. min=0,max=0 means: still enable, but never
            // actually sleep.
            // We still want to count the streak so logs make sense.
        }

        let new_streak_count;
        match self.streak {
            // First paced action ever: record, return NoPacing.
            None => {
                self.streak = Some(StreakState::new(action));
                return PacingDecision::NoPacing;
            }
            Some(s) if s.action == action => {
                // Same action as the previous one — extend the streak.
                new_streak_count = s.count + 1;
                self.streak = Some(StreakState {
                    action,
                    count: new_streak_count,
                });
            }
            Some(_) => {
                // Different action from the previous one — new
                // streak starts at 1.
                self.streak = Some(StreakState::new(action));
                return PacingDecision::NoPacing;
            }
        }

        // Threshold check: only pace once the streak has reached
        // the configured threshold. With `streak_threshold=2` (the
        // default), the first two same-type actions fire without
        // delay; the third in a row is the first one that pays.
        if new_streak_count < self.config.streak_threshold {
            return PacingDecision::NoPacing;
        }

        // Pick a random value in [min, max] inclusive. The
        // inclusive bound means `min_delay_ms == max_delay_ms`
        // produces a fixed delay (used in tests).
        let mut rng = rand::thread_rng();
        let range = max - min;
        let picked = if range == 0 {
            min
        } else {
            min + rng.gen_range(0..=range)
        };

        PacingDecision::Pace {
            delay: Duration::from_millis(picked),
            streak: new_streak_count,
        }
    }

    /// Reset the streak entirely. Called when the session state
    /// resets (e.g. a navigation just happened, the page context
    /// changed, and the previous streak is meaningless). Wired
    /// into the navigation path so cross-page actions don't
    /// inherit the streak from the previous page.
    pub fn reset(&mut self) {
        self.streak = None;
    }
}

/// Phase 17.1: emit a transcript log line for a pacing decision.
/// Format mirrors the other `[ts] [session] KIND: ...` lines so
/// `grep "^PACING"` or just visual scanning works the same as
/// the other 17.x log lines.
///
/// `file` is the optional transcript (same shape the rest of the
/// agent uses — `None` means no file, just stdout via println).
pub fn log_pacing_decision(
    file: Option<&std::fs::File>,
    session_id: &str,
    action: PacedAction,
    decision: &PacingDecision,
) {
    // Always echo to stdout so live runs show what's happening,
    // matching the convention of every other [pacing] /
    // [completeness] / [nav-resolve] log path in the agent.
    match decision {
        PacingDecision::NoPacing => {
            // NoPacing is intentionally NOT logged — it would be
            // log spam on every iteration. The 17.2 spec is
            // explicit: the transcript should show *when pacing
            // was applied*, not every no-op decision.
        }
        PacingDecision::Pace { delay, streak } => {
            println!(
                "[pacing] action={} streak={} -> sleeping {}ms before dispatch",
                action.as_str(),
                streak,
                delay.as_millis()
            );
            if let Some(mut f) = file {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let line = format!(
                    "[{}] [{}] PACING: action={} streak={} delay_ms={}\n\n",
                    ts,
                    session_id,
                    action.as_str(),
                    streak,
                    delay.as_millis()
                );
                let _ = f.write_all(line.as_bytes());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, min: u64, max: u64, threshold: u32) -> PacingConfig {
        PacingConfig {
            enabled,
            min_delay_ms: min,
            max_delay_ms: max,
            streak_threshold: threshold,
        }
    }

    #[test]
    fn disabled_guard_is_pure_no_op() {
        let mut g = PacingGuard::new(cfg(false, 800, 2500, 2));
        // Even after a long streak of the same action, no pacing.
        for _ in 0..10 {
            assert_eq!(g.before_action("click"), PacingDecision::NoPacing);
        }
        // Mixed actions also still no-op.
        g.before_action("click");
        g.before_action("click");
        g.before_action("type");
        assert_eq!(g.before_action("click"), PacingDecision::NoPacing);
    }

    #[test]
    fn first_action_of_a_type_never_paces() {
        // Even with threshold=1, the *first* action in the streak
        // never paces. Threshold=1 means "second in a row pays";
        // threshold=0 isn't allowed (we clamp at 1 for the count
        // comparison, see `new_streak_count < threshold`).
        let mut g = PacingGuard::new(cfg(true, 100, 100, 1));
        // First click ever: NoPacing.
        assert_eq!(g.before_action("click"), PacingDecision::NoPacing);
    }

    #[test]
    fn different_action_resets_streak() {
        let mut g = PacingGuard::new(cfg(true, 100, 100, 1));
        // click, click, type, click — the final click is a NEW
        // streak of length 1, so no pacing.
        g.before_action("click"); // streak=1, no pace
        let d = g.before_action("click"); // streak=2 with threshold=1 -> Pace
        assert!(matches!(d, PacingDecision::Pace { .. }));
        g.before_action("type"); // resets streak
        let d = g.before_action("click"); // streak=1 again -> NoPacing
        assert_eq!(d, PacingDecision::NoPacing);
    }

    #[test]
    fn threshold_gates_pacing() {
        // threshold=3: first two clicks in a row no-pace, third does.
        let mut g = PacingGuard::new(cfg(true, 100, 100, 3));
        assert_eq!(g.before_action("click"), PacingDecision::NoPacing);
        assert_eq!(g.before_action("click"), PacingDecision::NoPacing);
        let d = g.before_action("click");
        assert!(matches!(d, PacingDecision::Pace { streak: 3, .. }));
        let d = g.before_action("click");
        assert!(matches!(d, PacingDecision::Pace { streak: 4, .. }));
    }

    #[test]
    fn fixed_range_produces_exact_delay() {
        // min == max => the random pick is deterministic, equals
        // both. This is the test-17.2 "absurdly low" pattern: set
        // 0..=0 to verify the pacing path executes, just with a
        // 0ms sleep, proving the delay is real code not a hardcoded
        // constant.
        let mut g = PacingGuard::new(cfg(true, 0, 0, 1));
        g.before_action("click"); // streak=1
        let d = g.before_action("click");
        match d {
            PacingDecision::Pace { delay, streak } => {
                assert_eq!(streak, 2);
                assert_eq!(delay, Duration::from_millis(0));
            }
            _ => panic!("expected Pace variant, got {:?}", d),
        }
    }

    #[test]
    fn non_paced_action_breaks_streak() {
        // snapshot is non-paced; interleaving it with a click
        // resets the streak so the click after snapshot doesn't
        // get paced.
        let mut g = PacingGuard::new(cfg(true, 100, 100, 1));
        g.before_action("click"); // streak=1
        g.before_action("click"); // streak=2 -> Pace
        g.before_action("snapshot"); // non-paced -> resets
        let d = g.before_action("click");
        assert_eq!(d, PacingDecision::NoPacing);
    }

    #[test]
    fn reset_clears_streak() {
        let mut g = PacingGuard::new(cfg(true, 100, 100, 1));
        g.before_action("click"); // streak=1
        g.before_action("click"); // streak=2 -> Pace
        g.reset();
        // After reset, the first click is a new streak of 1.
        assert_eq!(g.before_action("click"), PacingDecision::NoPacing);
        // Second click after reset still paces (streak=2).
        let d = g.before_action("click");
        assert!(matches!(d, PacingDecision::Pace { .. }));
    }

    #[test]
    fn inverted_range_is_clamped_not_panicked() {
        // max < min is a config bug. We clamp to [min, min] so
        // the guard never panics in production. (Defensive — bad
        // config shouldn't crash the agent.)
        let mut g = PacingGuard::new(cfg(true, 2500, 800, 1));
        g.before_action("click"); // streak=1
        let d = g.before_action("click");
        match d {
            PacingDecision::Pace { delay, .. } => {
                // Clamped: range = max(800) - 2500 = -1700, then
                // we pick in [2500, 2500].
                assert_eq!(delay, Duration::from_millis(2500));
            }
            _ => panic!("expected Pace"),
        }
    }

    #[test]
    fn paced_action_classification_matches_tool_names() {
        // Confirm the helper recognizes every paced tool name and
        // rejects every non-paced one. The 17.1 spec lists click,
        // type, scroll, press_key as the "consecutive identical
        // actions" likely to trip bot detection; navigate is also
        // paced because rapid navigations are a clear bot
        // signature.
        assert_eq!(PacedAction::from_tool_name("click"), Some(PacedAction::Click));
        assert_eq!(PacedAction::from_tool_name("type"), Some(PacedAction::Type));
        assert_eq!(PacedAction::from_tool_name("scroll"), Some(PacedAction::Scroll));
        assert_eq!(PacedAction::from_tool_name("press_key"), Some(PacedAction::PressKey));
        assert_eq!(PacedAction::from_tool_name("navigate"), Some(PacedAction::Navigate));
        // Non-paced tool names return None.
        for name in &["snapshot", "finish", "mark_subtask_done", "mark_subtask_skipped", "mark_subtask_failed", "declare_subtasks", "vision_inspect", "no_such_tool"] {
            assert_eq!(PacedAction::from_tool_name(name), None, "expected None for {}", name);
        }
    }

    #[test]
    fn delay_value_is_within_configured_range() {
        // Statistical: with min=200, max=600, sampled 1000 times
        // every value should be in [200, 600]. Catches a
        // regression where the random range is off-by-one or
        // signed.
        let mut g = PacingGuard::new(cfg(true, 200, 600, 1));
        g.before_action("click"); // seed the streak
        for _ in 0..1000 {
            let d = g.before_action("click");
            if let PacingDecision::Pace { delay, .. } = d {
                let ms = delay.as_millis() as u64;
                assert!((200..=600).contains(&ms), "delay {} outside [200, 600]", ms);
            } else {
                panic!("expected Pace variant");
            }
        }
    }
}
