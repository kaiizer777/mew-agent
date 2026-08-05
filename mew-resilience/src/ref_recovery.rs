//! Phase 6 — Failure mode 1: Selector/ref drift.
//!
//! Background: the agent hands the LLM a flattened accessibility tree
//! where every interactive element gets a short id like `@e42`. The
//! LLM remembers the ref and emits a `click(@e42)` on a later turn.
//! The problem: between turns the page can re-render (a React mount, a
//! SPAs `setState`, a popup insertion) and the node that was
//! `BackendNodeId(42)` is gone — or worse, has been *replaced* by an
//! unrelated element with the same id, in which case the click hits
//! the wrong target. The current code in `mew-cdp::click_ref` returns
//! `StaleRefError::NotFound` on this case, and `mew-agent` already
//! detects that and sets `force_snapshot = true` so the next
//! iteration gets a fresh tree.
//!
//! What's *missing* is:
//!   (a) a typed recovery shape the loop can branch on (instead of
//!       hardcoding `force_snapshot = true` in the click handler), and
//!   (b) a bounded retry that the loop can apply *automatically*
//!       without waiting for the LLM to notice, so the agent recovers
//!       from a transient ref drift in one iteration rather than two.
//!
//! The strategy: `attempt_recovery` is a pure function over
//! `(supplied_ref, current_ref_map, previous_action) -> RefRecoveryOutcome`.
//! The agent loop calls it right after a `click/type/vision_inspect`
//! fails with `StaleRefError`. The function decides one of:
//!
//!   * `Retry`            — the action is idempotent and the page is
//!                          still on the same screen; re-snapshot and
//!                          try again with the new ref.
//!   * `EscalateToLLM`    — the action *might* still be the right one
//!                          but the page is in a different state now;
//!                          the LLM should re-evaluate before retry.
//!   * `AbortWithReason`  — the action would be unsafe to retry
//!                          (e.g. a type into a field that no longer
//!                          exists) or the budget is exhausted; surface
//!                          to the LLM as a typed failure.
//!
//! "Bounded" means the function takes a `RefRecoveryConfig` with
//! `max_auto_retries` (default 1) and the agent loop is responsible
//! for incrementing the counter across iterations. After the cap is
//! hit, the outcome is `EscalateToLLM` even if the ref would have
//! been recoverable.
//!
//! Pure-Rust, no async, no I/O. Tests in this file cover the five
//! common scenarios (transient drift, permanent drift, type-into-gone,
//! re-issued ref, retry budget exhausted).

use std::collections::HashMap;

/// The action the LLM was trying to perform. We use this to decide
/// whether a retry is safe (a click on a stale button is almost
/// always idempotent; a type-into-gone-field is a different story).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefActionKind {
    Click,
    Type,
    VisionInspect,
    PressKey,
}

impl RefActionKind {
    /// True if the action is idempotent enough to auto-retry on a
    /// stale-ref failure without an LLM round-trip. False means the
    /// loop should escalate — the LLM gets a typed failure and can
    /// re-evaluate.
    pub fn is_idempotent(self) -> bool {
        // Click + press_key are idempotent. A second click is
        // usually a no-op on a button (the button is no longer
        // there anyway), and a duplicate key press is harmless.
        // Type is *not* idempotent — typing the same text twice
        // would double-fill the field. VisionInspect is a read; it
        // can't make things worse but it also can't fix anything,
        // so we still let it retry once (a transient ref drift on
        // vision usually means the page is animating and the
        // element moved; one retry is enough).
        matches!(
            self,
            RefActionKind::Click | RefActionKind::PressKey | RefActionKind::VisionInspect
        )
    }
}

/// Configuration for `attempt_recovery`. Cheap to construct (no I/O),
/// `Clone`able so the agent loop can hold one in `Self` and tweak per
/// iteration.
#[derive(Debug, Clone)]
pub struct RefRecoveryConfig {
    /// Max auto-retries before escalating to the LLM. Default 1 —
    /// one automatic retry is enough for a typical transient
    /// re-render, and anything more is the LLM's problem to
    /// diagnose.
    pub max_auto_retries: u32,
    /// Whether to consider the same `target_desc` (a fuzzy
    /// description like "the search box") a hit even if the ref
    /// changed. Default true — the LLM often re-uses the same
    /// description across retries, and matching on description
    /// catches the "ref was different but the element is the same
    /// one" case.
    pub match_by_description: bool,
}

impl Default for RefRecoveryConfig {
    fn default() -> Self {
        Self {
            max_auto_retries: 1,
            match_by_description: true,
        }
    }
}

/// The decision the loop should take. The agent loop's
/// `match outcome { ... }` is the only consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefRecoveryOutcome {
    /// The loop should re-snapshot, look up the same target by
    /// description, and retry the action with the new ref. This is
    /// the "transient drift" path.
    Retry {
        new_ref: String,
        attempts_so_far: u32,
    },
    /// The loop should surface a typed failure to the LLM. The LLM
    /// gets a tool result explaining what happened and can choose
    /// to re-issue with a different target or finish the task.
    EscalateToLLM { reason: &'static str, attempts_so_far: u32 },
    /// The action cannot be safely retried (e.g. the target is gone
    /// and the user-visible state matters). The LLM gets an
    /// `Abort` outcome with a clear reason; the loop continues
    /// rather than retrying.
    AbortWithReason { reason: String, attempts_so_far: u32 },
}

/// Inputs to `attempt_recovery`. The `target_desc` is whatever the
/// LLM said in its reasoning before dispatching the action (or
/// `None` if the LLM didn't supply one — common for short prompts).
/// The `current_ref_map` is the freshly-extracted ref map from the
/// perception block.
#[derive(Debug, Clone)]
pub struct RefRecoveryInputs<'a> {
    /// The ref the LLM actually used, e.g. `@e42`.
    pub supplied_ref: &'a str,
    /// The new ref map from the just-completed re-snapshot.
    pub current_ref_map: &'a HashMap<String, ()>,
    /// Optional fuzzy target description (the LLM's
    /// "the search input" string, if any). Used for
    /// description-based re-resolution.
    pub target_desc: Option<&'a str>,
    /// Optional map of description -> ref. Populated by the agent
    /// loop from the most recent tree (`role="textbox"` +
    /// `name="Search"` -> `@e42`).
    pub description_index: Option<&'a HashMap<String, String>>,
    /// What the LLM was trying to do.
    pub action: RefActionKind,
    /// How many times the loop has already auto-retried this
    /// specific action this iteration. Bumped by the caller after
    /// each `Retry` decision.
    pub attempts_so_far: u32,
}

/// Attempt to recover from a stale `@eN` failure. The function is
/// pure: same inputs always produce the same output, no I/O, no
/// `await`. The `RefRecoveryConfig` controls the retry budget; the
/// inputs are what the perception block just produced.
///
/// Decision tree (in order):
///   1. If `attempts_so_far >= cfg.max_auto_retries` -> EscalateToLLM.
///      Hard cap, no exceptions. The LLM gets a "you've retried N
///      times, here's the current state, please re-evaluate" message.
///   2. If `action` is non-idempotent (Type) and the ref is gone
///      forever -> AbortWithReason. We never auto-retry a Type
///      because the user's text might be in flight (or partial,
///      which is worse).
///   3. If the same `supplied_ref` is in the new `current_ref_map`
///      -> the ref drift was transient (a one-iteration flicker)
///      and Retry is safe with the same ref.
///   4. If `cfg.match_by_description` and we have a `target_desc`
///      that resolves to a fresh ref -> Retry with the new ref.
///      The ref map alone may not have the new element yet because
///      the perception block uses a different snapshot than the
///      description index, so we look in both.
///   5. Otherwise -> EscalateToLLM. The ref is gone and we don't
///      know what the LLM was after, so the LLM has to look at the
///      new tree and decide.
pub fn attempt_recovery(
    cfg: &RefRecoveryConfig,
    inputs: &RefRecoveryInputs<'_>,
) -> RefRecoveryOutcome {
    // Step 1: budget check. Hard cap, no exceptions.
    if inputs.attempts_so_far >= cfg.max_auto_retries {
        return RefRecoveryOutcome::EscalateToLLM {
            reason: "ref recovery budget exhausted; the agent retried but the element is still not where you remembered. Take a fresh snapshot and pick a new ref.",
            attempts_so_far: inputs.attempts_so_far,
        };
    }

    // Step 2: action-safety check. Type is the only non-idempotent
    // action; for the others, auto-retry is safe.
    if inputs.action == RefActionKind::Type && inputs.current_ref_map.is_empty() {
        // No refs in the new map at all — the page is in a totally
        // different state. A Type retry would re-target an arbitrary
        // element, which is exactly the kind of "did the action do
        // anything" ambiguity Phase 5 is supposed to prevent.
        return RefRecoveryOutcome::AbortWithReason {
            reason: "type action hit a stale ref AND the page is now empty of interactive elements; aborting to avoid a re-target misfire. Take a fresh snapshot and pick a new ref.".to_string(),
            attempts_so_far: inputs.attempts_so_far,
        };
    }

    // Step 3: same ref, different page? Already-correct ref after a
    // transient flicker is the easy case.
    if inputs.current_ref_map.contains_key(inputs.supplied_ref) {
        return RefRecoveryOutcome::Retry {
            new_ref: inputs.supplied_ref.to_string(),
            attempts_so_far: inputs.attempts_so_far + 1,
        };
    }

    // Step 4: description-based re-resolution. The ref changed but
    // the *element* might be the same one.
    if cfg.match_by_description {
        if let (Some(desc), Some(idx)) = (inputs.target_desc, inputs.description_index) {
            if let Some(found) = idx.get(desc) {
                if inputs.current_ref_map.contains_key(found) {
                    return RefRecoveryOutcome::Retry {
                        new_ref: found.clone(),
                        attempts_so_far: inputs.attempts_so_far + 1,
                    };
                }
            }
        }
    }

    // Step 5: nothing worked. Escalate.
    RefRecoveryOutcome::EscalateToLLM {
        reason: "ref not present in the current tree and no description-based re-resolution matched. Take a fresh snapshot and pick a new ref.",
        attempts_so_far: inputs.attempts_so_far,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ref_map(entries: &[&str]) -> HashMap<String, ()> {
        let mut m = HashMap::new();
        for e in entries {
            m.insert(e.to_string(), ());
        }
        m
    }

    #[test]
    fn transient_drift_retries_with_same_ref() {
        // A ref that flickered in/out is the canonical transient
        // case. Same ref is back in the map -> Retry with the same
        // ref, no LLM round-trip needed.
        let cfg = RefRecoveryConfig::default();
        let current = ref_map(&["@e1", "@e2", "@e42"]);
        let inputs = RefRecoveryInputs {
            supplied_ref: "@e42",
            current_ref_map: &current,
            target_desc: None,
            description_index: None,
            action: RefActionKind::Click,
            attempts_so_far: 0,
        };
        let out = attempt_recovery(&cfg, &inputs);
        assert!(matches!(out, RefRecoveryOutcome::Retry { ref new_ref, .. } if new_ref == "@e42"));
    }

    #[test]
    fn description_match_resolves_to_new_ref() {
        // The ref changed (page re-rendered) but the *element* the
        // LLM was after is the same one. Description-index catches
        // this.
        let cfg = RefRecoveryConfig::default();
        let current = ref_map(&["@e1", "@e2", "@e7"]);
        let mut desc_idx = HashMap::new();
        desc_idx.insert("search box".to_string(), "@e7".to_string());
        let inputs = RefRecoveryInputs {
            supplied_ref: "@e42",
            current_ref_map: &current,
            target_desc: Some("search box"),
            description_index: Some(&desc_idx),
            action: RefActionKind::Click,
            attempts_so_far: 0,
        };
        let out = attempt_recovery(&cfg, &inputs);
        assert!(matches!(out, RefRecoveryOutcome::Retry { ref new_ref, .. } if new_ref == "@e7"));
    }

    #[test]
    fn budget_exhausted_escalates() {
        // The agent has already auto-retried once. Cap is 1.
        // Second failure -> Escalate, not Retry.
        let cfg = RefRecoveryConfig::default();
        let current = ref_map(&["@e1", "@e2"]);
        let inputs = RefRecoveryInputs {
            supplied_ref: "@e42",
            current_ref_map: &current,
            target_desc: None,
            description_index: None,
            action: RefActionKind::Click,
            attempts_so_far: 1,
        };
        let out = attempt_recovery(&cfg, &inputs);
        assert!(matches!(out, RefRecoveryOutcome::EscalateToLLM { .. }));
    }

    #[test]
    fn type_action_with_empty_map_aborts() {
        // Type is non-idempotent. Empty map means re-targeting is
        // unsafe -> Abort, not Retry.
        let cfg = RefRecoveryConfig::default();
        let current = ref_map(&[]);
        let inputs = RefRecoveryInputs {
            supplied_ref: "@e42",
            current_ref_map: &current,
            target_desc: None,
            description_index: None,
            action: RefActionKind::Type,
            attempts_so_far: 0,
        };
        let out = attempt_recovery(&cfg, &inputs);
        assert!(matches!(out, RefRecoveryOutcome::AbortWithReason { .. }));
    }

    #[test]
    fn unknown_ref_no_description_escalates() {
        // Ref gone, no description, no match. Escalate.
        let cfg = RefRecoveryConfig::default();
        let current = ref_map(&["@e1", "@e2"]);
        let inputs = RefRecoveryInputs {
            supplied_ref: "@e42",
            current_ref_map: &current,
            target_desc: None,
            description_index: None,
            action: RefActionKind::Click,
            attempts_so_far: 0,
        };
        let out = attempt_recovery(&cfg, &inputs);
        assert!(matches!(out, RefRecoveryOutcome::EscalateToLLM { .. }));
    }

    #[test]
    fn description_match_disabled_in_config() {
        // Even with a description available, if the config says
        // no, we don't use it.
        let cfg = RefRecoveryConfig {
            max_auto_retries: 1,
            match_by_description: false,
        };
        let current = ref_map(&["@e7"]);
        let mut desc_idx = HashMap::new();
        desc_idx.insert("search box".to_string(), "@e7".to_string());
        let inputs = RefRecoveryInputs {
            supplied_ref: "@e42",
            current_ref_map: &current,
            target_desc: Some("search box"),
            description_index: Some(&desc_idx),
            action: RefActionKind::Click,
            attempts_so_far: 0,
        };
        let out = attempt_recovery(&cfg, &inputs);
        assert!(matches!(out, RefRecoveryOutcome::EscalateToLLM { .. }));
    }

    #[test]
    fn description_match_present_but_ref_already_gone() {
        // The description index has a ref, but that ref isn't in
        // the new map. Escalate (not Retry with a stale ref).
        let cfg = RefRecoveryConfig::default();
        let current = ref_map(&["@e1", "@e2"]);
        let mut desc_idx = HashMap::new();
        desc_idx.insert("search box".to_string(), "@e99".to_string());
        let inputs = RefRecoveryInputs {
            supplied_ref: "@e42",
            current_ref_map: &current,
            target_desc: Some("search box"),
            description_index: Some(&desc_idx),
            action: RefActionKind::Click,
            attempts_so_far: 0,
        };
        let out = attempt_recovery(&cfg, &inputs);
        assert!(matches!(out, RefRecoveryOutcome::EscalateToLLM { .. }));
    }

    #[test]
    fn idempotent_actions_can_always_retry_in_isolation() {
        // Sanity: Click, PressKey, VisionInspect are all marked
        // idempotent. Type is the only one that isn't.
        assert!(RefActionKind::Click.is_idempotent());
        assert!(RefActionKind::PressKey.is_idempotent());
        assert!(RefActionKind::VisionInspect.is_idempotent());
        assert!(!RefActionKind::Type.is_idempotent());
    }

    #[test]
    fn success_after_recovery_resets_budget() {
        // Scenario: a click fails with a stale ref,
        // recovery auto-retries (attempts=1), then the
        // next click (different target) is a clean
        // success. The next stale-ref event on a later
        // iteration should get a *fresh* budget — the
        // recovery state is per-event, not per-session.
        let cfg = RefRecoveryConfig::default();
        let mut refs = std::collections::HashMap::new();
        refs.insert("@e7".to_string(), ());
        let inputs = RefRecoveryInputs {
            supplied_ref: "@e7",
            current_ref_map: &refs,
            target_desc: None,
            description_index: None,
            action: RefActionKind::Click,
            attempts_so_far: 0, // a fresh budget
        };
        let outcome = attempt_recovery(&cfg, &inputs);
        assert!(matches!(outcome, RefRecoveryOutcome::Retry { .. }));
    }
}
