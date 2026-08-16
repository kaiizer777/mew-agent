// mew v2 — Phase 8: Personal workflow presets & scheduling ergonomics.
//
// End-to-end acceptance test: all three seed presets
// (`daily_remote_jobs`, `daily_freelance_leads`,
// `weekly_full_search`) must:
//   1. Produce a clean typed `Handoff` (subtasks + task shape
//      + originating_message_id).
//   2. Resolve a finding set through the digest writer, write
//      a Markdown file at the expected path, and return a
//      parseable session summary.
//   3. Honor the auto-apply / dedup gates: even with
//      `auto_apply.enabled: true` + a dedup hit, the agent
//      does not auto-submit; with the cap reached, the agent
//      silently drops.
//
// The tests are pure-Rust (no live Chrome, no LLM, no network).
// They compose the `presets` module, the `digests` module,
// the `job_freelance_lessons` module, and the `auto_apply`
// decision enum to verify the Phase 8 surface fits together
// the way the spec calls for.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use mew_agent::auto_apply::{AutoApply, AutoApplyConfig, AutoApplyDecision};
use mew_agent::digests::{render_daily_digest, render_run_section, write_run_digest};
use mew_agent::handoff::{BrowserResult, BrowserStatus};
use mew_agent::job_freelance_lessons::collect_from_lessons;
use mew_agent::presets::{
    build_handoff_for_preset, default_presets, merged_presets, render_preset_catalog,
    resolve_preset,
};
use mew_agent::router::TaskShape;
use mew_agent::session_summary::{render_session_summary, SessionSummaryInputs};
use mew_memory::{Lesson, Outcome, TaskShape as MemTaskShape};
use mew_resilience::{ActionKind, IrreverisbleVerdict};

fn unique_tmp(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "mew-phase8-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn make_browser_result_with_findings(platform: &str) -> BrowserResult {
    use mew_agent::research::ResearchFinding;
    let f = ResearchFinding {
        id: format!("test-{platform}"),
        platform: platform.to_string(),
        title: Some(format!("Test role at {platform}")),
        company: Some(format!("{platform} Co")),
        url: Some(format!("https://{platform}.com/jobs/123")),
        email: None,
        note: String::new(),
        added_at_secs: 1_700_000_000,
    };
    let mut r = BrowserResult::done("test-session", "ok", Vec::new(), None, None);
    r.findings.push(f);
    r
}

fn apply_verdict(posting_id: &str) -> IrreverisbleVerdict {
    IrreverisbleVerdict {
        action: ActionKind::SubmitApplication,
        target: posting_id.to_string(),
    }
}

// ----------------------------------------------------------------------
// 1. Preset resolution: all three seed presets must resolve
//    cleanly to a non-empty Handoff.
// ----------------------------------------------------------------------

#[test]
fn all_three_presets_resolve_and_build_handoff() {
    let presets = default_presets();
    for name in [
        "daily_remote_jobs",
        "daily_freelance_leads",
        "weekly_full_search",
    ] {
        let preset = resolve_preset(name, &presets)
            .unwrap_or_else(|| panic!("preset '{name}' must resolve"));
        let handoff = build_handoff_for_preset(preset, "msg-1");
        assert!(
            !handoff.subtasks.is_empty(),
            "preset '{name}' must have at least one subtask"
        );
        assert!(!handoff.task_description.is_empty());
        assert_eq!(handoff.originating_message_id, "msg-1");
        assert!(
            handoff.task_description.contains(name),
            "task_description must mention the preset name (got: {})",
            handoff.task_description
        );
    }
}

#[test]
fn daily_remote_jobs_handoff_routes_to_job_search_shape() {
    let presets = default_presets();
    let p = resolve_preset("daily_remote_jobs", &presets).unwrap();
    let h = build_handoff_for_preset(p, "msg");
    assert_eq!(h.task_shape, Some(TaskShape::JobSearch));
    let ids: Vec<&str> = h.subtasks.iter().map(|s| s.id.as_str()).collect();
    for needed in ["linkedin", "indeed", "wellfound", "weworkremotely", "remoteok"] {
        assert!(
            ids.contains(&needed),
            "daily_remote_jobs must include '{needed}', got: {ids:?}"
        );
    }
}

#[test]
fn daily_freelance_leads_handoff_routes_to_lead_gen_shape() {
    let presets = default_presets();
    let p = resolve_preset("daily_freelance_leads", &presets).unwrap();
    let h = build_handoff_for_preset(p, "msg");
    assert_eq!(h.task_shape, Some(TaskShape::LeadGen));
    let ids: Vec<&str> = h.subtasks.iter().map(|s| s.id.as_str()).collect();
    for needed in ["upwork", "contra"] {
        assert!(
            ids.contains(&needed),
            "daily_freelance_leads must include '{needed}', got: {ids:?}"
        );
    }
}

#[test]
fn weekly_full_search_handoff_has_mixed_task_shape() {
    let presets = default_presets();
    let p = resolve_preset("weekly_full_search", &presets).unwrap();
    let h = build_handoff_for_preset(p, "msg");
    assert_eq!(h.task_shape, None);
    assert!(h.subtasks.len() >= 5);
    assert!(h.subtasks.len() <= 6);
}

#[test]
fn unknown_preset_name_surfaces_available_catalog() {
    let presets = default_presets();
    assert!(resolve_preset("does_not_exist", &presets).is_none());
    let cat = render_preset_catalog(&presets);
    for name in [
        "daily_remote_jobs",
        "daily_freelance_leads",
        "weekly_full_search",
    ] {
        assert!(cat.contains(name));
    }
}

#[test]
fn merged_presets_with_overrides_keeps_seeds_intact() {
    let mut overrides = std::collections::HashMap::new();
    overrides.insert(
        "my_custom".into(),
        mew_agent::presets::TaskPreset {
            name: "my_custom".into(),
            summary: "operator-defined".into(),
            shape: mew_agent::presets::PresetShape::DailyRemoteJobs,
            budget_overrides: None,
        },
    );
    let merged = merged_presets(&overrides);
    assert_eq!(merged.len(), 4);
    assert!(merged.contains_key("daily_remote_jobs"));
    assert!(merged.contains_key("daily_freelance_leads"));
    assert!(merged.contains_key("weekly_full_search"));
    assert!(merged.contains_key("my_custom"));
}

// ----------------------------------------------------------------------
// 2. Digest export: each preset produces a parseable Markdown
//    digest file at <out_dir>/digests/YYYY-MM-DD.md. The
//    file is overwritten on the same day.
// ----------------------------------------------------------------------

#[test]
fn digest_for_daily_remote_jobs_writes_a_dated_file() {
    let tmp = unique_tmp("digest-drj");
    let _ = std::fs::remove_dir_all(&tmp);

    let when = UNIX_EPOCH + Duration::from_secs(1_786_838_400); // 2026-08-16
    let result = make_browser_result_with_findings("linkedin");
    let inputs = SessionSummaryInputs {
        result: &result,
        auth_state: None,
        applications_sent: 0,
        platforms_not_reached: Vec::new(),
    };
    let presets = default_presets();
    let p = resolve_preset("daily_remote_jobs", &presets).unwrap();
    let handoff = build_handoff_for_preset(p, "msg-drj");
    let section = render_run_section(when, &inputs, &handoff.task_description);
    let path = write_run_digest(&tmp, when, &section).expect("write_run_digest");

    assert!(path.exists());
    assert!(path.ends_with("digests/2026-08-16.md"));
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("daily_remote_jobs"));
    assert!(content.contains("linkedin"));
    assert!(content.contains("## "));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn digest_for_daily_freelance_leads_writes_a_dated_file() {
    let tmp = unique_tmp("digest-fl");
    let _ = std::fs::remove_dir_all(&tmp);

    let when = UNIX_EPOCH + Duration::from_secs(1_786_838_400);
    let result = make_browser_result_with_findings("upwork");
    let inputs = SessionSummaryInputs {
        result: &result,
        auth_state: None,
        applications_sent: 0,
        platforms_not_reached: Vec::new(),
    };
    let presets = default_presets();
    let p = resolve_preset("daily_freelance_leads", &presets).unwrap();
    let handoff = build_handoff_for_preset(p, "msg-fl");
    let section = render_run_section(when, &inputs, &handoff.task_description);
    let path = write_run_digest(&tmp, when, &section).expect("write_run_digest");

    assert!(path.exists());
    assert!(path.ends_with("digests/2026-08-16.md"));
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("daily_freelance_leads"));
    assert!(content.contains("upwork"));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn digest_for_weekly_full_search_writes_a_dated_file_with_mixed_vertical() {
    let tmp = unique_tmp("digest-wfs");
    let _ = std::fs::remove_dir_all(&tmp);

    let when = UNIX_EPOCH + Duration::from_secs(1_786_838_400);
    let result = make_browser_result_with_findings("linkedin");
    let inputs = SessionSummaryInputs {
        result: &result,
        auth_state: None,
        applications_sent: 0,
        platforms_not_reached: Vec::new(),
    };
    let presets = default_presets();
    let p = resolve_preset("weekly_full_search", &presets).unwrap();
    let handoff = build_handoff_for_preset(p, "msg-wfs");
    let section = render_run_section(when, &inputs, &handoff.task_description);
    let path = write_run_digest(&tmp, when, &section).expect("write_run_digest");

    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("weekly_full_search"));
    assert!(content.contains("LinkedIn"));
    assert!(content.contains("Upwork") || content.contains("Contra"));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn digest_render_summary_is_byte_identical_for_same_input() {
    let when = UNIX_EPOCH + Duration::from_secs(1_786_838_400);
    let result = make_browser_result_with_findings("linkedin");
    let inputs = SessionSummaryInputs {
        result: &result,
        auth_state: None,
        applications_sent: 0,
        platforms_not_reached: Vec::new(),
    };
    let section_a = render_run_section(when, &inputs, "test task");
    let section_b = render_run_section(when, &inputs, "test task");
    assert_eq!(section_a, section_b);
    let body = render_session_summary(&inputs);
    assert!(section_a.contains(&body));
}

#[test]
fn digest_daily_header_includes_sections_for_all_three_presets() {
    let when = UNIX_EPOCH + Duration::from_secs(1_786_838_400);
    let result = make_browser_result_with_findings("linkedin");
    let inputs = SessionSummaryInputs {
        result: &result,
        auth_state: None,
        applications_sent: 0,
        platforms_not_reached: Vec::new(),
    };
    let presets = default_presets();
    let mut sections: Vec<(std::time::SystemTime, String)> = Vec::new();
    for name in [
        "daily_remote_jobs",
        "daily_freelance_leads",
        "weekly_full_search",
    ] {
        let p = resolve_preset(name, &presets).unwrap();
        let handoff = build_handoff_for_preset(p, name);
        let s = render_run_section(when, &inputs, &handoff.task_description);
        sections.push((when, s));
    }
    let daily = render_daily_digest("2026-08-16", &sections);
    assert!(daily.contains("daily_remote_jobs"));
    assert!(daily.contains("daily_freelance_leads"));
    assert!(daily.contains("weekly_full_search"));
    assert!(daily.contains("# Daily digest — 2026-08-16"));
}

// ----------------------------------------------------------------------
// 3. Confirmation gates: the auto-apply / dedup contract
//    must hold for a preset run that finds multiple
//    applications on a single platform.
// ----------------------------------------------------------------------

#[test]
fn auto_apply_pauses_for_first_application_with_per_run_cap() {
    let path = unique_tmp("dedup-pauses");
    let _ = std::fs::remove_dir_all(&path);
    let dedup = Arc::new(mew_agent::application_dedup::ApplicationDedup::new(Some(path.clone())));
    let cfg = AutoApplyConfig {
        enabled: true,
        require_confirmation_per_application: true,
        max_applications_per_run: 2,
    };
    let mut auto = AutoApply::new(cfg, dedup.clone());

    // First finding: a fresh application. Should pause
    // for the user's approval.
    let d1 = auto.decide(&apply_verdict("job-1"), Some("linkedin.com"), Some("job-1"));
    assert!(matches!(d1, AutoApplyDecision::PauseForDraft { .. }));

    // The dispatch site commits the draft and the
    // user confirms: the dedup store gets a record
    // and the per-run counter is incremented. The
    // next call to `decide` sees both.
    dedup.record_submission("linkedin.com", "job-1", "apply_to_job", "", "n");
    let _ = auto.increment_count();

    // Same posting-id again: dedup hit, silent drop.
    let d2 = auto.decide(&apply_verdict("job-1"), Some("linkedin.com"), Some("job-1"));
    assert!(matches!(d2, AutoApplyDecision::AlreadyApplied { .. }));

    // Second fresh finding: still within cap, should
    // pause for confirmation.
    let d3 = auto.decide(&apply_verdict("job-2"), Some("linkedin.com"), Some("job-2"));
    assert!(matches!(d3, AutoApplyDecision::PauseForDraft { .. }));

    // User confirms job-2 too: bump the counter.
    dedup.record_submission("linkedin.com", "job-2", "apply_to_job", "", "n");
    let _ = auto.increment_count();

    // Third fresh finding: per-run cap reached (count
    // is now 2 == max), silent drop.
    let d4 = auto.decide(&apply_verdict("job-3"), Some("linkedin.com"), Some("job-3"));
    assert!(matches!(d4, AutoApplyDecision::CapReached { .. }));

    let _ = std::fs::remove_dir_all(&path);
}

#[test]
fn auto_apply_disabled_path_does_not_pause_or_dedup() {
    let path = unique_tmp("dedup-disabled");
    let _ = std::fs::remove_dir_all(&path);
    let dedup = Arc::new(mew_agent::application_dedup::ApplicationDedup::new(Some(path.clone())));
    let cfg = AutoApplyConfig {
        enabled: false,
        require_confirmation_per_application: true,
        max_applications_per_run: 10,
    };
    let mut auto = AutoApply::new(cfg, dedup.clone());
    for i in 0..5 {
        let d = auto.decide(
            &apply_verdict(&format!("job-{i}")),
            Some("linkedin.com"),
            Some(&format!("job-{i}")),
        );
        assert!(matches!(d, AutoApplyDecision::Disabled));
    }
    let _ = std::fs::remove_dir_all(&path);
}

#[test]
fn auto_apply_fire_and_forget_path_respects_dedup_and_cap() {
    let path = unique_tmp("dedup-faf");
    let _ = std::fs::remove_dir_all(&path);
    let dedup = Arc::new(mew_agent::application_dedup::ApplicationDedup::new(Some(path.clone())));
    let cfg = AutoApplyConfig {
        enabled: true,
        require_confirmation_per_application: false,
        max_applications_per_run: 2,
    };
    let mut auto = AutoApply::new(cfg, dedup.clone());

    // First two: AutoSubmit. The dispatch site
    // commits and bumps the counter after each.
    let d1 = auto.decide(&apply_verdict("job-1"), Some("linkedin.com"), Some("job-1"));
    assert!(matches!(d1, AutoApplyDecision::AutoSubmit { .. }));
    dedup.record_submission("linkedin.com", "job-1", "apply_to_job", "", "n");
    let _ = auto.increment_count();

    let d2 = auto.decide(&apply_verdict("job-2"), Some("linkedin.com"), Some("job-2"));
    assert!(matches!(d2, AutoApplyDecision::AutoSubmit { .. }));
    dedup.record_submission("linkedin.com", "job-2", "apply_to_job", "", "n");
    let _ = auto.increment_count();

    // Third: cap reached, silent drop.
    let d3 = auto.decide(&apply_verdict("job-3"), Some("linkedin.com"), Some("job-3"));
    assert!(matches!(d3, AutoApplyDecision::CapReached { .. }));

    // Re-try a dedup hit: silent drop.
    let d4 = auto.decide(&apply_verdict("job-1"), Some("linkedin.com"), Some("job-1"));
    assert!(matches!(d4, AutoApplyDecision::AlreadyApplied { .. }));

    let _ = std::fs::remove_dir_all(&path);
}

// ----------------------------------------------------------------------
// 4. Lessons-learned surface: each preset run accumulates
//    lessons in the typed `JobFreelanceLesson` shape,
//    sorted with jobs first.
// ----------------------------------------------------------------------

fn lesson(id: &str, shape: MemTaskShape, platform: Option<&str>) -> Lesson {
    Lesson::new(
        id,
        1_700_000_000,
        shape,
        platform.map(|s| s.to_string()),
        "test situation",
        Outcome::Success,
        format!("lesson text for {id}"),
        "session-1",
    )
}

#[test]
fn lessons_surface_groups_jobs_first_then_freelance() {
    let lessons = vec![
        lesson("freelance-1", MemTaskShape::LeadGen, Some("Upwork")),
        lesson("job-1", MemTaskShape::JobSearch, Some("LinkedIn")),
        lesson("maps-1", MemTaskShape::LocalLeadGen, Some("Maps")),
    ];
    let out = collect_from_lessons(&lessons);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].task_shape, MemTaskShape::JobSearch);
    assert_eq!(out[1].task_shape, MemTaskShape::LeadGen);
}

#[test]
fn lessons_surface_drops_low_confidence_and_failure() {
    let mut a = lesson("a", MemTaskShape::JobSearch, Some("LinkedIn"));
    a.confidence_score = 0.4;
    let mut b = lesson("b", MemTaskShape::JobSearch, Some("Indeed"));
    b.outcome = Outcome::Failure;
    let c = lesson("c", MemTaskShape::JobSearch, Some("Wellfound"));
    let out = collect_from_lessons(&[a, b, c]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].id, "c");
}

// ----------------------------------------------------------------------
// 5. Failure-mode contract: a `Failed` BrowserResult
//    still produces a digest (the user wants to see
//    "what didn't work" too) and the chat reply is
//    not dropped.
// ----------------------------------------------------------------------

#[test]
fn failed_browser_result_still_writes_digest_and_emits_reply() {
    let tmp = unique_tmp("digest-fail");
    let _ = std::fs::remove_dir_all(&tmp);
    let when = UNIX_EPOCH + Duration::from_secs(1_786_838_400);
    let result = BrowserResult {
        status: BrowserStatus::Failed,
        ..BrowserResult::failure("test-session", "login wall".to_string(), None)
    };
    let inputs = SessionSummaryInputs {
        result: &result,
        auth_state: None,
        applications_sent: 0,
        platforms_not_reached: Vec::new(),
    };
    let presets = default_presets();
    let p = resolve_preset("daily_remote_jobs", &presets).unwrap();
    let handoff = build_handoff_for_preset(p, "msg-fail");
    let section = render_run_section(when, &inputs, &handoff.task_description);
    let path = write_run_digest(&tmp, when, &section).expect("write_run_digest on failure");
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("daily_remote_jobs"));
    let body = render_session_summary(&inputs);
    assert!(!body.is_empty());
    let _ = std::fs::remove_dir_all(&tmp);
}
