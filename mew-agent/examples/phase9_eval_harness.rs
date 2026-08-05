// mew v2 — Phase 9: evaluation harness smoke test.
//
// Purpose: prove the eval runner end-to-end on the
// default scenario set — every scenario in
// `default_scenarios()` is replayed through the same
// `ChatAgent::synthesize_reply` path the production
// orchestrator uses, the report is rendered to both
// Markdown and CSV, and the pass rate is printed.
//
// This is the "see it work" smoke test for the harness
// (similar to how `phase3_round_trip.rs` and
// `phase5_live_progress.rs` are smoke tests for their
// respective phases). The unit tests in
// `mew-agent/src/eval/` are the regression coverage;
// this example is what you run by hand to confirm the
// whole suite is healthy.
//
// Run with:
//   cargo run --example phase9_eval_harness -p mew-agent --features eval
//
// No live Chrome, no live LLM, no network. The
// synthesizer is deterministic templating, so the
// report is reproducible.

use mew_agent::eval::{harness::default_scenarios, runner};

fn main() -> anyhow::Result<()> {
    println!("== Phase 9 evaluation harness ==");
    println!("Running {} default scenarios...\n", default_scenarios().len());

    let chat_agent = runner::default_chat_agent();
    let report = runner::run_scenarios(&default_scenarios(), &chat_agent);

    println!("--- Markdown report ---");
    println!("{}", report.to_markdown());
    println!();
    println!("--- CSV report ---");
    println!("{}", report.to_csv());

    let pass_rate = report.pass_rate().unwrap_or(0.0);
    println!("\nFinal pass rate: {:.0}%", pass_rate * 100.0);

    if pass_rate < 1.0 {
        eprintln!("FAIL: pass rate below 100%; see report above");
        std::process::exit(1);
    }
    println!("OK: all scenarios passed the handoff contract");
    Ok(())
}
