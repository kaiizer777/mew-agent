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

        if let Some(sig) = snapshot_sig {
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
}

impl Default for StuckDetectorConfig {
    fn default() -> Self {
        Self {
            max_identical_action_repeat: 3,
            no_progress_timeout: Duration::from_secs(60),
            token_warning_threshold: 0.85,
            max_token_budget: None,
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
}
