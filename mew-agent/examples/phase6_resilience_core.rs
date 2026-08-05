//! Phase 6 — Resilience Core end-to-end smoke test.
//!
//! What this example does (no Chrome, no LLM — pure-Rust):
//!
//! 1. Walks the six failure-mode mock pages one at a time.
//! 2. For each page, runs the agent's `evaluate_page` hook and
//!    asserts the *decision* (AutoDismiss / Backoff /
//!    SurfaceAsFinding / PauseForUser / Continue) is what we
//!    expect for that page.
//! 3. Walks the irreversible-classification rules against a
//!    small fixture set and asserts each verdict.
//! 4. Walks the ref-recovery rules against a small set of
//!    ref-drift scenarios and asserts each outcome.
//! 5. Walks the vision-confidence scorer against three
//!    descriptions and asserts the confidence is in the right
//!    band for each.
//!
//! Why this is an `example` (not a test): the existing
//! `mew-agent/src/resilience.rs` test mod covers the individual
//! cases; this example composes them in a single run to verify
//! the cross-module behavior end-to-end without instantiating a
//! real `Agent`. The example is also a *user-facing* document —
//! the printout is what a reviewer reads to see "yes, the
//! resilience core catches all six failure modes" at a glance.
//!
//! Run with:
//!   cargo run --example phase6_resilience_core -p mew-agent

use mew_resilience::mock_fixtures;
use mew_resilience::{
    RefActionKind, RefRecoveryConfig, RefRecoveryInputs, RefRecoveryOutcome, VisionConfidence,
    VisionVerdict, attempt_recovery, classify_irreversible, score_vision,
};
use mew_agent::resilience::{
    ResilienceHookOutcome, evaluate_dispatch, evaluate_page, page_looks_dashboard_like,
};
use serde_json::json;

fn section(name: &str) {
    println!("\n=== {} ===", name);
}

fn assert_eq<T: PartialEq + std::fmt::Debug>(label: &str, actual: T, expected: T) {
    if actual == expected {
        println!("  [OK]   {} = {:?}", label, actual);
    } else {
        println!("  [FAIL] {}: expected {:?}, got {:?}", label, expected, actual);
        std::process::exit(1);
    }
}

fn main() {
    println!("Phase 6 — Resilience Core end-to-end smoke test");
    println!("=================================================");

    // Section 1: page-state detectors. The agent's
    // `evaluate_page` is the single entry point that scans a
    // tree for the three page-wide failure modes (rate limit,
    // session loss, modal). For each fixture we assert the
    // decision is what we expect.
    section("1. Page-state detectors");
    {
        // 1a. Clean homepage — no finding.
        let report = evaluate_page(&mock_fixtures::clean_homepage(), false);
        assert_eq("clean homepage", report.outcome, ResilienceHookOutcome::Continue);
    }
    {
        // 1b. Cookie banner — auto-dismiss.
        let report = evaluate_page(&mock_fixtures::cookie_banner_page(), false);
        match report.outcome {
            ResilienceHookOutcome::AutoDismiss { dismiss_ref } => {
                assert_eq("cookie banner dismiss_ref", dismiss_ref, "@e1".to_string());
            }
            other => {
                println!("  [FAIL] cookie banner expected AutoDismiss, got {:?}", other);
                std::process::exit(1);
            }
        }
    }
    {
        // 1c. HTTP 429 — backoff.
        let report = evaluate_page(&mock_fixtures::http_429_page(), false);
        assert_eq("429 backoff", report.outcome, ResilienceHookOutcome::Backoff { secs: 30 });
    }
    {
        // 1d. Cloudflare — backoff with 15s default.
        let report = evaluate_page(&mock_fixtures::cloudflare_page(), false);
        assert_eq("cloudflare backoff", report.outcome, ResilienceHookOutcome::Backoff { secs: 15 });
    }
    {
        // 1e. Login wall — strong session-loss signal fires
        // regardless of the prior (the strong-signal branch
        // doesn't depend on prior_was_dashboard_like).
        let report = evaluate_page(&mock_fixtures::login_wall_page(), false);
        match &report.outcome {
            ResilienceHookOutcome::SurfaceAsFinding { kind, summary } => {
                assert_eq("login wall kind", kind.clone(), "session_loss".to_string());
                assert!(
                    summary.contains("Sign in"),
                    "login wall summary should mention 'Sign in', got: {}",
                    summary
                );
            }
            other => {
                println!("  [FAIL] login wall expected SurfaceAsFinding(session_loss), got {:?}", other);
                std::process::exit(1);
            }
        }
    }
    {
        // 1f. Age gate — no dismiss ref, so the modal
        // detector returns SurfaceAsFinding. This is the
        // expected behavior: age gates usually need real
        // input, so the loop should not auto-dismiss.
        let report = evaluate_page(&mock_fixtures::age_gate_page(), false);
        match &report.outcome {
            ResilienceHookOutcome::SurfaceAsFinding { kind, .. } => {
                assert_eq("age gate kind", kind.clone(), "modal".to_string());
            }
            other => {
                println!("  [FAIL] age gate expected SurfaceAsFinding(modal), got {:?}", other);
                std::process::exit(1);
            }
        }
    }

    // Section 2: dashboard-like detection. The agent
    // updates `prior_was_dashboard_like` after every
    // perception cycle. The session-loss detector consumes
    // it on the *next* cycle.
    section("2. Dashboard-like detection");
    {
        assert_eq("dashboard is dashboard-like", page_looks_dashboard_like(&mock_fixtures::dashboard_page()), true);
        assert_eq("clean homepage is not", page_looks_dashboard_like(&mock_fixtures::clean_homepage()), false);
        assert_eq("login wall is not", page_looks_dashboard_like(&mock_fixtures::login_wall_page()), false);
    }

    // Section 3: irreversible-action classification. The
    // agent calls `evaluate_dispatch` before executing any
    // tool; if the verdict is `Some(PauseForUser)` the
    // session moves to `Paused` and the user is asked to
    // confirm.
    section("3. Irreversible-action classification");
    {
        let v = evaluate_dispatch("send_message", &json!({ "to": "@alice", "text": "hi" }));
        match v {
            Some(ResilienceHookOutcome::PauseForUser { target, action_kind }) => {
                assert_eq("send_message action_kind", action_kind, "send".to_string());
                assert!(target.contains("@alice"), "send_message target should contain @alice, got: {}", target);
            }
            other => {
                println!("  [FAIL] send_message expected PauseForUser, got {:?}", other);
                std::process::exit(1);
            }
        }
    }
    {
        let v = evaluate_dispatch("pay", &json!({ "amount": 42.5, "to": "ACME" }));
        assert_eq("pay is irreversible", v.is_some(), true);
    }
    {
        let v = evaluate_dispatch("click", &json!({ "ref": "@e1" }));
        assert_eq("click is reversible", v.is_none(), true);
    }
    {
        let v = evaluate_dispatch("snapshot", &json!({}));
        assert_eq("snapshot is reversible", v.is_none(), true);
    }
    {
        // Direct classifier call (bypassing the adapter)
        // to assert the underlying rule.
        let verdict = classify_irreversible("delete_post", &json!({ "post_id": "p123" }));
        assert!(verdict.is_some(), "delete_post should be irreversible");
    }

    // Section 4: ref-recovery. The agent calls
    // `attempt_recovery` when the CDP layer returns a stale
    // ref. The decision is Retry (auto-recover), Escalate
    // (let the LLM re-evaluate), or Abort (unsafe to retry).
    section("4. Ref-recovery");
    {
        // 4a. Transient drift: same ref still in the new
        // map -> Retry with the same ref.
        let mut refs = std::collections::HashMap::new();
        refs.insert("@e42".to_string(), ());
        let inputs = RefRecoveryInputs {
            supplied_ref: "@e42",
            current_ref_map: &refs,
            target_desc: None,
            description_index: None,
            action: RefActionKind::Click,
            attempts_so_far: 0,
        };
        let outcome = attempt_recovery(&RefRecoveryConfig::default(), &inputs);
        match outcome {
            RefRecoveryOutcome::Retry { new_ref, .. } => {
                assert_eq("transient drift retry ref", new_ref, "@e42".to_string());
            }
            other => {
                println!("  [FAIL] transient drift expected Retry, got {:?}", other);
                std::process::exit(1);
            }
        }
    }
    {
        // 4b. Budget exhausted: second auto-retry on a
        // still-stale ref -> EscalateToLLM.
        let refs = std::collections::HashMap::new();
        let inputs = RefRecoveryInputs {
            supplied_ref: "@e42",
            current_ref_map: &refs,
            target_desc: None,
            description_index: None,
            action: RefActionKind::Click,
            attempts_so_far: 1, // already used the 1-retry budget
        };
        let outcome = attempt_recovery(&RefRecoveryConfig::default(), &inputs);
        assert!(matches!(outcome, RefRecoveryOutcome::EscalateToLLM { .. }));
        println!("  [OK]   budget exhausted escalates to LLM");
    }
    {
        // 4c. Type action on an empty ref map -> Abort
        // (non-idempotent action against a guessed target
        // is unsafe).
        let refs = std::collections::HashMap::new();
        let inputs = RefRecoveryInputs {
            supplied_ref: "@e42",
            current_ref_map: &refs,
            target_desc: None,
            description_index: None,
            action: RefActionKind::Type,
            attempts_so_far: 0,
        };
        let outcome = attempt_recovery(&RefRecoveryConfig::default(), &inputs);
        assert!(matches!(outcome, RefRecoveryOutcome::AbortWithReason { .. }));
        println!("  [OK]   type-on-empty-map aborts");
    }

    // Section 5: vision-confidence scoring. The agent
    // wraps every `vision_inspect` result in `score()` and
    // gates on the threshold.
    section("5. Vision-confidence scoring");
    {
        let v: VisionVerdict = score_vision("", None);
        assert_eq("empty description is 0.0", v.confidence.score, 0.0_f32);
    }
    {
        let v = score_vision("I think this is a button", None);
        assert!(
            v.confidence.score < 0.5,
            "uncertain phrase should be below 0.5, got {}",
            v.confidence.score
        );
        println!("  [OK]   uncertain phrase -> {:.2} (below 0.5)", v.confidence.score);
    }
    {
        let v = score_vision(
            "A blue rectangular submit button with white text reading 'Sign in'",
            None,
        );
        assert!(
            v.confidence.is_acceptable(0.5),
            "substantive description should be acceptable, got {}",
            v.confidence.score
        );
        println!("  [OK]   substantive description -> {:.2} (acceptable)", v.confidence.score);
    }
    {
        // Large box -> tighten suggestion.
        let v = score_vision("a button", Some((10.0, 10.0, 400.0, 300.0)));
        assert!(v.tighten_crop.is_some(), "large box should suggest a tighter crop");
        println!("  [OK]   large box -> tighten suggestion present");
    }
    {
        // Confidence threshold accessor.
        let c = VisionConfidence { score: 0.3 };
        assert_eq("0.3 not acceptable at 0.5", c.is_acceptable(0.5), false);
        let c = VisionConfidence { score: 0.6 };
        assert_eq("0.6 acceptable at 0.5", c.is_acceptable(0.5), true);
    }

    println!("\nAll six failure modes verified. Resilience core end-to-end test PASSED.");
}
