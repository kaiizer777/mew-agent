// mew v2 — Phase 7: per-subtask step and time budgets for the
// long-horizon research loop.
//
// Why this exists (one paragraph):
//
// The Phase 7 spec line "add a per-platform time/step budget so
// one slow or broken site can't stall the whole task — on timeout,
// mark that subtask skipped and continue" is the smallest
// engineering problem on the checklist and the easiest to get
// wrong. The "skip" half is the trap: a research-shaped subtask
// that the budget guard force-closes is *not* a Skipped (the LLM
// decided not to do it) and *not* a Failed (the platform
// crashed). It is "we looked, we didn't have time to look enough,
// moving on" — Phase 7's new `Exhausted` status, which the
// tracker accepts as terminal-for-the-gate and the synthesizer
// distinguishes from "platform had no results" via the reason
// string.
//
// What this module is:
//
//   * `Budget` — a typed struct that holds a per-subtask
//     step-counter and a per-subtask start-timestamp. Two pure
//     methods: `tick(id, now_secs) -> BudgetDecision` and
//     `remaining_steps(id) -> u32`. The agent loop calls `tick`
//     after every tool call; the returned `BudgetDecision` tells
//     the loop whether to keep going, mark the subtask
//     `Exhausted` (and move on), or report the overrun to the
//     user.
//
//   * `BudgetDecision` — `StillUnder`, `StepExhausted { used, max }`,
//     `TimeExhausted { elapsed, limit }`, `UnknownSubtask`. The
//     agent loop pattern-matches on it.
//
//   * `SubtaskBudget` — the per-row config: max steps, max
//     wall-clock seconds. Mirrors the fields on
//     `research::ResearchSubTask` so the orchestrator can copy
//     them straight over.
//
// What this module is NOT:
//
//   * Not a thread-safe registry of in-flight subtasks. The
//     `Budget` struct is owned by the agent and the agent's
//     loop is the only writer; the test surface creates one
//     per test. The struct is `Send` but not `Sync` — wrap
//     in a `Mutex` if a future refactor moves tick() onto a
//     separate thread.
//   * Not a plan producer. The plan lives in `research.rs`;
//     this module is the runtime enforcer.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::completeness::CompletenessTracker;
use crate::research::{ResearchPlan, ResearchSubTask};

/// Per-subtask budget config. Mirrors `ResearchSubTask` so the
/// orchestrator can `SubtaskBudget::from_research_subtask(&p)`
/// without remapping. `step_budget = 0` is treated as "no step
/// limit" — a research subtask can run as many steps as the
/// loop's outer iteration cap. Same for `time_budget_secs = 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubtaskBudget {
    pub step_budget: u32,
    pub time_budget_secs: u64,
}

impl SubtaskBudget {
    pub fn from_research_subtask(s: &ResearchSubTask) -> Self {
        Self {
            step_budget: s.step_budget,
            time_budget_secs: s.time_budget_secs,
        }
    }

    /// True if neither dimension has a positive cap. A subtask
    /// with both zero is "unlimited" — the budget guard never
    /// fires. The agent loop's outer iteration cap is still the
    /// backstop.
    pub fn is_unlimited(&self) -> bool {
        self.step_budget == 0 && self.time_budget_secs == 0
    }
}

impl Default for SubtaskBudget {
    fn default() -> Self {
        // Phase 7's default: 10 steps / 75 seconds. The
        // motivating case ("visit platform, search, click
        // first 3 results") fits in this with room to spare.
        // The real defaults live in the
        // `default_job_board_platforms()` list in `research.rs`
        // and are copied onto each row by the planner.
        Self {
            step_budget: 10,
            time_budget_secs: 75,
        }
    }
}

/// What the budget guard wants the loop to do after the most
/// recent tool call. The loop pattern-matches on this; the
/// only branch that has a side effect is `StepExhausted` /
/// `TimeExhausted`, both of which the loop converts to a
/// `CompletenessTracker::mark_exhausted(id, reason)` call and
/// then *continues to the next platform* (the per-subtask
/// advance is the loop's job, not the budget's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetDecision {
    /// Subtask is still under both caps. Keep going.
    StillUnder {
        steps_used: u32,
        steps_max: u32,
        elapsed_secs: u64,
        time_max_secs: u64,
    },
    /// Step cap reached. The loop should mark the subtask
    /// `Exhausted` with reason "step budget exhausted: {used}/{max}"
    /// and advance to the next platform.
    StepExhausted { used: u32, max: u32 },
    /// Time cap reached. The loop should mark the subtask
    /// `Exhausted` with reason "time budget exhausted: {elapsed}s / {limit}s"
    /// and advance to the next platform.
    TimeExhausted { elapsed: u64, limit: u64 },
    /// The subtask id is not in the budget table. This is
    /// either "the LLM declared a subtask the planner didn't
    /// know about" (the loop's `declare_subtasks` happened
    /// after the budget was seeded) or "the loop tick'd the
    /// wrong id." The budget returns `UnknownSubtask` so the
    /// loop can decide — default behavior is to ignore the
    /// tick and keep going.
    UnknownSubtask,
}

impl BudgetDecision {
    /// True if the budget is *exhausted* (step or time) and the
    /// loop should call `mark_exhausted` and advance. Convenience
    /// for the loop's match arm.
    pub fn is_exhausted(&self) -> bool {
        matches!(
            self,
            BudgetDecision::StepExhausted { .. } | BudgetDecision::TimeExhausted { .. }
        )
    }

    /// Human-readable reason for the synthesizer / transcript.
    /// Returns `None` for `StillUnder` and `UnknownSubtask`.
    pub fn reason(&self) -> Option<String> {
        match self {
            BudgetDecision::StepExhausted { used, max } => {
                Some(format!("step budget exhausted: {used}/{max}"))
            }
            BudgetDecision::TimeExhausted { elapsed, limit } => {
                Some(format!("time budget exhausted: {elapsed}s / {limit}s"))
            }
            _ => None,
        }
    }
}

/// The runtime budget table. Built from a `ResearchPlan` (or
/// from a free-standing list of `(id, SubtaskBudget)` rows for
/// the tests). One entry per subtask; the per-row state is
/// `(steps_used, started_at_secs)`.
#[derive(Debug, Default, Clone)]
pub struct Budget {
    rows: HashMap<String, SubtaskBudget>,
    /// `steps_used[id]` — incremented on every `tick`.
    steps_used: HashMap<String, u32>,
    /// `started_at_secs[id]` — set on the first `tick` for
    /// that id; subsequent ticks read it to compute `elapsed`.
    started_at_secs: HashMap<String, u64>,
}

impl Budget {
    /// Build a budget from a `ResearchPlan`. The plan's per-
    /// platform subtasks are the source of truth for caps. The
    /// overall deadline is *not* applied here — the loop owns
    /// the global deadline and the per-platform budgets are the
    /// only thing the budget guard enforces.
    pub fn from_research_plan(plan: &ResearchPlan) -> Self {
        let mut b = Self::default();
        for p in &plan.platforms {
            b.rows.insert(
                p.id.clone(),
                SubtaskBudget {
                    step_budget: p.step_budget,
                    time_budget_secs: p.time_budget_secs,
                },
            );
        }
        b
    }

    /// Build a budget from an explicit list of `(id, budget)`
    /// pairs. Used by the test surface and by the orchestrator
    /// when a non-research subtask list is in play (Phase 7's
    /// spec is research-shaped, but the budget guard itself
    /// doesn't care about the research context).
    pub fn from_rows<I>(rows: I) -> Self
    where
        I: IntoIterator<Item = (String, SubtaskBudget)>,
    {
        let mut b = Self::default();
        for (id, budget) in rows {
            b.rows.insert(id, budget);
        }
        b
    }

    /// Number of tracked subtasks. The agent loop uses this to
    /// sanity-check that the budget covers the plan (mismatch
    /// is a soft warning, not a hard error).
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// True if the budget has no rows. A budget with no rows
    /// always returns `UnknownSubtask` on tick. The orchestrator
    /// uses this to short-circuit: a non-research task skips
    /// the budget guard entirely.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Read-only: the budget config for a subtask. Returns
    /// `None` for an unknown id.
    pub fn get(&self, id: &str) -> Option<SubtaskBudget> {
        self.rows.get(id).copied()
    }

    /// Steps used on a subtask. Returns 0 for unknown ids.
    pub fn steps_used(&self, id: &str) -> u32 {
        self.steps_used.get(id).copied().unwrap_or(0)
    }

    /// Reset a subtask's runtime state. Used when a subtask is
    /// retried (e.g. a `mark_subtask_done` call was rejected
    /// for stale evidence and the LLM is going to take more
    /// steps before retrying — we don't want the retry to
    /// double-count against the budget).
    pub fn reset(&mut self, id: &str) {
        self.steps_used.remove(id);
        self.started_at_secs.remove(id);
    }

    /// The main entry point. Called by the agent loop after
    /// every tool dispatch with the id of the subtask the
    /// dispatch belongs to. The `now_secs` is the wall-clock
    /// seconds (same source as the rest of the codebase —
    /// `SystemTime::now().duration_since(UNIX_EPOCH).as_secs()`)
    /// so the test surface can pin time.
    ///
    /// Side effect: increments the per-subtask step counter and
    /// records the first-call timestamp.
    pub fn tick(&mut self, id: &str, now_secs: u64) -> BudgetDecision {
        let Some(budget) = self.rows.get(id).copied() else {
            return BudgetDecision::UnknownSubtask;
        };
        // Unbounded subtasks: tick is a no-op, the budget guard
        // never fires. The agent loop's outer iteration cap is
        // the only backstop.
        if budget.is_unlimited() {
            let used = self.steps_used(id) + 1;
            self.steps_used.insert(id.to_string(), used);
            self.started_at_secs.entry(id.to_string()).or_insert(now_secs);
            return BudgetDecision::StillUnder {
                steps_used: used,
                steps_max: 0,
                elapsed_secs: 0,
                time_max_secs: 0,
            };
        }
        let used = self.steps_used(id) + 1;
        self.steps_used.insert(id.to_string(), used);
        let started = *self
            .started_at_secs
            .entry(id.to_string())
            .or_insert(now_secs);
        let elapsed = now_secs.saturating_sub(started);
        // Step cap wins: if both fired on the same tick, the
        // user-visible reason should be the step cap (it's the
        // more common failure mode and the easier to reason
        // about — the time cap firing is a backup).
        if budget.step_budget > 0 && used > budget.step_budget {
            return BudgetDecision::StepExhausted {
                used,
                max: budget.step_budget,
            };
        }
        if budget.time_budget_secs > 0 && elapsed > budget.time_budget_secs {
            return BudgetDecision::TimeExhausted {
                elapsed,
                limit: budget.time_budget_secs,
            };
        }
        BudgetDecision::StillUnder {
            steps_used: used,
            steps_max: budget.step_budget,
            elapsed_secs: elapsed,
            time_max_secs: budget.time_budget_secs,
        }
    }

    /// Mark the named subtask as Exhausted on the given tracker
    /// if the budget is exhausted. Returns the decision so the
    /// loop can log it. Convenience: this is the pattern the
    /// agent loop will use most often.
    ///
    /// `tracker` is a `&mut CompletenessTracker` because
    /// `mark_exhausted` mutates. The function does not consume
    /// the budget; the agent loop can call it on every tick and
    /// only one tick will actually trigger the mark.
    pub fn tick_and_enforce(
        &mut self,
        tracker: &mut CompletenessTracker,
        id: &str,
        now_secs: u64,
    ) -> BudgetDecision {
        let d = self.tick(id, now_secs);
        if let Some(reason) = d.reason() {
            // mark_exhausted is idempotent on already-terminal
            // subtasks (returns AlreadyTerminal instead of
            // MarkedExhausted). The loop calls this on every
            // tick so the second-and-later ticks are no-ops —
            // the tracker is the source of truth for "is this
            // subtask already closed."
            let _ = tracker.mark_exhausted(id, reason);
        }
        d
    }
}

/// Convenience: build a budget from a `ResearchPlan` and return
/// a `(Budget, plan.platforms.clone())` pair for callers that
/// want both. Saves a `.clone()` at the call site.
pub fn budget_for(plan: &ResearchPlan) -> Budget {
    Budget::from_research_plan(plan)
}

/// The current wall-clock in unix seconds. Wraps the
/// `SystemTime` call so tests can use a `now_secs: u64`
/// argument instead of mocking the system clock.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completeness::DeclareItem;

    fn research_plan_for_test() -> ResearchPlan {
        ResearchPlan {
            goal: "find rust jobs".into(),
            platforms: vec![
                ResearchSubTask {
                    id: "linkedin".into(),
                    platform: "LinkedIn".into(),
                    domain: "linkedin.com".into(),
                    entry_hint: String::new(),
                    acceptance: vec![],
                    step_budget: 5,
                    time_budget_secs: 60,
                    query: String::new(),
                },
                ResearchSubTask {
                    id: "indeed".into(),
                    platform: "Indeed".into(),
                    domain: "indeed.com".into(),
                    entry_hint: String::new(),
                    acceptance: vec![],
                    step_budget: 3,
                    time_budget_secs: 30,
                    query: String::new(),
                },
            ],
            synthesis_hint: String::new(),
            overall_deadline_secs: None,
            is_research: true,
            matched_pattern: "test".into(),
        }
    }

    #[test]
    fn from_research_plan_pulls_per_platform_caps() {
        let plan = research_plan_for_test();
        let b = Budget::from_research_plan(&plan);
        assert_eq!(b.len(), 2);
        assert_eq!(b.get("linkedin"), Some(SubtaskBudget { step_budget: 5, time_budget_secs: 60 }));
        assert_eq!(b.get("indeed"), Some(SubtaskBudget { step_budget: 3, time_budget_secs: 30 }));
        assert_eq!(b.get("nope"), None);
    }

    #[test]
    fn tick_increments_step_counter() {
        let plan = research_plan_for_test();
        let mut b = Budget::from_research_plan(&plan);
        let d = b.tick("linkedin", 1_000);
        match d {
            BudgetDecision::StillUnder { steps_used, steps_max, .. } => {
                assert_eq!(steps_used, 1);
                assert_eq!(steps_max, 5);
            }
            other => panic!("expected StillUnder, got {other:?}"),
        }
        // Second tick: still under, used=2.
        let d = b.tick("linkedin", 1_001);
        match d {
            BudgetDecision::StillUnder { steps_used, .. } => {
                assert_eq!(steps_used, 2);
            }
            other => panic!("expected StillUnder, got {other:?}"),
        }
        assert_eq!(b.steps_used("linkedin"), 2);
    }

    #[test]
    fn tick_fires_step_exhausted_when_cap_reached() {
        let plan = research_plan_for_test();
        let mut b = Budget::from_research_plan(&plan);
        // LinkedIn has step_budget=5; 5 ticks should be ok,
        // the 6th should fire.
        for _ in 0..5 {
            let d = b.tick("linkedin", 1_000);
            assert!(matches!(d, BudgetDecision::StillUnder { .. }), "got {d:?}");
        }
        let d = b.tick("linkedin", 1_000);
        assert!(matches!(d, BudgetDecision::StepExhausted { used: 6, max: 5 }), "got {d:?}");
    }

    #[test]
    fn tick_fires_time_exhausted_when_cap_reached() {
        let plan = research_plan_for_test();
        let mut b = Budget::from_research_plan(&plan);
        // First tick at t=0 sets the start time.
        b.tick("linkedin", 1_000);
        // Step 2 at t=10s — still under (limit is 60s).
        let d = b.tick("linkedin", 1_010);
        match d {
            BudgetDecision::StillUnder { elapsed_secs, time_max_secs, .. } => {
                assert_eq!(elapsed_secs, 10);
                assert_eq!(time_max_secs, 60);
            }
            other => panic!("expected StillUnder, got {other:?}"),
        }
        // Step 3 at t=70s — over the 60s cap.
        let d = b.tick("linkedin", 1_070);
        assert!(matches!(d, BudgetDecision::TimeExhausted { elapsed: 70, limit: 60 }), "got {d:?}");
    }

    #[test]
    fn tick_returns_unknown_for_unmapped_id() {
        let plan = research_plan_for_test();
        let mut b = Budget::from_research_plan(&plan);
        let d = b.tick("ghost", 1_000);
        assert_eq!(d, BudgetDecision::UnknownSubtask);
    }

    #[test]
    fn step_cap_wins_when_both_caps_fire_on_same_tick() {
        // Edge case: the very first tick is also the one
        // where the step cap is exceeded (cap=1) and the
        // time cap is exceeded (started 100s ago). Step cap
        // should be the answer — it's the more common
        // failure mode and the easier to reason about.
        let plan = ResearchPlan {
            goal: "x".into(),
            platforms: vec![ResearchSubTask {
                id: "a".into(),
                platform: "A".into(),
                domain: "a.com".into(),
                entry_hint: String::new(),
                acceptance: vec![],
                step_budget: 1,
                time_budget_secs: 30,
                query: String::new(),
            }],
            synthesis_hint: String::new(),
            overall_deadline_secs: None,
            is_research: true,
            matched_pattern: "test".into(),
        };
        let mut b = Budget::from_research_plan(&plan);
        let d = b.tick("a", 100);
        // Used=1, still under step cap. Elapsed=0, under time
        // cap. Still under.
        assert!(matches!(d, BudgetDecision::StillUnder { .. }), "got {d:?}");
        // 2nd tick at t=200. Used=2 (over cap of 1). Step
        // cap fires.
        let d = b.tick("a", 200);
        assert!(matches!(d, BudgetDecision::StepExhausted { .. }), "got {d:?}");
    }

    #[test]
    fn unlimited_budget_never_fires() {
        let b = Budget::from_rows(vec![(
            "open".to_string(),
            SubtaskBudget { step_budget: 0, time_budget_secs: 0 },
        )]);
        let mut b = b;
        for i in 0..50 {
            let d = b.tick("open", i);
            assert!(matches!(d, BudgetDecision::StillUnder { .. }), "iter {i} got {d:?}");
        }
    }

    #[test]
    fn tick_and_enforce_marks_tracker_exhausted() {
        // The end-to-end proof: the budget guard + the
        // tracker integration actually close the subtask.
        let plan = research_plan_for_test();
        let mut b = Budget::from_research_plan(&plan);
        let mut tracker = CompletenessTracker::new();
        tracker
            .declare(vec![DeclareItem {
                id: "linkedin".into(),
                description: "LinkedIn".into(),
            }])
            .unwrap();
        // Burn 5 ticks under cap.
        for _ in 0..5 {
            let d = b.tick_and_enforce(&mut tracker, "linkedin", 1_000);
            assert!(matches!(d, BudgetDecision::StillUnder { .. }));
        }
        // 6th tick: step cap fires, tracker gets marked.
        let d = b.tick_and_enforce(&mut tracker, "linkedin", 1_000);
        assert!(matches!(d, BudgetDecision::StepExhausted { .. }));
        // Tracker should be in Exhausted state.
        let sub = tracker.subtasks.iter().find(|s| s.id == "linkedin").unwrap();
        assert!(
            matches!(sub.status, crate::completeness::SubTaskStatus::Exhausted { .. }),
            "subtask should be Exhausted, got {:?}",
            sub.status,
        );
    }

    #[test]
    fn tick_and_enforce_is_idempotent() {
        // After the subtask is already Exhausted, a repeat
        // tick returns StepExhausted but does not re-mark
        // (tracker returns AlreadyTerminal).
        let plan = research_plan_for_test();
        let mut b = Budget::from_research_plan(&plan);
        let mut tracker = CompletenessTracker::new();
        tracker
            .declare(vec![DeclareItem {
                id: "linkedin".into(),
                description: "LinkedIn".into(),
            }])
            .unwrap();
        for _ in 0..6 {
            b.tick_and_enforce(&mut tracker, "linkedin", 1_000);
        }
        // 7th tick: tracker is already Exhausted, no re-mark.
        let d = b.tick_and_enforce(&mut tracker, "linkedin", 1_000);
        assert!(matches!(d, BudgetDecision::StepExhausted { .. }));
        // Still only one Exhausted status — verify by re-reading.
        let sub = tracker.subtasks.iter().find(|s| s.id == "linkedin").unwrap();
        assert!(matches!(
            sub.status,
            crate::completeness::SubTaskStatus::Exhausted { .. }
        ));
    }

    #[test]
    fn reset_clears_runtime_state() {
        let plan = research_plan_for_test();
        let mut b = Budget::from_research_plan(&plan);
        b.tick("linkedin", 1_000);
        b.tick("linkedin", 1_001);
        assert_eq!(b.steps_used("linkedin"), 2);
        b.reset("linkedin");
        assert_eq!(b.steps_used("linkedin"), 0);
    }

    #[test]
    fn subtask_budget_is_unlimited_when_both_zero() {
        let b = SubtaskBudget { step_budget: 0, time_budget_secs: 0 };
        assert!(b.is_unlimited());
        let b = SubtaskBudget { step_budget: 1, time_budget_secs: 0 };
        assert!(!b.is_unlimited());
    }
}
