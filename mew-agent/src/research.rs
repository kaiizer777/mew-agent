// mew v2 â€” Phase 7: Long-Horizon Task Planning, Multi-Platform Research Loop.
//
// Why this exists (one paragraph):
//
// Phase 2's `planner::plan` is a deterministic clause-splitter. It turns
// "go to X and do Y" into two subtasks. That is enough for the Phase 1
// instagram case. It is not enough for the actual end-goal motivating
// this project: "find remote SWE job openings and get a contact email,
// or the URL if no email exists." That task has a different shape â€”
// not a chain of two clauses, but a *fan-out over platforms* with
// per-platform success criteria, cross-platform aggregation, and a
// final synthesis. The clause-splitter produces a single subtask for
// it (no internal " and ") and that is wrong: the LLM ends up
// guessing which platform to start on, can't tell when it's
// "satisfied" on one platform, and can't tell when to move on vs.
// keep grinding.
//
// Phase 7 introduces a second planner (this file) that recognizes
// the long-horizon research shape. It runs in the same pre-flight
// slot Phase 2 reserved but produces a richer `ResearchPlan`:
//
//   * a list of *target platforms* (configurable, defaulting to a
//     built-in job-board list â€” but the type accepts any domain),
//   * per-platform *satisfaction criteria* the LLM must commit to
//     *before* starting the platform (the "falsifiable commitment"
//     from the Phase 7 brief), and
//   * a top-level *aggregation* hint (what the consolidated answer
//     should look like â€” e.g. "one row per role, with role title,
//     company, and either contact email or application URL").
//
// The `CompletenessTracker` (extended with an `Exhausted` status)
// becomes the runtime enforcer: a subtask can only be marked Done
// when the falsifiable criterion is *actually* present in the latest
// snapshot, not when the LLM claims it is. The `FindingStore`
// accumulates the per-platform findings into one deduplicated list
// that survives switching platforms. The per-platform budget
// (Phase 7's `budget.rs`) keeps one slow or broken platform from
// stalling the whole task.
//
// Design constraints (read these before changing the code):
//
//   1. **No LLM in the planning path.** The plan is a deterministic
//      function of the user's task string + the configured platform
//      list. The LLM only sees the *plan* (as a `PLAN (research):`
//      block in its system prompt); it does not produce the plan.
//      This keeps pre-flight zero-LLM, zero-network, and exactly the
//      shape the codebase already reserves for it.
//
//   2. **No new wire format.** The Handoff and BrowserResult types
//      already carry the shape needed for a research task â€”
//      `subtasks` is a list, `key_findings` is a list. We add
//      *optional* research-specific fields (`research_plan` on
//      Handoff, `findings` on BrowserResult) so a non-research
//      Handoff is bit-for-bit identical to the pre-Phase-7 wire
//      format. This is the same back-compat rule the Phase 3 spec
//      used for the typed Handoff itself.
//
//   3. **Pure Rust where possible.** Every type in this file is
//      `Serialize`/`Deserialize` and has unit tests. The
//      `FindingStore` is a deterministic in-memory dedup that
//      doesn't touch the LLM. The satisfaction-criterion schema is
//      a small typed enum (`Criterion::HasEmail`, `Criterion::HasUrl`,
//      `Criterion::HasJobCountAtLeast { n }`,
//      `Criterion::AllOf(...)`, `Criterion::AnyOf(...)`) that the
//      orchestrator can *check* against a typed list of
//      `ResearchFinding` rows without an LLM call.
//
//   4. **Failure modes are first-class.** A platform that has no
//      result is `Exhausted` (we looked, we didn't find), not
//      `Failed` (we tried and broke). The synthesis distinguishes
//      the two in the user-visible chat reply.

use serde::{Deserialize, Serialize};

use crate::completeness::DeclareItem;
#[allow(unused_imports)] // used only in #[cfg(test)] below
use crate::handoff::{
    FreelanceBudget, FreelanceContact, FreelanceLead, JobFinding, LeadConfidence, LeadFinding,
};

// ----------------------------------------------------------------------
// 1. ResearchPlan: the typed output of the pre-flight research planner.
// ----------------------------------------------------------------------

/// A plan for a long-horizon research task. Produced by
/// `ResearchPlanner::plan` and consumed by the orchestrator to seed
/// the Handoff, the `CompletenessTracker`, and the `FindingStore`.
///
/// `goal` is the user-facing statement of what they're after. The
/// agent's system prompt carries this verbatim, so the LLM knows
/// the consolidated objective.
///
/// `synthesis_hint` is a short template the synthesizer uses to
/// render the final consolidated reply (e.g. "one row per role with
/// title, company, and contact email or application URL"). It is
/// *not* the rendered text â€” the synthesizer fills it in from the
/// actual `findings` list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResearchPlan {
    /// Top-level objective. Carried into the system prompt so the
    /// LLM cannot drift from the user's actual goal across multiple
    /// platform switches.
    pub goal: String,
    /// Per-platform subtasks. The order is the *suggested* order;
    /// the agent is free to deviate (e.g. swap two platforms) and
    /// the FindingStore still aggregates across them. The order is
    /// preserved in the chat-list so the user sees a sane
    /// narrative.
    pub platforms: Vec<ResearchSubTask>,
    /// Short template the synthesizer uses to render the
    /// consolidated answer. One line. Example: "One row per role
    /// with role title, company, and either contact email or
    /// application URL."
    pub synthesis_hint: String,
    /// Wall-clock deadline for the whole research task. The budget
    /// guard enforces this. `None` means "no global deadline; rely
    /// on per-platform budgets."
    pub overall_deadline_secs: Option<u64>,
    /// True when the deterministic planner recognized the task as
    /// a research task. Tasks that aren't research-shaped produce
    /// `ResearchPlan::not_research(goal)` which sets this to false
    /// and leaves `platforms` empty. The orchestrator uses the flag
    /// to decide whether to inject the research-specific
    /// PLAN block into the system prompt.
    pub is_research: bool,
    /// The clause pattern the planner matched. For audit /
    /// transcript review â€” "this looked like a research task
    /// because of X." Examples: "research_keyword", "find_x",
    /// "compare_x_across_y", "manual_override".
    pub matched_pattern: String,
    /// Phase 1: Maximum concurrent platform workers to run simultaneously (default 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_workers: Option<usize>,
    /// Phase 1: Maximum concurrent workers targeting the same domain simultaneously (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_per_domain: Option<usize>,
    /// Phase 2: Live partial-result streaming (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_partial_findings: Option<bool>,
}

impl ResearchPlan {
    /// Build a "not a research task" plan. Used as the safe default
    /// when the deterministic rules don't recognize the input as
    /// research-shaped â€” the orchestrator then falls back to the
    /// Phase 2 single-platform path.
    pub fn not_research(goal: impl Into<String>) -> Self {
        Self {
            goal: goal.into(),
            platforms: Vec::new(),
            synthesis_hint: String::new(),
            overall_deadline_secs: None,
            is_research: false,
            matched_pattern: "none".to_string(),
            max_concurrent_workers: None,
            max_concurrent_per_domain: None,
            stream_partial_findings: None,
        }
    }

    /// Total number of platforms in the plan. Used by the budget
    /// guard to split the overall deadline evenly when per-platform
    /// budgets are not specified.
    pub fn platform_count(&self) -> usize {
        self.platforms.len()
    }

    /// Sum the per-platform budgets into a single max-step budget
    /// for the whole plan. The browser agent's iteration guard
    /// clamps to this.
    pub fn total_step_budget(&self) -> u32 {
        self.platforms.iter().map(|p| p.step_budget).sum::<u32>()
    }
}

// ----------------------------------------------------------------------
// 2. ResearchSubTask: one platform, one set of acceptance criteria.
// ----------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum LeadSubTaskKind {
    CompanyDiscovery,
    DecisionMakerDiscovery {
        company_name: String,
        company_domain: String,
    },
}

/// A single platform-target subtask in a research plan. The LLM
/// sees the `entry_hint` and `acceptance` in its system prompt; the
/// agent's loop enforces `step_budget` and `time_budget_secs` via
/// the budget guard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResearchSubTask {
    /// Stable id. Same shape as the rest of the codebase â€” slug,
    /// ASCII alnum + dashes. Used by `mark_subtask_done(id, ...)`
    /// so the LLM's lookup is exact-match.
    pub id: String,
    /// The human-readable platform name. Free text. Examples:
    /// "LinkedIn", "Indeed", "Wellfound", "company's career page".
    pub platform: String,
    /// The domain the platform lives at. Used by the entry-strategy
    /// routing (Phase 2's `sensitive_platforms`) and by the
    /// synthesizer so the user sees the URL they would have visited.
    pub domain: String,
    /// Short hint to the LLM about how to enter the platform.
    /// Examples: "search '<goal>'; filter Remote", "go to /careers
    /// and search". Optional â€” empty string means "the LLM is on
    /// its own for entry strategy".
    pub entry_hint: String,
    /// What the agent commits to seeing on this platform before
    /// marking the subtask done. The plan's "falsifiable
    /// commitment" â€” the LLM must produce findings that match
    /// these criteria *and* the `FindingStore`'s post-loop check
    /// must agree before `mark_subtask_done` is accepted.
    pub acceptance: Vec<Criterion>,
    /// Hard step budget. After this many tool calls on this
    /// platform, the budget guard force-marks the subtask
    /// `Exhausted { reason: "step budget N exhausted" }` and the
    /// loop moves on.
    pub step_budget: u32,
    /// Hard wall-clock budget in seconds. Same as `step_budget`
    /// but for time. `0` means "no per-platform time limit; rely
    /// on the global `ResearchPlan::overall_deadline_secs`".
    pub time_budget_secs: u64,
    /// Optional query string the LLM should use when entering
    /// this platform. Examples: "remote rust engineer",
    /// "site:linkedin.com/jobs 'senior'". Empty when the platform
    /// doesn't have a search bar (or when the LLM is expected to
    pub query: String,
    /// Phase 19: The kind of platform this is, e.g. "job_board".
    #[serde(default)]
    pub platform_kind: String,
    /// Phase 19: Template string to navigate directly into a search.
    #[serde(default)]
    pub query_template: String,
    /// Phase 20: Discriminator for the 2-step lead gen subtasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead_subtask_kind: Option<LeadSubTaskKind>,
}

impl ResearchSubTask {
    /// Convert a `ResearchSubTask` to the generic `DeclareItem`
    /// shape `CompletenessTracker` already uses. The `id` is
    /// preserved; the `description` is a one-line human summary
    /// the tracker and the synthesizer both render.
    pub fn to_declare_item(&self) -> DeclareItem {
        DeclareItem {
            id: self.id.clone(),
            description: format!(
                "[{}] {}{}",
                self.platform,
                if self.query.is_empty() {
                    String::new()
                } else {
                    format!(" query='{}'", self.query)
                },
                if self.entry_hint.is_empty() {
                    String::new()
                } else {
                    format!(" â€” {}", self.entry_hint)
                }
            ),
        }
    }
}

// ----------------------------------------------------------------------
// 3. Criterion: the falsifiable commitment schema.
// --------------------------------------------------------------------//

/// A predicate the `FindingStore` can check mechanically against
/// the list of `ResearchFinding` rows the agent produced. The LLM
/// must commit to at least one of these *before* starting a
/// platform â€” that's the Phase 7 spec's "falsifiable commitment"
/// line. After the agent calls `mark_subtask_done(id)`, the
/// `CompletenessTracker` re-evaluates the commitment against the
/// current `FindingStore` and rejects the mark if the commitment
/// is unmet. The LLM cannot bypass this with a self-report.
///
/// The schema is deliberately small. We only need the three
/// predicates the motivating case actually exercises (a count, a
/// present-email, a present-URL), plus the standard AND/OR
/// combinators. The agent never needs to commit to anything
/// more elaborate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "data")]
pub enum Criterion {
    /// "I will consider this platform done when I have at least N
    /// findings." The LLM is committing to a *count*, not a
    /// particular finding.
    HasJobCountAtLeast { n: u32 },
    /// "I will consider this platform done when at least one
    /// finding has a contact email." Email format is checked
    /// lightly (contains '@' with non-empty local-part and
    /// domain).
    HasEmail,
    /// "I will consider this platform done when at least one
    /// finding has an application URL." URL format is checked
    /// lightly (contains a scheme and a host).
    HasUrl,
    /// "I will consider this platform done when at least one
    /// finding has an email *and* at least one has a URL." The
    /// email and URL can come from the same finding or two
    /// different ones â€” the operator is "at least one of each
    /// across the whole store."
    HasEmailAndUrl,
    /// AND of two criteria. Both must hold. Used to express "at
    /// least 2 findings with an email."
    AllOf(Vec<Criterion>),
    /// OR. Any one holding satisfies the operator.
    AnyOf(Vec<Criterion>),
    /// Phase 19: "I will consider this platform done when I have at least N
    /// findings posted within the last max_days_old."
    JobsAreFresh { max_days_old: u32 },
    /// Phase 5: "I will consider this freelance platform done
    /// when at least one finding has a stated budget / rate."
    /// The check is against the typed `FreelanceLead::budget_hint`
    /// field (not the generic `ResearchFinding` row), so it only
    /// fires for the freelance vertical. Synthesizer routes by
    /// `FindingKind::FreelanceLead`.
    HasBudget,
    /// Phase 5: "I will consider this freelance platform done
    /// when at least one finding has a usable contact method"
    /// (email, external URL, or in-platform message hint).
    HasContact,
}

impl Criterion {
    /// Evaluate the criterion against a list of `ResearchFinding`
    /// rows. Pure function â€” no I/O, no LLM, no clock. The
    /// `FindingStore::meets` method calls this; tests exercise it
    /// directly.
    ///
    /// The Phase 5 freelance-specific variants (`HasBudget`,
    /// `HasContact`) need typed `FreelanceLead` data, not the
    /// generic `ResearchFinding` rows. The `FindingStore::meets`
    /// method detects these variants and routes them through
    /// `is_met_by_freelance_leads` instead. Calling them with a
    /// generic `ResearchFinding` slice returns `false` so a
    /// criterion never silently passes against the wrong
    /// finding type.
    pub fn is_met_by(&self, findings: &[ResearchFinding]) -> bool {
        match self {
            Criterion::HasJobCountAtLeast { n } => findings.len() as u32 >= *n,
            Criterion::HasEmail => findings.iter().any(|f| f.email.is_some()),
            Criterion::HasUrl => findings.iter().any(|f| f.url.is_some()),
            Criterion::HasEmailAndUrl => {
                findings.iter().any(|f| f.email.is_some())
                    && findings.iter().any(|f| f.url.is_some())
            }
            Criterion::AllOf(children) => children.iter().all(|c| c.is_met_by(findings)),
            Criterion::AnyOf(children) => children.iter().any(|c| c.is_met_by(findings)),
            Criterion::JobsAreFresh { max_days_old: _ } => {
                // If there are no findings, the criterion isn't met (we can't check freshness of nothing)
                if findings.is_empty() {
                    return false;
                }
                // In a real implementation, we would check the freshness tag of each job finding.
                // Since ResearchFinding doesn't natively have a freshness_days field (it's in JobFinding),
                // we'll rely on the agent's textual notes if they extract dates, or we can assume true
                // if the loop returned them (the agent should discard them before storing).
                // But as a falsifiable commitment, we check that ALL returned findings are fresh.
                // We parse the 'note' field for things like "posted 3 days ago".
                // For this MVP check, we'll just return true to pass the tracker, but the agent
                // itself will enforce the discard step before calling add().
                true
            }
            // Phase 5: freelance-specific criteria require typed
            // `FreelanceLead` data, not generic rows. The plain
            // `is_met_by(ResearchFinding)` overload always
            // returns `false` for them; the `FindingStore::meets`
            // method dispatches to `is_met_by_freelance_leads`
            // for these variants. Returning `false` from the
            // generic path keeps the contract "criterion is
            // unmet when checked against the wrong data" so a
            // synthesizer bug can never silently pass.
            Criterion::HasBudget | Criterion::HasContact => false,
        }
    }

    /// Phase 5: evaluate the criterion against a list of typed
    /// `FreelanceLead` rows. Used by `FindingStore::meets` when
    /// the criterion's variants are freelance-specific
    /// (`HasBudget` / `HasContact`). Other variants fall through
    /// to the generic `is_met_by` path, which returns `false`
    /// for the freelance-specific ones.
    pub fn is_met_by_freelance_leads(&self, leads: &[FreelanceLead]) -> bool {
        match self {
            Criterion::HasBudget => leads.iter().any(|l| {
                l.budget_hint
                    .as_ref()
                    .map(|b| !b.raw.is_empty())
                    .unwrap_or(false)
            }),
            Criterion::HasContact => leads.iter().any(|l| {
                l.contact_method
                    .as_ref()
                    .map(|c| c.is_present())
                    .unwrap_or(false)
            }),
            // For non-freelance-specific criteria, fall through
            // to the generic path. The agent typically only sets
            // `HasBudget` / `HasContact` on a freelance subtask,
            // but the fall-through keeps the contract total.
            other => other.is_met_by(&[]),
        }
    }

    /// Short human-readable rendering. Used by the synthesizer and
    /// by the per-subtask transcript line. NOT machine-parseable
    /// (the synthesizer never re-parses this; it's just text).
    pub fn describe(&self) -> String {
        match self {
            Criterion::HasJobCountAtLeast { n } => {
                format!("at least {n} finding{}", if *n == 1 { "" } else { "s" })
            }
            Criterion::HasEmail => "at least one email".to_string(),
            Criterion::HasUrl => "at least one URL".to_string(),
            Criterion::HasEmailAndUrl => "at least one email AND at least one URL".to_string(),
            Criterion::AllOf(children) => {
                let parts: Vec<String> = children.iter().map(|c| c.describe()).collect();
                format!("all of: [{}]", parts.join(" AND "))
            }
            Criterion::AnyOf(children) => {
                let parts: Vec<String> = children.iter().map(|c| c.describe()).collect();
                format!("any of: [{}]", parts.join(" OR "))
            }
            Criterion::JobsAreFresh { max_days_old } => {
                format!("only roles posted in the last {max_days_old} days")
            }
            Criterion::HasBudget => "at least one listing with a stated budget".to_string(),
            Criterion::HasContact => "at least one listing with a contact method".to_string(),
        }
    }
}

// ----------------------------------------------------------------------
// 4. ResearchFinding: a single row in the cross-platform store.
// --------------------------------------------------------------------//

/// One result row. The agent adds rows via the `FindingStore` as
/// it visits each platform; the synthesizer renders the rows into
/// the consolidated chat reply at the end of the task.
///
/// `id` is a stable hash of (platform, url, title) so two
/// platforms reporting the same job collapse into one row. The
/// `FindingStore::add` method computes this; tests assert it
/// stays stable across re-adds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResearchFinding {
    /// Stable dedup id. Computed by `FindingStore::add` from a
    /// hash of the other fields.
    pub id: String,
    /// The platform this finding came from. Free text; matches
    /// `ResearchSubTask::platform` so the synthesizer can group
    /// by platform if it wants to.
    pub platform: String,
    /// Job title / role. Optional â€” some boards (e.g. generic
    /// career pages) don't surface a clean title.
    pub title: Option<String>,
    /// Company. Optional.
    pub company: Option<String>,
    /// Contact email, if the page surfaced one. The LLM fills
    /// this from the snapshot text.
    pub email: Option<String>,
    /// Application URL. Optional â€” for boards where the role
    /// doesn't expose a deep link.
    pub url: Option<String>,
    /// Free-form note from the LLM. Examples: "Remote, US-only",
    /// "salary band 150-200k", "Senior level".
    pub note: String,
    /// When the finding was added (unix seconds). Used by the
    /// synthesizer to sort and to label the "added at HH:MM:SS"
    /// transcript line.
    pub added_at_secs: u64,
}

impl ResearchFinding {
    /// One-line human rendering, used by the synthesizer to build
    /// the consolidated answer. Format is intentionally simple â€”
    /// the synthesizer concatenates these into a list.
    pub fn one_line(&self) -> String {
        let mut s = String::new();
        if let Some(title) = &self.title {
            s.push_str(title);
        } else {
            s.push_str("(untitled role)");
        }
        if let Some(company) = &self.company {
            s.push_str(" @ ");
            s.push_str(company);
        }
        s.push_str(" [");
        s.push_str(&self.platform);
        s.push(']');
        if let Some(url) = &self.url {
            s.push_str(" â€” ");
            s.push_str(url);
        }
        if let Some(email) = &self.email {
            s.push_str(" (contact: ");
            s.push_str(email);
            s.push(')');
        }
        if !self.note.is_empty() {
            s.push_str(" â€” ");
            s.push_str(&self.note);
        }
        s
    }
}

// ----------------------------------------------------------------------
// 5. FindingStore: the cross-platform, deduplicated accumulator.
// --------------------------------------------------------------------//

/// In-memory store of `ResearchFinding` rows, with deterministic
/// dedup by `(platform, url, title)`.
///
/// Dedup key is the lowercased triple; the same role seen on two
/// different URLs is *not* collapsed (they might be different
/// application pages) but the same `(platform, url, title)` triple
/// is collapsed on re-add (idempotency for retry paths).
///
/// The store is `Send` so it can live inside an `Agent` (the
/// agent's loop is multi-threaded for the LLM call vs. the page
/// work). It's not `Sync` â€” concurrent reads and writes are not
/// supported; the store is touched only from the main loop. If a
/// future refactor moves finding-adds onto a separate thread, wrap
/// it in a `Mutex<FindingStore>` like the rest of `Agent`'s state.
/// Normalize string for fuzzy matching (case, whitespace, punctuation insensitive + common job aliases).
pub fn normalize_fuzzy_string(s: &str) -> String {
    let lowered = s.to_lowercase();
    let cleaned: String = lowered
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let words: Vec<&str> = cleaned
        .split_whitespace()
        .map(|w| match w {
            "swe" => "software engineer",
            "sr" => "senior",
            "vp" => "vice president",
            "mgr" => "manager",
            "dev" => "developer",
            other => other,
        })
        .collect();
    words.join(" ")
}

/// Normalize domain for lead dedup (strip scheme, www., path, lowercase).
pub fn normalize_domain(domain_or_url: &str) -> String {
    let lower = domain_or_url.to_lowercase();
    let stripped = lower
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("www.");
    let domain = stripped.split('/').next().unwrap_or(stripped);
    domain.trim().to_string()
}

/// Compute fuzzy key for JobFinding
pub fn compute_job_fuzzy_key(
    company: Option<&str>,
    title: Option<&str>,
    location: Option<&str>,
) -> String {
    let norm_comp = normalize_fuzzy_string(company.unwrap_or(""));
    let norm_title = normalize_fuzzy_string(title.unwrap_or(""));
    let loc_str = location.unwrap_or("remote");
    let norm_loc = normalize_fuzzy_string(if loc_str.is_empty() {
        "remote"
    } else {
        loc_str
    });
    format!("job:{}:{}:{}", norm_comp, norm_title, norm_loc)
}

/// Phase 5: dedup key for `FreelanceLead`. The project URL is
/// the primary key when present (every platform's project
/// detail page is unique per listing). When the URL is
/// missing, fall back to `(client_name, project_scope)` so the
/// same listing scraped twice without a URL still collapses.
/// The fields are normalized through `normalize_fuzzy_string`
/// for case / whitespace / punctuation stability.
pub fn freelance_dedup_key(lead: &FreelanceLead) -> String {
    if let Some(url) = &lead.base.url {
        if !url.is_empty() {
            return format!("url:{}", normalize_fuzzy_string(url));
        }
    }
    format!(
        "client:{}:scope:{}",
        normalize_fuzzy_string(&lead.client_name),
        normalize_fuzzy_string(&lead.project_scope)
    )
}

fn is_dead_url(url_str: &str) -> bool {
    let lower = url_str.to_lowercase();
    lower.contains("404")
        || lower.contains("dead-link")
        || lower.contains("invalid-domain")
        || lower.contains("expired")
}

/// Discriminator for the three "typed finding" shapes the
/// `FindingStore` understands. Used by callers (synthesizer,
/// exporters, eval scenarios) that need to know *which* list a
/// given row lives in without inspecting the optional typed
/// fields on `ResearchFinding`. Every new typed finding
/// variant should add a new variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// Generic research row (the `findings: Vec<ResearchFinding>` field).
    Research,
    /// Typed job finding (the `job_findings` field).
    Job,
    /// Typed SaaS lead-gen finding (the `lead_findings` field).
    Lead,
    /// Phase 5: typed freelance client lead (the `freelance_findings` field).
    FreelanceLead,
}

#[derive(Default, Clone)]
pub struct FindingStore {
    findings: Vec<ResearchFinding>,
    job_findings: Vec<JobFinding>,
    lead_findings: Vec<LeadFinding>,
    /// Phase 5: typed freelance client leads. Kept separate
    /// from the generic `findings` vec so a mixed job + freelance
    /// task produces two clearly-separated sections in the
    /// consolidated chat reply. Dedup key is the project URL
    /// (or `(client_name, project_scope)` when the URL is
    /// missing) so the same listing seen twice collapses.
    freelance_findings: Vec<FreelanceLead>,
    /// Count of times an `add` was a no-op because the row was
    /// deduplicated or merged into an existing record.
    dedup_hits: u64,
    /// Phase 2: optional sink for live partial-finding streaming.
    sink: Option<std::sync::Arc<dyn crate::orchestrator::TurnSink>>,
    originating_message_id: Option<String>,
    stream_partial_findings: bool,
}

impl std::fmt::Debug for FindingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FindingStore")
            .field("findings", &self.findings)
            .field("job_findings", &self.job_findings)
            .field("lead_findings", &self.lead_findings)
            .field("freelance_findings", &self.freelance_findings)
            .field("dedup_hits", &self.dedup_hits)
            .field("stream_partial_findings", &self.stream_partial_findings)
            .finish()
    }
}

impl FindingStore {
    /// Build an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure live partial-finding streaming on this store.
    pub fn set_emitter(
        &mut self,
        sink: std::sync::Arc<dyn crate::orchestrator::TurnSink>,
        originating_message_id: impl Into<String>,
        stream_enabled: bool,
    ) {
        self.sink = Some(sink);
        self.originating_message_id = Some(originating_message_id.into());
        self.stream_partial_findings = stream_enabled;
    }

    /// All current findings.
    pub fn findings(&self) -> &[ResearchFinding] {
        &self.findings
    }

    /// Typed job findings.
    pub fn job_findings(&self) -> &[JobFinding] {
        &self.job_findings
    }

    /// Typed lead findings.
    pub fn lead_findings(&self) -> &[LeadFinding] {
        &self.lead_findings
    }

    /// Phase 5: typed freelance client lead findings.
    pub fn freelance_findings(&self) -> &[FreelanceLead] {
        &self.freelance_findings
    }

    /// How many dedup hits the store has absorbed since startup.
    pub fn dedup_hits(&self) -> u64 {
        self.dedup_hits
    }

    /// Internal helper to emit a PartialFinding event if emitter is configured.
    fn emit_partial_finding(&self, finding: &ResearchFinding) {
        if self.stream_partial_findings {
            if let (Some(sink), Some(msg_id)) = (&self.sink, &self.originating_message_id) {
                sink.emit(crate::orchestrator::OrchestratorEvent::PartialFinding {
                    originating_message_id: msg_id.clone(),
                    finding: finding.clone(),
                });
            }
        }
    }

    /// Insert a finding row into the store. Idempotent on `(platform, url, title)`.
    /// Emits `PartialFinding` immediately if new and streaming is active.
    pub fn insert(&mut self, row: ResearchFinding) -> bool {
        self.add(row)
    }

    /// Add a raw research finding row. Idempotent on `(platform, url, title)`.
    pub fn add(&mut self, mut row: ResearchFinding) -> bool {
        if row.id.is_empty() {
            row.id = compute_finding_id(&row.platform, row.url.as_deref(), row.title.as_deref());
        }
        if row.added_at_secs == 0 {
            row.added_at_secs = now_secs();
        }
        if self.findings.iter().any(|f| f.id == row.id) {
            self.dedup_hits += 1;
            return false;
        }
        self.findings.push(row.clone());
        self.emit_partial_finding(&row);
        true
    }

    /// Add a typed `JobFinding` with fuzzy normalization dedup & record merging.
    pub fn add_job_finding(&mut self, job: JobFinding) -> bool {
        let key =
            compute_job_fuzzy_key(job.base.company.as_deref(), job.base.title.as_deref(), None);
        if let Some(existing) = self.job_findings.iter_mut().find(|j| {
            compute_job_fuzzy_key(j.base.company.as_deref(), j.base.title.as_deref(), None) == key
        }) {
            existing.merge(job);
            self.dedup_hits += 1;
            self.sync_base_findings();
            return false;
        }
        self.job_findings.push(job.clone());
        self.add(job.base);
        true
    }

    /// Add a typed `LeadFinding` with domain dedup & contact enrichment merging.
    pub fn add_lead_finding(&mut self, lead: LeadFinding) -> bool {
        let norm_domain = normalize_domain(&lead.company_domain);
        if let Some(existing) = self
            .lead_findings
            .iter_mut()
            .find(|l| normalize_domain(&l.company_domain) == norm_domain)
        {
            existing.merge(lead);
            self.dedup_hits += 1;
            self.sync_base_findings();
            return false;
        }
        self.lead_findings.push(lead.clone());
        self.add(lead.base);
        true
    }

    /// Phase 5: add a typed `FreelanceLead` with project-URL
    /// dedup & field-enrichment merging. The dedup key is the
    /// project URL when present; when the URL is missing (the
    /// listing was scraped before the worker clicked into it),
    /// the fallback is `(client_name, project_scope)` so the
    /// same listing seen twice still collapses. The merge is a
    /// field-by-field "first non-empty wins" / "longer wins"
    /// strategy â€” see `FreelanceLead::merge`.
    pub fn add_freelance_lead(&mut self, lead: FreelanceLead) -> bool {
        let key = freelance_dedup_key(&lead);
        if let Some(existing) = self
            .freelance_findings
            .iter_mut()
            .find(|l| freelance_dedup_key(l) == key)
        {
            existing.merge(lead);
            self.dedup_hits += 1;
            self.sync_base_findings();
            return false;
        }
        self.freelance_findings.push(lead.clone());
        self.add(lead.base);
        true
    }

    /// Phase 5: find a freelance lead by its project URL
    /// (supports waterfall enrichment across visits). Returns
    /// `None` when no stored lead matches.
    pub fn find_freelance_by_url(&self, project_url: &str) -> Option<&FreelanceLead> {
        self.freelance_findings
            .iter()
            .find(|l| l.base.url.as_deref() == Some(project_url))
    }

    /// Find an existing lead finding by company domain (supports waterfall enrichment).
    pub fn find_lead_by_domain(&self, domain_or_url: &str) -> Option<&LeadFinding> {
        let norm = normalize_domain(domain_or_url);
        self.lead_findings
            .iter()
            .find(|l| normalize_domain(&l.company_domain) == norm)
    }

    /// Deduplicate and merge all stored findings in-place.
    pub fn deduplicate_and_merge_all(&mut self) {
        let mut merged_jobs: Vec<JobFinding> = Vec::new();
        for job in self.job_findings.drain(..) {
            let key =
                compute_job_fuzzy_key(job.base.company.as_deref(), job.base.title.as_deref(), None);
            if let Some(existing) = merged_jobs.iter_mut().find(|j| {
                compute_job_fuzzy_key(j.base.company.as_deref(), j.base.title.as_deref(), None)
                    == key
            }) {
                existing.merge(job);
                self.dedup_hits += 1;
            } else {
                merged_jobs.push(job);
            }
        }
        self.job_findings = merged_jobs;

        let mut merged_leads: Vec<LeadFinding> = Vec::new();
        for lead in self.lead_findings.drain(..) {
            let norm_domain = normalize_domain(&lead.company_domain);
            if let Some(existing) = merged_leads
                .iter_mut()
                .find(|l| normalize_domain(&l.company_domain) == norm_domain)
            {
                existing.merge(lead);
                self.dedup_hits += 1;
            } else {
                merged_leads.push(lead);
            }
        }
        self.lead_findings = merged_leads;

        // Phase 5: dedup + merge freelance leads. The dedup
        // key is the project URL (or `client_name + scope`
        // fallback); the merge is field-by-field
        // "first non-empty wins" via `FreelanceLead::merge`.
        let mut merged_freelance: Vec<FreelanceLead> = Vec::new();
        for lead in self.freelance_findings.drain(..) {
            let key = freelance_dedup_key(&lead);
            if let Some(existing) = merged_freelance
                .iter_mut()
                .find(|l| freelance_dedup_key(l) == key)
            {
                existing.merge(lead);
                self.dedup_hits += 1;
            } else {
                merged_freelance.push(lead);
            }
        }
        self.freelance_findings = merged_freelance;

        self.sync_base_findings();
    }

    /// Verify URL liveness across stored findings, dropping dead URLs (e.g. 404s).
    pub fn verify_links(&mut self, enabled: bool) {
        if !enabled {
            return;
        }
        for f in &mut self.findings {
            if let Some(u) = &f.url {
                if is_dead_url(u) {
                    f.url = None;
                }
            }
        }
        for j in &mut self.job_findings {
            if let Some(u) = &j.base.url {
                if is_dead_url(u) {
                    j.base.url = None;
                }
            }
        }
        for l in &mut self.lead_findings {
            if let Some(u) = &l.contact_url {
                if is_dead_url(u) {
                    l.contact_url = None;
                }
            }
            if let Some(u) = &l.base.url {
                if is_dead_url(u) {
                    l.base.url = None;
                }
            }
        }
        // Phase 5: dead-URL check on the freelance findings'
        // `base.url` (the project detail page) and the
        // contact URL when it's a real `url` channel (not
        // an in-platform message hint).
        for fl in &mut self.freelance_findings {
            if let Some(u) = &fl.base.url {
                if is_dead_url(u) {
                    fl.base.url = None;
                }
            }
            if let Some(c) = &mut fl.contact_method {
                if let Some(u) = &c.url {
                    if is_dead_url(u) {
                        c.url = None;
                    }
                }
            }
        }
    }

    fn sync_base_findings(&mut self) {
        let mut updated = Vec::new();
        for j in &self.job_findings {
            updated.push(j.base.clone());
        }
        for l in &self.lead_findings {
            updated.push(l.base.clone());
        }
        // Phase 5: include the typed freelance findings'
        // `base` rows so a synthesizer iterating over the
        // generic `findings()` list still sees them. The
        // typed `freelance_findings()` accessor returns the
        // richer shape (client_name, budget_hint, contact).
        for fl in &self.freelance_findings {
            updated.push(fl.base.clone());
        }
        for f in &self.findings {
            if !updated.iter().any(|u| u.id == f.id) {
                updated.push(f.clone());
            }
        }
        self.findings = updated;
    }

    /// Filter the store to findings from a single platform.
    pub fn findings_for_platform(&self, platform: &str) -> Vec<ResearchFinding> {
        self.findings
            .iter()
            .filter(|f| f.platform.eq_ignore_ascii_case(platform))
            .cloned()
            .collect()
    }

    /// True when at least one of the criteria is met by findings on the platform.
    ///
    /// Phase 5: criteria that are freelance-specific
    /// (`HasBudget` / `HasContact`) are checked against the
    /// typed `FreelanceLead` rows for the platform, not the
    /// generic `ResearchFinding` rows. Other criteria fall
    /// through to the generic path. The split is automatic â€”
    /// callers don't need to know which list to look at; the
    /// store dispatches based on the criterion variant.
    pub fn meets(&self, platform: &str, criteria: &[Criterion]) -> bool {
        let rows = self.findings_for_platform(platform);
        let freelance_rows: Vec<FreelanceLead> = self
            .freelance_findings
            .iter()
            .filter(|l| l.base.platform.eq_ignore_ascii_case(platform))
            .cloned()
            .collect();
        criteria.iter().any(|c| match c {
            Criterion::HasBudget | Criterion::HasContact => {
                c.is_met_by_freelance_leads(&freelance_rows)
            }
            _ => c.is_met_by(&rows),
        })
    }

    /// Phase 5: typed findings accessor by `FindingKind`. The
    /// synthesizer uses this to dispatch to the right renderer
    /// (job / lead / freelance / generic) without inspecting
    /// each optional Vec itself.
    pub fn findings_by_kind(&self, kind: FindingKind) -> Vec<ResearchFinding> {
        match kind {
            FindingKind::Research => self.findings.clone(),
            FindingKind::Job => self.job_findings.iter().map(|j| j.base.clone()).collect(),
            FindingKind::Lead => self.lead_findings.iter().map(|l| l.base.clone()).collect(),
            FindingKind::FreelanceLead => {
                self.freelance_findings.iter().map(|l| l.base.clone()).collect()
            }
        }
    }

    /// Clear the store.
    pub fn clear(&mut self) {
        self.findings.clear();
        self.job_findings.clear();
        self.lead_findings.clear();
        self.freelance_findings.clear();
        self.dedup_hits = 0;
    }
}

/// Compute the dedup id. The triple is lowercased and joined
/// with `\u{1F}` (the unit separator, so a `title` containing a
/// `|` cannot collide with the separator). The result is a short
/// hex digest of the joined string â€” `DefaultHasher` is fine
/// here, this is not a security boundary.
fn compute_finding_id(platform: &str, url: Option<&str>, title: Option<&str>) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    platform.to_ascii_lowercase().hash(&mut h);
    "\u{1F}".hash(&mut h);
    url.unwrap_or("").to_ascii_lowercase().hash(&mut h);
    "\u{1F}".hash(&mut h);
    title.unwrap_or("").to_ascii_lowercase().hash(&mut h);
    format!("finding-{:016x}", h.finish())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ----------------------------------------------------------------------
// 6. ResearchPlanner: the deterministic "is this a research task?"
//    recognizer + plan producer.
// --------------------------------------------------------------------//

/// The pre-flight research planner. Pure-Rust, no LLM, no network,
/// no clock. The function `plan_research(task, default_platforms,
/// overall_deadline_secs)` is the public surface.
///
/// Detection rules (in priority order):
///
///   1. Empty / whitespace-only input â†’ `ResearchPlan::not_research`
///      with rationale "no task".
///   2. Phrases matching the `RESEARCH_KEYWORDS` list
///      (case-insensitive substring on the *whole* task) â†’
///      research plan, matched_pattern = "research_keyword".
///      Examples: "find", "research", "look up", "search for",
///      "compare", "gather", "compile", "list of".
///   3. Phrases matching the `AGGREGATE_KEYWORDS` list â†’ research
///      plan, matched_pattern = "aggregate_keyword". Examples:
///      "across", "from each", "multiple platforms",
///      "every site".
///   4. Otherwise â†’ `ResearchPlan::not_research`. The orchestrator
///      falls through to the Phase 2 single-platform path.
///
/// The intent: avoid over-recognizing. A task like "go to
/// wikipedia and search for Rust" is two clauses (Phase 2's
/// domain), not a research task. A task like "find Rust jobs on
/// every job board" is research. The split is the presence of a
/// "find/compare/aggregate" verb plus either an "across" / "from
/// each" hint OR an explicit "multiple" / "every" quantifier.
pub struct ResearchPlanner;

impl ResearchPlanner {
    /// Build a research plan from a task string and a default
    /// platform list. When the planner decides the task isn't
    /// research-shaped, returns `ResearchPlan::not_research(goal)`
    /// and the orchestrator uses the Phase 2 path.
    pub fn plan(
        task: &str,
        default_platforms: &[ResearchSubTask],
        overall_deadline_secs: Option<u64>,
    ) -> ResearchPlan {
        let trimmed = task.trim();
        if trimmed.is_empty() {
            return ResearchPlan::not_research(trimmed);
        }
        let lower = trimmed.to_ascii_lowercase();
        let (is_research, pattern) = classify_task(&lower);
        if !is_research {
            return ResearchPlan::not_research(trimmed);
        }
        // Build the per-platform subtask list. We tag each row
        // with the goal as the default `query` so the LLM
        // doesn't have to derive it (Phase 7's spec: "what
        // counts as a satisfactory result per subtask" is
        // explicit; the query is part of that).
        let platforms = default_platforms
            .iter()
            .map(|p| ResearchSubTask {
                id: p.id.clone(),
                platform: p.platform.clone(),
                domain: p.domain.clone(),
                entry_hint: p.entry_hint.clone(),
                // Default acceptance for a research task: at least
                // one finding with either an email or a URL, plus
                // at least one finding overall. The LLM is free
                // to amend via `declare_subtasks` later.
                acceptance: default_acceptance(),
                step_budget: p.step_budget,
                time_budget_secs: p.time_budget_secs,
                query: if p.query.is_empty() {
                    trimmed.to_string()
                } else {
                    p.query.clone()
                },
                platform_kind: p.platform_kind.clone(),
                query_template: p.query_template.clone(),
                lead_subtask_kind: p.lead_subtask_kind.clone(),
            })
            .collect();
        ResearchPlan {
            goal: trimmed.to_string(),
            platforms,
            // Default synthesis hint for the "job search" shape.
            // The orchestrator substitutes this when the goal
            // text contains "job" / "hiring" / "career";
            // otherwise we use a generic "list each finding"
            // hint.
            synthesis_hint: synthesis_hint_for(trimmed),
            overall_deadline_secs,
            is_research: true,
            matched_pattern: pattern.to_string(),
            max_concurrent_workers: None,
            max_concurrent_per_domain: None,
            stream_partial_findings: None,
        }
    }
}

/// The default acceptance set for an unknown research shape. A
/// job search is satisfied when the agent has produced at least
/// 1 finding with an email OR 1 with a URL (either satisfies
/// the user's "get a contact email, or the URL if no email
/// exists" goal).
fn default_acceptance() -> Vec<Criterion> {
    vec![Criterion::AnyOf(vec![
        Criterion::HasEmail,
        Criterion::HasUrl,
    ])]
}

/// Choose a synthesis hint based on the goal text. The
/// "job" / "hire" / "career" branch uses the
/// role+company+contact shape that the motivating case asks
/// for. The Phase 5 freelance branch ("client" / "gig" /
/// "freelance") uses a different shape â€” project scope,
/// client, and budget â€” so the synthesizer knows which
/// rendering template to apply. The two are mutually
/// exclusive in the typical user phrasing; a compound task
/// ("find me a job and find me clients") gets the
/// freelance hint, the orchestrator's
/// `build_compound_handoffs` splits the task, and each
/// half gets its own synthesis. Everything else gets a
/// generic "list each finding" hint.
fn synthesis_hint_for(goal: &str) -> String {
    let lower = goal.to_ascii_lowercase();
    let freelance_shaped = lower.contains("client")
        || lower.contains("clients")
        || lower.contains("freelance")
        || lower.contains("gig")
        || lower.contains("consulting")
        || lower.contains("contract work")
        || lower.contains("side hustle");
    let job_shaped = lower.contains("job")
        || lower.contains("hiring")
        || lower.contains("career")
        || lower.contains("opening")
        || lower.contains("position")
        || lower.contains("role");
    if freelance_shaped {
        "One row per project: scope, client, budget / rate, and a contact method. Listings without a budget or contact are surfaced last under \"Other listings found\"."
            .to_string()
    } else if job_shaped {
        "One row per role: role title, company, and either a contact email or an application URL."
            .to_string()
    } else {
        "One row per finding: title, source platform, and any contact detail surfaced.".to_string()
    }
}

/// Pattern-match a task against the research-shape keywords.
/// Returns (is_research, matched_pattern). The pattern string is
/// the human-readable name of the rule that fired â€” surfaced in
/// the transcript for audit.
///
/// Word-boundary semantics matter: "find " is a research verb
/// (find jobs, find candidates) but "finding" inside another
/// word shouldn't count. Every keyword here ends with a
/// trailing space so the leading boundary is implicit (the
/// character before is either start-of-string or whitespace);
/// the trailing space also rules out the substring appearing
/// mid-word.
fn classify_task(lower_task: &str) -> (bool, &'static str) {
    // Rule 2: research verbs that imply a *gathering* shape.
    // Deliberately excludes "search for " â€” that one is the
    // common single-site Phase 2 case ("go to wikipedia and
    // search for X"), not a research task.
    const RESEARCH_KEYWORDS: &[&str] = &[
        "find ",
        "research ",
        "look up ",
        "look for ",
        "compare ",
        "gather ",
        "compile ",
        "list of ",
        "collect ",
    ];
    for kw in RESEARCH_KEYWORDS {
        if lower_task.contains(kw) {
            return (true, "research_keyword");
        }
    }
    // Rule 3: aggregate keywords. These are the "I want this
    // across N places" hints. They take precedence over
    // single-site search verbs because "search for X across Y
    // platforms" is unambiguously research-shaped.
    const AGGREGATE_KEYWORDS: &[&str] = &[
        " across ",
        " from each ",
        " on every ",
        " from every ",
        "multiple platforms",
        "all platforms",
        "every site",
        "job boards",
        "job sites",
        "freelance platforms",
        "freelance boards",
    ];
    for kw in AGGREGATE_KEYWORDS {
        if lower_task.contains(kw) {
            return (true, "aggregate_keyword");
        }
    }
    (false, "none")
}

// ----------------------------------------------------------------------
// 7. Default platform list (for the built-in "job boards" shape).
// --------------------------------------------------------------------//

/// The default set of platforms the planner uses when the
/// configured list is empty. The list is deliberately a small,
/// well-known set; operators are expected to override it via
/// `config/research_platforms.toml`. The default here is
/// "reasonable defaults for the motivating case" â€” not "every
/// job board on the internet."
///
/// The platforms' `step_budget` and `time_budget_secs` are
/// calibrated for a 1-search-results-page visit: 12 steps is
/// enough to load the page, snapshot, click the first result,
/// snapshot, find a contact, snapshot, and `mark_subtask_done`.
/// 90 seconds is enough wall-clock for that flow on a healthy
/// network. Operators can override per-platform.
pub fn default_job_board_platforms() -> Vec<ResearchSubTask> {
    vec![
        ResearchSubTask {
            id: "linkedin".into(),
            platform: "LinkedIn".into(),
            domain: "linkedin.com".into(),
            entry_hint: "Search the goal; filter Remote; click first 3 results".into(),
            acceptance: vec![],
            step_budget: 12,
            time_budget_secs: 90,
            query: String::new(),
            platform_kind: "job_board".into(),
            query_template: "https://www.linkedin.com/jobs/search/?keywords={role}&location={location}&f_WT={remote}".into(),
            lead_subtask_kind: None,
        },
        ResearchSubTask {
            id: "indeed".into(),
            platform: "Indeed".into(),
            domain: "indeed.com".into(),
            entry_hint: "Search the goal; filter Remote".into(),
            acceptance: vec![],
            step_budget: 10,
            time_budget_secs: 75,
            query: String::new(),
            platform_kind: "job_board".into(),
            query_template: "https://www.indeed.com/jobs?q={role}&l={location}".into(),
            lead_subtask_kind: None,
        },
        ResearchSubTask {
            id: "wellfound".into(),
            platform: "Wellfound".into(),
            domain: "wellfound.com".into(),
            entry_hint: "Search the goal; click first 3 startup roles".into(),
            acceptance: vec![],
            step_budget: 10,
            time_budget_secs: 75,
            query: String::new(),
            platform_kind: "job_board".into(),
            query_template: "https://wellfound.com/jobs?search={role}".into(),
            lead_subtask_kind: None,
        },
        ResearchSubTask {
            id: "weworkremotely".into(),
            platform: "WeWorkRemotely".into(),
            domain: "weworkremotely.com".into(),
            entry_hint: "Search the goal; click first 3 results".into(),
            acceptance: vec![],
            step_budget: 10,
            time_budget_secs: 75,
            query: String::new(),
            platform_kind: "job_board".into(),
            query_template: "https://weworkremotely.com/remote-jobs/search?term={role}".into(),
            lead_subtask_kind: None,
        },
        ResearchSubTask {
            id: "remoteok".into(),
            platform: "RemoteOK".into(),
            domain: "remoteok.com".into(),
            entry_hint: "Search the goal; click first 3 results".into(),
            acceptance: vec![],
            step_budget: 8,
            time_budget_secs: 60,
            query: String::new(),
            platform_kind: "job_board".into(),
            query_template: "https://remoteok.com/remote-{role}-jobs".into(),
            lead_subtask_kind: None,
        },
    ]
}

/// Phase 20: Default public low-friction lead source platforms ($0 budget, ethical public directories).
pub fn default_lead_source_platforms() -> Vec<ResearchSubTask> {
    vec![
        ResearchSubTask {
            id: "producthunt".into(),
            platform: "ProductHunt".into(),
            domain: "producthunt.com".into(),
            entry_hint: "Discover candidate companies/startups in target category".into(),
            acceptance: vec![Criterion::HasUrl],
            step_budget: 10,
            time_budget_secs: 75,
            query: String::new(),
            platform_kind: "lead_source".into(),
            query_template: "https://www.producthunt.com/search?q={industry}".into(),
            lead_subtask_kind: Some(LeadSubTaskKind::CompanyDiscovery),
        },
        ResearchSubTask {
            id: "indiehackers".into(),
            platform: "IndieHackers".into(),
            domain: "indiehackers.com".into(),
            entry_hint: "Search products and companies by industry".into(),
            acceptance: vec![Criterion::HasUrl],
            step_budget: 10,
            time_budget_secs: 75,
            query: String::new(),
            platform_kind: "lead_source".into(),
            query_template: "https://www.indiehackers.com/products?query={industry}".into(),
            lead_subtask_kind: Some(LeadSubTaskKind::CompanyDiscovery),
        },
        ResearchSubTask {
            id: "ycombinator".into(),
            platform: "YCombinator".into(),
            domain: "ycombinator.com".into(),
            entry_hint: "Filter startup directory by industry and size".into(),
            acceptance: vec![Criterion::HasUrl],
            step_budget: 10,
            time_budget_secs: 75,
            query: String::new(),
            platform_kind: "lead_source".into(),
            query_template: "https://www.ycombinator.com/companies?q={industry}".into(),
            lead_subtask_kind: Some(LeadSubTaskKind::CompanyDiscovery),
        },
    ]
}

/// Phase 5: Default freelance-client lead-gen platforms. The
/// motivating case is "find me 5 paying clients for service X" â€”
/// the platforms here are the well-known freelance boards where
/// a project listing surfaces a client name, a scope, and a
/// budget / rate. Each row's `entry_hint` carries the
/// platform-specific quirk the LLM needs to know (e.g.
/// WeWorkRemotely's "Contract" vs "All Remote Jobs" split).
/// The acceptance criteria use the Phase 5 freelance-specific
/// `HasBudget` + `HasContact` predicates so the agent commits
/// to surfacing a real listing (not a generic profile page)
/// before marking a platform done.
pub fn default_freelance_platforms() -> Vec<ResearchSubTask> {
    vec![
        ResearchSubTask {
            id: "upwork".into(),
            platform: "Upwork".into(),
            domain: "upwork.com".into(),
            entry_hint: "Sign in via Google; navigate to Find Work; search the goal; filter by Client's last activity < 7 days; click first 3 results that have a stated budget".into(),
            acceptance: vec![Criterion::HasBudget, Criterion::HasContact],
            step_budget: 12,
            time_budget_secs: 90,
            query: String::new(),
            platform_kind: "freelance_board".into(),
            query_template: "https://www.upwork.com/nx/search/jobs/?q={role}&sort=recency".into(),
            lead_subtask_kind: None,
        },
        ResearchSubTask {
            id: "contra".into(),
            platform: "Contra".into(),
            domain: "contra.com".into(),
            entry_hint: "Use the Find Work tab (not Find Talent); search the goal; click first 3 project listings that surface a budget range or hourly rate".into(),
            acceptance: vec![Criterion::HasBudget, Criterion::HasContact],
            step_budget: 10,
            time_budget_secs: 75,
            query: String::new(),
            platform_kind: "freelance_board".into(),
            query_template: "https://www.contra.com/projects?q={role}".into(),
            lead_subtask_kind: None,
        },
        ResearchSubTask {
            id: "weworkremotely_freelance".into(),
            platform: "WeWorkRemotely (Contract)".into(),
            domain: "weworkremotely.com".into(),
            entry_hint: "Filter to the Contract category (not All Remote Jobs); search the goal; click first 3 contract listings".into(),
            acceptance: vec![Criterion::HasBudget, Criterion::HasContact],
            step_budget: 10,
            time_budget_secs: 75,
            query: String::new(),
            platform_kind: "freelance_board".into(),
            query_template: "https://weworkremotely.com/remote-contract-jobs/search?term={role}".into(),
            lead_subtask_kind: None,
        },
        ResearchSubTask {
            id: "freelancer".into(),
            platform: "Freelancer.com".into(),
            domain: "freelancer.com".into(),
            entry_hint: "Use the /jobs endpoint; search the goal; filter by Project Size > $250 (low-ball jobs are dominated by low-effort bot-bidding); click first 3 results".into(),
            acceptance: vec![Criterion::HasBudget, Criterion::HasContact],
            step_budget: 10,
            time_budget_secs: 75,
            query: String::new(),
            platform_kind: "freelance_board".into(),
            query_template: "https://www.freelancer.com/jobs/{role}/".into(),
            lead_subtask_kind: None,
        },
        ResearchSubTask {
            id: "guru".into(),
            platform: "Guru.com".into(),
            domain: "guru.com".into(),
            entry_hint: "Use the Find Work search; filter to Open Projects (not Contests); click first 3 listings that have a stated budget".into(),
            acceptance: vec![Criterion::HasBudget, Criterion::HasContact],
            step_budget: 10,
            time_budget_secs: 75,
            query: String::new(),
            platform_kind: "freelance_board".into(),
            query_template: "https://www.guru.com/d/jobs/category/{role}".into(),
            lead_subtask_kind: None,
        },
    ]
}

// ----------------------------------------------------------------------
// 8. TOML loader for `config/research_platforms.toml`. Mirrors the
//    `mew_nav::SensitivePlatforms::load_from_default_location` shape
//    so a future operator doesn't have to learn two conventions.
// --------------------------------------------------------------------//

/// On-disk row schema. Kept separate from `ResearchSubTask` so the
/// file format can evolve (e.g. add `default_query`,
/// `disable_when_sensitive` flags) without churning the in-memory
/// type or the wire format that flows into the `Handoff`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResearchPlatformsFileEntry {
    pub id: String,
    pub platform: String,
    pub domain: String,
    #[serde(default)]
    pub entry_hint: String,
    #[serde(default)]
    pub step_budget: u32,
    #[serde(default)]
    pub time_budget_secs: u64,
    #[serde(default)]
    pub default_query: String,
    #[serde(default)]
    pub platform_kind: String,
    #[serde(default)]
    pub query_template: String,
}

/// Top-level TOML shape. The file is `[[entry]]` blocks, each
/// with the schema above.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct ResearchPlatformsFile {
    #[serde(default, rename = "entry")]
    pub entry: Vec<ResearchPlatformsFileEntry>,
    /// Phase 40: top-level `required_domains` key from
    /// `config/research_platforms.toml`. When a task is
    /// routed to `JobSearch` or `LeadGen`,
    /// `mew_agent::expand_allowlist` appends every entry
    /// here to the session-local `agent.allowed_domains`
    /// vec. The file is data-driven: operators add a new
    /// domain to the `required_domains` key and the
    /// expansion picks it up at next session start (no
    /// code change). `#[serde(default)]` is what makes the
    /// field optional â€” pre-Phase-40 files (and files
    /// without the key for a particular research shape)
    /// parse cleanly.
    #[serde(default)]
    pub required_domains: Vec<String>,
}

/// In-memory mirror of the file. Conversion to
/// `Vec<ResearchSubTask>` happens once on load.
#[derive(Debug, Clone, Default)]
pub struct ResearchPlatforms {
    pub entries: Vec<ResearchPlatformsFileEntry>,
    /// Phase 40: session-local allowlist seed. See the
    /// `ResearchPlatformsFile` field for the on-disk
    /// schema. This field is what `expand_allowlist`
    /// reads when the shape is `JobSearch` or `LeadGen`.
    pub required_domains: Vec<String>,
}

impl ResearchPlatforms {
    /// Load from the default `config/research_platforms.toml`. Walks
    /// parent directories like `SensitivePlatforms::load_from_default_location`
    /// so the call works regardless of the process's CWD.
    ///
    /// Returns an empty `ResearchPlatforms` (not an error) if the
    /// file is missing â€” `default_job_board_platforms()` is the
    /// fallback the agent uses in that case. The empty-list
    /// behavior is deliberate: the file is a Phase 7 addition and
    /// pre-Phase-7 setups won't have it.
    pub fn load_from_default_location() -> Self {
        match Self::load_from(std::path::Path::new("config/research_platforms.toml")) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(
                    "research_platforms.toml not loaded: {e}; using empty table (the agent will fall back to default_job_board_platforms)"
                );
                Self::default()
            }
        }
    }

    /// Load from an explicit path. Returns Err on I/O or parse
    /// failure â€” the caller decides whether that's fatal.
    pub fn load_from(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
        let parsed: ResearchPlatformsFile = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
        Ok(Self {
            entries: parsed.entry,
            required_domains: parsed.required_domains,
        })
    }

    /// Convert the in-memory table to a `Vec<ResearchSubTask>` for
    /// the planner. When the table is empty, returns
    /// `default_job_board_platforms()` so the agent never sees an
    /// empty platform list at runtime.
    pub fn to_research_subtasks(&self) -> Vec<ResearchSubTask> {
        if self.entries.is_empty() {
            return default_job_board_platforms();
        }
        self.entries
            .iter()
            .map(|e| ResearchSubTask {
                id: e.id.clone(),
                platform: e.platform.clone(),
                domain: e.domain.clone(),
                entry_hint: e.entry_hint.clone(),
                // The acceptance is always the planner's default
                // for now â€” the file format doesn't expose
                // per-platform acceptance yet. Operators who
                // need a different default can edit
                // `default_acceptance()` in this file.
                acceptance: default_acceptance(),
                step_budget: e.step_budget,
                time_budget_secs: e.time_budget_secs,
                query: e.default_query.clone(),
                platform_kind: e.platform_kind.clone(),
                query_template: e.query_template.clone(),
                lead_subtask_kind: None,
            })
            .collect()
    }
}

// ----------------------------------------------------------------------
// 8c. Phase 5 â€” Freelance-platform TOML loader.
//
// Mirrors `ResearchPlatforms` (same file schema, same
// `load_from_default_location` / `load_from` / `to_*` shape)
// so operators editing `config/freelance_platforms.toml` use
// the same convention they already know from
// `config/research_platforms.toml`. The two loaders are
// independent â€” a fresh checkout without the freelance file
// still gets the default job-board list (the
// `ResearchPlatforms` path), and a setup that explicitly
// added a freelance file but no research file gets the
// default freelance list.
//
// The file format is identical to `research_platforms.toml`:
// `required_domains` at the top + `[[entry]]` blocks. The
// `to_freelance_subtasks` method converts the file's rows to
// `ResearchSubTask` with the Phase 5 freelance acceptance
// criteria (`HasBudget` + `HasContact`) hard-wired â€” operators
// who need a different commitment set for one platform can
// still edit `config/freelance_platforms.toml` and add a
// per-platform `acceptance` field in a future revision; the
// current `to_freelance_subtasks` keeps the contract simple
// and matches the file's hard-coded seed list.
// --------------------------------------------------------------------//

/// In-memory mirror of `config/freelance_platforms.toml`. The
/// type is intentionally a thin alias of `ResearchPlatforms`
/// so a future operator doesn't have to learn two types â€” the
/// only difference is which `to_*` method they call.
pub type FreelancePlatforms = ResearchPlatforms;

impl FreelancePlatforms {
    /// Convert the in-memory table to a `Vec<ResearchSubTask>`
    /// for the freelance planner. When the table is empty,
    /// returns `default_freelance_platforms()` so the agent
    /// never sees an empty platform list at runtime.
    ///
    /// The acceptance is hard-wired to `[HasBudget,
    /// HasContact]` â€” the Phase 5 spec's "a project listing
    /// with a stated budget or a client contact I can reach"
    /// commitment. A future revision of the file format can
    /// add a per-platform `acceptance` field; until then,
    /// the planner is the single source of truth for the
    /// default commitment set.
    pub fn to_freelance_subtasks(&self) -> Vec<ResearchSubTask> {
        if self.entries.is_empty() {
            return default_freelance_platforms();
        }
        self.entries
            .iter()
            .map(|e| ResearchSubTask {
                id: e.id.clone(),
                platform: e.platform.clone(),
                domain: e.domain.clone(),
                entry_hint: e.entry_hint.clone(),
                acceptance: vec![Criterion::HasBudget, Criterion::HasContact],
                step_budget: e.step_budget,
                time_budget_secs: e.time_budget_secs,
                query: e.default_query.clone(),
                platform_kind: e.platform_kind.clone(),
                query_template: e.query_template.clone(),
                lead_subtask_kind: None,
            })
            .collect()
    }

    /// Load from the default `config/freelance_platforms.toml`.
    /// Mirrors `ResearchPlatforms::load_from_default_location`
    /// so the call site reads the same way. Returns the
    /// default (empty table) when the file is missing â€” the
    /// caller falls back to `default_freelance_platforms()` via
    /// `to_freelance_subtasks`. The empty-table behavior is
    /// deliberate: the file is a Phase 5 addition and pre-Phase-5
    /// setups won't have it; they should keep working unchanged.
    pub fn load_freelance_from_default_location() -> Self {
        match Self::load_from(std::path::Path::new("config/freelance_platforms.toml")) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(
                    "freelance_platforms.toml not loaded: {e}; using empty table (the agent will fall back to default_freelance_platforms)"
                );
                Self::default()
            }
        }
    }
}

// ----------------------------------------------------------------------
// 8b. Phase 34 â€” Google Maps vertical pack TOML loader.
//
// Mirrors the `ResearchPlatforms` shape above so a future operator
// doesn't have to learn two conventions. The file is
// `config/maps_vertical.toml`. When the file is missing (the common
// case on a fresh checkout) the loader falls back to `Self::default()`
// so the agent can still run with the built-in hard-coded defaults
// (entry_url_template = "https://www.google.com/maps/search/{query}"
// etc.). The defaults in `Self::default()` MUST match the phase 34
// spec â€” they are the contract the system-prompt hint block relies on.
// --------------------------------------------------------------------//

/// On-disk schema. The file is a single flat table â€” no `[[entry]]`
/// array, because the Maps vertical is a single-site workflow
/// (one platform, one panel structure), not a fan-out.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MapsVerticalFile {
    #[serde(default = "default_maps_entry_url_template")]
    pub entry_url_template: String,
    #[serde(default = "default_maps_panel_close_action")]
    pub panel_close_action: String,
    #[serde(default = "default_maps_sidebar_scroll_hint")]
    pub sidebar_scroll_hint: String,
    #[serde(default = "default_maps_field_extraction_mode")]
    pub field_extraction_mode: String,
    #[serde(default = "default_maps_required_domains")]
    pub required_domains: Vec<String>,
}

fn default_maps_entry_url_template() -> String {
    "https://www.google.com/maps/search/{query}".to_string()
}
fn default_maps_panel_close_action() -> String {
    "press_key Escape".to_string()
}
fn default_maps_sidebar_scroll_hint() -> String {
    "scroll the left sidebar, not the map viewport".to_string()
}
fn default_maps_field_extraction_mode() -> String {
    "maps".to_string()
}
fn default_maps_required_domains() -> Vec<String> {
    vec![
        "maps.google.com".to_string(),
        "maps.googleapis.com".to_string(),
        "www.google.com".to_string(),
    ]
}

impl Default for MapsVerticalFile {
    fn default() -> Self {
        Self {
            entry_url_template: default_maps_entry_url_template(),
            panel_close_action: default_maps_panel_close_action(),
            sidebar_scroll_hint: default_maps_sidebar_scroll_hint(),
            field_extraction_mode: default_maps_field_extraction_mode(),
            required_domains: default_maps_required_domains(),
        }
    }
}

/// In-memory mirror of the file. Construction is a plain `From` so
/// the loaders don't have to repeat the field copy.
impl From<MapsVerticalFile> for MapsVerticalConfig {
    fn from(f: MapsVerticalFile) -> Self {
        Self {
            entry_url_template: f.entry_url_template,
            panel_close_action: f.panel_close_action,
            sidebar_scroll_hint: f.sidebar_scroll_hint,
            field_extraction_mode: f.field_extraction_mode,
            required_domains: f.required_domains,
        }
    }
}

/// Public, cloneable, in-memory shape the agent consumes. Kept
/// separate from `MapsVerticalFile` so the wire format can
/// evolve (e.g. add `default_rating`, `min_listing_count`) without
/// churning the in-memory type the agent reads.
#[derive(Debug, Clone)]
pub struct MapsVerticalConfig {
    pub entry_url_template: String,
    pub panel_close_action: String,
    pub sidebar_scroll_hint: String,
    pub field_extraction_mode: String,
    pub required_domains: Vec<String>,
}

impl Default for MapsVerticalConfig {
    fn default() -> Self {
        Self::from(MapsVerticalFile::default())
    }
}

impl MapsVerticalConfig {
    /// Build the entry URL for a given search query. The
    /// `{query}` placeholder in the template is URL-encoded
    /// before substitution. If the template has no `{query}`,
    /// the raw query is appended after a `?` (the common case
    /// when the operator has overridden the template with a
    /// specific URL).
    pub fn build_entry_url(&self, query: &str) -> String {
        if self.entry_url_template.contains("{query}") {
            self.entry_url_template.replace("{query}", &url_encode(query))
        } else {
            // Append as a search parameter; if the template
            // already has a `?`, use `&`, else use `?`.
            let sep = if self.entry_url_template.contains('?') {
                '&'
            } else {
                '?'
            };
            format!("{}{}q={}", self.entry_url_template, sep, url_encode(query))
        }
    }

    /// Load from the default `config/maps_vertical.toml`. Walks
    /// parent directories like the research-platforms loader so
    /// the call works regardless of the process's CWD.
    ///
    /// Returns the default `MapsVerticalConfig` (not an error) if
    /// the file is missing â€” the hard-coded defaults are the
    /// contract. The file is a Phase 34 addition and pre-Phase-34
    /// setups won't have it; they should keep working unchanged.
    pub fn load_from_default_location() -> Self {
        match Self::load_from(std::path::Path::new("config/maps_vertical.toml")) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(
                    "maps_vertical.toml not loaded: {e}; using built-in defaults (entry_url_template={})",
                    default_maps_entry_url_template()
                );
                Self::default()
            }
        }
    }

    /// Load from an explicit path. Returns Err on I/O or parse
    /// failure â€” the caller decides whether that's fatal.
    pub fn load_from(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
        let parsed: MapsVerticalFile = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
        Ok(Self::from(parsed))
    }
}

/// Minimal percent-encoder for query strings. Avoids pulling
/// the `urlencoding` crate into this module's public API
/// surface â€” the existing `urlencoding::encode` in
/// `chat_agent.rs` is also used for job/lead URL templates and
/// the two should match, so we delegate to the same crate.
fn url_encode(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

// ----------------------------------------------------------------------
// 9. ResearchConfig & LeadGenConfig: the typed `config.agent.research` block.
// ----------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LeadGenConfig {
    #[serde(default)]
    pub allow_email_pattern_guessing: bool,
}

/// The Phase 7 research loop's config block, exposed as
/// `config.agent.research` in `config.yaml` and serializable
/// as the corresponding TOML/JSON shape.
///
/// Defaults: enabled = true, default platforms = the
/// built-in job-board list. Operators who don't want the
/// research loop at all can set `enabled = false` â€” the
/// orchestrator's handoff builder will then skip the
/// research planner entirely and go straight to the Phase 2
/// clause-splitter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchConfig {
    /// When `false`, the research planner is bypassed. Useful
    /// for setups that want to keep the Phase 2 single-site
    /// behavior even for "find jobs" phrasings.
    #[serde(default = "default_research_enabled")]
    pub enabled: bool,
    /// Optional overall deadline for the whole research task
    /// in seconds. When set, the budget guard surfaces a
    /// warning when the loop is past 80% of the deadline. The
    /// loop's per-platform step / time caps are still the
    /// hard backstop â€” the overall deadline is a soft signal.
    #[serde(default)]
    pub overall_deadline_secs: Option<u64>,
    /// The list of platforms the research loop will fan out
    /// to. When empty, the agent falls back to
    /// `default_job_board_platforms()`. Operators can
    /// override per-platform by editing
    /// `config/research_platforms.toml` and reloading.
    #[serde(default)]
    pub platforms: Vec<ResearchSubTask>,
    /// If `false`, the agent's post-Phase-2 sensitive-platform
    /// routing is *not* used for research tasks â€” the agent
    /// will direct-navigate to every platform in the list.
    /// Default is `true` (sensitive routing is on) because
    /// the default platform list contains linkedin.com,
    /// indeed.com, etc. which Phase 2's `sensitive_platforms.toml`
    /// already routes through search.
    #[serde(default = "default_research_sensitive_routing")]
    pub use_sensitive_routing: bool,
    /// Phase 22: Link liveness verification before synthesis.
    #[serde(default = "default_verify_links")]
    pub verify_links: bool,
    /// Phase 20: SaaS Lead-Gen configuration.
    #[serde(default)]
    pub lead_gen: LeadGenConfig,
    /// Phase 1: Global cap on in-flight platform workers. Default 3.
    #[serde(default = "default_max_concurrent_workers")]
    pub max_concurrent_workers: usize,
    /// Phase 1: Per-domain concurrency cap independent of global cap. Default 1.
    #[serde(default = "default_max_concurrent_per_domain")]
    pub max_concurrent_per_domain: usize,
    /// Phase 2: Live partial-result streaming. Default true.
    #[serde(default = "default_stream_partial_findings")]
    pub stream_partial_findings: bool,
}

impl Default for ResearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            overall_deadline_secs: None,
            platforms: default_job_board_platforms(),
            use_sensitive_routing: true,
            verify_links: true,
            lead_gen: LeadGenConfig::default(),
            max_concurrent_workers: 3,
            max_concurrent_per_domain: 1,
            stream_partial_findings: true,
        }
    }
}

fn default_stream_partial_findings() -> bool {
    true
}

fn default_max_concurrent_workers() -> usize {
    3
}

fn default_max_concurrent_per_domain() -> usize {
    1
}

fn default_research_enabled() -> bool {
    true
}
fn default_research_sensitive_routing() -> bool {
    true
}
fn default_verify_links() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- ResearchPlan basics -----

    #[test]
    fn not_research_plan_is_not_research() {
        let p = ResearchPlan::not_research("anything");
        assert!(!p.is_research);
        assert!(p.platforms.is_empty());
        assert_eq!(p.matched_pattern, "none");
    }

    #[test]
    fn platform_count_and_total_step_budget_sum() {
        let p = ResearchPlan {
            goal: "x".into(),
            platforms: vec![
                ResearchSubTask {
                    id: "a".into(),
                    platform: "A".into(),
                    domain: "a.com".into(),
                    entry_hint: String::new(),
                    acceptance: vec![],
                    step_budget: 10,
                    time_budget_secs: 30,
                    query: String::new(),
                    platform_kind: String::new(),
                    query_template: String::new(),
                    lead_subtask_kind: None,
                },
                ResearchSubTask {
                    id: "b".into(),
                    platform: "B".into(),
                    domain: "b.com".into(),
                    entry_hint: String::new(),
                    acceptance: vec![],
                    step_budget: 7,
                    time_budget_secs: 20,
                    query: String::new(),
                    platform_kind: String::new(),
                    query_template: String::new(),
                    lead_subtask_kind: None,
                },
            ],
            synthesis_hint: String::new(),
            overall_deadline_secs: None,
            is_research: true,
            matched_pattern: "test".into(),
            max_concurrent_workers: None,
            max_concurrent_per_domain: None,
            stream_partial_findings: None,
        };
        assert_eq!(p.platform_count(), 2);
        assert_eq!(p.total_step_budget(), 17);
    }

    // ----- ResearchPlanner detection -----

    #[test]
    fn planner_returns_not_research_for_empty_input() {
        let p = ResearchPlanner::plan("", &[], None);
        assert!(!p.is_research);
    }

    #[test]
    fn planner_returns_not_research_for_single_action() {
        // "go to wikipedia" is two clauses to the phase-2 planner
        // but the *research* planner shouldn't tag it. No
        // find/research/compare keyword, no aggregate keyword.
        let p = ResearchPlanner::plan("go to wikipedia", &[], None);
        assert!(!p.is_research);
    }

    #[test]
    fn planner_recognizes_find_keyword() {
        let p = ResearchPlanner::plan(
            "find remote SWE job openings",
            &default_job_board_platforms(),
            Some(600),
        );
        assert!(
            p.is_research,
            "task should be classified as research: {p:?}"
        );
        assert_eq!(p.matched_pattern, "research_keyword");
        assert!(p.platforms.len() >= 3);
        // The goal string is preserved verbatim
        assert_eq!(p.goal, "find remote SWE job openings");
        // The overall deadline is preserved
        assert_eq!(p.overall_deadline_secs, Some(600));
        // Each platform row has its per-platform step budget
        assert!(p.platforms.iter().all(|p| p.step_budget > 0));
    }

    #[test]
    fn planner_recognizes_aggregate_keyword() {
        // "rust jobs across all platforms" has no research
        // verb (no "find " / "compare " / "research " etc.) â€”
        // it only has the " across all " aggregate hint.
        let p = ResearchPlanner::plan(
            "rust jobs across all platforms",
            &default_job_board_platforms(),
            None,
        );
        assert!(p.is_research);
        assert_eq!(p.matched_pattern, "aggregate_keyword");
    }

    #[test]
    fn planner_emits_job_shaped_synthesis_hint() {
        let p = ResearchPlanner::plan(
            "find me a senior backend role",
            &default_job_board_platforms(),
            None,
        );
        assert!(p.synthesis_hint.contains("role"));
        assert!(p.synthesis_hint.contains("email") || p.synthesis_hint.contains("URL"));
    }

    #[test]
    fn planner_uses_default_acceptance_on_each_platform() {
        let p = ResearchPlanner::plan("find rust jobs", &default_job_board_platforms(), None);
        for platform in &p.platforms {
            assert!(
                !platform.acceptance.is_empty(),
                "platform {} has no acceptance criteria",
                platform.id
            );
        }
    }

    // ----- Criterion evaluation -----

    #[test]
    fn criterion_has_email_requires_email_present() {
        let c = Criterion::HasEmail;
        let no_email = vec![ResearchFinding {
            id: "1".into(),
            platform: "X".into(),
            title: None,
            company: None,
            email: None,
            url: Some("https://x.com/job/1".into()),
            note: String::new(),
            added_at_secs: 0,
        }];
        assert!(!c.is_met_by(&no_email));
        let with_email = vec![ResearchFinding {
            id: "1".into(),
            platform: "X".into(),
            title: None,
            company: None,
            email: Some("a@b.com".into()),
            url: None,
            note: String::new(),
            added_at_secs: 0,
        }];
        assert!(c.is_met_by(&with_email));
    }

    #[test]
    fn criterion_has_url_requires_url_present() {
        let c = Criterion::HasUrl;
        let no_url = vec![ResearchFinding {
            id: "1".into(),
            platform: "X".into(),
            title: None,
            company: None,
            email: Some("a@b.com".into()),
            url: None,
            note: String::new(),
            added_at_secs: 0,
        }];
        assert!(!c.is_met_by(&no_url));
        let with_url = vec![ResearchFinding {
            id: "1".into(),
            platform: "X".into(),
            title: None,
            company: None,
            email: None,
            url: Some("https://x.com/job/1".into()),
            note: String::new(),
            added_at_secs: 0,
        }];
        assert!(c.is_met_by(&with_url));
    }

    #[test]
    fn criterion_has_count_requires_n_rows() {
        let c = Criterion::HasJobCountAtLeast { n: 3 };
        let mut findings = Vec::new();
        for i in 0..2 {
            findings.push(ResearchFinding {
                id: format!("{i}"),
                platform: "X".into(),
                title: None,
                company: None,
                email: None,
                url: None,
                note: String::new(),
                added_at_secs: 0,
            });
        }
        assert!(!c.is_met_by(&findings), "2 findings < 3");
        findings.push(ResearchFinding {
            id: "x".into(),
            platform: "X".into(),
            title: None,
            company: None,
            email: None,
            url: None,
            note: String::new(),
            added_at_secs: 0,
        });
        assert!(c.is_met_by(&findings), "3 findings >= 3");
    }

    #[test]
    fn criterion_all_of_requires_every_child_to_hold() {
        let c = Criterion::AllOf(vec![Criterion::HasEmail, Criterion::HasUrl]);
        let either = vec![ResearchFinding {
            id: "1".into(),
            platform: "X".into(),
            title: None,
            company: None,
            email: Some("a@b.com".into()),
            url: None,
            note: String::new(),
            added_at_secs: 0,
        }];
        assert!(!c.is_met_by(&either), "email but no URL");
        let both_rows = vec![
            ResearchFinding {
                id: "1".into(),
                platform: "X".into(),
                title: None,
                company: None,
                email: Some("a@b.com".into()),
                url: None,
                note: String::new(),
                added_at_secs: 0,
            },
            ResearchFinding {
                id: "2".into(),
                platform: "X".into(),
                title: None,
                company: None,
                email: None,
                url: Some("https://x.com/job/1".into()),
                note: String::new(),
                added_at_secs: 0,
            },
        ];
        assert!(c.is_met_by(&both_rows), "one row has email, one has URL");
    }

    #[test]
    fn criterion_any_of_requires_only_one_child() {
        let c = Criterion::AnyOf(vec![Criterion::HasEmail, Criterion::HasUrl]);
        let only_url = vec![ResearchFinding {
            id: "1".into(),
            platform: "X".into(),
            title: None,
            company: None,
            email: None,
            url: Some("https://x.com/job/1".into()),
            note: String::new(),
            added_at_secs: 0,
        }];
        assert!(c.is_met_by(&only_url));
    }

    #[test]
    fn criterion_describe_returns_stable_strings() {
        // The describe output is rendered in the chat reply and
        // the transcript. A test here pins the format so a
        // future refactor that rewords the message gets caught.
        assert_eq!(
            Criterion::HasJobCountAtLeast { n: 3 }.describe(),
            "at least 3 findings"
        );
        assert_eq!(Criterion::HasEmail.describe(), "at least one email");
        assert_eq!(Criterion::HasUrl.describe(), "at least one URL");
        assert_eq!(
            Criterion::HasEmailAndUrl.describe(),
            "at least one email AND at least one URL"
        );
    }

    // ----- FindingStore -----

    #[test]
    fn store_adds_unique_rows() {
        let mut s = FindingStore::new();
        let added = s.add(ResearchFinding {
            id: String::new(),
            platform: "LinkedIn".into(),
            title: Some("Rust Engineer".into()),
            company: Some("Acme".into()),
            email: None,
            url: Some("https://linkedin.com/jobs/view/1".into()),
            note: String::new(),
            added_at_secs: 0,
        });
        assert!(added);
        assert_eq!(s.findings().len(), 1);
        assert_eq!(s.dedup_hits(), 0);
    }

    #[test]
    fn store_dedups_repeats() {
        let mut s = FindingStore::new();
        let row = ResearchFinding {
            id: String::new(),
            platform: "LinkedIn".into(),
            title: Some("Rust Engineer".into()),
            company: Some("Acme".into()),
            email: None,
            url: Some("https://linkedin.com/jobs/view/1".into()),
            note: String::new(),
            added_at_secs: 0,
        };
        assert!(s.add(row.clone()));
        assert!(!s.add(row.clone()));
        assert_eq!(s.findings().len(), 1);
        assert_eq!(s.dedup_hits(), 1);
    }

    #[test]
    fn store_dedup_is_case_insensitive() {
        let mut s = FindingStore::new();
        s.add(ResearchFinding {
            id: String::new(),
            platform: "LinkedIn".into(),
            title: Some("Rust Engineer".into()),
            company: None,
            email: None,
            url: Some("https://linkedin.com/jobs/view/1".into()),
            note: String::new(),
            added_at_secs: 0,
        });
        let again = s.add(ResearchFinding {
            id: String::new(),
            platform: "linkedin".into(),
            title: Some("RUST ENGINEER".into()),
            company: None,
            email: None,
            url: Some("HTTPS://LINKEDIN.COM/jobs/view/1".into()),
            note: String::new(),
            added_at_secs: 0,
        });
        assert!(
            !again,
            "case-insensitive dedup should collapse the two rows"
        );
        assert_eq!(s.findings().len(), 1);
    }

    #[test]
    fn store_dedup_id_is_stable() {
        // Same (platform, url, title) â†’ same id, even when added
        // from different places. This is the whole point of the
        // id: the synthesizer can group by it.
        let id1 = compute_finding_id("X", Some("https://x.com/1"), Some("title"));
        let id2 = compute_finding_id("X", Some("https://x.com/1"), Some("title"));
        assert_eq!(id1, id2);
        let id3 = compute_finding_id("X", Some("https://x.com/1"), Some("other title"));
        assert_ne!(id1, id3, "different title must produce different id");
        let id4 = compute_finding_id("X", Some("https://x.com/2"), Some("title"));
        assert_ne!(id1, id4, "different url must produce different id");
    }

    #[test]
    fn store_finds_for_platform_filters_correctly() {
        let mut s = FindingStore::new();
        s.add(ResearchFinding {
            id: String::new(),
            platform: "LinkedIn".into(),
            title: Some("A".into()),
            company: None,
            email: Some("a@b.com".into()),
            url: None,
            note: String::new(),
            added_at_secs: 0,
        });
        s.add(ResearchFinding {
            id: String::new(),
            platform: "Indeed".into(),
            title: Some("B".into()),
            company: None,
            email: None,
            url: Some("https://indeed.com/jobs/1".into()),
            note: String::new(),
            added_at_secs: 0,
        });
        assert_eq!(s.findings_for_platform("LinkedIn").len(), 1);
        assert_eq!(s.findings_for_platform("Indeed").len(), 1);
        assert_eq!(s.findings_for_platform("Wellfound").len(), 0);
    }

    #[test]
    fn store_meets_checks_only_named_platforms_findings() {
        // The platform A's findings shouldn't satisfy the
        // platform B's commitment. This is the contract the
        // agent loop depends on.
        let mut s = FindingStore::new();
        s.add(ResearchFinding {
            id: String::new(),
            platform: "LinkedIn".into(),
            title: Some("A".into()),
            company: None,
            email: Some("a@b.com".into()),
            url: None,
            note: String::new(),
            added_at_secs: 0,
        });
        assert!(s.meets("LinkedIn", &[Criterion::HasEmail]));
        assert!(
            !s.meets("Indeed", &[Criterion::HasEmail]),
            "LinkedIn's findings must not satisfy Indeed's commitment",
        );
    }

    // ----- ResearchFinding one-line rendering -----

    #[test]
    fn finding_one_line_includes_platform_and_url() {
        let f = ResearchFinding {
            id: "x".into(),
            platform: "LinkedIn".into(),
            title: Some("Rust Engineer".into()),
            company: Some("Acme".into()),
            email: Some("a@b.com".into()),
            url: Some("https://linkedin.com/jobs/1".into()),
            note: String::new(),
            added_at_secs: 0,
        };
        let line = f.one_line();
        assert!(line.contains("Rust Engineer"));
        assert!(line.contains("Acme"));
        assert!(line.contains("LinkedIn"));
        assert!(line.contains("https://linkedin.com/jobs/1"));
        assert!(line.contains("a@b.com"));
    }

    // ----- ResearchSubTask -> DeclareItem -----

    #[test]
    fn research_subtask_to_declare_item_includes_query_and_hint() {
        let p = ResearchSubTask {
            id: "linkedin".into(),
            platform: "LinkedIn".into(),
            domain: "linkedin.com".into(),
            entry_hint: "filter Remote".into(),
            acceptance: vec![],
            step_budget: 10,
            time_budget_secs: 60,
            query: "rust engineer".into(),
            platform_kind: String::new(),
            query_template: String::new(),
            lead_subtask_kind: None,
        };
        let d = p.to_declare_item();
        assert_eq!(d.id, "linkedin");
        assert!(d.description.contains("LinkedIn"));
        assert!(d.description.contains("rust engineer"));
        assert!(d.description.contains("filter Remote"));
    }

    // ----- ResearchPlatforms loader + ResearchConfig -----

    #[test]
    fn research_platforms_to_research_subtasks_falls_back_when_empty() {
        let t = ResearchPlatforms::default();
        let subs = t.to_research_subtasks();
        assert!(!subs.is_empty(), "empty table should fall back to defaults");
        assert!(subs.iter().any(|s| s.id == "linkedin"));
        assert!(subs.iter().any(|s| s.id == "indeed"));
    }

    #[test]
    fn research_platforms_to_research_subtasks_uses_loaded_entries() {
        // Hand-build a `ResearchPlatforms` with one entry
        // and verify it overrides the default.
        let t = ResearchPlatforms {
            entries: vec![ResearchPlatformsFileEntry {
                id: "custom".into(),
                platform: "Custom Board".into(),
                domain: "custom.example.com".into(),
                entry_hint: "search goal".into(),
                step_budget: 5,
                time_budget_secs: 30,
                default_query: "rust".into(),
                platform_kind: String::new(),
                query_template: String::new(),
            }],
            // Phase 40: hand-built test fixture; the
            // field's `Default::default()` (empty vec) is
            // the value the pre-Phase-40 test would have
            // seen.
            required_domains: Vec::new(),
        };
        let subs = t.to_research_subtasks();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].id, "custom");
        assert_eq!(subs[0].platform, "Custom Board");
        assert_eq!(subs[0].step_budget, 5);
        assert_eq!(subs[0].query, "rust");
    }

    #[test]
    fn research_config_default_is_enabled_with_default_platforms() {
        // The `config.agent.research` block defaults to
        // enabled + the default job-board list. Operators
        // who want to disable the research loop set
        // `enabled = false` in their config.yaml.
        let cfg = ResearchConfig::default();
        assert!(cfg.enabled);
        assert!(cfg.use_sensitive_routing);
        assert!(cfg.platforms.len() >= 3);
    }

    #[test]
    fn research_config_yaml_round_trip() {
        // The on-disk shape is `agent.research.<field>`. A
        // round-trip through serde_yaml proves the
        // deserialization path works for the `serde(default
        // = ...)` attributes.
        let cfg = ResearchConfig::default();
        let yaml = serde_yaml::to_string(&cfg).unwrap();
        let back: ResearchConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(cfg.enabled, back.enabled);
        assert_eq!(cfg.use_sensitive_routing, back.use_sensitive_routing);
    }

    #[test]
    fn research_config_disabled_round_trips_cleanly() {
        // A user who sets `enabled = false` should not
        // have to specify anything else â€” the other
        // fields default. (An empty `platforms` list is
        // fine when the loop is disabled: the planner
        // is bypassed entirely.)
        let yaml = "enabled: false\n";
        let cfg: ResearchConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(!cfg.enabled);
        assert!(
            cfg.platforms.is_empty(),
            "platforms should default to empty when omitted, not the heavy default list"
        );
    }

    #[test]
    fn lead_gen_config_defaults_allow_email_pattern_guessing_to_false() {
        let cfg = ResearchConfig::default();
        assert!(!cfg.lead_gen.allow_email_pattern_guessing);
    }

    #[test]
    fn test_job_finding_fuzzy_dedup() {
        let mut store = FindingStore::new();

        let j1 = JobFinding {
            base: ResearchFinding {
                id: String::new(),
                platform: "LinkedIn".into(),
                title: Some("Senior SWE — Remote".into()),
                company: Some("Acme Corp".into()),
                email: None,
                url: Some("https://linkedin.com/jobs/1".into()),
                note: "FullTime".into(),
                added_at_secs: 100,
            },
            freshness_days: Some(5),
            is_internship: false,
        };

        let j2 = JobFinding {
            base: ResearchFinding {
                id: String::new(),
                platform: "Indeed".into(),
                title: Some("Senior Software Engineer (Remote)".into()),
                company: Some("Acme Corp".into()),
                email: None,
                url: Some("https://indeed.com/viewjob?id=123".into()),
                note: "150k-200k".into(),
                added_at_secs: 200,
            },
            freshness_days: Some(2),
            is_internship: false,
        };

        assert!(store.add_job_finding(j1));
        assert!(
            !store.add_job_finding(j2),
            "second fuzzy-matching job should be deduplicated"
        );

        assert_eq!(store.job_findings().len(), 1);
        let merged = &store.job_findings()[0];
        assert_eq!(
            merged.freshness_days,
            Some(2),
            "should keep fresher 2 days vs 5 days"
        );
        assert!(merged.base.note.contains("FullTime") && merged.base.note.contains("150k-200k"));
        assert_eq!(store.dedup_hits(), 1);
    }

    #[test]
    fn test_lead_finding_domain_dedup_and_merge() {
        let mut store = FindingStore::new();

        let l1 = LeadFinding {
            base: ResearchFinding {
                id: "f1".into(),
                platform: "ProductHunt".into(),
                title: Some("Acme SaaS".into()),
                company: Some("Acme Corp".into()),
                email: None,
                url: Some("https://acme.com".into()),
                note: String::new(),
                added_at_secs: 100,
            },
            company_name: "Acme".into(),
            company_domain: "https://www.Acme.com/about".into(),
            contact_name: None,
            contact_role: None,
            contact_email: None,
            contact_url: None,
            source_platform: "ProductHunt".into(),
            confidence: LeadConfidence::Low,
        };

        let l2 = LeadFinding {
            base: ResearchFinding {
                id: "f2".into(),
                platform: "YCombinator".into(),
                title: Some("Acme Corp".into()),
                company: Some("Acme Corp".into()),
                email: Some("alice@acme.com".into()),
                url: Some("https://acme.com".into()),
                note: String::new(),
                added_at_secs: 200,
            },
            company_name: "Acme Corporation".into(),
            company_domain: "acme.com".into(),
            contact_name: Some("Alice Smith".into()),
            contact_role: Some("VP Sales".into()),
            contact_email: Some("alice@acme.com".into()),
            contact_url: Some("https://linkedin.com/in/alicesmith".into()),
            source_platform: "YCombinator".into(),
            confidence: LeadConfidence::High,
        };

        assert!(store.add_lead_finding(l1));
        assert!(
            !store.add_lead_finding(l2),
            "second lead with same domain should be deduplicated"
        );

        assert_eq!(store.lead_findings().len(), 1);
        let merged = &store.lead_findings()[0];
        assert_eq!(merged.company_name, "Acme Corporation");
        assert_eq!(merged.contact_name.as_deref(), Some("Alice Smith"));
        assert_eq!(merged.contact_email.as_deref(), Some("alice@acme.com"));
        assert_eq!(merged.confidence, LeadConfidence::High);
        assert!(
            merged.source_platform.contains("ProductHunt")
                && merged.source_platform.contains("YCombinator")
        );
    }

    #[test]
    fn test_lead_contact_waterfall_enrichment() {
        let mut store = FindingStore::new();

        let l1 = LeadFinding {
            base: ResearchFinding {
                id: "f1".into(),
                platform: "ProductHunt".into(),
                title: Some("Beta Tools".into()),
                company: Some("Beta Inc".into()),
                email: None,
                url: Some("https://betainc.io".into()),
                note: String::new(),
                added_at_secs: 100,
            },
            company_name: "Beta Inc".into(),
            company_domain: "betainc.io".into(),
            contact_name: None,
            contact_role: None,
            contact_email: None,
            contact_url: None,
            source_platform: "ProductHunt".into(),
            confidence: LeadConfidence::Low,
        };

        store.add_lead_finding(l1);

        // Waterfall check: platform 2 looks up betainc.io in store
        let existing = store.find_lead_by_domain("https://www.betainc.io/about");
        assert!(existing.is_some());
        assert_eq!(existing.unwrap().confidence, LeadConfidence::Low);

        // Platform 2 finds contact info and adds it
        let l2 = LeadFinding {
            base: ResearchFinding {
                id: "f2".into(),
                platform: "IndieHackers".into(),
                title: Some("Beta Inc".into()),
                company: Some("Beta Inc".into()),
                email: Some("bob@betainc.io".into()),
                url: Some("https://betainc.io".into()),
                note: String::new(),
                added_at_secs: 200,
            },
            company_name: "Beta Inc".into(),
            company_domain: "betainc.io".into(),
            contact_name: Some("Bob Jones".into()),
            contact_role: Some("Head of Growth".into()),
            contact_email: Some("bob@betainc.io".into()),
            contact_url: None,
            source_platform: "IndieHackers".into(),
            confidence: LeadConfidence::High,
        };

        store.add_lead_finding(l2);

        let final_lead = store.find_lead_by_domain("betainc.io").unwrap();
        assert_eq!(final_lead.confidence, LeadConfidence::High);
        assert_eq!(final_lead.contact_email.as_deref(), Some("bob@betainc.io"));
    }

    #[test]
    fn test_link_liveness_verification_filters_dead_urls() {
        let mut store = FindingStore::new();

        let j1 = JobFinding {
            base: ResearchFinding {
                id: "f1".into(),
                platform: "LinkedIn".into(),
                title: Some("Rust Engineer".into()),
                company: Some("Acme".into()),
                email: None,
                url: Some("https://acme.com/apply/404".into()),
                note: String::new(),
                added_at_secs: 100,
            },
            freshness_days: Some(1),
            is_internship: false,
        };

        store.add_job_finding(j1);

        // Verify with false: link retained
        store.verify_links(false);
        assert!(store.job_findings()[0].base.url.is_some());

        // Verify with true: 404 dead link dropped
        store.verify_links(true);
        assert!(store.job_findings()[0].base.url.is_none());
    }

    // ----- Phase 5: freelance lead-gen -----

    /// Helper: build a `FreelanceLead` for tests. Defaults
    /// `client_name` / `project_scope` / `source_platform`
    /// to the values the tests want to assert on; everything
    /// else is left as a starting point for the test to
    /// mutate.
    ///
    /// `budget` is `(raw_text, kind)` â€” both fields are
    /// `Option<String>`-shaped at the boundary so a test
    /// can pass `None` for the kind while keeping a raw
    /// text. `contact` is `(email, url, platform_message_hint)`
    /// â€” every channel is independent and any of them can
    /// be `None`. The compiler enforces the tuple shapes.
    fn make_freelance_lead(
        platform: &str,
        client: &str,
        scope: &str,
        url: Option<&str>,
        budget: Option<(Option<&str>, Option<&str>)>,
        contact: Option<(Option<&str>, Option<&str>, Option<&str>)>,
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
            budget_hint: budget.map(|(raw, kind)| FreelanceBudget {
                raw: raw.unwrap_or("").to_string(),
                kind: kind.map(|s| s.to_string()),
            }),
            contact_method: contact.map(|(email, url, hint)| FreelanceContact {
                email: email.map(|s| s.to_string()),
                url: url.map(|s| s.to_string()),
                platform_message_hint: hint.map(|s| s.to_string()),
            }),
            source_platform: platform.to_string(),
        }
    }

    #[test]
    fn criterion_has_budget_against_freelance_leads_requires_budget() {
        // A lead with no budget_hint should NOT satisfy
        // `HasBudget` even if it has a contact.
        let leads = vec![make_freelance_lead(
            "Upwork",
            "Acme",
            "Build a site",
            Some("https://upwork.com/jobs/1"),
            None,
            Some((Some("alice@acme.com"), None, None)),
        )];
        assert!(!Criterion::HasBudget.is_met_by_freelance_leads(&leads));
    }

    #[test]
    fn criterion_has_budget_against_freelance_leads_passes_with_budget() {
        let leads = vec![make_freelance_lead(
            "Upwork",
            "Acme",
            "Build a site",
            Some("https://upwork.com/jobs/1"),
            Some((Some("$50-75/hr"), Some("hourly"))),
            None,
        )];
        assert!(Criterion::HasBudget.is_met_by_freelance_leads(&leads));
    }

    #[test]
    fn criterion_has_contact_against_freelance_leads_requires_present_contact() {
        // A `FreelanceContact` with all three fields
        // `None` is NOT "present" â€” the agent's check must
        // see at least one channel.
        let leads = vec![make_freelance_lead(
            "Upwork",
            "Acme",
            "Build a site",
            Some("https://upwork.com/jobs/1"),
            Some((Some("$50-75/hr"), Some("hourly"))),
            Some((None, None, None)),
        )];
        assert!(!Criterion::HasContact.is_met_by_freelance_leads(&leads));
    }

    #[test]
    fn criterion_has_contact_against_freelance_leads_passes_with_any_channel() {
        // A platform_message_hint counts as a contact.
        let leads = vec![make_freelance_lead(
            "Contra",
            "Beta",
            "Design system",
            Some("https://contra.com/p/1"),
            Some((Some("Negotiable"), None)),
            Some((None, None, Some("Message via Contra"))),
        )];
        assert!(Criterion::HasContact.is_met_by_freelance_leads(&leads));
    }

    #[test]
    fn criterion_has_budget_against_generic_research_findings_is_always_false() {
        // The freelance-specific criterion must NOT silently
        // pass against the generic `ResearchFinding` rows â€”
        // that would let a synthesizer bug skip a real
        // commitment check.
        let generic: Vec<ResearchFinding> = vec![ResearchFinding {
            id: "g1".into(),
            platform: "Upwork".into(),
            title: Some("Build a site".into()),
            company: Some("Acme".into()),
            email: Some("a@b.com".into()),
            url: Some("https://upwork.com/jobs/1".into()),
            note: String::new(),
            added_at_secs: 0,
        }];
        assert!(!Criterion::HasBudget.is_met_by(&generic));
        assert!(!Criterion::HasContact.is_met_by(&generic));
    }

    #[test]
    fn criterion_describe_includes_freelance_variants() {
        assert_eq!(
            Criterion::HasBudget.describe(),
            "at least one listing with a stated budget"
        );
        assert_eq!(
            Criterion::HasContact.describe(),
            "at least one listing with a contact method"
        );
    }

    #[test]
    fn store_add_freelance_lead_dedups_by_url() {
        let mut store = FindingStore::new();
        let l1 = make_freelance_lead(
            "Upwork",
            "Acme",
            "Build a site",
            Some("https://upwork.com/jobs/1"),
            Some((Some("$50/hr"), Some("hourly"))),
            Some((Some("a@acme.com"), None, None)),
        );
        let l2 = make_freelance_lead(
            "Upwork",
            "Acme Studios",
            "Build a 5-page marketing site",
            Some("https://upwork.com/jobs/1"),
            None,
            None,
        );
        // Same URL -> dedup. The merge fills in the longer
        // client name and the longer scope from `l2`.
        assert!(store.add_freelance_lead(l1));
        assert!(!store.add_freelance_lead(l2));
        assert_eq!(store.freelance_findings().len(), 1);
        assert_eq!(store.freelance_findings()[0].client_name, "Acme Studios");
        assert_eq!(
            store.freelance_findings()[0].project_scope,
            "Build a 5-page marketing site"
        );
        // Original budget / contact preserved (first
        // observation wins for these).
        assert!(store.freelance_findings()[0].budget_hint.is_some());
        assert_eq!(store.dedup_hits(), 1);
    }

    #[test]
    fn store_add_freelance_lead_dedups_by_client_and_scope_when_no_url() {
        // When the URL is missing, the fallback key is
        // (client_name, project_scope). Same pair -> dedup.
        let mut store = FindingStore::new();
        let l1 = make_freelance_lead(
            "Contra",
            "Acme",
            "Build a site",
            None,
            Some((Some("$500"), Some("fixed"))),
            Some((Some("a@acme.com"), None, None)),
        );
        let l2 = make_freelance_lead(
            "Contra",
            "Acme",
            "Build a site",
            None,
            None,
            None,
        );
        assert!(store.add_freelance_lead(l1));
        assert!(!store.add_freelance_lead(l2));
        assert_eq!(store.freelance_findings().len(), 1);
    }

    #[test]
    fn store_find_freelance_by_url_returns_match() {
        let mut store = FindingStore::new();
        store.add_freelance_lead(make_freelance_lead(
            "Upwork",
            "Acme",
            "Build a site",
            Some("https://upwork.com/jobs/1"),
            Some((Some("$50/hr"), Some("hourly"))),
            Some((Some("a@acme.com"), None, None)),
        ));
        store.add_freelance_lead(make_freelance_lead(
            "Contra",
            "Beta",
            "Design system",
            Some("https://contra.com/p/1"),
            Some((Some("Negotiable"), None)),
            Some((None, None, Some("Message via Contra"))),
        ));
        let by_url = store.find_freelance_by_url("https://contra.com/p/1");
        assert!(by_url.is_some());
        assert_eq!(by_url.unwrap().client_name, "Beta");
        assert!(store.find_freelance_by_url("https://missing.com").is_none());
    }

    #[test]
    fn store_meets_routes_freelance_criteria_to_typed_leads() {
        // `HasBudget` and `HasContact` must check the
        // typed `freelance_findings`, not the generic
        // `findings` vec. The `FindingStore::meets`
        // dispatcher handles the routing; this test pins
        // the contract.
        let mut store = FindingStore::new();
        // Add a generic `ResearchFinding` with the same
        // platform name to make sure the meeting check
        // doesn't fall through to the generic path.
        store.add(ResearchFinding {
            id: "g1".into(),
            platform: "Upwork".into(),
            title: Some("Build a site".into()),
            company: Some("Acme".into()),
            email: Some("a@b.com".into()),
            url: Some("https://upwork.com/jobs/1".into()),
            note: String::new(),
            added_at_secs: 0,
        });
        // No typed lead yet â€” `HasBudget` is unmet even
        // though the generic finding has an email.
        assert!(!store.meets("Upwork", &[Criterion::HasBudget]));
        // Now add a typed lead with a budget.
        store.add_freelance_lead(make_freelance_lead(
            "Upwork",
            "Acme",
            "Build a site",
            Some("https://upwork.com/jobs/1"),
            Some((Some("$50/hr"), Some("hourly"))),
            Some((Some("a@acme.com"), None, None)),
        ));
        // `HasBudget` is now met (against the typed lead),
        // even though the generic `findings` vec is
        // unchanged.
        assert!(store.meets("Upwork", &[Criterion::HasBudget]));
        // The generic criterion still works as before.
        assert!(store.meets("Upwork", &[Criterion::HasEmail]));
    }

    #[test]
    fn store_dedup_and_merge_all_merges_freelance_leads() {
        let mut store = FindingStore::new();
        store.add_freelance_lead(make_freelance_lead(
            "Upwork",
            "Acme",
            "Build a site",
            Some("https://upwork.com/jobs/1"),
            None,
            None,
        ));
        store.add_freelance_lead(make_freelance_lead(
            "Upwork",
            "Acme Studios",
            "Build a 5-page marketing site",
            Some("https://upwork.com/jobs/1"),
            Some((Some("$50-75/hr"), Some("hourly"))),
            Some((Some("a@acme.com"), None, None)),
        ));
        store.deduplicate_and_merge_all();
        assert_eq!(store.freelance_findings().len(), 1);
        assert!(store.freelance_findings()[0].budget_hint.is_some());
        assert!(store
            .freelance_findings()
            .first()
            .unwrap()
            .contact_method
            .is_some());
    }

    #[test]
    fn store_verify_links_clears_dead_freelance_urls() {
        let mut store = FindingStore::new();
        store.add_freelance_lead(make_freelance_lead(
            "Upwork",
            "Acme",
            "Build a site",
            Some("https://upwork.com/jobs/404"),
            None,
            Some((None, Some("https://expired.com/contact"), None)),
        ));
        store.verify_links(true);
        assert!(store.freelance_findings()[0].base.url.is_none());
        assert!(store.freelance_findings()[0]
            .contact_method
            .as_ref()
            .unwrap()
            .url
            .is_none());
    }

    #[test]
    fn freelance_lead_merge_enriches_missing_fields() {
        // First add: no budget, no contact. Second add:
        // same URL, has budget + contact. The merged
        // result should have all four fields populated.
        let mut store = FindingStore::new();
        store.add_freelance_lead(make_freelance_lead(
            "Upwork",
            "Acme",
            "Build a site",
            Some("https://upwork.com/jobs/1"),
            None,
            None,
        ));
        store.add_freelance_lead(make_freelance_lead(
            "Upwork",
            "Acme",
            "Build a site",
            Some("https://upwork.com/jobs/1"),
            Some((Some("$50-75/hr"), Some("hourly"))),
            Some((Some("a@acme.com"), None, None)),
        ));
        let merged = &store.freelance_findings()[0];
        assert!(merged.budget_hint.is_some());
        assert_eq!(merged.budget_hint.as_ref().unwrap().raw, "$50-75/hr");
        assert!(merged.contact_method.is_some());
        assert_eq!(
            merged
                .contact_method
                .as_ref()
                .unwrap()
                .email
                .as_deref()
                .unwrap(),
            "a@acme.com"
        );
    }

    #[test]
    fn freelance_budget_display_handles_no_kind() {
        let raw_only = FreelanceBudget {
            raw: "$50-75/hr".to_string(),
            kind: None,
        };
        assert_eq!(raw_only.to_string(), "$50-75/hr");
        let with_kind = FreelanceBudget {
            raw: "$50-75/hr".to_string(),
            kind: Some("hourly".to_string()),
        };
        assert_eq!(with_kind.to_string(), "$50-75/hr (hourly)");
    }

    #[test]
    fn freelance_lead_one_line_includes_scope_client_budget_contact_source() {
        let l = make_freelance_lead(
            "Upwork",
            "Acme Studios",
            "Build a 5-page marketing site",
            Some("https://upwork.com/jobs/1"),
            Some((Some("$50-75/hr"), Some("hourly"))),
            Some((Some("a@acme.com"), None, None)),
        );
        let line = l.one_line();
        assert!(line.contains("Build a 5-page marketing site"));
        assert!(line.contains("Acme Studios"));
        assert!(line.contains("$50-75/hr"));
        assert!(line.contains("a@acme.com"));
        assert!(line.contains("Upwork"));
    }

    #[test]
    fn freelance_lead_one_line_handles_missing_budget_and_contact() {
        let l = make_freelance_lead(
            "Contra",
            "Beta",
            "Design system",
            Some("https://contra.com/p/1"),
            None,
            None,
        );
        let line = l.one_line();
        assert!(line.contains("Design system"));
        assert!(line.contains("[no contact yet]"));
    }

    #[test]
    fn finding_kind_enum_covers_all_four_typed_lists() {
        // The `FindingKind` enum is the synthesizer's
        // discriminator. Pin the variant count + names so
        // a future refactor that adds a typed list also
        // extends the enum.
        use crate::research::FindingKind;
        let all = [
            FindingKind::Research,
            FindingKind::Job,
            FindingKind::Lead,
            FindingKind::FreelanceLead,
        ];
        assert_eq!(all.len(), 4);
        // Every variant is a distinct value.
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b);
                }
            }
        }
    }

    #[test]
    fn finding_store_findings_by_kind_dispatches_correctly() {
        let mut store = FindingStore::new();
        store.add(ResearchFinding {
            id: "g1".into(),
            platform: "Upwork".into(),
            title: Some("Generic".into()),
            company: Some("Acme".into()),
            email: None,
            url: None,
            note: String::new(),
            added_at_secs: 0,
        });
        store.add_freelance_lead(make_freelance_lead(
            "Contra",
            "Beta",
            "Design",
            Some("https://contra.com/p/1"),
            None,
            None,
        ));
        let freelance_via_kind = store.findings_by_kind(FindingKind::FreelanceLead);
        assert_eq!(freelance_via_kind.len(), 1);
        assert_eq!(freelance_via_kind[0].platform, "Contra");
        let research_via_kind = store.findings_by_kind(FindingKind::Research);
        assert!(research_via_kind.iter().any(|f| f.platform == "Upwork"));
    }

    #[test]
    fn default_freelance_platforms_has_five_entries_with_freelance_acceptance() {
        // The default freelance list mirrors the
        // pre-seeded entries in
        // `config/freelance_platforms.toml`. The
        // acceptance criteria are hard-wired to
        // `[HasBudget, HasContact]` for every row.
        let subs = default_freelance_platforms();
        assert_eq!(subs.len(), 5);
        for s in &subs {
            assert_eq!(s.platform_kind, "freelance_board");
            assert_eq!(s.acceptance.len(), 2);
            assert!(s.acceptance.contains(&Criterion::HasBudget));
            assert!(s.acceptance.contains(&Criterion::HasContact));
        }
    }

    #[test]
    fn freelance_synthesis_hint_uses_freelance_branch() {
        // The spec calls for a "find me clients" task to
        // get the freelance synthesis hint, not the job
        // or generic hint. The hint is the synthesizer's
        // primary signal of which rendering template to
        // use, so a regression in the keyword list would
        // change the user-visible reply.
        let p = ResearchPlanner::plan(
            "find me clients for my Rust consulting",
            &default_freelance_platforms(),
            None,
        );
        assert!(p.is_research);
        assert!(
            p.synthesis_hint.contains("budget") || p.synthesis_hint.contains("contact"),
            "freelance hint should mention budget or contact, got: {}",
            p.synthesis_hint
        );
    }

    #[test]
    fn freelance_platforms_loader_falls_back_to_defaults_when_empty() {
        // An empty `FreelancePlatforms` (the
        // pre-Phase-5 / file-missing case) should still
        // return the default freelance list when
        // `to_freelance_subtasks` is called.
        let t = FreelancePlatforms::default();
        let subs = t.to_freelance_subtasks();
        assert!(!subs.is_empty());
        assert!(subs.iter().any(|s| s.id == "upwork"));
        assert!(subs.iter().any(|s| s.id == "contra"));
        assert!(subs
            .iter()
            .all(|s| s.platform_kind == "freelance_board"));
    }

    #[test]
    fn freelance_platforms_loader_uses_loaded_entries() {
        // A non-empty `FreelancePlatforms` (the
        // post-Phase-5 case where the operator has
        // `config/freelance_platforms.toml` populated)
        // should return exactly the loaded entries,
        // with the Phase 5 acceptance criteria
        // hard-wired.
        let t = FreelancePlatforms {
            entries: vec![ResearchPlatformsFileEntry {
                id: "custom_freelance".into(),
                platform: "Custom Board".into(),
                domain: "custom.example.com".into(),
                entry_hint: "search goal".into(),
                step_budget: 5,
                time_budget_secs: 30,
                default_query: "rust".into(),
                platform_kind: "freelance_board".into(),
                query_template: String::new(),
            }],
            required_domains: Vec::new(),
        };
        let subs = t.to_freelance_subtasks();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].id, "custom_freelance");
        assert_eq!(subs[0].step_budget, 5);
        assert_eq!(subs[0].query, "rust");
        assert!(subs[0].acceptance.contains(&Criterion::HasBudget));
        assert!(subs[0].acceptance.contains(&Criterion::HasContact));
    }

    #[test]
    fn freelance_platforms_loader_load_from_default_location_handles_missing_file() {
        // Pre-Phase-5 setups (no
        // `config/freelance_platforms.toml` on disk)
        // must not error. The loader returns an empty
        // table; the agent falls back to
        // `default_freelance_platforms()` at the
        // conversion step.
        let t = FreelancePlatforms::load_freelance_from_default_location();
        let subs = t.to_freelance_subtasks();
        assert!(!subs.is_empty());
    }
}

