//! Phase 9.2 — eval report shape.
//!
//! The runner produces an `EvalReport` for one or more
//! scenarios. The report is a typed value with a `to_markdown`
//! method that turns it into a human-readable table for
//! `docs/eval-history.md` and a `to_csv` method for
//! downstream tooling (regression dashboards, etc.). A future
//! live-LLM integration will append more columns (per-step
//! LLM cost, latency histogram) without changing the
//! scenario-row shape — the report is deliberately additive.

use std::fmt::Write as _;
use std::time::Duration;

use crate::handoff::BrowserStatus;

use super::harness::ScenarioOutcome;

/// Per-scenario metrics recorded by the runner. One row per
/// scenario. Stable field order so a CSV consumer can rely
/// on the column index.
#[derive(Debug, Clone)]
pub struct RunMetrics {
    /// Stable scenario id (mirrors `Scenario::id`).
    pub scenario_id: String,
    /// Did the round-trip end in the expected state?
    pub passed: bool,
    /// The terminal `BrowserStatus` of the round-trip.
    pub status: BrowserStatus,
    /// The number of subtasks the planner produced for
    /// the scenario's task.
    pub subtask_count: usize,
    /// The number of `key_findings` the round-trip
    /// produced. Mirrors `step_count` in the
    /// orchestrator's `TaskCompleted` event.
    pub step_count: u32,
    /// Wall-clock time the runner spent on this scenario.
    pub elapsed: Duration,
    /// The failure modes the resilience detectors
    /// flagged on the scenario's `page_state`. Strings are
    /// the stable `FailureMode::as_str()` values.
    pub failure_modes_hit: Vec<String>,
    /// On failure, a one-line reason.
    pub failure_reason: String,
    /// The agent's user-facing chat reply. Empty for
    /// `Failed` (the orchestrator's `synthesize_reply` is
    /// the source of truth for "is this empty?").
    pub chat_reply: String,
}

impl RunMetrics {
    pub fn from_outcome(o: &ScenarioOutcome, elapsed: Duration) -> Self {
        Self {
            scenario_id: o.scenario_id.clone(),
            passed: o.passed,
            status: o.status,
            subtask_count: o.subtask_count,
            step_count: o.step_count,
            elapsed,
            failure_modes_hit: o.failure_modes_hit.clone(),
            failure_reason: o.failure_reason.clone(),
            chat_reply: o.chat_reply.clone(),
        }
    }
}

/// Top-level report. The runner returns one of these for
/// each invocation. `markdown` and `csv` are the two
/// serialization shapes; the typed fields are the source
/// of truth.
#[derive(Debug, Clone)]
pub struct EvalReport {
    pub started_at_unix_secs: u64,
    pub rows: Vec<RunMetrics>,
}

impl EvalReport {
    pub fn new(started_at_unix_secs: u64) -> Self {
        Self {
            started_at_unix_secs,
            rows: Vec::new(),
        }
    }

    pub fn push(&mut self, m: RunMetrics) {
        self.rows.push(m);
    }

    /// Aggregate pass rate (0.0..=1.0). `None` for an empty
    /// report.
    pub fn pass_rate(&self) -> Option<f64> {
        if self.rows.is_empty() {
            return None;
        }
        let passed = self.rows.iter().filter(|r| r.passed).count();
        Some(passed as f64 / self.rows.len() as f64)
    }

    /// Total wall-clock time the runner spent across all
    /// scenarios.
    pub fn total_elapsed(&self) -> Duration {
        self.rows.iter().map(|r| r.elapsed).sum()
    }

    /// Render the report as a Markdown table. The header
    /// row is fixed; the body rows follow the same column
    /// order. `failure_modes_hit` is rendered as a
    /// comma-separated string in the last column.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("| Scenario | Passed | Status | Subtasks | Steps | Elapsed (ms) | Failure modes |\n");
        out.push_str("| --- | --- | --- | --- | --- | --- | --- |\n");
        for r in &self.rows {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} | {} |",
                r.scenario_id,
                if r.passed { "✓" } else { "✗" },
                r.status.as_str(),
                r.subtask_count,
                r.step_count,
                r.elapsed.as_millis(),
                r.failure_modes_hit.join(", "),
            );
        }
        if let Some(rate) = self.pass_rate() {
            let _ = writeln!(
                out,
                "\n**Pass rate**: {:.0}% ({}/{})  |  **Total time**: {} ms",
                rate * 100.0,
                self.rows.iter().filter(|r| r.passed).count(),
                self.rows.len(),
                self.total_elapsed().as_millis(),
            );
        }
        out
    }

    /// Render the report as CSV. The header row matches
    /// the `RunMetrics` field order so a regression
    /// dashboard can `pandas.read_csv(...)` the file
    /// without remapping.
    pub fn to_csv(&self) -> String {
        let mut out = String::new();
        out.push_str("scenario_id,passed,status,subtask_count,step_count,elapsed_ms,failure_modes_hit,failure_reason\n");
        for r in &self.rows {
            // Quote failure_modes_hit with a safe CSV
            // encoding (the strings come from
            // `FailureMode::as_str()` so they never contain
            // commas, but we still escape defensively).
            let modes = r.failure_modes_hit.join(";");
            let reason = escape_csv(&r.failure_reason);
            let _ = writeln!(
                out,
                "{},{},{},{},{},{},{},{}",
                r.scenario_id,
                if r.passed { "true" } else { "false" },
                r.status.as_str(),
                r.subtask_count,
                r.step_count,
                r.elapsed.as_millis(),
                modes,
                reason,
            );
        }
        out
    }
}

fn escape_csv(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sample_row(id: &str, passed: bool) -> RunMetrics {
        RunMetrics {
            scenario_id: id.into(),
            passed,
            status: BrowserStatus::Done,
            subtask_count: 2,
            step_count: 2,
            elapsed: Duration::from_millis(12),
            failure_modes_hit: vec!["modal_interruption".into()],
            failure_reason: String::new(),
            chat_reply: "ok".into(),
        }
    }

    #[test]
    fn pass_rate_handles_empty() {
        let r = EvalReport::new(0);
        assert_eq!(r.pass_rate(), None);
    }

    #[test]
    fn pass_rate_counts() {
        let mut r = EvalReport::new(0);
        r.push(sample_row("a", true));
        r.push(sample_row("b", false));
        r.push(sample_row("c", true));
        assert_eq!(r.pass_rate(), Some(2.0 / 3.0));
    }

    #[test]
    fn markdown_includes_all_rows_and_pass_rate() {
        let mut r = EvalReport::new(0);
        r.push(sample_row("a", true));
        r.push(sample_row("b", false));
        let md = r.to_markdown();
        assert!(md.contains("| a |"));
        assert!(md.contains("| b |"));
        assert!(md.contains("Pass rate"));
    }

    #[test]
    fn csv_has_header_and_one_row_per_metric() {
        let mut r = EvalReport::new(0);
        r.push(sample_row("a", true));
        r.push(sample_row("b", false));
        let csv = r.to_csv();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3, "header + 2 rows");
        assert!(lines[0].starts_with("scenario_id,"));
    }

    #[test]
    fn csv_escapes_commas_in_failure_reason() {
        let mut r = EvalReport::new(0);
        let mut row = sample_row("a", false);
        row.failure_reason = "boom, then another".into();
        r.push(row);
        let csv = r.to_csv();
        // The row should quote the failure_reason.
        assert!(csv.contains("\"boom, then another\""));
    }
}
