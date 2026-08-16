//! Phase 26 — Worker Health Model & Stuck Detection.
//!
//! Provides pure-Rust data structures and functions to monitor worker health,
//! detect repetitive action loops or progress stalls, and flag approaching token limits.
//! Zero LLM cost, pure in-memory bookkeeping.

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

/// Operational metrics and state tracking for a single worker execution session.
#[derive(Debug, Clone)]
pub struct WorkerHealth {
    pub worker_id: String,
    pub last_heartbeat: Instant,
    pub last_progress_at: Instant,
    pub iteration_count: u32,
    pub recent_action_hashes: VecDeque<u64>,
    pub token_spend_so_far: u32,
    pub distinct_snapshot_signatures_seen: HashSet<u64>,
    pub action_history_capacity: usize,
    pub snapshot_capacity: usize,
    /// Phase 7: rolling window of the most recent snapshot
    /// signatures, in order. Used to detect a worker that is
    /// *technically active* (different actions) but not
    /// making forward progress (the AX tree is the same N
    /// snapshots in a row). Capacity matches
    /// `action_history_capacity` so a worker that exhausts
    /// the action-loop check has the same data window for
    /// the no-forward-progress check.
    pub recent_snapshot_signatures: VecDeque<u64>,
}

impl WorkerHealth {
    pub fn new(worker_id: impl Into<String>) -> Self {
        let now = Instant::now();
        Self {
            worker_id: worker_id.into(),
            last_heartbeat: now,
            last_progress_at: now,
            iteration_count: 0,
            recent_action_hashes: VecDeque::with_capacity(8),
            token_spend_so_far: 0,
            distinct_snapshot_signatures_seen: HashSet::with_capacity(100),
            action_history_capacity: 8,
            snapshot_capacity: 100,
            recent_snapshot_signatures: VecDeque::with_capacity(8),
        }
    }

    pub fn record_heartbeat(&mut self) {
        self.last_heartbeat = Instant::now();
    }

    /// Called at each `checkpoint()` pre-dispatch hook.
    pub fn on_checkpoint(&mut self) {
        self.record_heartbeat();
        self.iteration_count += 1;
    }

    pub fn record_action(&mut self, action_hash: u64, snapshot_sig: Option<u64>) {
        if self.recent_action_hashes.len() >= self.action_history_capacity {
            self.recent_action_hashes.pop_front();
        }
        self.recent_action_hashes.push_back(action_hash);

        // Phase 7: also push the snapshot signature into the
        // rolling window used by `StuckReason::NoForwardProgress`.
        // When the agent only calls `record_action` without
        // a signature (older call sites), the snapshot window
        // is not updated — the no-forward-progress detector
        // only fires on signatures the loop actually saw.
        if let Some(sig) = snapshot_sig {
            if self.recent_snapshot_signatures.len() >= self.action_history_capacity {
                self.recent_snapshot_signatures.pop_front();
            }
            self.recent_snapshot_signatures.push_back(sig);

            if self.distinct_snapshot_signatures_seen.len() >= self.snapshot_capacity
                && !self.distinct_snapshot_signatures_seen.contains(&sig)
            {
                self.distinct_snapshot_signatures_seen.clear();
            }
            self.distinct_snapshot_signatures_seen.insert(sig);
        }
    }

    pub fn record_progress(&mut self) {
        self.last_progress_at = Instant::now();
    }

    pub fn record_tokens(&mut self, token_amount: u32, is_delta: bool) {
        if is_delta {
            self.token_spend_so_far = self.token_spend_so_far.saturating_add(token_amount);
        } else {
            self.token_spend_so_far = token_amount;
        }
    }
}

/// Root cause details when a worker is suspected of being stuck.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StuckReason {
    /// Worker is executing the exact same action repeatedly without page snapshot evolution.
    ActionLoop {
        repeated_action_hash: u64,
        repeat_count: usize,
    },
    /// Iteration count is climbing but no progress has been registered within the timeout window.
    NoProgressTimeout {
        elapsed: Duration,
        iterations_completed: u32,
    },
    /// Phase 7: the worker is technically active (different
    /// actions are being dispatched) but the page state is
    /// not evolving — the same AX-tree signature has been
    /// observed for `consecutive_count` consecutive
    /// snapshots. This is the canonical "spinner" pattern:
    /// the LLM is doing work, but the page it sees is
    /// identical to the page it saw N iterations ago. Common
    /// triggers: a click that triggers no visible change
    /// (e.g. clicking a label, not the input it labels), an
    /// off-screen element, a modal that's been auto-dismissed
    /// but the LLM keeps "clicking" inside it, a navigation
    /// that the browser swallowed.
    NoForwardProgress {
        /// The snapshot signature that hasn't changed.
        stuck_signature: u64,
        /// How many consecutive snapshots matched it.
        consecutive_count: usize,
    },
}

/// Evaluated health report for supervisor inspection.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerHealthReport {
    pub worker_id: String,
    pub is_healthy: bool,
    pub suspected_stuck: bool,
    pub stuck_reason: Option<StuckReason>,
    pub token_limit_approaching: bool,
    pub token_spend_so_far: u32,
    pub max_token_budget: Option<u32>,
    pub token_used_fraction: f32,
    pub iteration_count: u32,
    pub last_heartbeat_elapsed: Duration,
    pub last_progress_elapsed: Duration,
}

/// Configuration parameters for health evaluation.
#[derive(Debug, Clone)]
pub struct StuckDetectorConfig {
    pub max_identical_action_repeat: usize,
    pub no_progress_timeout: Duration,
    pub token_warning_threshold: f32,
    pub max_token_budget: Option<u32>,
    /// Phase 7: how many consecutive identical snapshot
    /// signatures the worker must observe before the
    /// "no forward progress" detector fires. The default (4)
    /// is one greater than the default action-loop threshold
    /// (3) so a worker that genuinely needs a couple of
    /// iterations to land (e.g. "click into a field", "type
    /// query", "press enter" — three same-tree actions
    /// before the page actually changes) is not flagged.
    /// `usize::MAX` disables the check.
    pub max_identical_snapshot_repeat: usize,
}

impl Default for StuckDetectorConfig {
    fn default() -> Self {
        Self {
            max_identical_action_repeat: 3,
            no_progress_timeout: Duration::from_secs(60),
            token_warning_threshold: 0.85,
            max_token_budget: None,
            max_identical_snapshot_repeat: 4,
        }
    }
}

/// Pure-Rust stuck detector. Evaluates `WorkerHealth` state against thresholds.
pub fn detect_stuck(health: &WorkerHealth, config: &StuckDetectorConfig) -> WorkerHealthReport {
    let now = Instant::now();
    let last_heartbeat_elapsed = now.duration_since(health.last_heartbeat);
    let last_progress_elapsed = now.duration_since(health.last_progress_at);

    let mut stuck_reason = None;

    // 1. Action loop check: N consecutive identical action hashes
    let repeat_threshold = config.max_identical_action_repeat.max(2);
    if health.recent_action_hashes.len() >= repeat_threshold {
        let tail: Vec<u64> = health
            .recent_action_hashes
            .iter()
            .rev()
            .take(repeat_threshold)
            .copied()
            .collect();
        let last_hash = tail[0];
        let all_same = tail.iter().all(|&h| h == last_hash);

        if all_same {
            stuck_reason = Some(StuckReason::ActionLoop {
                repeated_action_hash: last_hash,
                repeat_count: repeat_threshold,
            });
        }
    }

    // 2. No progress wall-clock timeout check
    if stuck_reason.is_none()
        && health.iteration_count > 1
        && last_progress_elapsed >= config.no_progress_timeout
    {
        stuck_reason = Some(StuckReason::NoProgressTimeout {
            elapsed: last_progress_elapsed,
            iterations_completed: health.iteration_count,
        });
    }

    // 2b. Phase 7: no-forward-progress check. The worker
    // has dispatched N consecutive iterations on the same
    // AX-tree signature — actions may have been different
    // (so the action-loop check didn't fire) but the page
    // itself hasn't moved. This is the "spinner" pattern:
    // technically active, not stuck, but not making
    // observable progress. Runs only when the action-loop
    // check didn't already fire (it has the same data
    // window, so a single iteration can't trigger both
    // reasons) and only when the config threshold is finite.
    if stuck_reason.is_none() && config.max_identical_snapshot_repeat >= 2 {
        let threshold = config.max_identical_snapshot_repeat.max(2);
        if health.recent_snapshot_signatures.len() >= threshold {
            let tail: Vec<u64> = health
                .recent_snapshot_signatures
                .iter()
                .rev()
                .take(threshold)
                .copied()
                .collect();
            let last_sig = tail[0];
            let all_same = tail.iter().all(|&s| s == last_sig);

            if all_same {
                stuck_reason = Some(StuckReason::NoForwardProgress {
                    stuck_signature: last_sig,
                    consecutive_count: threshold,
                });
            }
        }
    }

    // 3. Token budget warning calculation
    let (token_used_fraction, token_limit_approaching) = match config.max_token_budget {
        Some(budget) if budget > 0 => {
            let frac = health.token_spend_so_far as f32 / budget as f32;
            (frac, frac >= config.token_warning_threshold)
        }
        _ => (0.0, false),
    };

    let suspected_stuck = stuck_reason.is_some();
    let is_healthy = !suspected_stuck && !token_limit_approaching;

    WorkerHealthReport {
        worker_id: health.worker_id.clone(),
        is_healthy,
        suspected_stuck,
        stuck_reason,
        token_limit_approaching,
        token_spend_so_far: health.token_spend_so_far,
        max_token_budget: config.max_token_budget,
        token_used_fraction,
        iteration_count: health.iteration_count,
        last_heartbeat_elapsed,
        last_progress_elapsed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_healthy_worker_not_flagged() {
        let mut health = WorkerHealth::new("w-healthy");
        health.on_checkpoint();
        health.record_action(101, Some(1001));
        health.record_action(102, Some(1002));
        health.record_progress();

        let cfg = StuckDetectorConfig {
            max_token_budget: Some(10000),
            ..Default::default()
        };
        let report = detect_stuck(&health, &cfg);

        assert!(report.is_healthy);
        assert!(!report.suspected_stuck);
        assert!(report.stuck_reason.is_none());
        assert!(!report.token_limit_approaching);
    }

    #[test]
    fn test_looping_worker_flagged() {
        let mut health = WorkerHealth::new("w-loop");
        let cfg = StuckDetectorConfig {
            max_identical_action_repeat: 3,
            ..Default::default()
        };

        for _ in 0..3 {
            health.on_checkpoint();
            health.record_action(999, Some(500));
        }

        let report = detect_stuck(&health, &cfg);
        assert!(report.suspected_stuck);
        assert!(!report.is_healthy);
        match report.stuck_reason {
            Some(StuckReason::ActionLoop {
                repeated_action_hash,
                repeat_count,
            }) => {
                assert_eq!(repeated_action_hash, 999);
                assert_eq!(repeat_count, 3);
            }
            other => panic!("Expected ActionLoop, got {:?}", other),
        }
    }

    #[test]
    fn test_spinning_worker_flagged_on_timeout() {
        let mut health = WorkerHealth::new("w-spin");
        health.iteration_count = 5;
        // Simulate progress in the past
        health.last_progress_at = Instant::now() - Duration::from_secs(120);

        let cfg = StuckDetectorConfig {
            no_progress_timeout: Duration::from_secs(60),
            ..Default::default()
        };

        let report = detect_stuck(&health, &cfg);
        assert!(report.suspected_stuck);
        match report.stuck_reason {
            Some(StuckReason::NoProgressTimeout {
                iterations_completed,
                ..
            }) => {
                assert_eq!(iterations_completed, 5);
            }
            other => panic!("Expected NoProgressTimeout, got {:?}", other),
        }
    }

    #[test]
    fn test_token_limit_approaching_emitted() {
        let mut health = WorkerHealth::new("w-tokens");
        health.record_tokens(8600, false);

        let cfg = StuckDetectorConfig {
            max_token_budget: Some(10000),
            token_warning_threshold: 0.85,
            ..Default::default()
        };

        let report = detect_stuck(&health, &cfg);
        assert!(report.token_limit_approaching);
        assert!(!report.is_healthy);
        assert_eq!(report.token_used_fraction, 0.86);

        // Below threshold check
        health.record_tokens(8400, false);
        let report_below = detect_stuck(&health, &cfg);
        assert!(!report_below.token_limit_approaching);
        assert!(report_below.is_healthy);
    }

    // -----------------------------------------------------------------
    // Phase 7: NoForwardProgress detection
    // -----------------------------------------------------------------

    #[test]
    fn test_no_forward_progress_flags_identical_snapshots() {
        // The motivating case: the worker is *active* (each
        // recorded action is different) but the AX-tree
        // signature is identical across N consecutive
        // snapshots. This is the spinner pattern — the
        // LLM is doing work, the page is not changing.
        // Action hashes MUST differ so the action-loop
        // check (which runs first) does not pre-empt this
        // check.
        let mut health = WorkerHealth::new("w-spinner");
        let cfg = StuckDetectorConfig::default(); // threshold 4

        let mut action_counter = 0u64;
        for _ in 0..4 {
            health.on_checkpoint();
            action_counter += 1;
            health.record_action(action_counter, Some(0xAABBCCDD));
        }

        let report = detect_stuck(&health, &cfg);
        assert!(report.suspected_stuck);
        assert!(!report.is_healthy);
        match report.stuck_reason {
            Some(StuckReason::NoForwardProgress {
                stuck_signature,
                consecutive_count,
            }) => {
                assert_eq!(stuck_signature, 0xAABBCCDD);
                assert_eq!(consecutive_count, 4);
            }
            other => panic!("Expected NoForwardProgress, got {:?}", other),
        }
    }

    #[test]
    fn test_no_forward_progress_resolves_when_snapshot_changes() {
        // Healthy worker: 4 different actions, the
        // snapshot evolves. The detector must NOT fire.
        let mut health = WorkerHealth::new("w-evolving");
        let cfg = StuckDetectorConfig::default();

        let mut action_counter = 0u64;
        for sig in [0x1000u64, 0x1000, 0x1000, 0x2000] {
            health.on_checkpoint();
            action_counter += 1;
            health.record_action(action_counter, Some(sig));
        }

        let report = detect_stuck(&health, &cfg);
        assert!(!report.suspected_stuck);
        assert!(report.is_healthy);
        assert!(report.stuck_reason.is_none());
    }

    #[test]
    fn test_no_forward_progress_under_threshold_does_not_fire() {
        // 3 identical snapshots with the default threshold
        // of 4 — the "click into field, type, press enter"
        // legitimate case. The detector must not fire.
        // Action hashes differ so the action-loop check
        // doesn't pre-empt.
        let mut health = WorkerHealth::new("w-typing");
        let cfg = StuckDetectorConfig::default();

        let mut action_counter = 0u64;
        for _ in 0..3 {
            health.on_checkpoint();
            action_counter += 1;
            health.record_action(action_counter, Some(0xCAFE));
        }

        let report = detect_stuck(&health, &cfg);
        assert!(!report.suspected_stuck);
        assert!(report.is_healthy);
    }

    #[test]
    fn test_no_forward_progress_disabled_when_threshold_max() {
        // Operator opt-out: setting the threshold to
        // usize::MAX disables the check. The detector must
        // not fire even with N identical snapshots.
        // Action hashes differ so the action-loop check
        // doesn't pre-empt.
        let mut health = WorkerHealth::new("w-disabled");
        let cfg = StuckDetectorConfig {
            max_identical_snapshot_repeat: usize::MAX,
            ..Default::default()
        };

        let mut action_counter = 0u64;
        for _ in 0..20 {
            health.on_checkpoint();
            action_counter += 1;
            health.record_action(action_counter, Some(0xDEAD));
        }

        let report = detect_stuck(&health, &cfg);
        assert!(!report.suspected_stuck);
    }

    #[test]
    fn test_action_loop_wins_over_no_forward_progress() {
        // When the same action is being repeated AND the
        // snapshot is identical, ActionLoop is the more
        // specific signal — it tells the supervisor
        // *exactly* what to do (issue a nudge, "click
        // something different"). The no-forward-progress
        // check must not override it.
        let mut health = WorkerHealth::new("w-both");
        let cfg = StuckDetectorConfig::default();

        for _ in 0..4 {
            health.on_checkpoint();
            health.record_action(0xBEEF, Some(0xABCD));
        }

        let report = detect_stuck(&health, &cfg);
        assert!(matches!(report.stuck_reason, Some(StuckReason::ActionLoop { .. })));
    }

    #[test]
    fn test_no_forward_progress_with_mixed_action_loop_history() {
        // The realistic spinner: an action loop happens,
        // the supervisor nudges, the worker tries a
        // different action — but the snapshot is still
        // unchanged. The action-loop check no longer
        // fires (the action is now different), so the
        // no-forward-progress check takes over. To test
        // this cleanly we need the recent-action window
        // to NOT contain 3 same-hash actions, but the
        // recent-snapshot window to contain 4 same
        // signatures.
        let mut health = WorkerHealth::new("w-post-nudge");
        let cfg = StuckDetectorConfig::default();

        // Phase 1: 3 same-action ticks with same snapshot.
        // This drops out of the recent_action window as
        // we add more ticks below.
        for _ in 0..3 {
            health.on_checkpoint();
            health.record_action(0xA, Some(0xCAFE));
        }
        // Phase 2: 4 different-action ticks but the
        // snapshot is still unchanged. The most recent 4
        // actions are now (B, B, B, B) — wait, that's
        // still an action loop. We need the actions in
        // the most recent 3 to differ. Use 3 different
        // action hashes in the most recent 3 ticks; the
        // 4th tick can match one of the earlier ones.
        // Simplest: alternate two distinct hashes so the
        // rolling 3-window never has 3 identical.
        for hash in [0xB, 0xC, 0xD, 0xE] {
            health.on_checkpoint();
            health.record_action(hash, Some(0xCAFE));
        }

        let report = detect_stuck(&health, &cfg);
        assert!(report.suspected_stuck);
        match report.stuck_reason {
            Some(StuckReason::NoForwardProgress {
                stuck_signature,
                consecutive_count,
            }) => {
                assert_eq!(stuck_signature, 0xCAFE);
                assert_eq!(consecutive_count, 4);
            }
            other => panic!(
                "Expected NoForwardProgress after action-loop history, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_snapshot_window_resets_on_signature_change() {
        // If a single snapshot changes mid-window, the
        // rolling window must reset — the most recent 4
        // must all match for the detector to fire. Use
        // different action hashes so the action-loop
        // check doesn't pre-empt.
        let mut health = WorkerHealth::new("w-reset");
        let cfg = StuckDetectorConfig::default();

        let mut action_counter = 0u64;
        // 3 identical, 1 different, 3 identical — only
        // the most recent 3 are the same, threshold is 4,
        // so no fire.
        for _ in 0..3 {
            health.on_checkpoint();
            action_counter += 1;
            health.record_action(action_counter, Some(0xAAAA));
        }
        health.on_checkpoint();
        action_counter += 1;
        health.record_action(action_counter, Some(0xBBBB));
        for _ in 0..3 {
            health.on_checkpoint();
            action_counter += 1;
            health.record_action(action_counter, Some(0xAAAA));
        }

        let report = detect_stuck(&health, &cfg);
        assert!(!report.suspected_stuck);
    }
}
