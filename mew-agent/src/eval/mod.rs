//! mew v2 — Phase 9: evaluation harness.
//!
//! Background: the rest of the crate has unit tests for every
//! individual surface (planner, handoff, summarizer, the four
//! resilience detectors, the orchestrator's event sequence), but
//! there was no *task-level* test — a fixed scenario "go to X and
//! do Y" that the eval runner replays end-to-end and grades. The
//! Phase 9 spec calls for a local mock-site harness so the
//! regression suite doesn't depend on live third-party sites, a
//! test runner that records success / steps / time / which of
//! the six Phase 6 failure modes fired, and handoff-specific
//! assertions for the `ChatAgent → BrowserAgent → ChatAgent`
//! round trip.
//!
//! Design notes (chosen up front, applies to the whole module):
//!
//! * All scenarios are pure-Rust. They live entirely on
//!   `mew_perception::TreeNode` and the typed `BrowserResult`
//!   shape; there is no live Chrome, no LLM call, no live
//!   network. The same fixtures in `mew_resilience::mock_fixtures`
//!   that drive the unit tests are the building blocks here.
//!   That's what makes the suite runnable in CI and on a
//!   laptop in 50ms.
//! * A scenario is a *typed value*, not a trait. The
//!   `Scenario` struct carries the user task, the page state
//!   the agent would see, the expected terminal state, and the
//!   failure modes the scenario is *known* to trip. That makes
//!   the suite data-driven: adding a new scenario is one
//!   constructor call, not a new trait impl.
//! * The runner never calls `Agent::run`. It calls
//!   `ChatAgent::synthesize_reply` (which is pure-Rust) and
//!   asserts the round-trip's *shape* — the right task got
//!   dispatched, the result was reflected in the chat reply,
//!   and failure paths still produce a user-facing message. The
//!   LLM-driven loop is exercised in a separate suite (Phase 7
//!   benchmarks, when the env prerequisites are present).
//! * Assertions live in `assertions.rs` and are *reusable*: a
//!   test outside this module can `use mew_agent::eval::assertions::*`
//!   and check the same handoff contract without rebuilding
//!   the runner.
//!
//! The `eval` feature flag gates the whole module. The default
//! build (`cargo test -p mew-agent`) does not compile it so
//! a tree without the fixtures dir still builds cleanly. CI
//! runs the full suite with `cargo test --features eval
//! -p mew-agent`.

pub mod assertions;
pub mod harness;
pub mod report;
pub mod runner;

pub use harness::{Scenario, ScenarioOutcome};
pub use report::{EvalReport, RunMetrics};
pub use runner::{run_scenario, run_scenarios};
