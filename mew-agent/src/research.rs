// mew v2 — Phase 7: Long-Horizon Task Planning, Multi-Platform Research Loop.
//
// Why this exists (one paragraph):
//
// Phase 2's `planner::plan` is a deterministic clause-splitter. It turns
// "go to X and do Y" into two subtasks. That is enough for the Phase 1
// instagram case. It is not enough for the actual end-goal motivating
// this project: "find remote SWE job openings and get a contact email,
// or the URL if no email exists." That task has a different shape —
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
//     built-in job-board list — but the type accepts any domain),
//   * per-platform *satisfaction criteria* the LLM must commit to
//     *before* starting the platform (the "falsifiable commitment"
//     from the Phase 7 brief), and
//   * a top-level *aggregation* hint (what the consolidated answer
//     should look like — e.g. "one row per role, with role title,
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
//      already carry the shape needed for a research task —
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
/// *not* the rendered text — the synthesizer fills it in from the
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
    /// transcript review — "this looked like a research task
    /// because of X." Examples: "research_keyword", "find_x",
    /// "compare_x_across_y", "manual_override".
    pub matched_pattern: String,
}

impl ResearchPlan {
    /// Build a "not a research task" plan. Used as the safe default
    /// when the deterministic rules don't recognize the input as
    /// research-shaped — the orchestrator then falls back to the
    /// Phase 2 single-platform path.
    pub fn not_research(goal: impl Into<String>) -> Self {
        Self {
            goal: goal.into(),
            platforms: Vec::new(),
            synthesis_hint: String::new(),
            overall_deadline_secs: None,
            is_research: false,
            matched_pattern: "none".to_string(),
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
        self.platforms
            .iter()
            .map(|p| p.step_budget)
            .sum::<u32>()
    }
}

// ----------------------------------------------------------------------
// 2. ResearchSubTask: one platform, one set of acceptance criteria.
// ----------------------------------------------------------------------

/// A single platform-target subtask in a research plan. The LLM
/// sees the `entry_hint` and `acceptance` in its system prompt; the
/// agent's loop enforces `step_budget` and `time_budget_secs` via
/// the budget guard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResearchSubTask {
    /// Stable id. Same shape as the rest of the codebase — slug,
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
    /// and search". Optional — empty string means "the LLM is on
    /// its own for entry strategy".
    pub entry_hint: String,
    /// What the agent commits to seeing on this platform before
    /// marking the subtask done. The plan's "falsifiable
    /// commitment" — the LLM must produce findings that match
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
    /// browse the listings directly).
    pub query: String,
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
                    format!(" — {}", self.entry_hint)
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
/// platform — that's the Phase 7 spec's "falsifiable commitment"
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
    /// different ones — the operator is "at least one of each
    /// across the whole store."
    HasEmailAndUrl,
    /// AND of two criteria. Both must hold. Used to express "at
    /// least 2 findings with an email."
    AllOf(Vec<Criterion>),
    /// OR. Any one holding satisfies the operator.
    AnyOf(Vec<Criterion>),
}

impl Criterion {
    /// Evaluate the criterion against a list of `ResearchFinding`
    /// rows. Pure function — no I/O, no LLM, no clock. The
    /// `FindingStore::meets` method calls this; tests exercise it
    /// directly.
    pub fn is_met_by(&self, findings: &[ResearchFinding]) -> bool {
        match self {
            Criterion::HasJobCountAtLeast { n } => {
                findings.len() as u32 >= *n
            }
            Criterion::HasEmail => findings.iter().any(|f| f.email.is_some()),
            Criterion::HasUrl => findings.iter().any(|f| f.url.is_some()),
            Criterion::HasEmailAndUrl => {
                findings.iter().any(|f| f.email.is_some())
                    && findings.iter().any(|f| f.url.is_some())
            }
            Criterion::AllOf(children) => {
                children.iter().all(|c| c.is_met_by(findings))
            }
            Criterion::AnyOf(children) => {
                children.iter().any(|c| c.is_met_by(findings))
            }
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
            Criterion::HasEmailAndUrl => {
                "at least one email AND at least one URL".to_string()
            }
            Criterion::AllOf(children) => {
                let parts: Vec<String> = children.iter().map(|c| c.describe()).collect();
                format!("all of: [{}]", parts.join(" AND "))
            }
            Criterion::AnyOf(children) => {
                let parts: Vec<String> = children.iter().map(|c| c.describe()).collect();
                format!("any of: [{}]", parts.join(" OR "))
            }
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
    /// Job title / role. Optional — some boards (e.g. generic
    /// career pages) don't surface a clean title.
    pub title: Option<String>,
    /// Company. Optional.
    pub company: Option<String>,
    /// Contact email, if the page surfaced one. The LLM fills
    /// this from the snapshot text.
    pub email: Option<String>,
    /// Application URL. Optional — for boards where the role
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
    /// the consolidated answer. Format is intentionally simple —
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
            s.push_str(" — ");
            s.push_str(url);
        }
        if let Some(email) = &self.email {
            s.push_str(" (contact: ");
            s.push_str(email);
            s.push(')');
        }
        if !self.note.is_empty() {
            s.push_str(" — ");
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
/// work). It's not `Sync` — concurrent reads and writes are not
/// supported; the store is touched only from the main loop. If a
/// future refactor moves finding-adds onto a separate thread, wrap
/// it in a `Mutex<FindingStore>` like the rest of `Agent`'s state.
#[derive(Debug, Default, Clone)]
pub struct FindingStore {
    findings: Vec<ResearchFinding>,
    /// Count of times a `add` was a no-op because the row was
    /// already in the store. Surfaced to the transcript so a
    /// reviewer can tell at a glance whether the dedup is doing
    /// anything (a high count means the LLM is re-adding the
    /// same rows on every iteration).
    dedup_hits: u64,
}

impl FindingStore {
    /// Build an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// All current findings. Cheap clone. The order is "insertion
    /// order" — the synthesizer renders in this order, with the
    /// most recently added rows last.
    pub fn findings(&self) -> &[ResearchFinding] {
        &self.findings
    }

    /// How many dedup hits the store has absorbed since startup.
    /// Exposed for the per-platform transcript line and for the
    /// orchestrator's "did this actually aggregate?" sanity check.
    pub fn dedup_hits(&self) -> u64 {
        self.dedup_hits
    }

    /// Add a row. Idempotent on `(platform, url, title)`. The
    /// returned `ResearchFinding` is the *stored* row — the
    /// `added_at_secs` and `id` fields are filled in by the
    /// store, so callers that pass partial rows (no `id`, no
    /// timestamp) still get a fully-populated row back.
    ///
    /// Returns `true` when the row was a new addition,
    /// `false` when it was deduplicated against an existing row.
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
        self.findings.push(row);
        true
    }

    /// Filter the store to findings from a single platform.
    /// Used by the per-subtask satisfaction check (the LLM's
    /// commitment only applies to *its* platform, not the cross-
    /// platform aggregate — otherwise finding the right answer
    /// on platform A and a wrong answer on platform B would
    /// still satisfy platform A's commitment).
    pub fn findings_for_platform(&self, platform: &str) -> Vec<ResearchFinding> {
        self.findings
            .iter()
            .filter(|f| f.platform.eq_ignore_ascii_case(platform))
            .cloned()
            .collect()
    }

    /// True when at least one of the criteria is met by the
    /// findings on the named platform. Used by the
    /// `CompletenessTracker` extended mark-done path.
    pub fn meets(&self, platform: &str, criteria: &[Criterion]) -> bool {
        let rows = self.findings_for_platform(platform);
        criteria.iter().any(|c| c.is_met_by(&rows))
    }

    /// Clear the store. Used by the budget guard when a platform
    /// is marked `Exhausted` — its findings are kept (cross-
    /// platform aggregation is the whole point) but the
    /// per-platform filter is what subsequent satisfaction
    /// checks key on, so the platform tag stays stable.
    ///
    /// (We do not implement "remove all findings for platform X"
    /// because that would defeat cross-platform dedup. A finding
    /// observed on one platform and re-observed on another is
    /// still a single finding.)
    pub fn clear(&mut self) {
        self.findings.clear();
        self.dedup_hits = 0;
    }
}

/// Compute the dedup id. The triple is lowercased and joined
/// with `\u{1F}` (the unit separator, so a `title` containing a
/// `|` cannot collide with the separator). The result is a short
/// hex digest of the joined string — `DefaultHasher` is fine
/// here, this is not a security boundary.
fn compute_finding_id(
    platform: &str,
    url: Option<&str>,
    title: Option<&str>,
) -> String {
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
///   1. Empty / whitespace-only input → `ResearchPlan::not_research`
///      with rationale "no task".
///   2. Phrases matching the `RESEARCH_KEYWORDS` list
///      (case-insensitive substring on the *whole* task) →
///      research plan, matched_pattern = "research_keyword".
///      Examples: "find", "research", "look up", "search for",
///      "compare", "gather", "compile", "list of".
///   3. Phrases matching the `AGGREGATE_KEYWORDS` list → research
///      plan, matched_pattern = "aggregate_keyword". Examples:
///      "across", "from each", "multiple platforms",
///      "every site".
///   4. Otherwise → `ResearchPlan::not_research`. The orchestrator
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
/// for. Everything else gets a generic "list each finding"
/// hint.
fn synthesis_hint_for(goal: &str) -> String {
    let lower = goal.to_ascii_lowercase();
    let job_shaped = lower.contains("job")
        || lower.contains("hiring")
        || lower.contains("career")
        || lower.contains("opening")
        || lower.contains("position")
        || lower.contains("role");
    if job_shaped {
        "One row per role: role title, company, and either a contact email or an application URL.".to_string()
    } else {
        "One row per finding: title, source platform, and any contact detail surfaced.".to_string()
    }
}

/// Pattern-match a task against the research-shape keywords.
/// Returns (is_research, matched_pattern). The pattern string is
/// the human-readable name of the rule that fired — surfaced in
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
    // Deliberately excludes "search for " — that one is the
    // common single-site Phase 2 case ("go to wikipedia and
    // search for X"), not a research task.
    const RESEARCH_KEYWORDS: &[&str] = &[
        "find ", "research ", "look up ", "look for ",
        "compare ", "gather ", "compile ", "list of ", "collect ",
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
        " across ", " from each ", " on every ", " from every ",
        "multiple platforms", "all platforms", "every site",
        "job boards", "job sites",
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
/// "reasonable defaults for the motivating case" — not "every
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
}

/// Top-level TOML shape. The file is `[[entry]]` blocks, each
/// with the schema above.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct ResearchPlatformsFile {
    #[serde(default, rename = "entry")]
    pub entry: Vec<ResearchPlatformsFileEntry>,
}

/// In-memory mirror of the file. Conversion to
/// `Vec<ResearchSubTask>` happens once on load.
#[derive(Debug, Clone, Default)]
pub struct ResearchPlatforms {
    pub entries: Vec<ResearchPlatformsFileEntry>,
}

impl ResearchPlatforms {
    /// Load from the default `config/research_platforms.toml`. Walks
    /// parent directories like `SensitivePlatforms::load_from_default_location`
    /// so the call works regardless of the process's CWD.
    ///
    /// Returns an empty `ResearchPlatforms` (not an error) if the
    /// file is missing — `default_job_board_platforms()` is the
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
    /// failure — the caller decides whether that's fatal.
    pub fn load_from(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            anyhow::anyhow!("read {}: {e}", path.display())
        })?;
        let parsed: ResearchPlatformsFile = toml::from_str(&content).map_err(|e| {
            anyhow::anyhow!("parse {}: {e}", path.display())
        })?;
        Ok(Self { entries: parsed.entry })
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
                // for now — the file format doesn't expose
                // per-platform acceptance yet. Operators who
                // need a different default can edit
                // `default_acceptance()` in this file.
                acceptance: default_acceptance(),
                step_budget: e.step_budget,
                time_budget_secs: e.time_budget_secs,
                query: e.default_query.clone(),
            })
            .collect()
    }
}

// ----------------------------------------------------------------------
// 9. ResearchConfig: the typed `config.agent.research` block.
// --------------------------------------------------------------------//

/// The Phase 7 research loop's config block, exposed as
/// `config.agent.research` in `config.yaml` and serializable
/// as the corresponding TOML/JSON shape.
///
/// Defaults: enabled = true, default platforms = the
/// built-in job-board list. Operators who don't want the
/// research loop at all can set `enabled = false` — the
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
    /// hard backstop — the overall deadline is a soft signal.
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
    /// routing is *not* used for research tasks — the agent
    /// will direct-navigate to every platform in the list.
    /// Default is `true` (sensitive routing is on) because
    /// the default platform list contains linkedin.com,
    /// indeed.com, etc. which Phase 2's `sensitive_platforms.toml`
    /// already routes through search.
    #[serde(default = "default_research_sensitive_routing")]
    pub use_sensitive_routing: bool,
}

impl Default for ResearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            overall_deadline_secs: None,
            platforms: default_job_board_platforms(),
            use_sensitive_routing: true,
        }
    }
}

fn default_research_enabled() -> bool {
    true
}
fn default_research_sensitive_routing() -> bool {
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
                },
            ],
            synthesis_hint: String::new(),
            overall_deadline_secs: None,
            is_research: true,
            matched_pattern: "test".into(),
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
        assert!(p.is_research, "task should be classified as research: {p:?}");
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
        // verb (no "find " / "compare " / "research " etc.) —
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
        let p = ResearchPlanner::plan(
            "find rust jobs",
            &default_job_board_platforms(),
            None,
        );
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
        assert!(!again, "case-insensitive dedup should collapse the two rows");
        assert_eq!(s.findings().len(), 1);
    }

    #[test]
    fn store_dedup_id_is_stable() {
        // Same (platform, url, title) → same id, even when added
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
            }],
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
        // have to specify anything else — the other
        // fields default. (An empty `platforms` list is
        // fine when the loop is disabled: the planner
        // is bypassed entirely.)
        let yaml = "enabled: false\n";
        let cfg: ResearchConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(!cfg.enabled);
        assert!(cfg.platforms.is_empty(), "platforms should default to empty when omitted, not the heavy default list");
    }
}
