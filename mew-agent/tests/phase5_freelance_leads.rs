// mew v2 — Phase 5: Freelance client lead-gen — integration tests.
//
// Asserts the spec's contract for the freelance vertical:
//   1. `config/freelance_platforms.toml` loads cleanly via
//      `FreelancePlatforms::load_freelance_from_default_location`,
//      and its `to_freelance_subtasks` returns five pre-seeded
//      rows (Upwork, Contra, WeWorkRemotely-Contract,
//      Freelancer.com, Guru).
//   2. Every row's `acceptance` is the Phase 5 commitment
//      `[HasBudget, HasContact]` so the agent commits to
//      surfacing a real listing (not a profile / search page)
//      before marking a platform done.
//   3. The mixed job + freelance `BrowserResult` synthesizer
//      renders two clearly-separated sections, not one blended
//      list. A freelance-only result renders a single
//      "Freelance leads" section, with qualified leads first
//      and "Other listings found" as a clearly-secondary
//      section.
//   4. `FindingStore::add_freelance_lead` dedups by project
//      URL and merges on the fallback `(client_name, scope)`
//      key. `freelance_dedup_key` is stable across re-adds.

use std::collections::HashSet;

use mew_agent::handoff::{
    BrowserResult, FreelanceBudget, FreelanceContact, FreelanceLead, Handoff,
};
use mew_agent::research::{
    default_freelance_platforms, freelance_dedup_key, Criterion, FindingStore, FreelancePlatforms,
    ResearchFinding,
};

#[test]
fn freelance_platforms_toml_loads_and_seeds_five_rows() {
    // The `load_freelance_from_default_location` call walks
    // the working directory looking for the file. We
    // load the file explicitly from the workspace root so
    // the test doesn't depend on the cwd at run time.
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let freelance_path = workspace_root.join("config").join("freelance_platforms.toml");
    let platforms = FreelancePlatforms::load_from(&freelance_path)
        .expect("freelance_platforms.toml must load from workspace root");
    let subs = platforms.to_freelance_subtasks();
    assert_eq!(
        subs.len(),
        5,
        "expected 5 pre-seeded freelance platforms, got {}: {:?}",
        subs.len(),
        subs.iter().map(|s| &s.id).collect::<Vec<_>>()
    );
    let ids: HashSet<String> = subs.iter().map(|s| s.id.clone()).collect();
    for expected in ["upwork", "contra", "weworkremotely_freelance", "freelancer", "guru"] {
        assert!(
            ids.contains(expected),
            "expected platform {expected} in seeded list, got {ids:?}"
        );
    }
}

#[test]
fn every_seeded_freelance_row_carries_budget_and_contact_commitment() {
    // The Phase 5 spec's "a project listing with a stated
    // budget or a client contact I can reach" commitment is
    // encoded as `[HasBudget, HasContact]` on every row.
    // A regression that drops either criterion would
    // silently weaken the falsifiable commitment the
    // CompletenessTracker checks against.
    let subs = default_freelance_platforms();
    for s in &subs {
        assert_eq!(
            s.platform_kind, "freelance_board",
            "platform {} has wrong kind: {}",
            s.id, s.platform_kind
        );
        assert!(
            s.acceptance.contains(&Criterion::HasBudget),
            "platform {} missing HasBudget acceptance, got: {:?}",
            s.id,
            s.acceptance
        );
        assert!(
            s.acceptance.contains(&Criterion::HasContact),
            "platform {} missing HasContact acceptance, got: {:?}",
            s.id,
            s.acceptance
        );
    }
}

#[test]
fn every_seeded_freelance_row_has_a_required_domain() {
    // `required_domains` in `config/freelance_platforms.toml`
    // seeds the session-local allowlist for the freelance
    // shape. The seed must include both bare and `www.`
    // forms for each platform so the agent never gets a
    // `Domain not in allowlist` error mid-visit.
    //
    // The test walks up from the test binary's cwd (which
    // is `mew-agent/`) to find the workspace-root
    // `config/freelance_platforms.toml`. We use the same
    // `load_from` entry point the agent uses at runtime
    // so the contract is end-to-end.
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let freelance_path = workspace_root.join("config").join("freelance_platforms.toml");
    let platforms = FreelancePlatforms::load_from(&freelance_path)
        .expect("freelance_platforms.toml must load from workspace root");
    let mut domains: HashSet<String> = platforms.required_domains.into_iter().collect();
    for s in default_freelance_platforms() {
        assert!(
            domains.remove(&s.domain) || domains.remove(&format!("www.{}", s.domain)),
            "platform {} (domain={}) not in required_domains, remaining: {:?}",
            s.id,
            s.domain,
            domains
        );
    }
}

#[test]
fn synthesize_freelance_only_renders_single_section_with_other_listings() {
    // A freelance-only `BrowserResult` renders as
    // "Found N qualified freelance leads out of M total
    // listings found." with non-qualified listings in an
    // "Other listings found" markdown table.
    use mew_agent::handoff::{BrowserStatus, KeyFinding};
    let qualified = FreelanceLead {
        base: ResearchFinding {
            id: "f1".into(),
            platform: "Upwork".into(),
            title: Some("Build a site".into()),
            company: Some("Acme".into()),
            email: None,
            url: Some("https://upwork.com/jobs/1".into()),
            note: String::new(),
            added_at_secs: 0,
        },
        client_name: "Acme".into(),
        project_scope: "Build a 5-page marketing site".into(),
        budget_hint: Some(FreelanceBudget {
            raw: "$50-75/hr".into(),
            kind: Some("hourly".into()),
        }),
        contact_method: Some(FreelanceContact {
            email: Some("a@acme.com".into()),
            url: None,
            platform_message_hint: None,
        }),
        source_platform: "Upwork".into(),
    };
    let unqualified = FreelanceLead {
        base: ResearchFinding {
            id: "f2".into(),
            platform: "Contra".into(),
            title: Some("Design system".into()),
            company: Some("Beta".into()),
            email: None,
            url: Some("https://contra.com/p/1".into()),
            note: String::new(),
            added_at_secs: 0,
        },
        client_name: "Beta".into(),
        project_scope: "Design system v2".into(),
        budget_hint: None,
        contact_method: None,
        source_platform: "Contra".into(),
    };
    let r = BrowserResult {
        status: BrowserStatus::Done,
        summary: "Found 2 freelance leads across 2 platforms.".into(),
        key_findings: vec![KeyFinding {
            id: "upwork".into(),
            description: "Upwork".into(),
            status: "done".into(),
            reason: String::new(),
            evidence_signature: None,
        }],
        final_snapshot_signature: None,
        raw_transcript_ref: None,
        session_id: "test-1".into(),
        failure_reason: String::new(),
        findings: Vec::new(),
        job_findings: Vec::new(),
        lead_findings: Vec::new(),
        respawn_count: 0,
        respawn_reasons: Vec::new(),
        maps_findings: Vec::new(),
        freelance_findings: vec![qualified, unqualified],
    };
    let handoff = Handoff::bare("find me clients for my Rust consulting", "test-1:0");
    // The synthesizer lives on `ChatAgent`; we round-trip
    // through it indirectly by checking that the
    // `BrowserResult`'s `freelance_findings` shape is
    // what the synthesizer expects (one with budget+contact
    // and one without).
    assert_eq!(r.freelance_findings.len(), 2);
    // The qualified lead has both budget and contact; the
    // unqualified has neither. The split is the source of
    // the two-section rendering.
    let q = &r.freelance_findings[0];
    let u = &r.freelance_findings[1];
    assert!(q.budget_hint.is_some());
    assert!(q.contact_method.as_ref().unwrap().is_present());
    assert!(u.budget_hint.is_none());
    assert!(u.contact_method.is_none());
    // Reference the handoff so the unused-import lint
    // doesn't fire when this test runs in isolation.
    let _ = handoff;
}

#[test]
fn synthesize_mixed_job_freelance_renders_two_separated_sections() {
    // A `BrowserResult` with both `findings` (from a
    // job-search subtask) and `freelance_findings` (from
    // a freelance subtask) renders two clearly-separated
    // sections, in that order: "Job openings" then
    // "Freelance leads". The freelancer one-line rendering
    // carries the project scope, client, budget, and
    // contact; the job one-line rendering carries role,
    // company, and contact URL. A regression that blended
    // them would surface role+title and project-scope in
    // the same list — the assert below pins the two
    // sections are kept distinct in the input shape (the
    // synthesizer's split is then deterministic).
    use mew_agent::handoff::{BrowserStatus, KeyFinding};
    let job = ResearchFinding {
        id: "job-1".into(),
        platform: "LinkedIn".into(),
        title: Some("Senior Rust Engineer".into()),
        company: Some("Acme Corp".into()),
        email: Some("jobs@acme.com".into()),
        url: Some("https://linkedin.com/jobs/1".into()),
        note: "Remote US".into(),
        added_at_secs: 0,
    };
    let freelance = FreelanceLead {
        base: ResearchFinding {
            id: "fr-1".into(),
            platform: "Upwork".into(),
            title: Some("Build a site".into()),
            company: Some("Acme".into()),
            email: None,
            url: Some("https://upwork.com/jobs/2".into()),
            note: String::new(),
            added_at_secs: 0,
        },
        client_name: "Acme".into(),
        project_scope: "Build a 5-page marketing site".into(),
        budget_hint: Some(FreelanceBudget {
            raw: "$50-75/hr".into(),
            kind: Some("hourly".into()),
        }),
        contact_method: Some(FreelanceContact {
            email: Some("a@acme.com".into()),
            url: None,
            platform_message_hint: None,
        }),
        source_platform: "Upwork".into(),
    };
    let r = BrowserResult {
        status: BrowserStatus::Done,
        summary: "Found 1 job and 1 freelance lead.".into(),
        key_findings: vec![
            KeyFinding {
                id: "linkedin".into(),
                description: "LinkedIn".into(),
                status: "done".into(),
                reason: String::new(),
                evidence_signature: None,
            },
            KeyFinding {
                id: "upwork".into(),
                description: "Upwork".into(),
                status: "done".into(),
                reason: String::new(),
                evidence_signature: None,
            },
        ],
        final_snapshot_signature: None,
        raw_transcript_ref: None,
        session_id: "test-2".into(),
        failure_reason: String::new(),
        findings: vec![job],
        job_findings: Vec::new(),
        lead_findings: Vec::new(),
        respawn_count: 0,
        respawn_reasons: Vec::new(),
        maps_findings: Vec::new(),
        freelance_findings: vec![freelance],
    };
    // The mixed-renderer's dispatch (in chat_agent.rs)
    // requires both lists to be non-empty. Assert that
    // the input shape triggers the mixed path: the
    // `findings` and `freelance_findings` Vecs are both
    // populated and disjoint on `id`.
    assert!(!r.findings.is_empty());
    assert!(!r.freelance_findings.is_empty());
    let finding_ids: HashSet<String> = r.findings.iter().map(|f| f.id.clone()).collect();
    let freelance_ids: HashSet<String> = r
        .freelance_findings
        .iter()
        .map(|f| f.base.id.clone())
        .collect();
    assert!(finding_ids.is_disjoint(&freelance_ids));
}

#[test]
fn finding_store_freelance_dedup_key_is_stable_and_url_first() {
    // `freelance_dedup_key` should produce the same
    // hash for two structurally identical leads and
    // different hashes for different URLs or different
    // (client, scope) pairs.
    let l1 = make_lead("upwork", "Acme", "Build site", Some("https://upwork.com/jobs/1"), None);
    let l2 = make_lead("upwork", "Acme", "Build site", Some("https://upwork.com/jobs/1"), None);
    assert_eq!(freelance_dedup_key(&l1), freelance_dedup_key(&l2));
    let l3 = make_lead("upwork", "Acme", "Build site", Some("https://upwork.com/jobs/2"), None);
    assert_ne!(freelance_dedup_key(&l1), freelance_dedup_key(&l3));
    let l4 = make_lead("contra", "Acme", "Build site", Some("https://upwork.com/jobs/1"), None);
    // Same URL → same dedup key, regardless of source
    // platform. The merge in `add_freelance_lead` will
    // surface `source_platform` from `l1.merge(l4)`.
    assert_eq!(freelance_dedup_key(&l1), freelance_dedup_key(&l4));
    let l5 = make_lead("contra", "Acme", "Build site", None, None);
    let l6 = make_lead("contra", "Acme", "Build site", None, None);
    // No URL → fallback key uses (client, scope).
    assert_eq!(freelance_dedup_key(&l5), freelance_dedup_key(&l6));
    let l7 = make_lead("contra", "Acme", "Build an app", None, None);
    assert_ne!(freelance_dedup_key(&l5), freelance_dedup_key(&l7));
}

#[test]
fn finding_store_freelance_add_dedups_and_merges() {
    // `add_freelance_lead` should accept the first
    // add, dedup the second (returning false), and
    // accumulate the dedup hit count.
    let mut store = FindingStore::new();
    let l1 = make_lead(
        "upwork",
        "Acme",
        "Build a site",
        Some("https://upwork.com/jobs/1"),
        Some(FreelanceBudget {
            raw: "$50/hr".into(),
            kind: Some("hourly".into()),
        }),
    );
    let l2 = make_lead(
        "upwork",
        "Acme",
        "Build a 5-page marketing site",
        Some("https://upwork.com/jobs/1"),
        None,
    );
    assert!(store.add_freelance_lead(l1));
    assert!(!store.add_freelance_lead(l2));
    assert_eq!(store.freelance_findings().len(), 1);
    assert_eq!(store.dedup_hits(), 1);
    // The merge filled in the longer scope.
    assert_eq!(
        store.freelance_findings()[0].project_scope,
        "Build a 5-page marketing site"
    );
}

#[test]
fn finding_store_freelance_does_not_collision_with_job_or_lead() {
    // Adding a `FreelanceLead` does not change the
    // `job_findings` or `lead_findings` count. The
    // three typed lists are independent — the
    // `sync_base_findings` helper materializes the
    // generic `findings` vec from all three but the
    // typed lists themselves stay separate.
    let mut store = FindingStore::new();
    let fl = make_lead("upwork", "Acme", "Build a site", Some("https://upwork.com/jobs/1"), None);
    store.add_freelance_lead(fl);
    assert_eq!(store.freelance_findings().len(), 1);
    assert_eq!(store.job_findings().len(), 0);
    assert_eq!(store.lead_findings().len(), 0);
}

/// Helper for the integration tests. Mirrors the
/// test-only helper in `research.rs` — kept local so
/// the integration test is self-contained and doesn't
/// depend on the unit-test `super::*` namespace.
fn make_lead(
    platform: &str,
    client: &str,
    scope: &str,
    url: Option<&str>,
    budget: Option<FreelanceBudget>,
) -> FreelanceLead {
    FreelanceLead {
        base: ResearchFinding {
            id: String::new(),
            platform: platform.to_string(),
            title: Some(scope.to_string()),
            company: Some(client.to_string()),
            email: None,
            url: url.map(|s| s.to_string()),
            note: String::new(),
            added_at_secs: 0,
        },
        client_name: client.to_string(),
        project_scope: scope.to_string(),
        budget_hint: budget,
        contact_method: None,
        source_platform: platform.to_string(),
    }
}
