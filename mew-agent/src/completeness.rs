// mew v2 — Phase 15.1: Task completeness check (avoid silent partial-completion).
//
// Problem: the agent believes a task is done when it isn't. The dominant
// failure mode in the 2026 GUI-agent literature is the model calling
// `finish()` without checking real on-screen evidence, often reporting a
// blanket "done" when only some of the sub-actions actually succeeded.
//
// Fix (this file): the model still *decides* what to do, but the code owns
// the canonical answer to "what was the task supposed to accomplish" and to
// "was each item actually verified done with a fresh snapshot." The
// ReAct loop's `finish()` is intercepted by a gate that cannot be bypassed
// when any tracked subtask is still `done: false`.
//
// Design (one paragraph): the LLM calls a new `declare_subtasks` tool to
// enumerate the sub-items it intends to perform (id + description, plain
// list). The tracker stores them with `done: false`. To mark one done, the
// LLM calls `mark_subtask_done(id)` — that call is *rejected* unless a
// fresh snapshot has been taken since the previous mark or since the start
// of the loop, i.e. the evidence is guaranteed to be on-screen, not from
// stale model memory. `finish()` first checks "any subtask still not
// done?"; if yes, it forces one more snapshot iteration and injects a
// `role: user` message demanding the LLM either mark it done (with
// evidence) or explicitly tag it `skipped` (with a reason). The gate only
// releases the `finish()` on the *second* attempt, after the model has
// had a chance to actually verify. The end-of-session summary is always
// written to the transcript, including for non-`finish` exits, so a real
// partial completion is visible — not glossed over.
//
// The tracker is deliberately small. It is *not* a planner, *not* a
// verifier, and it does not interpret task text. It is a checklist the
// model itself populates, plus an evidence rule the code enforces. That
// shape is the standard "LLM decides, code decides truth" pattern from
// the Completeness Verifier research.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

/// Why a subtask ended in the state it's in now. The end-of-session
/// summary includes this so a "skipped on purpose" is reported, not
/// silently lumped in with "failed".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubTaskStatus {
    /// Subtask is on the list but not yet attempted or not yet verified.
    Pending,
    /// Subtask was attempted and verified by a fresh snapshot.
    Done,
    /// Subtask was deliberately not attempted; the reason is the model's
    /// own justification, kept here verbatim.
    Skipped { reason: String },
    /// Subtask was attempted but the action failed or the model could
    /// not verify the outcome with a fresh snapshot.
    Failed { reason: String },
}

impl SubTaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SubTaskStatus::Pending => "pending",
            SubTaskStatus::Done => "done",
            SubTaskStatus::Skipped { .. } => "skipped",
            SubTaskStatus::Failed { .. } => "failed",
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            SubTaskStatus::Pending => None,
            SubTaskStatus::Done => None,
            SubTaskStatus::Skipped { reason } => Some(reason.as_str()),
            SubTaskStatus::Failed { reason } => Some(reason.as_str()),
        }
    }
}

/// A single tracked sub-item. Identity is the model-supplied `id` (a short
/// string like "msg-to-alice"). Evidence points to a concrete iteration
/// where a fresh snapshot was taken; the value is a small opaque signature
/// derived from the page state, not the full state, so the per-subtask
/// summary stays compact.
#[derive(Debug, Clone)]
pub struct SubTask {
    pub id: String,
    pub description: String,
    pub status: SubTaskStatus,
    /// The iteration number of the snapshot that confirmed this subtask.
    /// `None` if the subtask is not yet done.
    pub evidence_iteration: Option<usize>,
    /// Opaque signature of the page state at the moment of evidence.
    /// Recorded so the summary can mention "diff showed X" without us
    /// having to re-snapshot.
    pub evidence_signature: Option<String>,
    /// Real wall-clock seconds when the subtask was marked done/skipped/
    /// failed. `None` if still pending.
    pub decided_at_secs: Option<u64>,
}

/// Result of attempting to mark a subtask done. The ReAct loop turns
/// `Rejected` into a tool error back to the model so it has to call
/// `snapshot()` (or use the prior snapshot) and try again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkOutcome {
    /// Subtask moved to Done; now confirmed with the recorded evidence.
    MarkedDone {
        evidence_iteration: usize,
        evidence_signature: String,
    },
    /// Subtask moved to Skipped with the model's reason.
    MarkedSkipped { reason: String },
    /// No such id in the tracker.
    UnknownId,
    /// Evidence was rejected: no fresh snapshot since the last mark.
    StaleEvidence { last_snapshot_iteration: usize, current_iteration: usize },
    /// The subtask is already in a terminal status.
    AlreadyTerminal { current: SubTaskStatus },
}

/// The completeness tracker. Owned by `Agent`; the ReAct loop interacts
/// with it through a handful of methods. The tracker is intentionally
/// append-mostly: declare once, then update in place, then summarize once
/// at the end. There is no "remove" operation, no re-declaration.
#[derive(Debug, Default)]
pub struct CompletenessTracker {
    pub subtasks: Vec<SubTask>,
    /// The iteration number of the most recent successful `snapshot()` /
    /// tree-extract. Used to enforce the "evidence is a fresh snapshot,
    /// not stale memory" rule.
    pub last_snapshot_iteration: Option<usize>,
    /// A small opaque signature of that snapshot — enough to log "diff
    /// signature X" in the per-subtask summary without bloating the
    /// tracker with full page state. The agent computes this itself;
    /// the tracker just stores it.
    pub last_snapshot_signature: Option<String>,
    /// How many times `finish()` was called. The gate permits a first
    /// call (which becomes a "force snapshot + re-prompt"), and only
    /// releases a second call if the tracker is clean.
    pub finish_attempts: usize,
    /// Set the first time the gate forced a snapshot+re-prompt cycle.
    /// The summary mentions this so a one-shot session that "passed"
    /// because it only had one attempt is still distinguishable from a
    /// session that genuinely needed to be re-prompted.
    pub gate_triggered: bool,
}

impl CompletenessTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Convenience: did the model bother to declare any subtasks? If
    /// not, the task is treated as a single undifferentiated unit and
    /// the gate degenerates to "the LLM said finish, just let it
    /// through" — the spec explicitly scopes this to "tasks involving
    /// multiple similar sub-actions."
    pub fn has_subtasks(&self) -> bool {
        !self.subtasks.is_empty()
    }

    /// Convenience: how many subtasks are not yet in a terminal state.
    /// Used by the gate. `Pending` counts as non-terminal.
    pub fn incomplete_count(&self) -> usize {
        self.subtasks
            .iter()
            .filter(|s| !matches!(s.status, SubTaskStatus::Done | SubTaskStatus::Skipped { .. } | SubTaskStatus::Failed { .. }))
            .count()
    }

    /// Bulk-replace the subtask list. Called from the `declare_subtasks`
    /// tool handler. The model is allowed to re-declare (a follow-up
    /// declaration is treated as "the previous list was wrong, here's
    /// the real one") — but only while every subtask is still Pending.
    /// If any subtask is already terminal, the declaration is rejected
    /// so a re-declare can't quietly wipe evidence.
    pub fn declare(&mut self, items: Vec<DeclareItem>) -> Result<usize, &'static str> {
        if self.subtasks.iter().any(|s| !matches!(s.status, SubTaskStatus::Pending)) {
            return Err("cannot re-declare subtasks after any have been resolved");
        }
        let n = items.len();
        self.subtasks = items
            .into_iter()
            .map(|d| SubTask {
                id: d.id,
                description: d.description,
                status: SubTaskStatus::Pending,
                evidence_iteration: None,
                evidence_signature: None,
                decided_at_secs: None,
            })
            .collect();
        Ok(n)
    }

    /// Record that a fresh snapshot was taken on the given iteration
    /// with the given signature. The ReAct loop calls this from the
    /// perception block — the same call site that already does
    /// `force_snapshot` — so the tracker stays accurate without the
    /// model having to do anything explicit.
    pub fn record_snapshot(&mut self, iteration: usize, signature: String) {
        self.last_snapshot_iteration = Some(iteration);
        self.last_snapshot_signature = Some(signature);
    }

    /// Attempt to mark a subtask Done. The `snapshot_signature` argument
    /// is the caller's view of the current page state; it must equal
    /// `last_snapshot_signature` (i.e., the model is not allowed to
    /// invent a signature) and `last_snapshot_iteration` must be set
    /// and not stale. Returns `MarkOutcome::StaleEvidence` if the
    /// caller needs to call `snapshot()` first.
    pub fn mark_done(
        &mut self,
        id: &str,
        snapshot_signature: &str,
    ) -> MarkOutcome {
        let Some(sub) = self.subtasks.iter_mut().find(|s| s.id == id) else {
            return MarkOutcome::UnknownId;
        };
        match &sub.status {
            SubTaskStatus::Done
            | SubTaskStatus::Skipped { .. }
            | SubTaskStatus::Failed { .. } => {
                let current = sub.status.clone();
                return MarkOutcome::AlreadyTerminal { current };
            }
            SubTaskStatus::Pending => {}
        }

        // Evidence freshness check: was a snapshot taken since the
        // last time we marked something done? If we have no snapshot
        // at all, the model is hallucinating evidence.
        let Some(last_iter) = self.last_snapshot_iteration else {
            return MarkOutcome::StaleEvidence {
                last_snapshot_iteration: 0,
                current_iteration: 0,
            };
        };
        let Some(last_sig) = self.last_snapshot_signature.as_ref() else {
            return MarkOutcome::StaleEvidence {
                last_snapshot_iteration: last_iter,
                current_iteration: last_iter,
            };
        };
        if last_sig != snapshot_signature {
            return MarkOutcome::StaleEvidence {
                last_snapshot_iteration: last_iter,
                current_iteration: last_iter,
            };
        }

        sub.status = SubTaskStatus::Done;
        sub.evidence_iteration = Some(last_iter);
        sub.evidence_signature = Some(last_sig.clone());
        sub.decided_at_secs = Some(now_secs());

        MarkOutcome::MarkedDone {
            evidence_iteration: last_iter,
            evidence_signature: last_sig.clone(),
        }
    }

    /// Mark a subtask Skipped, with the model's own reason. Skipped is
    /// also a terminal status and it does NOT require a fresh snapshot
    /// — the model is explicitly saying "I'm not doing this on purpose"
    /// rather than "I did this and saw it work." If the model marks
    /// something Skipped to dodge a real failure, the transcript will
    /// show that; a reviewer reading the per-subtask summary can see
    /// the Skip rate and judge.
    pub fn mark_skipped(&mut self, id: &str, reason: String) -> MarkOutcome {
        let Some(sub) = self.subtasks.iter_mut().find(|s| s.id == id) else {
            return MarkOutcome::UnknownId;
        };
        match &sub.status {
            SubTaskStatus::Done
            | SubTaskStatus::Skipped { .. }
            | SubTaskStatus::Failed { .. } => {
                let current = sub.status.clone();
                return MarkOutcome::AlreadyTerminal { current };
            }
            SubTaskStatus::Pending => {}
        }
        sub.status = SubTaskStatus::Skipped { reason: reason.clone() };
        sub.decided_at_secs = Some(now_secs());
        MarkOutcome::MarkedSkipped { reason }
    }

    /// Mark a subtask Failed. Different from Skipped: this means "I
    /// tried and could not verify success." Failure is also terminal
    /// and does not require a fresh snapshot — the model's account
    /// of what happened is the evidence.
    pub fn mark_failed(&mut self, id: &str, reason: String) -> MarkOutcome {
        let Some(sub) = self.subtasks.iter_mut().find(|s| s.id == id) else {
            return MarkOutcome::UnknownId;
        };
        match &sub.status {
            SubTaskStatus::Done
            | SubTaskStatus::Skipped { .. }
            | SubTaskStatus::Failed { .. } => {
                let current = sub.status.clone();
                return MarkOutcome::AlreadyTerminal { current };
            }
            SubTaskStatus::Pending => {}
        }
        sub.status = SubTaskStatus::Failed { reason: reason.clone() };
        sub.decided_at_secs = Some(now_secs());
        MarkOutcome::MarkedSkipped { reason }
    }

    /// `true` if the gate would let `finish()` through right now. The
    /// gate has two components:
    ///   1. If no subtasks were declared, the gate is a no-op (single
    ///      unit task; not what 15.1 targets).
    ///   2. If subtasks were declared, every one must be in a terminal
    ///      status (Done / Skipped / Failed).
    /// `finish_attempts` is *not* part of the gate — the loop counts
    /// attempts separately to decide whether to force a snapshot
    /// re-prompt or let `finish()` through.
    pub fn gate_open(&self) -> bool {
        if !self.has_subtasks() {
            return true;
        }
        self.incomplete_count() == 0
    }

    /// Bump the per-session attempt counter. Called by the ReAct loop
    /// from the `finish` tool handler.
    pub fn record_finish_attempt(&mut self) -> usize {
        self.finish_attempts += 1;
        self.finish_attempts
    }

    /// Mark that the gate fired (i.e. `finish()` was called while
    /// subtasks were still incomplete, forcing a snapshot re-prompt).
    /// Recorded so the end-of-session summary mentions it.
    pub fn note_gate_triggered(&mut self) {
        self.gate_triggered = true;
    }

    /// Write the per-subtask end-of-session summary to the transcript
    /// file. Always called before the loop exits, regardless of how
    /// it exits. The format is plain text, grep-friendly, and mirrors
    /// the style of the existing `[<ts>] [<session_id>] ...` lines so
    /// a transcript reviewer can find it with the same tools.
    pub fn write_summary(
        &self,
        file: Option<&std::fs::File>,
        session_id: &str,
        task_summary: &str,
    ) {
        let ts = now_secs();
        let mut out = String::new();
        out.push_str(&format!(
            "\n[{}] [{}] === COMPLETENESS SUMMARY ===\n",
            ts, session_id
        ));
        out.push_str(&format!(
            "[{}] [{}] task: {}\n",
            ts, session_id, task_summary
        ));
        if !self.has_subtasks() {
            out.push_str(&format!(
                "[{}] [{}] no subtasks were declared; gate was a no-op.\n",
                ts, session_id
            ));
        } else {
            let total = self.subtasks.len();
            let done = self
                .subtasks
                .iter()
                .filter(|s| matches!(s.status, SubTaskStatus::Done))
                .count();
            let skipped = self
                .subtasks
                .iter()
                .filter(|s| matches!(s.status, SubTaskStatus::Skipped { .. }))
                .count();
            let failed = self
                .subtasks
                .iter()
                .filter(|s| matches!(s.status, SubTaskStatus::Failed { .. }))
                .count();
            let pending = self
                .subtasks
                .iter()
                .filter(|s| matches!(s.status, SubTaskStatus::Pending))
                .count();
            out.push_str(&format!(
                "[{}] [{}] counts: total={} done={} skipped={} failed={} pending={}\n",
                ts, session_id, total, done, skipped, failed, pending
            ));
            if self.gate_triggered {
                out.push_str(&format!(
                    "[{}] [{}] gate_triggered: yes (finish() forced a snapshot re-prompt at least once)\n",
                    ts, session_id
                ));
            } else {
                out.push_str(&format!(
                    "[{}] [{}] gate_triggered: no\n",
                    ts, session_id
                ));
            }
            for (i, sub) in self.subtasks.iter().enumerate() {
                let evidence = match (&sub.status, sub.evidence_iteration, &sub.evidence_signature) {
                    (SubTaskStatus::Done, Some(it), Some(sig)) => {
                        format!("evidence=iter:{} sig:{}", it, sig)
                    }
                    _ => "evidence=none".to_string(),
                };
                let reason = sub
                    .status
                    .reason()
                    .map(|r| format!(" reason={}", r))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "[{}] [{}]   [{:>2}] id={} status={}{} desc=\"{}\"\n",
                    ts, session_id, i + 1, sub.id, sub.status.as_str(), reason, sub.description
                ));
                out.push_str(&format!(
                    "[{}] [{}]        {}\n",
                    ts, session_id, evidence
                ));
            }
        }
        out.push_str(&format!(
            "[{}] [{}] === END COMPLETENESS SUMMARY ===\n\n",
            ts, session_id
        ));

        if let Some(mut f) = file {
            let _ = f.write_all(out.as_bytes());
        }
    }

    /// Short, single-line status for live `println!` output during a
    /// session. Not the transcript version — that lives in
    /// `write_summary`. This is the thing the developer sees in their
    /// terminal while the agent is running.
    pub fn inline_status(&self) -> String {
        if !self.has_subtasks() {
            return "no subtasks declared".to_string();
        }
        let total = self.subtasks.len();
        let done = self
            .subtasks
            .iter()
            .filter(|s| matches!(s.status, SubTaskStatus::Done))
            .count();
        let skipped = self
            .subtasks
            .iter()
            .filter(|s| matches!(s.status, SubTaskStatus::Skipped { .. }))
            .count();
        let failed = self
            .subtasks
            .iter()
            .filter(|s| matches!(s.status, SubTaskStatus::Failed { .. }))
            .count();
        let pending = self.incomplete_count();
        // Show all four buckets — "0 pending" alone is misleading when
        // everything is failed (a real run of the partial-success
        // scenario showed 0/3 done, 0 pending which looked like a
        // bug, when really 3/3 had been resolved as failed).
        format!(
            "{}/{} done ({} skipped, {} failed), {} pending",
            done, total, skipped, failed, pending
        )
    }
}

/// The shape of one entry in the `declare_subtasks` tool call.
#[derive(Debug, Clone)]
pub struct DeclareItem {
    pub id: String,
    pub description: String,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests. Pure unit tests for the tracker so we don't need to spin up the
// whole agent to verify the rules. The behavior tests (snapshot-evidence
// enforcement, gate behavior) live here; the integration "did the real
// agent actually use this end-to-end" tests live in
// `examples/test_completeness.rs` and are run manually per the 15.2 spec.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    fn sig(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn declare_populates_list_and_starts_pending() {
        let mut t = CompletenessTracker::new();
        let n = t
            .declare(vec![
                DeclareItem { id: "a".into(), description: "first".into() },
                DeclareItem { id: "b".into(), description: "second".into() },
            ])
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(t.subtasks.len(), 2);
        assert!(t.subtasks.iter().all(|s| matches!(s.status, SubTaskStatus::Pending)));
    }

    #[test]
    fn mark_done_requires_fresh_snapshot() {
        let mut t = CompletenessTracker::new();
        t.declare(vec![DeclareItem { id: "a".into(), description: "first".into() }])
            .unwrap();

        // No snapshot recorded yet — must reject.
        match t.mark_done("a", &sig("")) {
            MarkOutcome::StaleEvidence { .. } => {}
            other => panic!("expected StaleEvidence, got {:?}", other),
        }

        // Now record a snapshot and mark.
        t.record_snapshot(3, sig("page-sig-1"));
        match t.mark_done("a", &sig("page-sig-1")) {
            MarkOutcome::MarkedDone { evidence_iteration, .. } => {
                assert_eq!(evidence_iteration, 3);
            }
            other => panic!("expected MarkedDone, got {:?}", other),
        }

        // Marking again is rejected as already terminal.
        match t.mark_done("a", &sig("page-sig-1")) {
            MarkOutcome::AlreadyTerminal { current } => {
                assert!(matches!(current, SubTaskStatus::Done));
            }
            other => panic!("expected AlreadyTerminal, got {:?}", other),
        }
    }

    #[test]
    fn mark_done_rejects_wrong_signature() {
        let mut t = CompletenessTracker::new();
        t.declare(vec![DeclareItem { id: "a".into(), description: "x".into() }])
            .unwrap();
        t.record_snapshot(2, sig("snap-A"));
        match t.mark_done("a", &sig("snap-B")) {
            MarkOutcome::StaleEvidence { .. } => {}
            other => panic!("expected StaleEvidence, got {:?}", other),
        }
    }

    #[test]
    fn gate_open_only_when_all_terminal() {
        let mut t = CompletenessTracker::new();
        t.declare(vec![
            DeclareItem { id: "a".into(), description: "x".into() },
            DeclareItem { id: "b".into(), description: "y".into() },
        ])
        .unwrap();
        assert!(!t.gate_open(), "fresh declaration should not be open");
        t.record_snapshot(1, sig("s1"));
        t.mark_done("a", &sig("s1")).unwrap_marker_done();
        assert!(!t.gate_open(), "one pending should keep gate closed");
        t.mark_skipped("b", "out of scope".to_string());
        assert!(t.gate_open(), "both resolved should open gate");
    }

    #[test]
    fn re_declare_rejected_after_any_resolution() {
        let mut t = CompletenessTracker::new();
        t.declare(vec![DeclareItem { id: "a".into(), description: "x".into() }])
            .unwrap();
        t.record_snapshot(1, sig("s1"));
        t.mark_done("a", &sig("s1")).unwrap_marker_done();
        let res = t.declare(vec![DeclareItem { id: "a".into(), description: "y".into() }]);
        assert!(res.is_err(), "redeclare after Done must be rejected");
    }

    #[test]
    fn empty_subtasks_means_gate_is_no_op() {
        let t = CompletenessTracker::new();
        assert!(t.gate_open(), "no subtasks declared should be an open gate");
        assert!(!t.has_subtasks());
    }

    // Tiny test helpers to keep the assertions above readable.
    trait MarkDoneExt {
        fn unwrap_marker_done(self);
    }
    impl MarkDoneExt for MarkOutcome {
        fn unwrap_marker_done(self) {
            assert!(
                matches!(self, MarkOutcome::MarkedDone { .. }),
                "expected MarkedDone, got {:?}",
                self
            );
        }
    }
}
