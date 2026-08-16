// mew v2 — Phase 27: Respawn Policy Engine.
//
// Governs supervisor decisions when workers experience loops (Stuck),
// approach token budget limits (TokenLimitApproaching), crash (Crashed),
// or become unresponsive (UnresponsiveHeartbeat).
//
// Strategies:
//   * Stuck: Attempt 1 corrective prompt nudge first. Only respawn if stuck
//     persists for N bounded iterations after nudge. If nudge resolves issue,
//     skip respawn.
//   * TokenLimitApproaching: Proactively respawn BEFORE hard budget cutoff.
//   * Crashed / UnresponsiveHeartbeat: Immediately respawn (no nudge possible).
//   * Cap: Max respawns per task (default 2, configurable).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

use crate::handoff::RespawnHandoff;
use crate::supervisor::RespawnReason;
use mew_resilience::stuck_detector::{StuckReason, WorkerHealthReport};

/// Evaluated supervisor decision for a worker's health status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "details")]
pub enum RespawnDecision {
    /// Worker is functioning normally; no supervisor intervention needed.
    NoAction,
    /// Inject a corrective prompt hint into the worker's next iteration.
    Nudge { instruction: String },
    /// Teardown current worker process and spin up a fresh replacement worker.
    Respawn {
        reason: RespawnReason,
        attempt: usize,
    },
    /// Max respawn cap reached; fail the task with terminal error reason.
    Fail { reason: String },
}

/// Tracks task execution state and enforces supervisor respawn policy rules.
#[derive(Debug, Clone)]
pub struct RespawnPolicy {
    pub max_respawns_per_task: usize,
    pub heartbeat_timeout: Duration,
    respawn_counts: HashMap<String, usize>,
    nudge_pending: HashMap<String, bool>,
    nudge_counts: HashMap<String, usize>,
    stuck_since_iteration: HashMap<String, u32>,
    respawn_history: HashMap<String, Vec<RespawnHandoff>>,
}

impl Default for RespawnPolicy {
    fn default() -> Self {
        Self {
            max_respawns_per_task: 2,
            heartbeat_timeout: Duration::from_secs(30),
            respawn_counts: HashMap::new(),
            nudge_pending: HashMap::new(),
            nudge_counts: HashMap::new(),
            stuck_since_iteration: HashMap::new(),
            respawn_history: HashMap::new(),
        }
    }
}

impl RespawnPolicy {
    pub fn new(max_respawns_per_task: usize, heartbeat_timeout: Duration) -> Self {
        Self {
            max_respawns_per_task,
            heartbeat_timeout,
            respawn_counts: HashMap::new(),
            nudge_pending: HashMap::new(),
            nudge_counts: HashMap::new(),
            stuck_since_iteration: HashMap::new(),
            respawn_history: HashMap::new(),
        }
    }

    pub fn respawn_count(&self, task_id: &str) -> usize {
        self.respawn_counts.get(task_id).copied().unwrap_or(0)
    }

    pub fn nudge_count(&self, task_id: &str) -> usize {
        self.nudge_counts.get(task_id).copied().unwrap_or(0)
    }

    pub fn is_nudge_pending(&self, task_id: &str) -> bool {
        self.nudge_pending.get(task_id).copied().unwrap_or(false)
    }

    pub fn history(&self, task_id: &str) -> &[RespawnHandoff] {
        self.respawn_history
            .get(task_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn record_handoff(&mut self, task_id: &str, handoff: RespawnHandoff) {
        self.respawn_history
            .entry(task_id.to_string())
            .or_default()
            .push(handoff);
    }

    pub fn reset_task(&mut self, task_id: &str) {
        self.respawn_counts.remove(task_id);
        self.nudge_pending.remove(task_id);
        self.nudge_counts.remove(task_id);
        self.stuck_since_iteration.remove(task_id);
        self.respawn_history.remove(task_id);
    }

    /// Primary policy evaluation function. Accepts `WorkerHealthReport` and determines
    /// whether to do nothing, issue a prompt nudge, respawn worker, or mark task as failed.
    pub fn evaluate(&mut self, task_id: &str, report: &WorkerHealthReport) -> RespawnDecision {
        // 1. Check for unresponsive heartbeat (distinct from stuck-but-alive)
        if report.last_heartbeat_elapsed > self.heartbeat_timeout {
            let reason = RespawnReason::UnresponsiveHeartbeat {
                elapsed_secs: report.last_heartbeat_elapsed.as_secs(),
            };
            return self.trigger_respawn_or_fail(task_id, reason);
        }

        // 2. Check for proactive token budget limit approaching
        if report.token_limit_approaching {
            let reason = RespawnReason::TokenLimitApproaching {
                spend: report.token_spend_so_far,
                max: report.max_token_budget.unwrap_or(0),
            };
            return self.trigger_respawn_or_fail(task_id, reason);
        }

        // 3. Check for stuck state (ActionLoop or NoProgressTimeout)
        if report.suspected_stuck {
            let stuck_desc = match &report.stuck_reason {
                Some(StuckReason::ActionLoop {
                    repeated_action_hash,
                    repeat_count,
                }) => {
                    format!(
                        "Action loop detected: action {:x} repeated {} times",
                        repeated_action_hash, repeat_count
                    )
                }
                Some(StuckReason::NoProgressTimeout {
                    elapsed,
                    iterations_completed,
                }) => {
                    format!(
                        "Progress stall: {} iterations completed over {:?}",
                        iterations_completed, elapsed
                    )
                }
                None => "Suspected stuck state detected".to_string(),
            };

            let is_nudge_active = self.is_nudge_pending(task_id);
            let first_stuck_iter = *self
                .stuck_since_iteration
                .entry(task_id.to_string())
                .or_insert(report.iteration_count);

            if !is_nudge_active {
                // Issue bounded nudge first!
                self.nudge_pending.insert(task_id.to_string(), true);
                *self.nudge_counts.entry(task_id.to_string()).or_insert(0) += 1;
                return RespawnDecision::Nudge {
                    instruction: format!(
                        "System nudge: You appear to be repeating actions or stalled without progress ({}). Try a different approach or click an alternative element.",
                        stuck_desc
                    ),
                };
            } else {
                // Nudge was already issued for this stuck episode. Give 2 grace iterations post-nudge before respawning.
                if report.iteration_count.saturating_sub(first_stuck_iter) >= 2 {
                    let reason = RespawnReason::Stuck(stuck_desc);
                    return self.trigger_respawn_or_fail(task_id, reason);
                } else {
                    // Grace window active after nudge
                    return RespawnDecision::NoAction;
                }
            }
        } else {
            // Worker is healthy / not stuck. Reset nudge state if previously active!
            if self.is_nudge_pending(task_id) {
                self.nudge_pending.insert(task_id.to_string(), false);
                self.stuck_since_iteration.remove(task_id);
            }
        }

        RespawnDecision::NoAction
    }

    /// Evaluates explicit process crash or CDP connection drop.
    pub fn handle_crash(&mut self, task_id: &str, crash_reason: &str) -> RespawnDecision {
        let reason = RespawnReason::Crashed(crash_reason.to_string());
        self.trigger_respawn_or_fail(task_id, reason)
    }

    fn trigger_respawn_or_fail(&mut self, task_id: &str, reason: RespawnReason) -> RespawnDecision {
        let current_count = self.respawn_count(task_id);
        if current_count >= self.max_respawns_per_task {
            let base_reason = format!(
                "exhausted maximum respawn limit ({}/{}) due to {}",
                current_count,
                self.max_respawns_per_task,
                reason.as_str()
            );
            let history = self.history(task_id);
            let summary = RespawnHandoff::format_multi_attempt_summary(history, &base_reason);
            RespawnDecision::Fail { reason: summary }
        } else {
            let next_attempt = current_count + 1;
            self.respawn_counts
                .insert(task_id.to_string(), next_attempt);
            self.nudge_pending.insert(task_id.to_string(), false);
            self.stuck_since_iteration.remove(task_id);
            RespawnDecision::Respawn {
                reason,
                attempt: next_attempt,
            }
        }
    }
}
