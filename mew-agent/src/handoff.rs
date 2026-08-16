// mew v2 — Phase 3: typed Handoff/Result structs for the two-agent split.
//
// Background (see `docs/architecture-current.md` for the pre-Phase-3
// map): the user message and the browser agent's final answer were
// shuttled through `String` across every layer — the classifier's
// `reply`, the `Intent::BrowserTask(task)` payload, the
// `run_browser_task` argument, the `app.emit("chat-reply", text)`
// payload, and the frontend's `appendMessage(text, 'agent')`. There
// was no contract that said "this is what handoff-in looks like" or
// "this is what handoff-out looks like." A regression in any layer
// could silently lose information — that was the Bug #2 user-visible
// failure mode.
//
// Phase 3 fixes that by introducing two *typed* structs and wiring
// them through the orchestrator path. The wire format is intentionally
// JSON-friendly (every field is `String` / `Vec<...>` / `bool`) so
// `serde_json::to_value` produces a payload the existing Tauri Channel
// surface can carry without bespoke (de)serialization adapters.
//
// Design notes:
//
//   * `Handoff` is what `ChatAgent` hands to `BrowserAgent`. It
//     carries the *task* (one string), the *plan* (the pre-flight
//     decomposition's subtask list — the code is the source of truth
//     even if the LLM amends it later), any *constraints* the user
//     has expressed that the browser agent must honor (time budget,
//     sensitive-platform rules, etc.), and an
//     `originating_message_id` so a follow-up `chat-reply` can be
//     traced back to the specific UI event that triggered it.
//
//   * `BrowserResult` is what `BrowserAgent` hands back to
//     `ChatAgent`. It carries the *status* (Done / Partial / Failed —
//     see the enum), a one-paragraph *summary* suitable for showing
//     in the chat list verbatim, a list of *key_findings* (per-subtask
//     outcomes pulled from `CompletenessTracker`), the
//     *final_snapshot_signature* (the page-state hash from the most
//     recent snapshot — useful for "view details" affordances and for
//     future replay tooling), and a *raw_transcript_ref* pointing at
//     the on-disk session log a user (or a debugger) can read for the
//     full record.
//
//   * The ChatAgent's `synthesize_reply(&BrowserResult, &ConversationContext)`
//     turns the typed Result into the actual one-liner the user sees.
//     Phase 3.1 keeps the synthesis deterministic (templated), so the
//     common case is one LLM-free string. The synthesis *can* be
//     upgraded to an LLM call in a later phase without changing the
//     orchestrator wiring; the `Result` carries enough structured
//     detail that any future summarizer has what it needs.
//
//   * Both structs implement `Serialize`/`Deserialize` because the
//     test surface uses JSON round-tripping as a sanity check, and
//     because a future remote-orchestrator could carry the same
//     types over a wire protocol.

use serde::{Deserialize, Serialize};

use crate::completeness::SubTask;
use crate::research::{ResearchFinding, ResearchPlan};
pub use crate::router::TaskShape;
use crate::supervisor::RespawnReason;
use crate::todo::{TodoId, TodoStatus};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobQuery {
    pub roles: Vec<String>,
    pub locations: Vec<String>,
    pub is_remote: bool,
    pub is_internship: bool,
    pub experience_level: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobFinding {
    #[serde(flatten)]
    pub base: ResearchFinding,
    pub freshness_days: Option<u32>,
    pub is_internship: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeadQuery {
    pub industry: Vec<String>,
    pub company_size_range: Option<(u32, u32)>,
    pub geography: Vec<String>,
    pub target_role_titles: Vec<String>,
    pub product_category_keywords: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum LeadConfidence {
    High,
    Medium,
    Low,
}

impl std::fmt::Display for LeadConfidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeadConfidence::High => write!(f, "High"),
            LeadConfidence::Medium => write!(f, "Medium"),
            LeadConfidence::Low => write!(f, "Low"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeadFinding {
    #[serde(flatten)]
    pub base: ResearchFinding,
    pub company_name: String,
    pub company_domain: String,
    pub contact_name: Option<String>,
    pub contact_role: Option<String>,
    pub contact_email: Option<String>,
    pub contact_url: Option<String>,
    pub source_platform: String,
    pub confidence: LeadConfidence,
}

// ----------------------------------------------------------------------
// Phase 5: Freelance client lead-gen — typed `FreelanceLead` finding.
//
// Distinct from `LeadFinding` (SaaS product-lead-gen) and
// `JobFinding` (full-time job search). A `FreelanceLead` is the
// output of "find me paying clients for service X" — a project
// listing that has a client name, a scope of work, a budget /
// rate hint, and a contact method. The four required fields
// differ from a `LeadFinding` (which keys on a company domain
// and a contact person) and from a `JobFinding` (which keys on
// a role title and a posting URL). The synthesizer uses the
// `kind` discriminator to render a separate "Freelance leads"
// section in the consolidated chat reply, so a mixed task
// ("find me a job and find me clients") produces two
// clearly-separated lists rather than one blended soup.
// ----------------------------------------------------------------------

/// A budget / rate hint the LLM extracted from a freelance
/// project listing. Free-form string so the agent can record
/// whatever the page actually surfaced ("$50-75/hr",
/// "$5,000 fixed", "Negotiable"). The synthesizer renders
/// the raw value verbatim; no normalization is applied.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FreelanceBudget {
    /// Raw text from the page. Empty when the page didn't
    /// surface a number (e.g. "negotiable" only).
    pub raw: String,
    /// "hourly", "fixed", or "range" — best-effort
    /// classification. The agent's parser is heuristic, so
    /// callers must not assume this is always correct. `None`
    /// when the page gave no usable shape.
    pub kind: Option<String>,
}

impl std::fmt::Display for FreelanceBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            Some(k) => write!(f, "{} ({})", self.raw, k),
            None => f.write_str(&self.raw),
        }
    }
}

/// The contact method the LLM extracted. A listing may
/// surface a direct email, a messaging link back to the
/// platform (e.g. "message client on Upwork"), or an external
/// URL (e.g. the client's own site). Exactly one of these is
/// typically populated, but all are kept `Option<String>` so
/// the LLM can record a listing that surfaces more than one.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FreelanceContact {
    /// Direct email address (e.g. "alice@example.com").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// External URL (e.g. the client's portfolio site or a
    /// Calendly link).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// In-platform contact note (e.g. "Message via Upwork" or
    /// "DM the client on Contra"). Used when the platform
    /// gates the actual contact behind its own messaging UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_message_hint: Option<String>,
}

impl FreelanceContact {
    /// True when at least one contact channel is populated.
    /// Used by `FindingStore` and the synthesis filter to
    /// determine whether a listing is "reachable."
    pub fn is_present(&self) -> bool {
        self.email.is_some() || self.url.is_some() || self.platform_message_hint.is_some()
    }
}

/// A freelance client lead — one project listing on a
/// freelance board. Distinct from `LeadFinding` (which is for
/// SaaS product companies) and `JobFinding` (which is for
/// full-time roles).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FreelanceLead {
    #[serde(flatten)]
    pub base: ResearchFinding,
    /// Client name (e.g. "Acme Studios"). May be a real
    /// company or a pseudonym ("Private Client") — the agent
    /// records whatever the page surfaced.
    pub client_name: String,
    /// Short project scope. Examples: "Build a 5-page
    /// marketing site", "Migrate Postgres to Aurora". The
    /// agent extracts this from the listing's title or its
    /// first paragraph; it is *not* a full description.
    pub project_scope: String,
    /// Budget / rate hint, when the page surfaced one. `None`
    /// when the listing is "negotiable" or the budget is
    /// gated behind a "reveal" button the agent can't click.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_hint: Option<FreelanceBudget>,
    /// Contact method (email / URL / in-platform message).
    /// `None` when the page gave no contact information at
    /// all (the listing is "message client on Upwork" with
    /// no further detail). The synthesizer renders such
    /// listings with a `[no contact yet]` tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_method: Option<FreelanceContact>,
    /// Freelance board this came from. Free text; matches
    /// `ResearchSubTask::platform` for the platform the
    /// worker visited. Multiple leads from the same platform
    /// are kept distinct (no source-merge like
    /// `LeadFinding::merge` does).
    pub source_platform: String,
}

impl FreelanceLead {
    /// One-line rendering, used by the synthesizer to build
    /// the consolidated "Freelance leads" section. Format:
    ///
    ///   <project_scope> for <client_name> — <budget> — <contact> [<source>]
    ///
    /// Each component is omitted cleanly when not present so
    /// the line never ends with a stray `—`.
    pub fn one_line(&self) -> String {
        let mut s = String::new();
        if !self.project_scope.is_empty() {
            s.push_str(&self.project_scope);
        } else {
            s.push_str("(no scope)");
        }
        s.push_str(" for ");
        s.push_str(&self.client_name);
        if let Some(b) = &self.budget_hint {
            s.push_str(" — ");
            s.push_str(&b.to_string());
        }
        match &self.contact_method {
            Some(c) if c.is_present() => {
                s.push_str(" — ");
                if let Some(e) = &c.email {
                    s.push_str(e);
                } else if let Some(u) = &c.url {
                    s.push_str(u);
                } else if let Some(h) = &c.platform_message_hint {
                    s.push_str(h);
                }
            }
            _ => {
                s.push_str(" — [no contact yet]");
            }
        }
        s.push_str(" [");
        s.push_str(&self.source_platform);
        s.push(']');
        if let Some(u) = &self.base.url {
            s.push_str(" — ");
            s.push_str(u);
        }
        s
    }
}

impl JobFinding {
    /// Merge another `JobFinding` into `self`, keeping the best available fields.
    pub fn merge(&mut self, other: JobFinding) {
        if self.base.title.as_deref().unwrap_or("").len()
            < other.base.title.as_deref().unwrap_or("").len()
        {
            self.base.title = other.base.title;
        }
        if self.base.company.is_none() && other.base.company.is_some() {
            self.base.company = other.base.company;
        }
        if self.base.url.is_none() && other.base.url.is_some() {
            self.base.url = other.base.url;
        }
        if self.base.email.is_none() && other.base.email.is_some() {
            self.base.email = other.base.email;
        }
        if !other.base.note.is_empty() {
            if self.base.note.is_empty() {
                self.base.note = other.base.note;
            } else if !self.base.note.contains(&other.base.note) {
                self.base.note.push_str(" | ");
                self.base.note.push_str(&other.base.note);
            }
        }
        match (self.freshness_days, other.freshness_days) {
            (Some(a), Some(b)) => self.freshness_days = Some(a.min(b)),
            (None, Some(b)) => self.freshness_days = Some(b),
            _ => {}
        }
        self.is_internship = self.is_internship || other.is_internship;
    }
}

impl LeadFinding {
    /// Merge another `LeadFinding` into `self`, enriching missing or low-confidence fields.
    pub fn merge(&mut self, other: LeadFinding) {
        if self.company_name.len() < other.company_name.len() {
            self.company_name = other.company_name;
        }
        if self.contact_name.is_none() && other.contact_name.is_some() {
            self.contact_name = other.contact_name;
        }
        if self.contact_role.is_none() && other.contact_role.is_some() {
            self.contact_role = other.contact_role;
        }
        if self.contact_email.is_none() && other.contact_email.is_some() {
            self.contact_email = other.contact_email;
            self.confidence = other.confidence;
        } else if other.confidence > self.confidence {
            if let Some(email) = other.contact_email {
                self.contact_email = Some(email);
            }
            self.confidence = other.confidence;
        }
        if self.contact_url.is_none() && other.contact_url.is_some() {
            self.contact_url = other.contact_url;
        }
        if self.base.url.is_none() && other.base.url.is_some() {
            self.base.url = other.base.url;
        }
        if !other.source_platform.is_empty()
            && !self.source_platform.contains(&other.source_platform)
        {
            self.source_platform.push_str(", ");
            self.source_platform.push_str(&other.source_platform);
        }
    }
}

impl FreelanceLead {
    /// Merge another `FreelanceLead` into `self`, filling in
    /// fields the original listing did not surface but a
    /// follow-up visit (e.g. the platform's project detail
    /// page) did. The merge strategy is "first non-empty
    /// wins, longer wins for free-text fields, any contact
    /// channel is better than none."
    pub fn merge(&mut self, other: FreelanceLead) {
        if self.client_name.len() < other.client_name.len() {
            self.client_name = other.client_name;
        }
        if self.project_scope.len() < other.project_scope.len() {
            self.project_scope = other.project_scope;
        }
        if self.base.url.is_none() && other.base.url.is_some() {
            self.base.url = other.base.url;
        }
        if !other.base.note.is_empty() {
            if self.base.note.is_empty() {
                self.base.note = other.base.note;
            } else if !self.base.note.contains(&other.base.note) {
                self.base.note.push_str(" | ");
                self.base.note.push_str(&other.base.note);
            }
        }
        if self.budget_hint.is_none() && other.budget_hint.is_some() {
            self.budget_hint = other.budget_hint;
        }
        // Contact merge: any channel from `other` that we
        // don't have fills in. If both have an email, we keep
        // ours (first observation wins) — flipping would
        // change the user-visible reply mid-synthesis.
        match (&mut self.contact_method, other.contact_method) {
            (Some(c_self), Some(c_other)) => {
                if c_self.email.is_none() {
                    c_self.email = c_other.email;
                }
                if c_self.url.is_none() {
                    c_self.url = c_other.url;
                }
                if c_self.platform_message_hint.is_none() {
                    c_self.platform_message_hint = c_other.platform_message_hint;
                }
            }
            (slot @ None, Some(c_other)) => {
                *slot = Some(c_other);
            }
            _ => {}
        }
        if !other.source_platform.is_empty()
            && !self.source_platform.contains(&other.source_platform)
        {
            self.source_platform.push_str(", ");
            self.source_platform.push_str(&other.source_platform);
        }
    }
}

// ----------------------------------------------------------------------
// Phase 34: Google Maps vertical pack — typed `MapsQuery` / `MapsFinding`.
//
// Maps is a different shape from the existing `JobSearch` / `LeadGen`
// verticals: the user is on a single page (the Maps SPA) but the page
// has a sidebar of listings and a panel overlay per listing. We need
// a query struct that the deterministic parser fills in from the
// user's free-text task ("salons in Mumbai with no website, rating
// above 4.0, check 15 listings") and a finding struct per listing
// visited, so the final chat reply can render a ranked table.
//
// `MapsQuery::parse` is deterministic — no LLM call. The Maps
// vertical is a tight, well-bounded parser, not a free-form reasoner.
// --------------------------------------------------------------------//

/// Hard cap on how many listings the agent will visit. The phase
/// spec calls for `max 50`; the cost-aware default stays at 15. The
/// cap exists so a typo ("check 5000 listings") can't blow the
/// per-task token budget.
pub const MAPS_LISTING_HARD_CAP: usize = 50;
/// Default listings the agent will visit if the task doesn't
/// specify a count. Matches the phase 34 acceptance scenario.
pub const MAPS_LISTING_DEFAULT: usize = 15;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MapsQuery {
    /// Full search string the user asked for, e.g. "salons in Mumbai".
    pub search_query: String,
    /// City / location, extracted by splitting on " in " (last
    /// token). The Maps entry URL template
    /// (`config/maps_vertical.toml`) takes the full search query,
    /// so this field is mostly for the system-prompt hint and for
    /// the eval-scenario asserts.
    pub city: String,
    /// Number of listings the agent should visit. Capped at
    /// `MAPS_LISTING_HARD_CAP` (50) so a runaway parse can't blow
    /// the budget. Default `MAPS_LISTING_DEFAULT` (15).
    pub listing_count: usize,
    /// Minimum rating filter (e.g. `Some(4.0)` for "rating above
    /// 4.0"). `None` means "no rating filter."
    pub min_rating: Option<f32>,
    /// `true` when the user said "no website" / "without website" /
    /// "missing website" — only listings with no website qualify.
    pub require_no_website: bool,
    /// `true` when the user said "with phone" / "must have phone"
    /// — only listings with a phone qualify.
    pub require_phone: bool,
    /// Phase 37: how many parallel workers to spawn for a
    /// multi-category Maps task. Default 1 = single-tab, fully
    /// backward-compatible with pre-Phase-37 Maps tasks. The
    /// orchestrator only uses parallelism > 1 when
    /// `categories.len() > 1` (a multi-category task) and a
    /// `ParallelMapsFactory` is wired in.
    #[serde(default = "default_maps_parallelism")]
    pub parallelism: usize,
    /// Phase 37: the per-worker category names for a
    /// multi-category task (e.g. `["salons", "gyms",
    /// "clinics"]` for "salons and gyms and clinics in
    /// Mumbai"). Empty for single-category tasks. The
    /// orchestrator's parallel dispatch path creates one
    /// `Handoff` per category, each carrying a single-element
    /// `categories` list and a re-scoped `search_query`.
    #[serde(default)]
    pub categories: Vec<String>,
}

fn default_maps_parallelism() -> usize {
    1
}

impl MapsQuery {
    /// Deterministic keyword parser. No LLM call. The Maps vertical
    /// is a tight, well-bounded parser: it extracts the things the
    /// rest of the system can act on, and falls back to sensible
    /// defaults for everything else.
    ///
    /// Extraction rules (in order):
    ///   1. `search_query`  — the original task string verbatim.
    ///   2. `city`          — the substring after the last ` in `
    ///                        token (case-insensitive). Empty if
    ///                        no ` in ` found.
    ///   3. `listing_count` — first integer preceded by "first" /
    ///                        "check" / "top" / "visit" / "list"
    ///                        or followed by "listings" / "places"
    ///                        / "results" / "businesses". Capped
    ///                        at `MAPS_LISTING_HARD_CAP`. Default
    ///                        `MAPS_LISTING_DEFAULT`.
    ///   4. `min_rating`    — first float in the task that appears
    ///                        after the literal "rating" (within
    ///                        ~5 tokens). `None` if no such float.
    ///   5. `require_no_website` — true if any of "no website",
    ///                        "without website", "missing website"
    ///                        appears (case-insensitive).
    ///   6. `require_phone` — true if any of "with phone", "must
    ///                        have phone" appears (case-insensitive).
    pub fn parse(task: &str) -> Self {
        let lower = task.to_lowercase();
        let trimmed = task.trim();

        // (2) City: the *first word* after the LAST " in "
        // (case-insensitive). Maps listings almost always say
        // "X in <city>" and the city is one word (or a
        // hyphenated phrase). The spec says "extracted by
        // splitting on 'in' + taking the last word" — we
        // take the first whitespace-delimited token after the
        // final " in ", with hyphens and apostrophes treated
        // as part of the word. Examples that match correctly:
        //   "salons in Mumbai" -> "Mumbai"
        //   "clinics in San Francisco" -> "San"
        //   "restaurants near Bengaluru" -> "Bengaluru"
        //     (we also accept " near " as a location preposition)
        fn first_word_after(s: &str, marker: &str) -> String {
            s.rsplit_once(marker)
                .map(|(_, rest)| {
                    rest.trim()
                        .chars()
                        .take_while(|c| c.is_alphabetic() || *c == '-' || *c == '\'')
                        .collect::<String>()
                })
                .unwrap_or_default()
        }
        let city_in = first_word_after(&lower, " in ");
        let city_near = first_word_after(&lower, " near ");
        // Preserve both for the Phase 37 multi-category
        // re-attach step below — `city` is just one of them
        // (the in-marker winner), but we may need the
        // near-marker fallback if the in-marker produced
        // nothing.
        let city = if !city_in.is_empty() {
            city_in.clone()
        } else {
            city_near.clone()
        };

        // (3) listing_count. Look for the first integer in the
        // task that is anchored by a "count hint" word. We accept
        // either "<hint> <N>" or "N <listings>". Cap at 50.
        let count_hint_before = ["first ", "check ", "top ", "visit ", "list ", "scrape "];
        let count_hint_after = [" listings", " places", " results", " businesses", " leads", " salons", " gyms", " clinics", " restaurants"];
        let mut listing_count: Option<usize> = None;
        for hint in &count_hint_before {
            if let Some(idx) = lower.find(hint) {
                let after = &lower[idx + hint.len()..];
                let n: Option<usize> = after
                    .trim_start()
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .ok();
                if let Some(n) = n {
                    listing_count = Some(n);
                    break;
                }
            }
        }
        if listing_count.is_none() {
            for hint in &count_hint_after {
                if let Some(idx) = lower.find(hint) {
                    let before = &lower[..idx];
                    let n: Option<usize> = before
                        .chars()
                        .rev()
                        .take_while(|c| c.is_ascii_digit() || c.is_whitespace())
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect::<String>()
                        .trim()
                        .parse()
                        .ok();
                    if let Some(n) = n {
                        listing_count = Some(n);
                        break;
                    }
                }
            }
        }
        let listing_count = listing_count
            .map(|n| n.min(MAPS_LISTING_HARD_CAP).max(1))
            .unwrap_or(MAPS_LISTING_DEFAULT);

        // (4) min_rating. Find the substring "rating" and look
        // forward ~20 chars for the first float. Accepts "4.0",
        // "4", "4.5", etc.
        let min_rating = lower
            .find("rating")
            .and_then(|idx| {
                let after = &lower[idx + "rating".len()..];
                // Skip up to ~10 non-digit prefix chars (" above ",
                // " >= ", " greater than ", ":", etc.).
                let window: String = after
                    .chars()
                    .take(20)
                    .skip_while(|c| !c.is_ascii_digit() && *c != '.')
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect();
                window.parse::<f32>().ok()
            });

        // (5) require_no_website.
        let require_no_website = lower.contains("no website")
            || lower.contains("without website")
            || lower.contains("missing website")
            || lower.contains("doesn't have a website")
            || lower.contains("do not have a website");

        // (6) require_phone.
        let require_phone = lower.contains("with phone")
            || lower.contains("must have phone")
            || lower.contains("with a phone")
            || lower.contains("phone number");

        // Phase 37 (7) detect multi-category tasks. The user
        // says "salons and gyms and clinics in Mumbai" — we
        // split on the canonical English conjunctions and
        // also accept `, ` and `+ ` as category separators.
        // Each split chunk becomes one `category` entry. The
        // `search_query` of each spawned worker is the
        // category plus the city (e.g. "salons in Mumbai").
        //
        // We only count a split as a real multi-category
        // task when (a) the result has 2+ non-empty chunks
        // AND (b) the original task contains a city marker
        // (" in " or " near "). Without a city marker, a
        // `salons and spas` task is not a Maps task at all
        // (the LocalLeadGen classifier rejects it), and we
        // don't want a stray "go to wikipedia and open the
        // page" task to spuriously fan out.
        let raw_categories: Vec<String> = split_multi_category(&lower);
        let has_city_marker =
            lower.contains(" in ") || lower.contains(" near ");
        let categories: Vec<String> = if has_city_marker
            && raw_categories.len() > 1
        {
            // Re-attach the city to each category so the
            // spawned worker's `search_query` is a complete
            // Maps query string. The orchestrator hands
            // each category to a fresh worker. `city` (the
            // owned field on `Self` below) already holds
            // either `city_in` or `city_near`; we re-use
            // its owned value here for the re-attach.
            let city_owned = city.clone();
            raw_categories
                .into_iter()
                .map(|c| {
                    if city_owned.is_empty() {
                        c
                    } else {
                        format!("{} in {}", c, city_owned)
                    }
                })
                .collect()
        } else {
            // Single-category: keep the original
            // `search_query` verbatim, set parallelism = 1
            // so the orchestrator's parallel-dispatch path
            // short-circuits and the single-factory path
            // runs unchanged. The `categories` vec is empty
            // for the single-category case — the
            // orchestrator's `parallelism > 1` guard
            // already prevents fan-out.
            Vec::new()
        };
        let parallelism = if categories.len() > 1 {
            // Cap default parallelism at 3 — that's the
            // Phase 37 spec cap ("min(category_count, 3)").
            // Operators can raise the cap via
            // `MapsQuery { parallelism, .. }` in test
            // fixtures; the orchestrator's own dispatch
            // path also caps by `max_concurrent_tasks` from
            // the YAML config.
            categories.len().min(3)
        } else {
            1
        };

        Self {
            search_query: trimmed.to_string(),
            city,
            listing_count,
            min_rating,
            require_no_website,
            require_phone,
            parallelism,
            categories,
        }
    }
}

/// Phase 37: split a multi-category Maps task into its
/// per-category chunks. The grammar we accept is the union of
/// the Phase 37 spec list (` and `, `" and "`, `, `, `+ `).
/// We normalize to lowercase first so a user-typed
/// `"Salons and gyms"` parses the same as
/// `"salons and gyms"`.
///
/// The function is intentionally conservative: a `salons and
/// spas` task with no other category separators falls
/// through as a single chunk (so the parallel-dispatch guard
/// `categories.len() > 1` rejects it and the task stays on
/// the single-worker path). The downstream
/// `has_city_marker` check in `MapsQuery::parse` then
/// determines whether a 2+ chunk split is actually a
/// multi-category Maps task.
fn split_multi_category(lower: &str) -> Vec<String> {
    // 1) Split on the conjunction ` and ` (with surrounding
    //    spaces) and `, ` and `+ ` (with a trailing space so
    //    "salons+spas" without a space doesn't fire). We
    //    also accept the curly-quote variant `" and "` —
    //    smart-quoted input is common in copy-paste from
    //    chat clients.
    let mut pieces: Vec<String> = vec![lower.to_string()];
    for sep in &[" and ", "\" and \"", ", ", "+ "] {
        let mut next: Vec<String> = Vec::new();
        for p in pieces {
            for q in p.split(sep) {
                let t = q.trim().to_string();
                if !t.is_empty() {
                    next.push(t);
                }
            }
        }
        pieces = next;
        if pieces.len() > 1 {
            // Stop at the first separator that produces a
            // multi-piece split; we don't re-split the
            // already-split chunks on a different separator.
            // The "salons and gyms, clinics" case is rare
            // enough that a single-separator pass is fine.
            break;
        }
    }
    if pieces.len() <= 1 {
        return pieces;
    }
    // 2) Strip a leading "find " / "show me " / "search
    //    for " / "check " from the first chunk. We don't
    //    strip trailing prepositions / city markers — the
    //    orchestrator re-attaches the city per chunk, so
    //    the leftover "salons" in the first chunk is the
    //    canonical category. ("Find salons and gyms in
    //    Mumbai" → first chunk is "find salons" → strip
    //    "find " → "salons".)
    let mut out: Vec<String> = Vec::new();
    for (idx, p) in pieces.iter().enumerate() {
        let cleaned = if idx == 0 {
            strip_leading_intent_verb(p)
        } else {
            p.clone()
        };
        let cleaned = cleaned.trim().to_string();
        if !cleaned.is_empty() {
            out.push(cleaned);
        }
    }
    out
}

fn strip_leading_intent_verb(s: &str) -> String {
    const VERBS: &[&str] = &[
        "find me ",
        "find ",
        "show me ",
        "show ",
        "search for ",
        "search ",
        "check ",
        "scrape ",
    ];
    for v in VERBS {
        if let Some(rest) = s.strip_prefix(v) {
            return rest.to_string();
        }
    }
    s.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MapsFinding {
    /// 1-based rank in the order the agent visited the listing.
    pub rank: usize,
    pub name: String,
    pub phone: Option<String>,
    pub address: Option<String>,
    pub rating: Option<f32>,
    pub review_count: Option<u32>,
    /// `false` when the listing has no website link, `true` when
    /// one is present. Computed by the extract_fields tool; the
    /// agent should not guess.
    pub has_website: bool,
    pub website_url: Option<String>,
    pub category: Option<String>,
    /// The Maps panel URL (or canonical search result URL) for
    /// this listing. Always present so a downstream user can
    /// click through.
    pub maps_url: String,
}

impl MapsFinding {
    /// Apply the query's filters to a single finding. Returns
    /// `true` if the finding passes every filter the user
    /// expressed in the task.
    ///
    /// Semantics:
    ///   * `min_rating` set => `finding.rating` must be `>= q.min_rating`.
    ///     A finding with `rating = None` fails this filter (we
    ///     don't know the rating, so we can't say it qualifies).
    ///   * `require_no_website` set => `finding.has_website` must
    ///     be `false`.
    ///   * `require_phone` set => `finding.phone` must be `Some(_)`.
    pub fn is_qualified_lead(&self, query: &MapsQuery) -> bool {
        if let Some(min) = query.min_rating {
            match self.rating {
                Some(r) if r >= min => {}
                _ => return false,
            }
        }
        if query.require_no_website && self.has_website {
            return false;
        }
        if query.require_phone && self.phone.is_none() {
            return false;
        }
        true
    }

    /// Phase 37: merge another finding into `self`, keeping
    /// the more-complete version of every field. Used by
    /// `merge_maps_findings` when two parallel workers
    /// produce a duplicate (same business, scraped at slightly
    /// different moments — the two snapshots may have
    /// different field coverage). The "richer wins" rule is
    /// the spec's "keep the entry with more non-null fields
    /// on collision."
    pub fn merge_from(&mut self, other: MapsFinding) {
        if self.phone.is_none() && other.phone.is_some() {
            self.phone = other.phone;
        }
        if self.address.is_none() && other.address.is_some() {
            self.address = other.address;
        }
        if self.rating.is_none() && other.rating.is_some() {
            self.rating = other.rating;
        }
        if self.review_count.is_none() && other.review_count.is_some() {
            self.review_count = other.review_count;
        }
        if self.category.is_none() && other.category.is_some() {
            self.category = other.category;
        }
        // website_url: the truthier of the two. If
        // `other.has_website` is true and we don't have a
        // URL yet, take theirs; if `other.has_website` is
        // false and we already know we have one, leave
        // ours. The `has_website` flag flips true if either
        // side says we have a website.
        if !self.has_website && other.has_website {
            self.has_website = true;
        }
        if self.website_url.is_none() {
            self.website_url = other.website_url;
        }
        // The lower rank wins (the listing appeared earlier
        // in *some* worker's enumeration, so it's the more
        // authoritative rank).
        if other.rank < self.rank {
            self.rank = other.rank;
        }
    }

    /// Phase 37: count of non-null optional fields. Used
    /// by `merge_maps_findings` to decide which duplicate
    /// entry is the "richer" one. Excludes the always-set
    /// `name` and `maps_url` and the always-set `rank` and
    /// `has_website` (the latter is a bool, not optional).
    pub fn richness(&self) -> usize {
        let mut n = 0;
        if self.phone.is_some() {
            n += 1;
        }
        if self.address.is_some() {
            n += 1;
        }
        if self.rating.is_some() {
            n += 1;
        }
        if self.review_count.is_some() {
            n += 1;
        }
        if self.category.is_some() {
            n += 1;
        }
        if self.website_url.is_some() {
            n += 1;
        }
        n
    }

    /// Phase 37: dedup key for `merge_maps_findings`. The
    /// spec rule is "normalize (lowercase + strip
    /// whitespace) on (name, address)." When address is
    /// None we still produce a stable key (the name alone
    /// is enough to detect a re-visit of the same
    /// business), but two listings with the same name and
    /// no address are NOT collapsed (the spec's
    /// normalization rule still collapses them, but a
    /// same-name-no-address collision is a strong
    /// duplicate signal in practice — the maps_url is
    /// the tie-breaker).
    pub fn dedup_key(&self) -> (String, String) {
        (
            normalize_dedup(&self.name),
            normalize_dedup(self.address.as_deref().unwrap_or("")),
        )
    }
}

fn normalize_dedup(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_lowercase()
}

/// Phase 37: merge a list of `MapsFinding`s from parallel
/// workers into a single deduplicated, sorted list.
///
/// Merge strategy (Phase 37 spec line 177):
///   1. Collect every entry from every worker.
///   2. Dedup on `dedup_key()` (normalized name+address).
///      On collision, keep the entry with the higher
///      `richness()` score; on tie, keep the lower-rank
///      entry (the one that appeared earlier in its
///      worker's enumeration).
///   3. Re-sort by rating desc (a simple proxy for the
///      Phase 39 "is_qualified_lead score" — Phase 39
///      will swap in the real scorer).
///   4. Re-rank (1-based, contiguous).
pub fn merge_maps_findings(
    inputs: impl IntoIterator<Item = MapsFinding>,
) -> Vec<MapsFinding> {
    use std::collections::HashMap;
    let mut by_key: HashMap<(String, String), MapsFinding> = HashMap::new();

    for f in inputs {
        let key = f.dedup_key();
        match by_key.get_mut(&key) {
            Some(existing) => {
                let incoming_richness = f.richness();
                let existing_richness = existing.richness();
                if incoming_richness > existing_richness {
                    let prior = std::mem::replace(existing, f);
                    existing.merge_from(prior);
                } else {
                    existing.merge_from(f);
                }
            }
            None => {
                by_key.insert(key, f);
            }
        }
    }

    let mut out: Vec<MapsFinding> = by_key.into_values().collect();
    out.sort_by(|a, b| {
        // Rating desc, with `None` ratings sorted last so a
        // listing without a rating never beats one with a
        // rating. Ties broken by name asc for determinism.
        //
        // Comparator returns the order of `a` relative to
        // `b` (a < b → Less). We want Some-ratings first
        // and None-ratings last, so:
        //   * both Some: b.rating < a.rating → Less (a
        //     comes first, the higher rating).
        //   * a Some, b None: a comes first → Less.
        //   * a None, b Some: a comes last → Greater.
        //   * both None: tie-break by name asc.
        match (a.rating, b.rating) {
            (Some(ar), Some(br)) => br.partial_cmp(&ar).unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.name.cmp(&b.name),
        }
    });
    // Re-rank (1-based, contiguous) so the chat synthesizer
    // renders a clean table.
    for (idx, f) in out.iter_mut().enumerate() {
        f.rank = idx + 1;
    }
    out
}

/// Result of running a single `Todo` on the supervised worker.
/// Carries both the signature and the raw AX-tree text so the planner
/// can verify the evidence hash independently.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoResult {
    pub todo_id: TodoId,
    pub status: TodoStatus,
    pub last_snapshot_signature: String,
    pub last_obs_text: String,
    pub last_snapshot_iteration: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    /// Phase 13: distinct from `failure_reason`. When the supervisor
    /// sent a `Cancel` signal, the todo terminated without completing
    /// the work. Phase 14's `TauriSink` maps this to "Task stopped by
    /// user" instead of "Task failed."
    #[serde(default)]
    pub cancelled: bool,
    /// Phase 2: findings produced during execution of this todo item.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<ResearchFinding>,
    /// Phase 2: typed job findings produced during execution of this todo item.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub job_findings: Vec<JobFinding>,
    /// Phase 2: typed lead findings produced during execution of this todo item.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lead_findings: Vec<LeadFinding>,
}

impl TodoResult {
    pub fn success(
        todo_id: TodoId,
        last_snapshot_signature: impl Into<String>,
        last_obs_text: impl Into<String>,
        last_snapshot_iteration: usize,
    ) -> Self {
        Self {
            todo_id,
            status: TodoStatus::Done,
            last_snapshot_signature: last_snapshot_signature.into(),
            last_obs_text: last_obs_text.into(),
            last_snapshot_iteration,
            failure_reason: None,
            cancelled: false,
            findings: Vec::new(),
            job_findings: Vec::new(),
            lead_findings: Vec::new(),
        }
    }

    pub fn failure(
        todo_id: TodoId,
        reason: impl Into<String>,
        last_snapshot_signature: impl Into<String>,
        last_obs_text: impl Into<String>,
        last_snapshot_iteration: usize,
    ) -> Self {
        let reason_str = reason.into();
        Self {
            todo_id,
            status: TodoStatus::Failed {
                reason: reason_str.clone(),
            },
            last_snapshot_signature: last_snapshot_signature.into(),
            last_obs_text: last_obs_text.into(),
            last_snapshot_iteration,
            failure_reason: Some(reason_str),
            cancelled: false,
            findings: Vec::new(),
            job_findings: Vec::new(),
            lead_findings: Vec::new(),
        }
    }

    pub fn cancelled(
        todo_id: TodoId,
        last_snapshot_signature: impl Into<String>,
        last_obs_text: impl Into<String>,
        last_snapshot_iteration: usize,
    ) -> Self {
        Self {
            todo_id,
            status: TodoStatus::Failed {
                reason: "cancelled by supervisor".to_string(),
            },
            last_snapshot_signature: last_snapshot_signature.into(),
            last_obs_text: last_obs_text.into(),
            last_snapshot_iteration,
            failure_reason: None,
            cancelled: true,
            findings: Vec::new(),
            job_findings: Vec::new(),
            lead_findings: Vec::new(),
        }
    }

    pub fn with_findings(
        mut self,
        findings: Vec<ResearchFinding>,
        job_findings: Vec<JobFinding>,
        lead_findings: Vec<LeadFinding>,
    ) -> Self {
        self.findings = findings;
        self.job_findings = job_findings;
        self.lead_findings = lead_findings;
        self
    }
}

/// The thing `ChatAgent` hands to `BrowserAgent` when it decides the
/// user message is a browser task.
///
/// All fields are required except `research_plan` (Phase 7, optional
/// — present only when the deterministic `ResearchPlanner` recognized
/// the task as long-horizon research-shaped). The orchestrator fills
/// the required fields in from the classifier output + the
/// conversation context + the planner output. `BrowserAgent::run`
/// takes exactly this struct (and a `&Page`) — there is no longer a
/// free-floating `&str task` argument that loses every other piece of
/// context.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Handoff {
    /// The rephrased, standalone browser task description. The
    /// classifier fills this in (or the orchestrator re-uses the
    /// user message verbatim when `Intent::Chat` was not a candidate
    /// but the user *did* say "go to ..."). Always non-empty.
    pub task_description: String,
    /// The pre-flight decomposition's subtask list. The code
    /// (planner + `CompletenessTracker`) is the canonical source of
    /// truth; the browser agent's LLM is free to amend via
    /// `declare_subtasks` but cannot quietly skip declaration when
    /// this list is non-empty.
    pub subtasks: Vec<HandoffSubTask>,
    /// Hard constraints the browser agent must honor. The Phase 3
    /// orchestrator fills these from `mew_nav::SensitivePlatforms`
    /// (entry-strategy) and from session-level settings (time
    /// budget). The agent's `finish()` gate is *not* currently
    /// wired to these — that's a future hardening. For Phase 3 the
    /// constraints are advisory: the agent's system prompt mentions
    /// them, and a violation shows up in the transcript.
    pub constraints: Vec<String>,
    /// Identifier of the originating UI message. Format is
    /// `<session_id>:<unix_secs>` (matches the wall-clock seconds
    /// the `chat-reply` listener uses to order messages). Lets a
    /// follow-up `chat-reply` payload be matched back to the user
    /// turn that produced it, even after the orchestrator has
    /// reordered work in flight.
    pub originating_message_id: String,
    /// Phase 7: the typed research plan when the task is
    /// long-horizon research-shaped. `None` for non-research
    /// tasks. The `ResearchPlanner` produces this; the
    /// orchestrator copies it onto the Handoff so the
    /// `BrowserAgent` sees the plan verbatim in its system prompt
    /// and the budget guard can read the per-platform caps.
    /// The default is `None` so a pre-Phase-7 Handoff is
    /// bit-for-bit identical to the wire format before this
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub research_plan: Option<ResearchPlan>,
    /// Phase 19: when the user asks for a job/internship search, the
    /// classifier populates this structured query. The `ChatAgent`
    /// uses it to template `query_template` URLs on job boards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_query: Option<JobQuery>,
    /// Phase 20: when the user asks for SaaS lead generation, the
    /// classifier populates this structured query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead_query: Option<LeadQuery>,
    /// Phase 21: specific task shape determined by ChatAgent router classification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_shape: Option<TaskShape>,
    /// Phase 28: Distilled context transplanted from a prior failed worker attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub respawn_handoff: Option<RespawnHandoff>,
    /// Phase 34: deterministic Google-Maps search query, parsed
    /// from the user's task by `MapsQuery::parse`. Present iff
    /// `task_shape == Some(TaskShape::LocalLeadGen)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maps_query: Option<MapsQuery>,
}

/// A single subtask the browser agent must work through. Mirrors
/// the shape `planner::Plan::subtasks` already produces (and which
/// `CompletenessTracker::declare` consumes), so the orchestrator
/// can move the planner's output straight into the Handoff without
/// remapping. The `description` is the human-readable task text;
/// `id` is the slug the LLM uses with `mark_subtask_done`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandoffSubTask {
    pub id: String,
    pub description: String,
}

impl HandoffSubTask {
    pub fn new(id: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            description: description.into(),
        }
    }
}

impl Handoff {
    /// Build a Handoff from a task string alone (no pre-flight
    /// decomposition, no constraints, no research plan). Used by
    /// tests and by the orchestrator's "no plan available" fallback.
    pub fn bare(task: impl Into<String>, originating_message_id: impl Into<String>) -> Self {
        Self {
            task_description: task.into(),
            subtasks: Vec::new(),
            constraints: Vec::new(),
            originating_message_id: originating_message_id.into(),
            research_plan: None,
            job_query: None,
            lead_query: None,
            task_shape: None,
            respawn_handoff: None,
            maps_query: None,
        }
    }

    /// Phase 7: build a Handoff that carries a research plan. Used
    /// by the orchestrator's `ChatAgent::build_research_handoff`
    /// path when the deterministic `ResearchPlanner` recognized
    /// the task as long-horizon research-shaped.
    pub fn with_research_plan(
        task: impl Into<String>,
        originating_message_id: impl Into<String>,
        plan: ResearchPlan,
    ) -> Self {
        // Seed the subtask list from the research plan's
        // platforms so the `CompletenessTracker` has the
        // canonical list from the moment the agent is built.
        // The LLM is still free to amend via
        // `declare_subtasks`; the Phase 2 idempotency rule
        // holds because the platform ids are stable across
        // construction.
        let subtasks: Vec<HandoffSubTask> = plan
            .platforms
            .iter()
            .map(|p| HandoffSubTask {
                id: p.id.clone(),
                description: format!(
                    "[{}] {}{}",
                    p.platform,
                    if p.query.is_empty() {
                        String::new()
                    } else {
                        format!(" query='{}'", p.query)
                    },
                    if p.entry_hint.is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", p.entry_hint)
                    }
                ),
            })
            .collect();
        Self {
            task_description: task.into(),
            subtasks,
            constraints: Vec::new(),
            originating_message_id: originating_message_id.into(),
            research_plan: Some(plan),
            job_query: None, // Can be injected separately if needed
            lead_query: None,
            task_shape: None,
            respawn_handoff: None,
            maps_query: None,
        }
    }
}

/// Terminal status of a browser task. Mirrors the three states a
/// user actually sees in the chat: the agent finished cleanly, the
/// agent finished some subtasks but not all (or finished with a
/// failure flagged), or the agent could not run to completion (page
/// crash, LLM error, stop() called, etc.).
///
/// `Partial` is deliberately distinct from `Failed` because the
/// user-visible reply text differs: a `Failed` says "the task did
/// not run" while a `Partial` says "here is what got done." Phase
/// 3's `ChatAgent::synthesize_reply` templates differently per
/// status.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum BrowserStatus {
    /// All declared subtasks are Done (or, if no subtasks were
    /// declared, the agent called `finish()` with a non-empty
    /// payload).
    Done,
    /// At least one declared subtask is `Failed` or `Skipped` with
    /// a failure reason, or the agent called `finish()` with a
    /// "best-effort" answer after exhausting its iteration budget.
    /// The user-visible reply says "some of this worked, some
    /// didn't" and lists the gaps.
    Partial,
    /// The browser task could not complete at all. `failure_reason`
    /// on the parent `BrowserResult` carries the human-readable
    /// cause. The user-visible reply says "I couldn't do this" with
    /// the reason.
    Failed,
}

impl BrowserStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            BrowserStatus::Done => "done",
            BrowserStatus::Partial => "partial",
            BrowserStatus::Failed => "failed",
        }
    }
}

/// A single finding the browser agent produced. Mirrors
/// `completeness::SubTaskStatus` so the orchestrator can map
/// tracker state to the user-visible list without lossy
/// stringification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyFinding {
    pub id: String,
    pub description: String,
    /// `"done"`, `"skipped"`, `"failed"`, or `"pending"`. Mirrors
    /// `SubTaskStatus::as_str` so the synthesizer can keep its
    /// templating logic in plain string-land.
    pub status: String,
    /// Per-finding reason (set when `status` is `"skipped"` or
    /// `"failed"`). Empty for `"done"` / `"pending"`.
    pub reason: String,
    /// The page-state signature from the most recent snapshot
    /// recorded against this subtask. `None` when no snapshot
    /// evidence exists (e.g. the subtask never got attempted).
    pub evidence_signature: Option<String>,
}

/// What `BrowserAgent` hands back to `ChatAgent` at the end of a
/// session. The orchestrator treats this as the source of truth for
/// "what to show the user next" and never falls back to the raw LLM
/// `finish()` text — that string lives in the transcript (and
/// optionally the transcript-ref below) but the user-facing reply
/// is always synthesized from this struct.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BrowserResult {
    pub status: BrowserStatus,
    /// One-paragraph user-facing summary, derived from the LLM's
    /// `finish()` text by the browser agent's own post-process step
    /// (it strips "I clicked X. I typed Y. I called finish()" and
    /// keeps the final state). Used by `ChatAgent::synthesize_reply`
    /// as the substance of the user-visible chat message.
    pub summary: String,
    /// Per-subtask outcomes. Empty when the agent declared no
    /// subtasks (the common single-action case). The synthesizer
    /// uses this list to render "3 of 4 done" headers.
    pub key_findings: Vec<KeyFinding>,
    /// The page-state signature from the most recent snapshot the
    /// agent took. Mirrors `completeness::last_snapshot_signature`
    /// on success; `None` when the agent never reached the snapshot
    /// step (early failure).
    pub final_snapshot_signature: Option<String>,
    /// On-disk path of the session's transcript file. Format is
    /// relative-to-cwd when the agent used the default transcript
    /// dir; absolute when the Tauri UI passed an `app_data_dir()`
    /// path. Frontend "view transcript" affordance opens this
    /// file.
    pub raw_transcript_ref: Option<String>,
    /// Session id (matches `Agent::session_id`). Carried
    /// explicitly so the orchestrator can log a single
    /// `chat_reply_synthesized` trace event keyed by session
    /// without dereferencing state.
    pub session_id: String,
    /// On `Failed` (or sometimes `Partial`), the human-readable
    /// reason. Examples: "page crashed before snapshot," "LLM
    /// returned 429 after 8 iterations," "agent stopped by
    /// user." Empty on `Done` and on `Partial` with no failure
    /// reason.
    pub failure_reason: String,
    /// Phase 7: aggregated, deduplicated cross-platform findings
    /// for research-shaped tasks. Empty for non-research tasks
    /// (the synthesizer checks for this and falls back to the
    /// single-platform reply path). The synthesizer renders this
    /// list as the consolidated user-visible answer when present.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<ResearchFinding>,
    /// Phase 19: specifically tagged job findings (so we can render
    /// freshness/internship info). Empty if not a job search.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub job_findings: Vec<JobFinding>,
    /// Phase 20: specifically tagged lead findings (with company, contact, confidence).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lead_findings: Vec<LeadFinding>,
    /// Phase 29: number of respawns experienced during task execution.
    #[serde(default)]
    pub respawn_count: usize,
    /// Phase 29: reasons for respawns experienced during task execution.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub respawn_reasons: Vec<crate::supervisor::RespawnReason>,
    /// Phase 34: per-listing findings for a Google Maps
    /// local-lead-gen task. Empty for non-Maps tasks. The
    /// synthesizer filters this list through
    /// `MapsFinding::is_qualified_lead` to render the final
    /// ranked markdown table the user sees.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub maps_findings: Vec<MapsFinding>,
    /// Phase 5: freelance client lead findings. Empty for
    /// non-freelance tasks. The synthesizer renders a
    /// separate "Freelance leads" section in the consolidated
    /// chat reply when this list is non-empty, so a mixed
    /// task ("find me a job and find me clients") produces
    /// two clearly-separated sections rather than one
    /// blended list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub freelance_findings: Vec<FreelanceLead>,
}

impl BrowserResult {
    /// Build a `Failed` result with a reason. Used by the
    /// orchestrator's catch-all: any error path that does *not*
    /// reach the agent's normal return (e.g. Chrome failed to
    /// launch) still produces a typed `BrowserResult` so the user
    /// always sees a `chat-reply` and the synthesizer always has
    /// something to work with.
    pub fn failure(
        session_id: impl Into<String>,
        reason: impl Into<String>,
        raw_transcript_ref: Option<String>,
    ) -> Self {
        Self {
            status: BrowserStatus::Failed,
            summary: String::new(),
            key_findings: Vec::new(),
            final_snapshot_signature: None,
            raw_transcript_ref,
            session_id: session_id.into(),
            failure_reason: reason.into(),
            findings: Vec::new(),
            job_findings: Vec::new(),
            lead_findings: Vec::new(),
            respawn_count: 0,
            respawn_reasons: Vec::new(),
            maps_findings: Vec::new(),
            freelance_findings: Vec::new(),
        }
    }

    /// Convenience: a `Done` result with the agent's LLM
    /// `finish()` text. The browser agent's post-processing step
    /// is what calls this in the success path.
    pub fn done(
        session_id: impl Into<String>,
        summary: impl Into<String>,
        key_findings: Vec<KeyFinding>,
        final_snapshot_signature: Option<String>,
        raw_transcript_ref: Option<String>,
    ) -> Self {
        Self {
            status: BrowserStatus::Done,
            summary: summary.into(),
            key_findings,
            final_snapshot_signature,
            raw_transcript_ref,
            session_id: session_id.into(),
            failure_reason: String::new(),
            findings: Vec::new(),
            job_findings: Vec::new(),
            lead_findings: Vec::new(),
            respawn_count: 0,
            respawn_reasons: Vec::new(),
            maps_findings: Vec::new(),
            freelance_findings: Vec::new(),
        }
    }

    /// Phase 7: build a research-shaped `Done` result with the
    /// cross-platform `findings` list pre-populated. The
    /// synthesizer uses this list as the substance of the
    /// consolidated reply. The `key_findings` list is still
    /// populated (per-platform subtask outcomes) so the existing
    /// "N of M sub-tasks completed" footer still works.
    pub fn done_research(
        session_id: impl Into<String>,
        summary: impl Into<String>,
        key_findings: Vec<KeyFinding>,
        final_snapshot_signature: Option<String>,
        raw_transcript_ref: Option<String>,
        findings: Vec<ResearchFinding>,
    ) -> Self {
        Self {
            status: BrowserStatus::Done,
            summary: summary.into(),
            key_findings,
            final_snapshot_signature,
            raw_transcript_ref,
            session_id: session_id.into(),
            failure_reason: String::new(),
            findings,
            job_findings: Vec::new(),
            lead_findings: Vec::new(),
            respawn_count: 0,
            respawn_reasons: Vec::new(),
            maps_findings: Vec::new(),
            freelance_findings: Vec::new(),
        }
    }

    /// Phase 20: build a lead-research-shaped `Done` result with `lead_findings`.
    pub fn done_lead_research(
        session_id: impl Into<String>,
        summary: impl Into<String>,
        key_findings: Vec<KeyFinding>,
        final_snapshot_signature: Option<String>,
        raw_transcript_ref: Option<String>,
        findings: Vec<ResearchFinding>,
        lead_findings: Vec<LeadFinding>,
    ) -> Self {
        Self {
            status: BrowserStatus::Done,
            summary: summary.into(),
            key_findings,
            final_snapshot_signature,
            raw_transcript_ref,
            session_id: session_id.into(),
            failure_reason: String::new(),
            findings,
            job_findings: Vec::new(),
            lead_findings,
            respawn_count: 0,
            respawn_reasons: Vec::new(),
            maps_findings: Vec::new(),
            freelance_findings: Vec::new(),
        }
    }

    /// Phase 5: build a freelance-lead-shaped `Done` result with
    /// `freelance_findings`. The synthesizer renders a separate
    /// "Freelance leads" section in the consolidated reply when
    /// this list is non-empty. A `freelance_findings` Vec is
    /// returned in addition to the generic `findings` so the
    /// streaming / partial paths can still inspect the base
    /// shape; the synthesizer ignores the generic list when the
    /// typed list is non-empty.
    pub fn done_freelance(
        session_id: impl Into<String>,
        summary: impl Into<String>,
        key_findings: Vec<KeyFinding>,
        final_snapshot_signature: Option<String>,
        raw_transcript_ref: Option<String>,
        findings: Vec<ResearchFinding>,
        freelance_findings: Vec<FreelanceLead>,
    ) -> Self {
        Self {
            status: BrowserStatus::Done,
            summary: summary.into(),
            key_findings,
            final_snapshot_signature,
            raw_transcript_ref,
            session_id: session_id.into(),
            failure_reason: String::new(),
            findings,
            job_findings: Vec::new(),
            lead_findings: Vec::new(),
            respawn_count: 0,
            respawn_reasons: Vec::new(),
            maps_findings: Vec::new(),
            freelance_findings,
        }
    }

    /// Phase 29: Attach respawn count and reasons to the result.
    pub fn with_respawns(
        mut self,
        count: usize,
        reasons: Vec<crate::supervisor::RespawnReason>,
    ) -> Self {
        self.respawn_count = count;
        self.respawn_reasons = reasons;
        self
    }

    /// Phase 34: build a Maps-shaped `Done` result with the
    /// per-listing `maps_findings` pre-populated. The
    /// synthesizer uses this list as the substance of the
    /// final ranked markdown table. The `key_findings` list is
    /// still populated (per-listing subtask outcomes) so the
    /// existing "N of M sub-tasks completed" footer still
    /// works.
    pub fn done_maps(
        session_id: impl Into<String>,
        summary: impl Into<String>,
        key_findings: Vec<KeyFinding>,
        final_snapshot_signature: Option<String>,
        raw_transcript_ref: Option<String>,
        maps_findings: Vec<MapsFinding>,
    ) -> Self {
        Self {
            status: BrowserStatus::Done,
            summary: summary.into(),
            key_findings,
            final_snapshot_signature,
            raw_transcript_ref,
            session_id: session_id.into(),
            failure_reason: String::new(),
            findings: Vec::new(),
            job_findings: Vec::new(),
            lead_findings: Vec::new(),
            respawn_count: 0,
            respawn_reasons: Vec::new(),
            maps_findings,
            freelance_findings: Vec::new(),
        }
    }
}

/// mew v2 — Phase 28: Cross-Worker Handoff & Context Transplant on Respawn.
///
/// Distilled execution state transferred from a failed/confused worker process
/// to a fresh replacement worker on respawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RespawnHandoff {
    pub original_task_description: String,
    pub subtasks_completed: Vec<SubTask>,
    pub subtasks_remaining: Vec<SubTask>,
    pub last_known_good_snapshot_signature: Option<String>,
    pub respawn_reason: RespawnReason,
    pub failure_context: String,
}

impl RespawnHandoff {
    pub fn new(
        original_task_description: impl Into<String>,
        subtasks_completed: Vec<SubTask>,
        subtasks_remaining: Vec<SubTask>,
        last_known_good_snapshot_signature: Option<String>,
        respawn_reason: RespawnReason,
        failure_context: impl Into<String>,
    ) -> Self {
        Self {
            original_task_description: original_task_description.into(),
            subtasks_completed,
            subtasks_remaining,
            last_known_good_snapshot_signature,
            respawn_reason,
            failure_context: failure_context.into(),
        }
    }

    /// Renders the `RESUMING (from a previous attempt):` block for system prompt injection.
    pub fn render_resuming_block(&self) -> String {
        let mut out = String::new();
        out.push_str("RESUMING (from a previous attempt):\n");
        out.push_str(&format!(
            "- Reason for previous restart: {}\n",
            self.respawn_reason.as_str()
        ));
        out.push_str(&format!(
            "- Warning (what went wrong): {}\n",
            self.failure_context
        ));

        if !self.subtasks_completed.is_empty() {
            out.push_str("- Subtasks Already Completed (DO NOT RE-DO):\n");
            for st in &self.subtasks_completed {
                out.push_str(&format!("  - [x] {}: {}\n", st.id, st.description));
            }
        } else {
            out.push_str("- Subtasks Already Completed: (none)\n");
        }

        if !self.subtasks_remaining.is_empty() {
            out.push_str("- Remaining Subtasks (Active Plan):\n");
            for st in &self.subtasks_remaining {
                out.push_str(&format!("  - [ ] {}: {}\n", st.id, st.description));
            }
        } else {
            out.push_str("- Remaining Subtasks: (none)\n");
        }

        out
    }

    /// Converts this `RespawnHandoff` into an episodic `Lesson` for `mew_memory::write_lesson`.
    pub fn to_lesson(&self, session_id: &str) -> mew_memory::Lesson {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        mew_memory::Lesson::new(
            format!("les_respawn_{}", now_secs),
            now_secs,
            mew_memory::TaskShape::GenericBrowse,
            self.last_known_good_snapshot_signature.clone(),
            format!(
                "Worker respawn triggered due to {}",
                self.respawn_reason.as_str()
            ),
            mew_memory::Outcome::Failure,
            self.failure_context.clone(),
            session_id.to_string(),
        )
    }

    /// Formats a multi-attempt narrative summary across all respawn attempts for terminal failures.
    pub fn format_multi_attempt_summary(history: &[RespawnHandoff], final_reason: &str) -> String {
        if history.is_empty() {
            return final_reason.to_string();
        }

        let mut out = format!(
            "Task failed after {} respawn attempt(s). Narrative: ",
            history.len()
        );
        for (idx, handoff) in history.iter().enumerate() {
            if idx > 0 {
                out.push_str(", then ");
            }
            out.push_str(&format!(
                "Attempt {}: {} ({})",
                idx + 1,
                handoff.respawn_reason.as_str(),
                handoff.failure_context
            ));
        }
        out.push_str(&format!(". Final status: {}", final_reason));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_bare_has_empty_subtasks_and_constraints() {
        let h = Handoff::bare("go to wikipedia", "session_1:1700000000");
        assert_eq!(h.task_description, "go to wikipedia");
        assert_eq!(h.originating_message_id, "session_1:1700000000");
        assert!(h.subtasks.is_empty());
        assert!(h.constraints.is_empty());
    }

    #[test]
    fn handoff_subtask_new_sets_fields() {
        let s = HandoffSubTask::new("step-1", "open wikipedia");
        assert_eq!(s.id, "step-1");
        assert_eq!(s.description, "open wikipedia");
    }

    #[test]
    fn browser_status_as_str_is_stable() {
        // The frontend / transcript / tracing layer all key on
        // these strings. Changing one is a wire-format break.
        assert_eq!(BrowserStatus::Done.as_str(), "done");
        assert_eq!(BrowserStatus::Partial.as_str(), "partial");
        assert_eq!(BrowserStatus::Failed.as_str(), "failed");
    }

    #[test]
    fn browser_result_failure_carries_reason() {
        let r = BrowserResult::failure(
            "session_42",
            "Chrome failed to launch: ENOENT",
            Some("/tmp/transcript_session_42.log".to_string()),
        );
        assert_eq!(r.status, BrowserStatus::Failed);
        assert_eq!(r.failure_reason, "Chrome failed to launch: ENOENT");
        assert_eq!(r.session_id, "session_42");
        assert!(r.summary.is_empty());
        assert!(r.key_findings.is_empty());
        assert_eq!(
            r.raw_transcript_ref.as_deref(),
            Some("/tmp/transcript_session_42.log")
        );
    }

    #[test]
    fn browser_result_done_carries_findings_and_summary() {
        let r = BrowserResult::done(
            "session_7",
            "Message sent to Alice.",
            vec![KeyFinding {
                id: "step-1".into(),
                description: "navigate to instagram".into(),
                status: "done".into(),
                reason: String::new(),
                evidence_signature: Some("len:0123abcd".into()),
            }],
            Some("len:0123abcd".into()),
            Some("/tmp/transcript_session_7.log".to_string()),
        );
        assert_eq!(r.status, BrowserStatus::Done);
        assert_eq!(r.summary, "Message sent to Alice.");
        assert_eq!(r.key_findings.len(), 1);
        assert_eq!(r.key_findings[0].id, "step-1");
        assert_eq!(
            r.key_findings[0].evidence_signature.as_deref(),
            Some("len:0123abcd")
        );
        assert!(r.failure_reason.is_empty());
    }

    #[test]
    fn handoff_and_result_survive_json_round_trip() {
        // The wire format is JSON. The orchestrator stores these
        // in tracing events; future remote runtimes may carry them
        // over the network. Either path must round-trip.
        let h = Handoff {
            task_description: "go to gmail".into(),
            subtasks: vec![HandoffSubTask::new("step-1", "open gmail")],
            constraints: vec!["enter via search".into()],
            originating_message_id: "session_99:1700000000".into(),
            research_plan: None,
            job_query: None,
            lead_query: None,
            task_shape: None,
            respawn_handoff: None,
            maps_query: None,
        };
        let json = serde_json::to_string(&h).unwrap();
        let back: Handoff = serde_json::from_str(&json).unwrap();
        assert_eq!(h, back);

        let r = BrowserResult::done(
            "session_99",
            "Inbox opened.",
            vec![],
            Some("len:deadbeef".into()),
            None,
        );
        let json = serde_json::to_string(&r).unwrap();
        let back: BrowserResult = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }

    // ---- Phase 7: research fields on Handoff / BrowserResult ----

    #[test]
    fn handoff_bare_has_research_plan_none() {
        let h = Handoff::bare("x", "y");
        assert!(h.research_plan.is_none());
    }

    #[test]
    fn handoff_with_research_plan_seeds_subtasks_from_platforms() {
        let plan = ResearchPlan {
            goal: "find rust jobs".into(),
            platforms: vec![crate::research::ResearchSubTask {
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
            }],
            synthesis_hint: "One row per role".into(),
            overall_deadline_secs: Some(600),
            is_research: true,
            matched_pattern: "research_keyword".into(),
            max_concurrent_workers: None,
            max_concurrent_per_domain: None,
            stream_partial_findings: None,
        };
        let h = Handoff::with_research_plan("find rust jobs", "chat:1:0", plan.clone());
        assert!(h.research_plan.is_some());
        assert_eq!(h.subtasks.len(), 1);
        assert_eq!(h.subtasks[0].id, "linkedin");
        assert!(h.subtasks[0].description.contains("LinkedIn"));
        assert!(h.subtasks[0].description.contains("rust engineer"));
    }

    #[test]
    fn handoff_serialization_is_back_compatible_when_research_plan_is_none() {
        // Pre-Phase-7 callers serialize a Handoff with no
        // research_plan. The wire format must round-trip
        // without the field present. We assert by serializing
        // a `bare` handoff and re-reading it.
        let h = Handoff::bare("go to wikipedia", "session_1:0");
        let json = serde_json::to_string(&h).unwrap();
        let back: Handoff = serde_json::from_str(&json).unwrap();
        assert_eq!(h, back);
        // Sanity: the JSON does not contain "research_plan"
        // when the field is None (skip_serializing_if).
        assert!(
            !json.contains("research_plan"),
            "json leaked the optional field: {json}"
        );
    }

    #[test]
    fn browser_result_with_findings_round_trips() {
        let r = BrowserResult::done_research(
            "session_x",
            "Found 3 roles.",
            vec![],
            Some("len:abcd".into()),
            None,
            vec![ResearchFinding {
                id: "f1".into(),
                platform: "LinkedIn".into(),
                title: Some("Rust Engineer".into()),
                company: Some("Acme".into()),
                email: Some("a@b.com".into()),
                url: Some("https://linkedin.com/jobs/1".into()),
                note: String::new(),
                added_at_secs: 0,
            }],
        );
        assert_eq!(r.findings.len(), 1);
        let json = serde_json::to_string(&r).unwrap();
        let back: BrowserResult = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn browser_result_without_findings_serialization_omits_field() {
        // Back-compat: pre-Phase-7 callers serialize a
        // BrowserResult with no findings. The JSON should
        // not contain the "findings" key (the
        // `Vec::is_empty` skip rule kicks in for empty
        // vecs). We use a more specific search so we
        // don't accidentally match "key_findings" — the
        // substring "findings" appears in key_findings on
        // every result, so a `contains("findings")` check
        // is useless.
        let r = BrowserResult::done("session_x", "Done.", vec![], None, None);
        assert!(r.findings.is_empty());
        let json = serde_json::to_string(&r).unwrap();
        let back: BrowserResult = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
        assert!(
            !json.contains("\"findings\""),
            "empty findings should not appear in JSON: {json}"
        );
    }

    // ---- Phase 37: MapsQuery multi-category parsing + dedup/merge ----

    /// A canonical "salons and gyms and clinics in Mumbai"
    /// task should parse into 3 categories, parallelism=3,
    /// and each re-scoped category query should carry the
    /// city.
    #[test]
    fn maps_query_parse_three_categories() {
        let q = MapsQuery::parse(
            "find salons and gyms and clinics in Mumbai, 15 each",
        );
        assert_eq!(q.parallelism, 3, "three categories → parallelism=3, got {}", q.parallelism);
        assert_eq!(q.categories.len(), 3, "three categories, got {}", q.categories.len());
        // Each per-worker query should be a complete
        // "<category> in Mumbai" string. The parser
        // lower-cases for the city extraction step, so the
        // re-attached city is also lowercase — the worker
        // re-runs the city detection on the per-category
        // query if it needs the canonical case. We assert
        // case-insensitive here.
        for c in &q.categories {
            assert!(
                c.to_lowercase().contains("in mumbai"),
                "each category must carry the city: {c}"
            );
        }
        // The default cap is `min(category_count, 3)` so 3
        // categories → parallelism 3 (no further cap).
        assert_eq!(q.listing_count, 15, "listing_count must parse 15");
    }

    /// Two-category variant — parallel with cap=2.
    #[test]
    fn maps_query_parse_two_categories() {
        let q = MapsQuery::parse("salons and gyms in Mumbai");
        assert_eq!(q.parallelism, 2, "two categories → parallelism=2, got {}", q.parallelism);
        assert_eq!(q.categories.len(), 2);
    }

    /// Single-category task: parallelism=1, empty
    /// categories (the orchestrator's `parallelism > 1`
    /// guard prevents fan-out).
    #[test]
    fn maps_query_parse_single_category_stays_serial() {
        let q = MapsQuery::parse("salons in Mumbai with no website");
        assert_eq!(q.parallelism, 1);
        assert!(
            q.categories.is_empty(),
            "single-category must produce empty categories, got {:?}",
            q.categories
        );
    }

    /// A non-Maps task (no physical-business keyword, no
    /// "in <city>") should NOT fan out, even though it
    /// contains " and ".
    #[test]
    fn maps_query_parse_non_maps_does_not_fan_out() {
        let q = MapsQuery::parse("go to wikipedia and search for rust");
        // No city marker (" in " or " near " after a
        // physical-business keyword), so the parser
        // short-circuits.
        assert_eq!(q.parallelism, 1);
        assert!(q.categories.is_empty());
    }

    /// Comma-separated multi-category also fans out.
    #[test]
    fn maps_query_parse_comma_separated_multi_category() {
        let q = MapsQuery::parse("salons, gyms, clinics in Delhi");
        assert_eq!(q.parallelism, 3, "comma-separated should also fan out, got {}", q.parallelism);
        assert_eq!(q.categories.len(), 3);
    }

    /// `+ ` separator also fans out (the spec lists it).
    #[test]
    fn maps_query_parse_plus_separator() {
        let q = MapsQuery::parse("salons + gyms in Pune");
        assert_eq!(q.parallelism, 2);
        assert_eq!(q.categories.len(), 2);
    }

    /// Edge: 4 categories cap to 3 (spec rule
    /// `min(category_count, 3)`).
    #[test]
    fn maps_query_parse_four_caps_to_three() {
        let q = MapsQuery::parse(
            "salons and gyms and clinics and restaurants in Mumbai",
        );
        assert_eq!(q.categories.len(), 4);
        assert_eq!(q.parallelism, 3, "4 categories → cap=3, got {}", q.parallelism);
    }

    /// `dedup_key` normalizes whitespace and case.
    #[test]
    fn maps_finding_dedup_key_normalizes() {
        let a = MapsFinding {
            rank: 1,
            name: "Salon A".into(),
            phone: None,
            address: Some("Bandra West".into()),
            rating: None,
            review_count: None,
            has_website: false,
            website_url: None,
            category: None,
            maps_url: "https://maps/x".into(),
        };
        let b = MapsFinding {
            rank: 5,
            name: "  salon   a  ".into(),
            phone: Some("+91 98765 11111".into()),
            address: Some("BANDRA   west".into()),
            rating: Some(4.5),
            review_count: None,
            has_website: false,
            website_url: None,
            category: None,
            maps_url: "https://maps/x".into(),
        };
        assert_eq!(a.dedup_key(), b.dedup_key());
    }

    /// `merge_maps_findings` deduplicates on normalized
    /// (name, address) and keeps the richer entry.
    #[test]
    fn merge_maps_findings_dedups_and_keeps_richer() {
        let a = MapsFinding {
            rank: 1,
            name: "Salon A".into(),
            phone: None,
            address: Some("Bandra West".into()),
            rating: None,
            review_count: None,
            has_website: false,
            website_url: None,
            category: None,
            maps_url: "https://maps/a".into(),
        };
        // Same business, fuller fields.
        let b = MapsFinding {
            rank: 3,
            name: "Salon A".into(),
            phone: Some("+91 1".into()),
            address: Some("Bandra West".into()),
            rating: Some(4.5),
            review_count: Some(120),
            has_website: false,
            website_url: None,
            category: Some("Beauty".into()),
            maps_url: "https://maps/a".into(),
        };
        // Different business.
        let c = MapsFinding {
            rank: 2,
            name: "Salon B".into(),
            phone: Some("+91 2".into()),
            address: Some("Andheri".into()),
            rating: Some(4.2),
            review_count: Some(80),
            has_website: false,
            website_url: None,
            category: None,
            maps_url: "https://maps/b".into(),
        };
        let merged = merge_maps_findings(vec![a, b, c]);
        // Two unique businesses, not three.
        assert_eq!(merged.len(), 2);
        // The richer of (a, b) wins — has phone, rating,
        // review_count, category.
        let keep = merged.iter().find(|f| f.name == "Salon A").unwrap();
        assert_eq!(keep.phone.as_deref(), Some("+91 1"));
        assert_eq!(keep.rating, Some(4.5));
        assert_eq!(keep.review_count, Some(120));
        assert_eq!(keep.category.as_deref(), Some("Beauty"));
    }

    /// Merge result is sorted rating-desc with `None`
    /// ratings last.
    #[test]
    fn merge_maps_findings_sorts_rating_desc() {
        let make = |name: &str, rating: Option<f32>| MapsFinding {
            rank: 0,
            name: name.into(),
            phone: None,
            address: Some("addr".into()),
            rating,
            review_count: None,
            has_website: false,
            website_url: None,
            category: None,
            maps_url: "https://x".into(),
        };
        let merged = merge_maps_findings(vec![
            make("Low", Some(3.5)),
            make("None", None),
            make("High", Some(4.8)),
            make("Mid", Some(4.0)),
        ]);
        // Ratings in desc order, None last.
        assert_eq!(merged[0].name, "High");
        assert_eq!(merged[1].name, "Mid");
        assert_eq!(merged[2].name, "Low");
        assert_eq!(merged[3].name, "None");
        // Re-ranked 1..4.
        assert_eq!(merged[0].rank, 1);
        assert_eq!(merged[3].rank, 4);
    }

    /// `richness` counts optional fields.
    #[test]
    fn maps_finding_richness_counts_optional_fields() {
        let mut f = MapsFinding {
            rank: 0,
            name: "X".into(),
            phone: None,
            address: None,
            rating: None,
            review_count: None,
            has_website: false,
            website_url: None,
            category: None,
            maps_url: "x".into(),
        };
        assert_eq!(f.richness(), 0);
        f.phone = Some("p".into());
        f.rating = Some(4.0);
        f.category = Some("c".into());
        assert_eq!(f.richness(), 3);
    }
}
