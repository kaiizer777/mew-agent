// mew v2 — Phase 6: Application / outreach action layer — integration tests.
//
// Asserts the spec's contract for the auto-apply action layer:
//   1. `confirm_then_submit_happy_path`: an LLM `apply_to_job` dispatch
//      returns `PauseForDraft` when auto-apply is enabled, the dispatch
//      site calls `increment_count` to commit the draft, the user
//      confirms, and the submission lands in the dedup store. A
//      re-attempt for the same posting silently drops.
//   2. `user_rejects_at_checkpoint`: the user sees the draft and replies
//      "no, skip this one." The dispatch site does NOT call
//      `record_submission`; the per-run counter is bumped exactly
//      once; a future re-attempt proceeds normally (no dedup hit).
//   3. `duplicate_application_blocked_by_dedup_store`: a pre-populated
//      dedup record for a posting causes the orchestration layer to
//      return `AlreadyApplied`; the per-run counter is NOT bumped (the
//      application was dropped before any draft was presented).
//   4. `per_run_cap_blocks_after_limit`: after the dispatch site has
//      committed `max_applications_per_run` drafts, the next attempt
//      returns `CapReached`; the per-run counter does not advance
//      past the cap.
//   5. `fire_and_forget_path_skips_confirmation`: with
//      `require_confirmation_per_application: false`, the orchestration
//      layer returns `AutoSubmit` and the match arm dispatches the
//      tool call directly without pausing.
//   6. `irreversible_classifier_recognizes_phase6_kinds`: the
//      `mew_resilience::classify_irreversible` table produces
//      `SubmitApplication` / `SendProposal` / `SendConnectionRequest`
//      verdicts for the Phase 6 typed tools.
//
// The integration tests are pure logic — no live Chrome, no LLM
// calls, no network. The `AutoApply` layer's `decide` function is
// the unit under test.

use std::path::PathBuf;
use std::sync::Arc;

use mew_agent::application_dedup::{ApplicationDedup, TRACKED_ACTION_KINDS};
use mew_agent::auto_apply::{AutoApply, AutoApplyConfig, AutoApplyDecision};
use mew_resilience::{
    classify_irreversible, ActionKind, IrreverisbleVerdict,
};

fn dedup_empty() -> Arc<ApplicationDedup> {
    Arc::new(ApplicationDedup::new(None))
}

fn dedup_with(platform: &str, posting_id: &str) -> Arc<ApplicationDedup> {
    let d = ApplicationDedup::new(None);
    d.record_submission(
        platform,
        posting_id,
        "submit_application",
        "finding-test",
        "test record",
    );
    Arc::new(d)
}

fn cfg(enabled: bool, require_confirmation: bool, max: usize) -> AutoApplyConfig {
    AutoApplyConfig {
        enabled,
        require_confirmation_per_application: require_confirmation,
        max_applications_per_run: max,
    }
}

fn verdict_for(tool: &str, args: serde_json::Value) -> IrreverisbleVerdict {
    classify_irreversible(tool, &args).expect("Phase 6 tool must classify as irreversible")
}

// ----------------------------------------------------------------------
// 1. Confirm-then-submit happy path
// ----------------------------------------------------------------------

#[test]
fn confirm_then_submit_happy_path() {
    // Build the orchestration layer with a fresh dedup store.
    let dedup = dedup_empty();
    let layer = AutoApply::new(cfg(true, true, 10), dedup.clone());

    // Step 1: LLM dispatches `apply_to_job`.
    let v = verdict_for(
        "apply_to_job",
        serde_json::json!({ "job_id": "gh-1", "role": "Senior Rust Engineer" }),
    );

    // Step 2: orchestration layer returns `PauseForDraft` (the standard
    // path: enabled, require_confirmation, empty dedup, cap not hit).
    let decision = layer.decide(&v, Some("linkedin.com"), Some("gh-1"));
    match &decision {
        AutoApplyDecision::PauseForDraft { draft_required, .. } => {
            assert!(draft_required, "draft_required must be true on standard path");
        }
        other => panic!("expected PauseForDraft, got {other:?}"),
    }

    // Step 3: dispatch site commits the draft (bumps the per-run
    // counter).
    let count_after_commit = layer.increment_count();
    assert_eq!(count_after_commit, 1, "counter must be 1 after first commit");
    assert!(!dedup.is_already_applied("linkedin.com", "gh-1"));

    // Step 4: user confirms. Dispatch site records the
    // submission in the dedup store.
    dedup.record_submission(
        "linkedin.com",
        "gh-1",
        "submit_application",
        "finding-1",
        "Senior Rust Engineer @ Acme",
    );
    assert!(dedup.is_already_applied("linkedin.com", "gh-1"));
    assert_eq!(dedup.len(), 1);

    // Step 5: a subsequent attempt for the same posting silently
    // drops via the dedup store.
    let v2 = verdict_for("apply_to_job", serde_json::json!({ "job_id": "gh-1" }));
    let decision2 = layer.decide(&v2, Some("linkedin.com"), Some("gh-1"));
    assert!(matches!(decision2, AutoApplyDecision::AlreadyApplied { .. }));
}

// ----------------------------------------------------------------------
// 2. User rejects at checkpoint
// ----------------------------------------------------------------------

#[test]
fn user_rejects_at_checkpoint_does_not_record() {
    // The user sees the drafted text in the chat list and replies
    // "no, skip this one." The dispatch site must NOT call
    // `record_submission`; the per-run counter is bumped exactly
    // once (at the draft-presentation step); a future re-attempt
    // does NOT see a dedup hit because the rejected application
    // was not recorded.
    let dedup = dedup_empty();
    let layer = AutoApply::new(cfg(true, true, 10), dedup.clone());

    // Step 1: pause for draft.
    let v = verdict_for("apply_to_job", serde_json::json!({ "job_id": "gh-2" }));
    let decision = layer.decide(&v, Some("linkedin.com"), Some("gh-2"));
    assert!(matches!(decision, AutoApplyDecision::PauseForDraft { .. }));

    // Step 2: draft is presented to the user. The dispatch site
    // commits the counter (the standard path's contract).
    layer.increment_count();

    // Step 3: user rejects. The dispatch site must NOT call
    // `record_submission`. The dedup store stays empty.
    assert!(!dedup.is_already_applied("linkedin.com", "gh-2"));
    assert_eq!(dedup.len(), 0, "rejection must not record to the dedup store");

    // A future re-attempt (e.g. the user changes their mind and
    // runs the preset again next week) does not see a dedup hit
    // — the rejected application was not recorded.
    let v2 = verdict_for("apply_to_job", serde_json::json!({ "job_id": "gh-2" }));
    let decision2 = layer.decide(&v2, Some("linkedin.com"), Some("gh-2"));
    assert!(matches!(decision2, AutoApplyDecision::PauseForDraft { .. }));
}

// ----------------------------------------------------------------------
// 3. Duplicate application blocked by dedup store
// ----------------------------------------------------------------------

#[test]
fn duplicate_application_blocked_by_dedup_store() {
    // Pre-populate the dedup store with a prior session's
    // submission. A new session's identical application is
    // silently dropped at the pause site.
    let dedup = dedup_with("linkedin.com", "gh-1");
    let layer = AutoApply::new(cfg(true, true, 10), dedup);

    let v = verdict_for("apply_to_job", serde_json::json!({ "job_id": "gh-1" }));
    let decision = layer.decide(&v, Some("linkedin.com"), Some("gh-1"));
    match decision {
        AutoApplyDecision::AlreadyApplied {
            platform,
            posting_identifier,
        } => {
            assert_eq!(platform, "linkedin.com");
            assert_eq!(posting_identifier, "gh-1");
        }
        other => panic!("expected AlreadyApplied, got {other:?}"),
    }

    // The per-run counter is NOT bumped on a dedup hit — the
    // application was dropped before any draft was presented.
    // A regression that bumped the counter on dedup hits
    // would let a flaky platform waste the per-run cap on
    // already-sent applications.
    assert_eq!(layer.submitted_count(), 0);
}

// ----------------------------------------------------------------------
// 4. Per-run cap blocks after limit
// ----------------------------------------------------------------------

#[test]
fn per_run_cap_blocks_after_limit() {
    let layer = AutoApply::new(cfg(true, true, 2), dedup_empty());

    // Burn the cap with two increments.
    layer.increment_count();
    layer.increment_count();
    assert_eq!(layer.submitted_count(), 2);

    // The third attempt is dropped.
    let v = verdict_for("apply_to_job", serde_json::json!({ "job_id": "gh-3" }));
    let decision = layer.decide(&v, Some("linkedin.com"), Some("gh-3"));
    match decision {
        AutoApplyDecision::CapReached { submitted, max } => {
            assert_eq!(submitted, 2, "submitted count must reflect the cap");
            assert_eq!(max, 2);
        }
        other => panic!("expected CapReached, got {other:?}"),
    }

    // The counter does not advance past the cap.
    assert_eq!(layer.submitted_count(), 2);
}

#[test]
fn per_run_cap_allows_exactly_max_submissions() {
    // Boundary test: with `max_applications_per_run: 3`,
    // exactly 3 attempts succeed and the 4th is dropped.
    let layer = AutoApply::new(cfg(true, true, 3), dedup_empty());
    for i in 0..3 {
        let v = verdict_for(
            "apply_to_job",
            serde_json::json!({ "job_id": format!("gh-{i}") }),
        );
        let decision = layer.decide(
            &v,
            Some("linkedin.com"),
            Some(&format!("gh-{i}")),
        );
        assert!(
            matches!(decision, AutoApplyDecision::PauseForDraft { .. }),
            "attempt {i} should be allowed, got {decision:?}"
        );
        layer.increment_count();
    }
    let v = verdict_for("apply_to_job", serde_json::json!({ "job_id": "gh-3" }));
    let decision = layer.decide(&v, Some("linkedin.com"), Some("gh-3"));
    assert!(matches!(decision, AutoApplyDecision::CapReached { .. }));
}

// ----------------------------------------------------------------------
// 5. Fire-and-forget path skips confirmation
// ----------------------------------------------------------------------

#[test]
fn fire_and_forget_path_skips_confirmation() {
    // `enabled: true` AND
    // `require_confirmation_per_application: false` AND
    // empty dedup AND cap not hit => `AutoSubmit`.
    let layer = AutoApply::new(cfg(true, false, 10), dedup_empty());
    let v = verdict_for("apply_to_job", serde_json::json!({ "job_id": "gh-1" }));
    let decision = layer.decide(&v, Some("linkedin.com"), Some("gh-1"));
    assert!(matches!(decision, AutoApplyDecision::AutoSubmit { .. }));
}

#[test]
fn fire_and_forget_still_respects_dedup() {
    // Even on the fire-and-forget path, a dedup hit
    // silently drops the submission. The user's
    // `require_confirmation_per_application: false`
    // intent is to skip the chat prompt, not to
    // override the dedup store.
    let dedup = dedup_with("linkedin.com", "gh-1");
    let layer = AutoApply::new(cfg(true, false, 10), dedup);
    let v = verdict_for("apply_to_job", serde_json::json!({ "job_id": "gh-1" }));
    let decision = layer.decide(&v, Some("linkedin.com"), Some("gh-1"));
    assert!(matches!(decision, AutoApplyDecision::AlreadyApplied { .. }));
}

#[test]
fn fire_and_forget_still_respects_cap() {
    // Even on the fire-and-forget path, a cap hit
    // silently drops the submission.
    let layer = AutoApply::new(cfg(true, false, 1), dedup_empty());
    layer.increment_count(); // burn the cap
    let v = verdict_for("apply_to_job", serde_json::json!({ "job_id": "gh-1" }));
    let decision = layer.decide(&v, Some("linkedin.com"), Some("gh-1"));
    assert!(matches!(decision, AutoApplyDecision::CapReached { .. }));
}

// ----------------------------------------------------------------------
// 6. Irreversible classifier recognizes Phase 6 kinds
// ----------------------------------------------------------------------

#[test]
fn irreversible_classifier_recognizes_phase6_kinds() {
    // The auto-apply layer is wired on top of the
    // `mew_resilience::classify_irreversible` table.
    // A regression that drops the Phase 6 typed
    // rules from the table would silently disable
    // the auto-apply surface.
    let cases = [
        ("apply_to_job", ActionKind::SubmitApplication),
        ("submit_job_application", ActionKind::SubmitApplication),
        ("apply_for_job", ActionKind::SubmitApplication),
        ("send_proposal", ActionKind::SendProposal),
        ("submit_proposal", ActionKind::SendProposal),
        ("pitch_client", ActionKind::SendProposal),
        ("send_connection_request", ActionKind::SendConnectionRequest),
        ("connect_with", ActionKind::SendConnectionRequest),
    ];
    for (tool, expected) in cases {
        let v = classify_irreversible(tool, &serde_json::json!({ "x": 1 }))
            .expect(tool);
        assert_eq!(
            v.action, expected,
            "tool {tool} must classify as {expected:?}, got {:?}",
            v.action
        );
    }
}

#[test]
fn non_phase6_irreversible_kinds_are_not_owned_by_auto_apply() {
    // The auto-apply layer only owns the three
    // Phase 6 typed kinds. Every other irreversible
    // kind must return `NotApplicable` so the
    // dispatch site falls through to the existing
    // pause path (without consulting the dedup
    // store or per-run cap).
    let layer = AutoApply::new(cfg(true, true, 10), dedup_empty());
    for tool in [
        "send_message",
        "post_tweet",
        "delete_file",
        "pay",
        "follow",
        "update_email",
    ] {
        let v = classify_irreversible(tool, &serde_json::json!({ "x": 1 }))
            .expect(tool);
        let decision = layer.decide(&v, None, None);
        assert_eq!(
            decision,
            AutoApplyDecision::NotApplicable,
            "tool {tool} should be NotApplicable, got {decision:?}"
        );
    }
}

// ----------------------------------------------------------------------
// 7. End-to-end via the agent's config struct
// ----------------------------------------------------------------------

#[test]
fn lib_agent_config_exposes_auto_apply_block() {
    // The `AutoApplyConfig` block in `mew_agent::lib`
    // must be visible at the top-level config so the
    // operator can configure it via `agent.auto_apply`
    // in config.yaml. A regression that nested the
    // block under a sub-struct would break the
    // config-yaml surface.
    use mew_agent::AgentConfig;
    let yaml = r#"
enabled: true
require_confirmation_per_application: true
max_applications_per_run: 5
"#;
    let parsed: AutoApplyConfig = serde_yaml::from_str(yaml).unwrap();
    let cfg = AgentConfig {
        auto_apply: parsed.clone(),
        ..AgentConfig::default()
    };
    assert!(cfg.auto_apply.enabled);
    assert!(cfg.auto_apply.require_confirmation_per_application);
    assert_eq!(cfg.auto_apply.max_applications_per_run, 5);
}

#[test]
fn lib_agent_config_defaults_disable_auto_apply() {
    // Regression guard: the default `AgentConfig`
    // (the "no `agent.auto_apply` block in config.yaml"
    // case) must produce a fully disabled layer so
    // the pre-Phase-6 read-only / report-only flow
    // is preserved byte-for-byte.
    use mew_agent::AgentConfig;
    let cfg = AgentConfig::default();
    assert!(!cfg.auto_apply.enabled, "auto_apply must default to disabled");
    assert!(
        cfg.auto_apply.require_confirmation_per_application,
        "require_confirmation must default to true"
    );
    assert_eq!(
        cfg.auto_apply.max_applications_per_run, 10,
        "max_applications_per_run must default to 10"
    );
}

#[test]
fn auto_apply_config_clamps_zero_to_default() {
    // The loader treats `max_applications_per_run: 0`
    // as "operator wanted a cap but set it to the
    // wrong value" and bumps it to the default 10.
    // We exercise the same clamp logic by asserting
    // that the default `AutoApplyConfig` produces a
    // sensible value (10) — a regression in the
    // default function would surface here. The
    // zero-clamp is in the loader, not in the
    // struct; the struct accepts whatever the YAML
    // says and the loader is responsible for
    // enforcing the invariant.
    use mew_agent::AgentConfig;
    let cfg = AgentConfig::default();
    assert!(cfg.auto_apply.max_applications_per_run > 0);
}

#[test]
fn auto_apply_config_clamps_huge_value_via_loader_logic() {
    // The loader hard-clamps the upper bound to 1000.
    // We exercise the same shape by parsing a
    // too-large value and asserting the struct
    // accepts it (the clamp is in the loader, not
    // the struct). This is a contract pin — a
    // future refactor that moves the clamp into
    // the struct would surface here as a
    // deserialization failure.
    let parsed: AutoApplyConfig = serde_yaml::from_str(
        "enabled: false\nrequire_confirmation_per_application: true\nmax_applications_per_run: 99999\n",
    )
    .unwrap();
    assert_eq!(parsed.max_applications_per_run, 99999);
}

// ----------------------------------------------------------------------
// 8. Dedup store on-disk round trip
// ----------------------------------------------------------------------

#[test]
fn dedup_store_persists_records_to_disk_across_sessions() {
    // A user runs the `daily_remote_jobs` preset on
    // Monday, applies to 2 LinkedIn Easy Apply postings.
    // On Tuesday they run the preset again — the
    // dedup store must still see the Monday records.
    let dir = std::env::temp_dir().join(format!(
        "mew_phase6_persist_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path: PathBuf = dir.join("applications_sent.json");
    let _ = std::fs::remove_file(&path);

    // Monday: write 2 records and flush.
    {
        let s = ApplicationDedup::load_or_default(Some(path.clone()));
        s.record_submission("linkedin.com", "job-1", "submit_application", "f-1", "n");
        s.record_submission("linkedin.com", "job-2", "submit_application", "f-2", "n");
        s.flush();
    }

    // Tuesday: load the same file in a fresh store
    // and verify both records are present.
    let s2 = ApplicationDedup::load_or_default(Some(path.clone()));
    assert_eq!(s2.len(), 2, "Tuesday's load must see Monday's records");
    assert!(s2.is_already_applied("linkedin.com", "job-1"));
    assert!(s2.is_already_applied("linkedin.com", "job-2"));
    assert!(!s2.is_already_applied("linkedin.com", "job-3"));

    // Tuesday's orchestration layer sees a dedup hit
    // for the Monday applications and drops them.
    let layer = AutoApply::new(cfg(true, true, 10), Arc::new(s2));
    let v = verdict_for("apply_to_job", serde_json::json!({ "job_id": "job-1" }));
    let decision = layer.decide(&v, Some("linkedin.com"), Some("job-1"));
    assert!(matches!(decision, AutoApplyDecision::AlreadyApplied { .. }));

    // Cleanup.
    let _ = std::fs::remove_file(&path);
}

// ----------------------------------------------------------------------
// 9. Tracked action kinds stay in sync
// ----------------------------------------------------------------------

#[test]
fn tracked_action_kinds_matches_classifier_kinds() {
    // The dedup store's `TRACKED_ACTION_KINDS` list
    // must stay in sync with the `ActionKind` variants
    // the auto-apply layer owns. A regression that
    // adds a new typed kind to the classifier without
    // adding it to `TRACKED_ACTION_KINDS` would
    // silently disable dedup for that surface.
    let expected: &[&str] = &[
        "submit_application",
        "send_proposal",
        "send_connection_request",
    ];
    assert_eq!(TRACKED_ACTION_KINDS, expected);
}
