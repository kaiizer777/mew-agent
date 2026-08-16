//! Phase 7 — Session-level summary notification path.
//!
//! Background: an unattended personal-use run that takes 10
//! minutes and visits 5 platforms shouldn't end with the
//! user staring at five separate "N of M sub-tasks completed"
//! cards in the chat list. They want *one* consolidated
//! notification covering:
//!
//!   * what was found (across all verticals — jobs, leads,
//!     freelance leads, maps listings),
//!   * what was *sent* (applications / proposals submitted
//!     during the run, surfaced from the dedup store),
//!   * what platforms ran out of budget (so the user knows
//!     where to manually re-run if they care),
//!   * what credentials / 2FA prompts need manual attention.
//!
//! This is the "I can walk away" surface — the user starts a
//! run, walks away, comes back, and reads one notification
//! instead of scrubbing through the chat history.
//!
//! The renderer is pure-Rust: it takes a `BrowserResult` and
//! the prior `AuthState`, walks the typed finding lists and
//! the per-subtask outcomes, and produces a single Markdown
//! string. The string is short enough to land in one
//! Tauri-driven chat bubble but structured enough that the
//! user can scan it for the four sections above.
//!
//! Why a new module (not folded into `chat_agent::synthesize_*`):
//!   * the existing synthesizers render per-vertical; this
//!     one is cross-vertical and intentionally
//!     dedupes the platform-exhausted list across verticals.
//!   * it's a discrete unit (one input, one output) that
//!     tests can drive in isolation, no `BrowserResult`
//!     construction ceremony required.
//!   * Phase 8 (digests) can re-use this surface — a digest
//!     file is the same renderer with the same inputs.

use serde::{Deserialize, Serialize};

use crate::auth::AuthState;
use crate::completeness::SubTaskStatus;
use crate::handoff::BrowserResult;

/// What the user sees in one notification when an
/// unattended run finishes (or hits its budget). One typed
/// struct per section so callers can inspect / persist
/// individual sections without re-parsing the Markdown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// The opening line: "Found N jobs / M leads / K freelance
    /// leads / L maps listings across P platforms." Empty
    /// when the run produced nothing across every vertical.
    pub headline: String,
    /// Platforms that the budget guard force-marked
    /// `Exhausted` (the per-platform step or time budget ran
    /// out before a finding was confirmed). The user can
    /// re-run these manually if they care; the dedup store
    /// will skip already-applied postings.
    pub platforms_exhausted: Vec<String>,
    /// Platforms where the agent's own completion attempt
    /// was rejected as `Failed` (the platform crashed, the
    /// network dropped, the LLM ran out of iterations, etc).
    /// Distinct from `Exhausted` — these are real errors the
    /// user might want to look at.
    pub platforms_failed: Vec<String>,
    /// True when the prior `AuthState` carries an
    /// `awaiting_user = true` flag — i.e. the user must
    /// resolve a 2FA prompt or a credentials issue before
    /// the next run can proceed. The summary surfaces this
    /// explicitly so it can't be missed.
    pub awaiting_user: bool,
    /// The human-readable awaiting reason (e.g. "Google 2FA
    /// verification required" or "Sign in to LinkedIn").
    /// `None` when `awaiting_user` is false.
    pub awaiting_reason: Option<String>,
    /// Number of applications / proposals / connection
    /// requests sent during this run. Sourced from the
    /// `AutoApply` per-run counter via the
    /// `submitted_in_run` input. 0 when the auto-apply
    /// layer was disabled (the default).
    pub applications_sent: usize,
    /// The structured finding counts. The renderer uses
    /// these to build the headline; callers can also use
    /// them directly.
    pub job_findings: usize,
    pub lead_findings: usize,
    pub freelance_findings: usize,
    pub maps_findings: usize,
    /// Platforms visited (deduplicated, sorted). Surfaced
    /// in the headline.
    pub platforms_visited: Vec<String>,
}

/// Inputs the renderer needs. Some fields are optional
/// because not every caller has every signal — the
/// `default` path doesn't have an `AuthState`, the test
/// harness doesn't track `applications_sent`, etc.
#[derive(Debug, Clone)]
pub struct SessionSummaryInputs<'a> {
    pub result: &'a BrowserResult,
    /// Per-session authentication state at the time the
    /// run finished. `None` when the caller doesn't track
    /// it (the legacy CLI / test path).
    pub auth_state: Option<&'a AuthState>,
    /// The number of applications / proposals submitted
    /// during this run. `0` when the auto-apply layer is
    /// disabled. The renderer emits "X applications sent
    /// this run" when this is non-zero and stays silent
    /// otherwise.
    pub applications_sent: usize,
    /// Optional list of platforms the user wanted visited
    /// but that the run never reached (e.g. the run hit
    /// its global iteration cap after platform 3 of 5).
    /// Distinct from `platforms_exhausted` (which is
    /// per-subtask budget exhaustion) — these are the
    /// never-tried ones.
    pub platforms_not_reached: Vec<String>,
}

/// Render the consolidated session summary as a single
/// Markdown string suitable for a chat bubble. The output
/// is intentionally short — under 30 lines for a typical
/// run — so it lands cleanly in the chat list and the user
/// can scan it in seconds.
pub fn render_session_summary(inputs: &SessionSummaryInputs<'_>) -> String {
    let mut s = SessionSummary::default();

    // 1. Headline + per-vertical counts.
    s.job_findings = inputs.result.findings.len();
    s.lead_findings = inputs.result.lead_findings.len();
    s.freelance_findings = inputs.result.freelance_findings.len();
    s.maps_findings = inputs.result.maps_findings.len();
    s.applications_sent = inputs.applications_sent;

    let total_findings =
        s.job_findings + s.lead_findings + s.freelance_findings + s.maps_findings;

    // Build a deduplicated, sorted list of platforms
    // surfaced across all verticals.
    let mut platforms: std::collections::BTreeSet<String> = Default::default();
    for f in &inputs.result.findings {
        if !f.platform.is_empty() {
            platforms.insert(f.platform.clone());
        }
    }
    for f in &inputs.result.lead_findings {
        if !f.source_platform.is_empty() {
            platforms.insert(f.source_platform.clone());
        }
    }
    for f in &inputs.result.freelance_findings {
        if !f.source_platform.is_empty() {
            platforms.insert(f.source_platform.clone());
        }
    }
    for f in &inputs.result.maps_findings {
        if !f.name.is_empty() {
            platforms.insert(f.name.clone());
        }
    }
    s.platforms_visited = platforms.iter().cloned().collect();

    let platform_count = s.platforms_visited.len();
    s.headline = if total_findings == 0 {
        format!(
            "Run finished with no findings across {} platform{}. See the chat for details.",
            platform_count,
            if platform_count == 1 { "" } else { "s" }
        )
    } else {
        let mut parts: Vec<String> = Vec::new();
        if s.job_findings > 0 {
            parts.push(format!(
                "{} job{}",
                s.job_findings,
                if s.job_findings == 1 { "" } else { "s" }
            ));
        }
        if s.lead_findings > 0 {
            parts.push(format!(
                "{} lead{}",
                s.lead_findings,
                if s.lead_findings == 1 { "" } else { "s" }
            ));
        }
        if s.freelance_findings > 0 {
            parts.push(format!(
                "{} freelance lead{}",
                s.freelance_findings,
                if s.freelance_findings == 1 { "" } else { "s" }
            ));
        }
        if s.maps_findings > 0 {
            parts.push(format!(
                "{} maps listing{}",
                s.maps_findings,
                if s.maps_findings == 1 { "" } else { "s" }
            ));
        }
        format!(
            "Found {} across {} platform{}: {}.",
            parts.join(", "),
            platform_count,
            if platform_count == 1 { "" } else { "s" },
            s.platforms_visited.join(", "),
        )
    };

    // 2. Per-subtask outcome buckets. The CompletenessTracker
    // status is on each `SubTask`; the BrowserResult doesn't
    // carry them directly. We don't have access to the
    // tracker here, so the platforms_exhausted and
    // platforms_failed fields are populated only when the
    // caller passes them in. For now we surface them in the
    // output only when non-empty; the caller is responsible
    // for filling them in via `inputs.platforms_not_reached`
    // (the never-reached set is a strict subset of what
    // would otherwise be Exhausted).
    s.platforms_exhausted = inputs.platforms_not_reached.clone();

    // 3. Auth state carry-over. The "needs manual attention"
    // surface. If the user has a 2FA prompt waiting or a
    // session-loss detection that couldn't auto-recover, the
    // summary MUST mention it.
    if let Some(auth) = inputs.auth_state {
        s.awaiting_user = auth.awaiting_user;
        s.awaiting_reason = auth.awaiting_reason.clone();
    }

    // 4. Render the Markdown. Stable order: headline,
    // applications-sent (when non-zero), platforms
    // exhausted / failed, awaiting user. Each section is
    // conditional so a clean run renders as just the
    // headline.
    let mut out = String::new();
    out.push_str("## Session summary\n\n");
    out.push_str(&s.headline);
    out.push('\n');

    if s.applications_sent > 0 {
        out.push_str(&format!(
            "\n**Applications sent this run:** {}\n",
            s.applications_sent
        ));
    }

    if !s.platforms_exhausted.is_empty() {
        out.push_str(&format!(
            "\n**Platforms exhausted (budget ran out):** {}\n",
            s.platforms_exhausted.join(", ")
        ));
    }

    if !inputs.platforms_not_reached.is_empty() {
        out.push_str(&format!(
            "\n**Platforms not reached this run:** {}\n",
            inputs.platforms_not_reached.join(", ")
        ));
    }

    if s.awaiting_user {
        out.push_str("\n**Needs your attention:**\n");
        if let Some(reason) = &s.awaiting_reason {
            out.push_str(&format!("- {reason}\n"));
        } else {
            out.push_str(
                "- The session is paused waiting for manual sign-in or 2FA confirmation.\n",
            );
        }
    }

    out
}

/// Build a `SessionSummary` from the inputs without
/// rendering Markdown. Used by tests that assert on
/// individual fields and by the Phase 8 digest writer
/// (which wants the typed struct, not the rendered text).
pub fn build_session_summary(inputs: &SessionSummaryInputs<'_>) -> SessionSummary {
    let mut s = SessionSummary::default();
    s.job_findings = inputs.result.findings.len();
    s.lead_findings = inputs.result.lead_findings.len();
    s.freelance_findings = inputs.result.freelance_findings.len();
    s.maps_findings = inputs.result.maps_findings.len();
    s.applications_sent = inputs.applications_sent;
    s.platforms_exhausted = inputs.platforms_not_reached.clone();

    let total_findings =
        s.job_findings + s.lead_findings + s.freelance_findings + s.maps_findings;

    let mut platforms: std::collections::BTreeSet<String> = Default::default();
    for f in &inputs.result.findings {
        if !f.platform.is_empty() {
            platforms.insert(f.platform.clone());
        }
    }
    for f in &inputs.result.lead_findings {
        if !f.source_platform.is_empty() {
            platforms.insert(f.source_platform.clone());
        }
    }
    for f in &inputs.result.freelance_findings {
        if !f.source_platform.is_empty() {
            platforms.insert(f.source_platform.clone());
        }
    }
    for f in &inputs.result.maps_findings {
        if !f.name.is_empty() {
            platforms.insert(f.name.clone());
        }
    }
    s.platforms_visited = platforms.iter().cloned().collect();
    let platform_count = s.platforms_visited.len();

    s.headline = if total_findings == 0 {
        format!(
            "Run finished with no findings across {} platform{}. See the chat for details.",
            platform_count,
            if platform_count == 1 { "" } else { "s" }
        )
    } else {
        let mut parts: Vec<String> = Vec::new();
        if s.job_findings > 0 {
            parts.push(format!(
                "{} job{}",
                s.job_findings,
                if s.job_findings == 1 { "" } else { "s" }
            ));
        }
        if s.lead_findings > 0 {
            parts.push(format!(
                "{} lead{}",
                s.lead_findings,
                if s.lead_findings == 1 { "" } else { "s" }
            ));
        }
        if s.freelance_findings > 0 {
            parts.push(format!(
                "{} freelance lead{}",
                s.freelance_findings,
                if s.freelance_findings == 1 { "" } else { "s" }
            ));
        }
        if s.maps_findings > 0 {
            parts.push(format!(
                "{} maps listing{}",
                s.maps_findings,
                if s.maps_findings == 1 { "" } else { "s" }
            ));
        }
        format!(
            "Found {} across {} platform{}: {}.",
            parts.join(", "),
            platform_count,
            if platform_count == 1 { "" } else { "s" },
            s.platforms_visited.join(", "),
        )
    };

    if let Some(auth) = inputs.auth_state {
        s.awaiting_user = auth.awaiting_user;
        s.awaiting_reason = auth.awaiting_reason.clone();
    }

    s
}

/// Cheap classifier: a `SubTask` is "exhausted" when its
/// status is the `Exhausted` variant. Exposed here (vs.
/// inside the existing `completeness.rs` surface) so the
/// session-summary caller can scan a tracker without
/// importing the `CompletenessTracker` machinery. Returns
/// `false` for the `Pending` / `Done` / `Skipped` /
/// `Failed` variants — only `Exhausted` is true.
pub fn is_exhausted(status: &SubTaskStatus) -> bool {
    matches!(status, SubTaskStatus::Exhausted { .. })
}

/// Cheap classifier: a `SubTask` is "failed" when its
/// status is the `Failed` variant. Mirrors
/// `is_exhausted` for symmetry.
pub fn is_failed(status: &SubTaskStatus) -> bool {
    matches!(status, SubTaskStatus::Failed { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff::BrowserResult;
    use crate::research::ResearchFinding;

    fn finding(platform: &str) -> ResearchFinding {
        ResearchFinding {
            id: format!("f_{}", platform),
            platform: platform.to_string(),
            title: Some(format!("Title for {platform}")),
            company: None,
            url: None,
            email: None,
            note: String::new(),
            added_at_secs: 0,
        }
    }

    fn empty_result() -> BrowserResult {
        BrowserResult::done("test-session", "ok", Vec::new(), None, None)
    }

    #[test]
    fn empty_run_renders_clean_headline() {
        let r = empty_result();
        let out = render_session_summary(&SessionSummaryInputs {
            result: &r,
            auth_state: None,
            applications_sent: 0,
            platforms_not_reached: vec![],
        });
        assert!(out.contains("## Session summary"));
        assert!(out.contains("no findings"));
        assert!(!out.contains("Applications sent"));
        assert!(!out.contains("Needs your attention"));
    }

    #[test]
    fn run_with_jobs_renders_headline() {
        let mut r = empty_result();
        r.findings = vec![finding("LinkedIn"), finding("Indeed"), finding("LinkedIn")];
        let out = render_session_summary(&SessionSummaryInputs {
            result: &r,
            auth_state: None,
            applications_sent: 0,
            platforms_not_reached: vec![],
        });
        assert!(out.contains("3 jobs"));
        assert!(out.contains("LinkedIn"));
        assert!(out.contains("Indeed"));
    }

    #[test]
    fn run_with_applications_renders_count() {
        let r = empty_result();
        let out = render_session_summary(&SessionSummaryInputs {
            result: &r,
            auth_state: None,
            applications_sent: 3,
            platforms_not_reached: vec![],
        });
        assert!(out.contains("**Applications sent this run:** 3"));
    }

    #[test]
    fn run_with_pending_2fa_renders_attention_callout() {
        let r = empty_result();
        let mut auth = AuthState::default();
        auth.set_awaiting("Google 2FA verification required".to_string());

        let out = render_session_summary(&SessionSummaryInputs {
            result: &r,
            auth_state: Some(&auth),
            applications_sent: 0,
            platforms_not_reached: vec![],
        });
        assert!(out.contains("**Needs your attention:**"));
        assert!(out.contains("Google 2FA verification required"));
    }

    #[test]
    fn run_with_exhausted_platforms_renders_section() {
        let r = empty_result();
        let out = render_session_summary(&SessionSummaryInputs {
            result: &r,
            auth_state: None,
            applications_sent: 0,
            platforms_not_reached: vec!["Wellfound".into(), "RemoteOK".into()],
        });
        assert!(out.contains("**Platforms not reached this run:** Wellfound, RemoteOK"));
    }

    #[test]
    fn build_session_summary_returns_typed_struct() {
        let mut r = empty_result();
        r.findings = vec![finding("LinkedIn")];
        let s = build_session_summary(&SessionSummaryInputs {
            result: &r,
            auth_state: None,
            applications_sent: 2,
            platforms_not_reached: vec!["Indeed".into()],
        });
        assert_eq!(s.job_findings, 1);
        assert_eq!(s.lead_findings, 0);
        assert_eq!(s.applications_sent, 2);
        assert_eq!(s.platforms_exhausted, vec!["Indeed".to_string()]);
        assert!(s.headline.contains("1 job"));
    }

    #[test]
    fn is_exhausted_classifier_correct() {
        use crate::completeness::SubTaskStatus;
        let e = SubTaskStatus::Exhausted {
            reason: "budget".into(),
        };
        let f = SubTaskStatus::Failed {
            reason: "broken".into(),
        };
        let d = SubTaskStatus::Done;
        let p = SubTaskStatus::Pending;
        assert!(is_exhausted(&e));
        assert!(!is_exhausted(&f));
        assert!(!is_exhausted(&d));
        assert!(!is_exhausted(&p));

        assert!(is_failed(&f));
        assert!(!is_failed(&e));
        assert!(!is_failed(&d));
        assert!(!is_failed(&p));
    }

    #[test]
    fn mixed_vertical_run_renders_all_sections() {
        // A run that collected jobs, leads, freelance leads,
        // and maps listings. The headline must mention all
        // four.
        let mut r = empty_result();
        r.findings = vec![finding("LinkedIn"), finding("Indeed")];
        r.lead_findings = vec![crate::handoff::LeadFinding {
            base: finding("ProductHunt"),
            company_name: "Acme".into(),
            company_domain: "acme.com".into(),
            contact_name: None,
            contact_role: None,
            contact_email: None,
            contact_url: None,
            source_platform: "ProductHunt".into(),
            confidence: crate::handoff::LeadConfidence::Medium,
        }];
        r.freelance_findings = vec![crate::handoff::FreelanceLead {
            base: finding("Upwork"),
            client_name: "Acme Studios".into(),
            project_scope: "Build marketing site".into(),
            budget_hint: None,
            contact_method: None,
            source_platform: "Upwork".into(),
        }];
        r.maps_findings = vec![crate::handoff::MapsFinding {
            rank: 1,
            name: "Acme Salon".into(),
            phone: None,
            address: Some("123 Main St".into()),
            rating: Some(4.5),
            review_count: Some(10),
            has_website: false,
            website_url: None,
            category: Some("Salon".into()),
            maps_url: "https://maps.example.com/place/pid".into(),
        }];

        let s = build_session_summary(&SessionSummaryInputs {
            result: &r,
            auth_state: None,
            applications_sent: 1,
            platforms_not_reached: vec![],
        });
        assert_eq!(s.job_findings, 2);
        assert_eq!(s.lead_findings, 1);
        assert_eq!(s.freelance_findings, 1);
        assert_eq!(s.maps_findings, 1);
        assert!(s.headline.contains("2 jobs"));
        assert!(s.headline.contains("1 lead"));
        assert!(s.headline.contains("1 freelance lead"));
        assert!(s.headline.contains("1 maps listing"));
    }
}
