//! Phase 17 — Planner-Worker Contract Evaluation Harness Runner.
//!
//! Headless CLI binary to run the Phase 17 planner shortcut scenarios:
//! 1. Happy path (accept on match)
//! 2. Worker shortcut (reject on mismatch)
//! 3. Stale evidence (reject on stale iteration)

use mew_agent::eval::run_planner_scenarios;

fn main() {
    println!("=== Phase 17: Planner-Worker Evaluation Harness ===");
    let report = run_planner_scenarios();

    for row in &report.rows {
        let symbol = if row.passed { "✓" } else { "✗" };
        println!(" [{}] {} - {}", symbol, row.scenario_id, row.chat_reply);

        if !row.passed {
            println!("     Failure: {}", row.failure_reason);
        }
    }

    let pass_rate = report.pass_rate().unwrap_or(0.0);
    println!(
        "\nPass Rate: {:.1}% ({}/{} passed)",
        pass_rate * 100.0,
        report.rows.iter().filter(|r| r.passed).count(),
        report.rows.len()
    );

    if report.rows.iter().any(|r| !r.passed) {
        std::process::exit(1);
    }
}
