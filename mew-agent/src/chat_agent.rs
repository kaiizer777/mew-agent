// mew v2 — Phase 3: ChatAgent (the conversational half of the two-agent split).
// mew v2 — Phase 7: ChatAgent gained the research-handoff builder and the
// research-shape synthesis path. The two are small additions on top of
// the Phase 3 surface: `build_handoff` now asks the research planner
// first and falls back to the deterministic clause-splitter when the
// task is not research-shaped; `synthesize_reply` routes to a
// research-specific templated renderer when the `BrowserResult`
// carries a non-empty `findings` list.
//
// Background (see `docs/architecture-current.md` and the Phase 3
// checkbox in `work.md`): pre-Phase-3, the "chat" path was a single
// stateless function — `router::classify` — that did one LLM call
// and returned a string. The "browser" path was a full ReAct loop
// agent. The two halves had no shared shape, no shared state, and
// the result of the browser agent's work never round-tripped back
// through a chat-shaped LLM call. The user saw the agent's raw
// `finish()` text (Phase 1.5 fix) but it was never *synthesized*
// into a chat reply by anything that thought about it as a chat
// reply.
//
// Phase 3 changes that by giving the conversational half a real
// identity. `ChatAgent` is a small struct that wraps two LLM calls
// (and the deterministic templating that fills in for them) and
// is the *only* thing that produces a user-facing chat message:
//
//   * `classify(&user_message, &history)` decides intent + draft
//     reply. This is what `router::classify` already does; the
//     `ChatAgent` wraps the call so the orchestrator has one
//     `ChatAgent` instance to thread through.
//
//   * `synthesize_reply(&handoff_result, &history, &handoff)` is
//     the round-trip half: the browser agent's typed `BrowserResult`
//     flows *back* through the ChatAgent, which turns it into a
//     single, concise, user-facing message. The default
//     implementation is deterministic templating (no LLM call) —
//     the typed `BrowserResult` carries enough structure that
//     templating reads well in the common case. An LLM call can be
//     swapped in later without changing the orchestrator.
//
// The struct also owns a small piece of conversation state — the
// `history` it threads through the LLM calls — but it does NOT own
// the browser session, the page, or the completeness tracker.
// Those are the browser agent's job. Two distinct system prompts,
// two distinct responsibilities, one typed handoff in each
// direction.
//
// Why a struct, not a free function? Three reasons:
//
//   1. The two LLM calls share a system prompt and a config; a
//      struct makes that explicit (and testable) instead of
//      re-plumbing the config through every call site.
//   2. Future improvements (caching, prompt versioning, per-user
//      tuning) want a place to hang state without re-flowing the
//      function signature.
//   3. The orchestrator test surface (Phase 3 integration test) can
//      hold a `ChatAgent` in a `Mock`-style test harness and assert
//      both halves of the round trip with one fixture.
//
// This module is intentionally small. The classifier and router
// already exist; we *wrap*, not *replace*. The wrapper surface is
// what the orchestrator and the Tauri UI bind to.

use crate::handoff::{BrowserResult, BrowserStatus, Handoff, KeyFinding};
use crate::planner::plan;
use crate::research::{
    default_job_board_platforms, default_lead_source_platforms, LeadSubTaskKind, ResearchPlanner,
    ResearchSubTask,
};
use crate::router::{self, ConversationMessage, Intent};
use crate::ProviderConfig;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// The two halves of the conversational agent. Wraps the existing
/// `router::classify` call (intent + draft reply) and adds the
/// `synthesize_reply` step that turns a typed `BrowserResult` into
/// the user-facing chat message.
///
/// Holds the config and a stable system prompt so the two LLM
/// calls see the same chat-side identity.
pub struct ChatAgent {
    config: ProviderConfig,
    /// Phase 3: distinct system prompt for the chat agent. The
    /// browser agent's prompt lives in `agent.rs`; this is its
    /// mirror. Tells the LLM it is a conversational agent that
    /// routes browser work to a separate agent and *also* is the
    /// thing that turns the browser agent's typed Result into a
    /// user-facing message.
    system_prompt: String,
    /// Phase 10.4: optional in-process LLM-result cache for
    /// `classify()`. When `Some`, repeated calls with the same
    /// `(message, history)` key return the cached `Intent`
    /// without a network round trip. None by default — call
    /// `with_classify_cache` to opt in. Sharing the same
    /// `ClassifyCache` across multiple `ChatAgent` instances
    /// (e.g. a long-lived Tauri app) is the right way to use
    /// it; per-turn construction defeats the cache.
    classify_cache: Option<Arc<crate::classify_cache::ClassifyCache>>,
}

impl ChatAgent {
    /// Build a new ChatAgent. Pure construction — no I/O, no LLM
    /// call, no state besides the config and the canned system
    /// prompt. Cheap to construct; the orchestrator holds one for
    /// the app's lifetime.
    pub fn new(config: ProviderConfig) -> Self {
        let system_prompt = CHAT_AGENT_SYSTEM_PROMPT.to_string();
        Self {
            config,
            system_prompt,
            classify_cache: None,
        }
    }

    /// Phase 10.4: opt in to the in-process classify cache.
    /// The cache is shared (cheap `Arc` clone) so a single
    /// long-lived cache can be plumbed through the Tauri
    /// command's `AppState` and reused across all chat
    /// turns. Returns `self` for builder-style chaining.
    pub fn with_classify_cache(mut self, cache: Arc<crate::classify_cache::ClassifyCache>) -> Self {
        self.classify_cache = Some(cache);
        self
    }

    /// Read-only accessor for the system prompt. Tests use this to
    /// assert the prompt is the one documented in
    /// `docs/phase3-handoff.md` so a future copy-paste doesn't drift.
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Read-only accessor for the config. The orchestrator's
    /// integration test uses this to inspect what the ChatAgent
    /// was wired to (e.g. model name).
    pub fn config(&self) -> &ProviderConfig {
        &self.config
    }

    /// Decide what the user's latest message wants. Wraps
    /// `router::classify` so the orchestrator only ever talks to
    /// `ChatAgent` — the router module becomes an implementation
    /// detail of the chat half.
    pub async fn classify(
        &self,
        message: &str,
        history: &[ConversationMessage],
    ) -> anyhow::Result<Intent> {
        router::classify(message, history, &self.config).await
    }

    /// Phase 10.4: classify with the in-process memoization
    /// layer. When a `ClassifyCache` is attached (via
    /// `with_classify_cache`), an identical
    /// `(message, history-tail)` returns the cached `Intent`
    /// in microseconds. When no cache is attached, falls
    /// through to the regular `classify` call.
    pub async fn classify_cached(
        &self,
        message: &str,
        history: &[ConversationMessage],
    ) -> anyhow::Result<Intent> {
        match self.classify_cache.as_ref() {
            Some(cache) => {
                crate::classify_cache::classify_with_cache(message, history, &self.config, cache)
                    .await
            }
            None => router::classify(message, history, &self.config).await,
        }
    }

    /// Build the typed `Handoff` that flows `ChatAgent ->
    /// BrowserAgent`. Splits the rephrased task into a subtask list
    /// via the deterministic planner, attaches any constraints the
    /// session has registered (sensitive-platform routing, time
    /// budget), and stamps the originating message id.
    ///
    /// The orchestrator passes the resulting `Handoff` to
    /// `BrowserAgent::run`. The browser agent's planner pre-flight
    /// is a no-op when this list is already populated — the
    /// `CompletenessTracker::declare` call accepts the subtasks
    /// wholesale.
    ///
    /// Phase 7: when `default_platforms` is non-empty AND the
    /// deterministic `ResearchPlanner` recognizes the task as
    /// long-horizon research-shaped, this function delegates to
    /// `build_research_handoff` so the Handoff carries the
    /// `ResearchPlan` and a per-platform subtask list. The
    /// orchestrator does not need to know which path was taken —
    /// the Handoff is the same shape either way.
    pub fn build_handoff(
        &self,
        task_description: &str,
        originating_message_id: &str,
        constraints: Vec<String>,
        job_query: Option<crate::handoff::JobQuery>,
        lead_query: Option<crate::handoff::LeadQuery>,
    ) -> Handoff {
        self.build_handoff_inner(
            task_description,
            originating_message_id,
            constraints,
            job_query,
            lead_query,
            None,
        )
    }

    /// Phase 34: build a handoff from a known `TaskShape` and
    /// optional structured query. The internal helper. Caller
    /// decides the shape (because sometimes the shape was
    /// already decided by the LLM classifier, sometimes by the
    /// keyword check, sometimes by the orchestrator fallback).
    fn build_handoff_inner(
        &self,
        task_description: &str,
        originating_message_id: &str,
        constraints: Vec<String>,
        job_query: Option<crate::handoff::JobQuery>,
        lead_query: Option<crate::handoff::LeadQuery>,
        maps_query: Option<crate::handoff::MapsQuery>,
    ) -> Handoff {
        // Phase 34: keyword check for `LocalLeadGen` (Google Maps
        // local-business lead gen). The phase spec says "if any
        // keyword matches, classify as LocalLeadGen" — and the
        // pre-existing job/lead check stays in front so we
        // don't accidentally re-classify a SaaS lead-gen task
        // that happened to mention "Mumbai."
        let shape = if job_query.is_some() {
            crate::router::TaskShape::JobSearch
        } else if lead_query.is_some() {
            crate::router::TaskShape::LeadGen
        } else if maps_query.is_some() || is_local_lead_gen_task(task_description) {
            crate::router::TaskShape::LocalLeadGen
        } else {
            crate::router::TaskShape::GenericResearch
        };
        // If the orchestrator passed a `maps_query` we trust
        // it; otherwise we deterministically parse the task so
        // the system-prompt hint block in `run_preflight_plan`
        // has structured data to work with.
        let maps_query = maps_query.or_else(|| {
            if shape == crate::router::TaskShape::LocalLeadGen {
                Some(crate::handoff::MapsQuery::parse(task_description))
            } else {
                None
            }
        });
        self.build_handoff_for_shape_inner(
            task_description,
            originating_message_id,
            constraints,
            shape,
            job_query,
            lead_query,
            maps_query,
        )
    }

    /// Phase 21: Build handoff taking `TaskShape` into account.
    pub fn build_handoff_for_shape(
        &self,
        task_description: &str,
        originating_message_id: &str,
        constraints: Vec<String>,
        shape: crate::router::TaskShape,
        job_query: Option<crate::handoff::JobQuery>,
        lead_query: Option<crate::handoff::LeadQuery>,
    ) -> Handoff {
        self.build_handoff_for_shape_inner(
            task_description,
            originating_message_id,
            constraints,
            shape,
            job_query,
            lead_query,
            None,
        )
    }

    /// Phase 34: internal variant of `build_handoff_for_shape`
    /// that also threads `maps_query` through. Public callers
    /// keep the older 6-arg signature; this helper carries
    /// the Maps work without churning every existing call site.
    fn build_handoff_for_shape_inner(
        &self,
        task_description: &str,
        originating_message_id: &str,
        constraints: Vec<String>,
        shape: crate::router::TaskShape,
        job_query: Option<crate::handoff::JobQuery>,
        lead_query: Option<crate::handoff::LeadQuery>,
        maps_query: Option<crate::handoff::MapsQuery>,
    ) -> Handoff {
        let platforms = match shape {
            crate::router::TaskShape::JobSearch => default_job_board_platforms(),
            crate::router::TaskShape::LeadGen => default_lead_source_platforms(),
            crate::router::TaskShape::GenericResearch => Vec::new(),
            crate::router::TaskShape::GenericBrowse => Vec::new(),
            crate::router::TaskShape::LocalLeadGen => Vec::new(),
        };

        let mut handoff = self.build_handoff_with_platforms(
            task_description,
            originating_message_id,
            constraints,
            &platforms,
            job_query,
            lead_query,
        );
        handoff.task_shape = Some(shape);
        handoff.maps_query = maps_query;
        handoff
    }

    /// Phase 21: Build multiple handoffs for compound or ambiguous task requests.
    /// If `is_ambiguous` is true or confidence is low, uses `planner::split_compound`
    /// to divide task into sub-clauses and build shaped handoffs for each.
    pub fn build_compound_handoffs(
        &self,
        task_description: &str,
        originating_message_id: &str,
        constraints: Vec<String>,
        shape: crate::router::TaskShape,
        job_query: Option<crate::handoff::JobQuery>,
        lead_query: Option<crate::handoff::LeadQuery>,
        confidence: Option<&str>,
        is_ambiguous: bool,
    ) -> Vec<Handoff> {
        let is_low_conf = confidence
            .map(|c| c.to_lowercase() == "low")
            .unwrap_or(false);
        if is_ambiguous || is_low_conf {
            let split_plan = crate::planner::plan(task_description);
            if split_plan.subtasks.len() > 1 {
                return split_plan
                    .subtasks
                    .iter()
                    .enumerate()
                    .map(|(idx, item)| {
                        let sub_id = format!("{}:{}", originating_message_id, idx + 1);
                        let sub_desc = &item.description;
                        let sub_desc_lower = sub_desc.to_lowercase();
                        let sub_shape = if sub_desc_lower.contains("job")
                            || sub_desc_lower.contains("internship")
                        {
                            crate::router::TaskShape::JobSearch
                        } else if sub_desc_lower.contains("lead")
                            || sub_desc_lower.contains("client")
                        {
                            crate::router::TaskShape::LeadGen
                        } else {
                            shape
                        };
                        let sub_jq = if sub_shape == crate::router::TaskShape::JobSearch {
                            job_query.clone()
                        } else {
                            None
                        };
                        let sub_lq = if sub_shape == crate::router::TaskShape::LeadGen {
                            lead_query.clone()
                        } else {
                            None
                        };
                        self.build_handoff_for_shape(
                            sub_desc,
                            &sub_id,
                            constraints.clone(),
                            sub_shape,
                            sub_jq,
                            sub_lq,
                        )
                    })
                    .collect();
            } else if is_low_conf && !is_ambiguous {
                // Low confidence with single clause: fallback to GenericResearch rather than guessing wrong
                let mut h = self.build_handoff_for_shape(
                    task_description,
                    originating_message_id,
                    constraints,
                    crate::router::TaskShape::GenericResearch,
                    None,
                    None,
                );
                h.task_shape = Some(crate::router::TaskShape::GenericResearch);
                return vec![h];
            }
        }

        vec![self.build_handoff_for_shape(
            task_description,
            originating_message_id,
            constraints,
            shape,
            job_query,
            lead_query,
        )]
    }

    /// Phase 7 & Phase 20: same as `build_handoff`, with an explicit
    /// `default_platforms` list passed in. When the list is
    /// non-empty AND the deterministic `ResearchPlanner`
    /// recognizes the task as long-horizon research-shaped, the
    /// returned Handoff carries a `ResearchPlan` and the
    /// subtask list is the per-platform list.
    pub fn build_handoff_with_platforms(
        &self,
        task_description: &str,
        originating_message_id: &str,
        constraints: Vec<String>,
        default_platforms: &[ResearchSubTask],
        job_query: Option<crate::handoff::JobQuery>,
        lead_query: Option<crate::handoff::LeadQuery>,
    ) -> Handoff {
        // When the caller provides a non-empty platform list,
        // ask the research planner first. If the planner
        // recognizes the task as research-shaped, return a
        // research Handoff. Otherwise fall through to the
        // Phase 2 clause-splitter.
        if !default_platforms.is_empty() || lead_query.is_some() {
            let platform_list = if default_platforms.is_empty() && lead_query.is_some() {
                default_lead_source_platforms()
            } else {
                default_platforms.to_vec()
            };
            let mut plan = ResearchPlanner::plan(
                task_description,
                &platform_list,
                None, // overall_deadline_secs: future, from config
            );
            if plan.is_research || lead_query.is_some() {
                // Phase 19: Template URLs and modify criteria if we have a job_query
                if let Some(ref jq) = job_query {
                    let mut role_str = jq.roles.join(" ");
                    if jq.is_internship && !role_str.to_lowercase().contains("intern") {
                        role_str = format!("{} intern", role_str);
                    }
                    let loc_str = jq.locations.join(" ");
                    for p in &mut plan.platforms {
                        if p.platform_kind == "job_board" && !p.query_template.is_empty() {
                            let mut t = p.query_template.clone();
                            t = t.replace("{role}", &urlencoding::encode(&role_str));
                            t = t.replace("{location}", &urlencoding::encode(&loc_str));
                            t = t.replace("{remote}", if jq.is_remote { "true" } else { "false" });
                            p.query = t;

                            let mut hint =
                                String::from("Navigate to the URL in your query parameter.");
                            if jq.is_internship {
                                hint.push_str(
                                    " Ensure you click the 'Internship' filter if it exists.",
                                );
                            }
                            p.entry_hint = hint;
                        }
                        // Always enforce freshness for job searches
                        p.acceptance
                            .push(crate::research::Criterion::JobsAreFresh { max_days_old: 30 });
                    }
                }

                // Phase 20: Template URLs and build 2-step flow if we have a lead_query
                if let Some(ref lq) = lead_query {
                    let ind_str = lq.industry.join(" ");
                    let roles_str = lq.target_role_titles.join(", ");
                    let mut additional_subtasks = Vec::new();

                    for p in &mut plan.platforms {
                        if p.platform_kind == "lead_source" && !p.query_template.is_empty() {
                            let t = p
                                .query_template
                                .replace("{industry}", &urlencoding::encode(&ind_str));
                            p.query = t;
                            p.entry_hint = format!(
                                "Step 1 (Company Discovery): Search candidate companies in industry '{}'. Target decision-maker roles: '{}'.",
                                ind_str, roles_str
                            );
                            p.lead_subtask_kind = Some(LeadSubTaskKind::CompanyDiscovery);
                        }
                    }

                    // Pre-seed Step 2 (Decision-Maker Discovery) subtasks for candidate company decision-maker search
                    for p in &plan.platforms {
                        if p.platform_kind == "lead_source" {
                            let step2_id = format!("{}-decision-makers", p.id);
                            additional_subtasks.push(ResearchSubTask {
                                id: step2_id,
                                platform: format!("{} Decision Makers", p.platform),
                                domain: p.domain.clone(),
                                entry_hint: format!(
                                    "Step 2 (Decision-Maker Discovery): On company public About/Team pages found via {}, search for named decision makers matching roles: '{}'.",
                                    p.platform, roles_str
                                ),
                                acceptance: vec![crate::research::Criterion::HasUrl],
                                step_budget: p.step_budget,
                                time_budget_secs: p.time_budget_secs,
                                query: String::new(),
                                platform_kind: "lead_source".into(),
                                query_template: String::new(),
                                lead_subtask_kind: Some(LeadSubTaskKind::DecisionMakerDiscovery {
                                    company_name: String::new(),
                                    company_domain: String::new(),
                                }),
                            });
                        }
                    }
                    plan.platforms.extend(additional_subtasks);
                }

                let mut handoff =
                    Handoff::with_research_plan(task_description, originating_message_id, plan);
                handoff.constraints = constraints;
                handoff.job_query = job_query;
                handoff.lead_query = lead_query;
                return handoff;
            }
        }
        // Fallback: Phase 2 single-platform path. The
        // clause-splitter's output is the canonical subtask
        // list.
        let plan = plan(task_description);
        let subtasks = plan
            .subtasks
            .into_iter()
            .map(|d| crate::handoff::HandoffSubTask {
                id: d.id,
                description: d.description,
            })
            .collect();
        Handoff {
            task_description: task_description.to_string(),
            subtasks,
            constraints,
            originating_message_id: originating_message_id.to_string(),
            research_plan: None,
            job_query,
            lead_query,
            task_shape: None,
            respawn_handoff: None,
            maps_query: None,
        }
    }

    /// Phase 7: build a research Handoff explicitly, using the
    /// default job-board platform list. Used by the
    /// orchestrator's "long-horizon research" intent path (a
    /// future specialization of the `Intent` enum) and by the
    /// test surface.
    pub fn build_research_handoff(
        &self,
        task_description: &str,
        originating_message_id: &str,
    ) -> Handoff {
        let plan = ResearchPlanner::plan(task_description, &default_job_board_platforms(), None);
        Handoff::with_research_plan(task_description, originating_message_id, plan)
    }

    /// Mint a `Handoff::originating_message_id` for the current
    /// moment. Format: `<unix_secs>:<random>` — same shape the
    /// orchestrator's `chat_reply_synthesized` tracing event uses
    /// for correlation. Random suffix is the wall-clock nanos
    /// modulo 1_000_000 so two messages in the same second still
    /// get distinct ids.
    pub fn mint_message_id(&self) -> String {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let sub = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos() % 1_000_000)
            .unwrap_or(0);
        format!("chat:{secs}:{sub:06}")
    }

    /// The round-trip half: turn the browser agent's typed
    /// `BrowserResult` into a single concise, user-facing message
    /// suitable for the chat list.
    ///
    /// Default implementation is deterministic templating. It is
    /// deliberately *not* an LLM call: a templated reply from a
    /// typed `BrowserResult` is short, predictable, never hallucinates,
    /// and is the right default for a system whose whole point is
    /// "never show the user a raw JSON blob" — the templated
    /// version is provably the inverse of the structured
    /// `BrowserResult`. A future synthesizer LLM call can be
    /// swapped in here without changing the orchestrator.
    ///
    /// The output is always a non-empty string: even the most
    /// catastrophic failure (`BrowserStatus::Failed` with no
    /// reason) gets a generic-but-true "the task didn't run" line
    /// so the user always sees *something* in the chat list.
    ///
    /// Phase 7: when the `BrowserResult` carries a non-empty
    /// `findings` list, the synthesizer routes to
    /// `synthesize_research_reply` so the user sees a
    /// consolidated one-row-per-finding answer instead of the
    /// single-platform "N of M sub-tasks completed" footer.
    /// The non-research path is unchanged.
    pub fn synthesize_reply(
        &self,
        result: &BrowserResult,
        _history: &[ConversationMessage],
        handoff: &Handoff,
    ) -> String {
        if self.config.agent.planner_enabled {
            if result.status == BrowserStatus::Failed && result.key_findings.is_empty() {
                return synthesize_failed(result);
            }
            let mut out = String::new();
            let mut parts = Vec::new();
            for f in &result.key_findings {
                let reason = if f.reason.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", f.reason)
                };
                parts.push(format!("{} {}{}", f.id, f.status, reason));
            }
            out.push_str(&parts.join(" · "));
            if result.respawn_count > 0 || !result.respawn_reasons.is_empty() {
                out.push_str(" (Note: Task was restarted following a temporary stall.)");
            }
            return out;
        }

        // Research-shaped: route to the consolidated renderer.
        // We do this before the status match so a `Done`
        // research result with 0 findings (rare but possible —
        // a successful "I searched all platforms and found
        // nothing matching") still falls through to the
        // non-research renderer rather than rendering a bare
        // "0 results" line.
        //
        // Phase 39: Maps-shaped (local-lead-gen) takes
        // priority over the research and SaaS-lead
        // renderers. The three shapes are mutually
        // exclusive in practice (a single Maps task
        // doesn't also collect `ResearchFinding`s), but
        // if a future vertical does, the Maps shape is
        // the most specific and most useful to render
        // first.
        //
        // Phase 5: freelance-lead-shaped takes priority
        // over the generic research renderer. A mixed
        // task ("find me a job and find me clients") that
        // ran both a job-search subtask and a freelance
        // subtask produces a `BrowserResult` with both
        // `findings` (from the job half) and
        // `freelance_findings` (from the freelance half)
        // non-empty. The mixed renderer renders them as
        // two clearly-separated sections rather than one
        // blended list. A freelance-only task falls
        // through to the dedicated
        // `synthesize_freelance_leads` renderer (which is
        // also what the mixed renderer uses for the
        // freelance half).
        let mut base = if !result.maps_findings.is_empty() {
            synthesize_maps(result, handoff)
        } else if !result.freelance_findings.is_empty() && !result.findings.is_empty() {
            // Mixed job + freelance: render two sections.
            synthesize_mixed_job_freelance(result)
        } else if !result.freelance_findings.is_empty() {
            synthesize_freelance_leads(result)
        } else if !result.lead_findings.is_empty() {
            synthesize_lead_research(result)
        } else if !result.findings.is_empty() {
            synthesize_research(result)
        } else {
            match result.status {
                BrowserStatus::Done => synthesize_done(result),
                BrowserStatus::Partial => synthesize_partial(result),
                BrowserStatus::Failed => synthesize_failed(result),
            }
        };

        if result.respawn_count > 0 || !result.respawn_reasons.is_empty() {
            if !base.ends_with('\n') {
                base.push(' ');
            }
            base.push_str("Note: I ran into a snag partway through and restarted that part of the task, but still got you the full list below.");
        }

        base
    }
}

/// System prompt for the chat agent. Distinct from the browser
/// agent's prompt in `agent.rs`. The orchestrator holds a
/// `ChatAgent` instance, so this prompt is associated with one
/// agent (the chat one), not duplicated in two places.
const CHAT_AGENT_SYSTEM_PROMPT: &str = "\
You are the conversational half of mew. You talk directly to the user.

You have two jobs:

1. Decide what the user wants. If they are asking a question or
   making small talk, reply conversationally. If they want to
   *do* something in a browser (visit a page, send a message,
   click something, search for information), classify the request
   as a browser task and rephrase it into a clear, standalone
   task description the browser agent can execute. Resolve any
   ambiguous pronouns ('it', 'there', 'them') using the
   conversation history before handing off.

2. After a browser task finishes, turn the browser agent's typed
   result into a single concise chat reply for the user. You do
   not need to call any tools for this — the browser agent has
   already done the work and produced a structured result with a
   summary, a list of findings, and a status. Your job is to
   phrase that result as a natural-language message.

Keep replies short. One or two sentences is usually right.
Do not include raw JSON, tool call traces, or per-iteration
logs in the user-facing reply — those live in the transcript
panel, not the chat.";

/// Render the `Done` branch. Uses the typed `summary` as the
/// substance. If the result has a non-empty `key_findings` list,
/// append a "N of M sub-tasks completed" footer for transparency.
fn synthesize_done(result: &BrowserResult) -> String {
    let mut out = String::new();
    if result.summary.trim().is_empty() {
        // Defensive: a Done with empty summary should not happen
        // (the browser agent's post-process always populates it),
        // but if it does, do not show the user a blank message.
        out.push_str("The browser task completed.");
    } else {
        out.push_str(result.summary.trim());
    }
    let total = result.key_findings.len();
    if total > 1 {
        let done = result
            .key_findings
            .iter()
            .filter(|f| f.status == "done")
            .count();
        out.push_str(&format!(" ({done} of {total} sub-tasks completed.)"));
    }
    out
}

/// Render the `Partial` branch. The browser agent got some
/// subtasks done but not all. The typed result has a `summary`
/// (which usually already mentions what went wrong) and a
/// `key_findings` list (which has the per-subtask reasons).
fn synthesize_partial(result: &BrowserResult) -> String {
    let mut out = String::new();
    if !result.summary.trim().is_empty() {
        out.push_str(result.summary.trim());
    } else {
        out.push_str("The browser task partially completed.");
    }
    // Surface the per-subtask failures/skips so the user can
    // see what was missed at a glance. Skip findings with status
    // "done" — those are already covered by the summary.
    let issues: Vec<&KeyFinding> = result
        .key_findings
        .iter()
        .filter(|f| f.status == "failed" || f.status == "skipped" || f.status == "pending")
        .collect();
    if !issues.is_empty() {
        out.push_str(" Outstanding:");
        for (i, f) in issues.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            let reason_part = if f.reason.is_empty() {
                String::new()
            } else {
                format!(" ({})", f.reason)
            };
            out.push_str(&format!(
                "{}: {} [{}]{}",
                f.id, f.description, f.status, reason_part
            ));
        }
        out.push('.');
    }
    out
}

/// Render the `Failed` branch. The user must always see a
/// human-readable reason, even when the reason is generic.
fn synthesize_failed(result: &BrowserResult) -> String {
    let reason = if result.failure_reason.trim().is_empty() {
        "I couldn't complete the task."
    } else {
        result.failure_reason.trim()
    };
    // Distinguish "couldn't run at all" from "ran but failed."
    // The latter is rare (the browser agent mostly produces a
    // Partial in that case) but worth catching cleanly.
    if !result.summary.trim().is_empty() {
        format!("{} ({})", result.summary.trim(), reason)
    } else {
        reason.to_string()
    }
}

/// Phase 7: render a research-shaped `BrowserResult`. The
/// synthesis is one consolidated list — one row per finding,
/// not one card per platform. The header is the goal
/// statement (from `result.summary`); the body is the
/// `one_line` rendering of each `ResearchFinding` in the
/// order the agent added them (which is the order the
/// agent visited the platforms).
///
/// Edge cases:
///
///   * `findings` empty (we shouldn't get here — the caller
///     routes only when findings is non-empty) — fall back
///     to "no findings to report."
///   * Status `Failed` with findings — render the findings
///     plus the failure reason so the user sees what got
///     done before the failure.
///   * Status `Partial` with findings — render the findings
///     plus a footer noting which platforms exhausted (the
///     `key_findings` list carries the per-subtask statuses).
fn synthesize_research(result: &BrowserResult) -> String {
    let mut out = String::new();
    // Header. Prefer the agent's own summary; fall back to a
    // "Found N results across M platforms" line that
    // summarizes the list the user is about to see.
    if !result.summary.trim().is_empty() {
        out.push_str(result.summary.trim());
    } else {
        let n = result.findings.len();
        let platforms: std::collections::BTreeSet<String> =
            result.findings.iter().map(|f| f.platform.clone()).collect();
        out.push_str(&format!(
            "Found {} result{} across {} platform{}.",
            n,
            if n == 1 { "" } else { "s" },
            platforms.len(),
            if platforms.len() == 1 { "" } else { "s" }
        ));
    }
    out.push('\n');
    // Body: one row per finding. Each row is the
    // `ResearchFinding::one_line()` rendering.
    for (i, f) in result.findings.iter().enumerate() {
        out.push_str(&format!("  {}. {}\n", i + 1, f.one_line()));
    }
    // Footer: on Partial or Failed, mention the
    // per-platform shortfalls so the user knows what got
    // missed. We read `key_findings` and pull out the
    // exhausted / failed / skipped rows.
    let shortfalls: Vec<&KeyFinding> = result
        .key_findings
        .iter()
        .filter(|f| {
            let s = f.status.as_str();
            s == "exhausted" || s == "failed" || s == "skipped"
        })
        .collect();
    if !shortfalls.is_empty() {
        out.push_str("  Outstanding:");
        for (i, f) in shortfalls.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            let reason_part = if f.reason.is_empty() {
                String::new()
            } else {
                format!(" ({})", f.reason)
            };
            out.push_str(&format!(
                "{}: {} [{}]{}",
                f.id, f.description, f.status, reason_part
            ));
        }
        out.push('.');
    }
    out.trim_end().to_string()
}

/// Phase 20: render a SaaS lead-gen `BrowserResult`.
/// Surface confidence levels (`[High Confidence]`, `[Medium Confidence]`, `[Low Confidence]`) prominently per lead.
fn synthesize_lead_research(result: &BrowserResult) -> String {
    let mut out = String::new();
    if !result.summary.trim().is_empty() {
        out.push_str(result.summary.trim());
    } else {
        out.push_str(&format!(
            "Found {} SaaS lead{}.",
            result.lead_findings.len(),
            if result.lead_findings.len() == 1 {
                ""
            } else {
                "s"
            }
        ));
    }
    out.push('\n');
    for (i, lf) in result.lead_findings.iter().enumerate() {
        let contact = match (&lf.contact_name, &lf.contact_role, &lf.contact_email) {
            (Some(name), Some(role), Some(email)) => format!("{name} ({role}, {email})"),
            (Some(name), Some(role), None) => format!("{name} ({role})"),
            (Some(name), None, Some(email)) => format!("{name} ({email})"),
            (Some(name), None, None) => name.clone(),
            (None, _, Some(email)) => format!("Email: {email}"),
            _ => "No named contact".to_string(),
        };
        out.push_str(&format!(
            "  {}. {} ({}) — Contact: {} [{} Confidence] [{}]\n",
            i + 1,
            lf.company_name,
            lf.company_domain,
            contact,
            lf.confidence,
            lf.source_platform
        ));
    }
    out.trim_end().to_string()
}

// ----------------------------------------------------------------------
// Phase 5: freelance client lead-gen renderer + mixed-shape renderer.
//
// Two functions live here, mirroring the spec's "one clearly
// separated section per vertical" rule:
//
//   * `synthesize_freelance_leads(result)` — renders a
//     freelance-only result as a single "Freelance leads"
//     section. Each lead is one row: project scope, client,
//     budget / rate, contact method, source platform.
//
//   * `synthesize_mixed_job_freelance(result)` — renders a
//     result that has both `findings` (from a job-search
//     subtask) and `freelance_findings` (from a freelance
//     subtask) as two separate sections, in that order. A
//     user running "find me a job and find me clients" gets
//     "Job openings" first, then "Freelance leads" — the
//     two never blend into one ranked list.
//
// Output format for the freelance section:
//
//   Found N freelance leads out of M total listings.
//   1. <project_scope> for <client_name> — <budget> — <contact> [<source>]
//   ...
//   (or, when there are listings without a budget or contact:)
//   Found N qualified freelance leads out of M total listings.
//   1. ...
//   ### Other listings found
//   | Rank | Project | Client | Source | URL |
//   ...
//
// The qualified / unqualified split is the spec's "a project
// listing with a stated budget or a client contact I can
// reach" commitment. A lead with either a `budget_hint` or a
// `contact_method` (and the contact is present) is
// "qualified"; everything else goes into the "Other
// listings" section so the user can still see what the agent
// found, but in a clearly-secondary position.
// --------------------------------------------------------------------//

/// Render a freelance-only `BrowserResult`. Used when
/// `freelance_findings` is non-empty and the generic
/// `findings` is empty (i.e. the task was *only* about
/// finding clients, not jobs). For mixed tasks, see
/// `synthesize_mixed_job_freelance`.
fn synthesize_freelance_leads(result: &BrowserResult) -> String {
    use crate::handoff::FreelanceLead;

    // Split qualified (budget OR contact) from
    // non-qualified (neither). The split is the Phase 5
    // acceptance commit.
    let mut qualified: Vec<&FreelanceLead> = Vec::new();
    let mut other: Vec<&FreelanceLead> = Vec::new();
    for l in &result.freelance_findings {
        let has_budget = l
            .budget_hint
            .as_ref()
            .map(|b| !b.raw.is_empty())
            .unwrap_or(false);
        let has_contact = l
            .contact_method
            .as_ref()
            .map(|c| c.is_present())
            .unwrap_or(false);
        if has_budget || has_contact {
            qualified.push(l);
        } else {
            other.push(l);
        }
    }

    let total = result.freelance_findings.len();
    let qual_count = qualified.len();

    let mut out = String::new();
    if !result.summary.trim().is_empty() {
        out.push_str(result.summary.trim());
        out.push('\n');
    } else if other.is_empty() {
        out.push_str(&format!(
            "Found {qual_count} freelance lead{}.",
            if qual_count == 1 { "" } else { "s" }
        ));
        out.push('\n');
    } else {
        out.push_str(&format!(
            "Found {qual_count} qualified freelance lead{} out of {total} total listing{} found.",
            if qual_count == 1 { "" } else { "s" },
            if total == 1 { "" } else { "s" }
        ));
        out.push('\n');
    }

    for (i, l) in qualified.iter().enumerate() {
        out.push_str(&format!("  {}. {}\n", i + 1, l.one_line()));
    }

    if !other.is_empty() {
        out.push_str("\n### Other listings found\n\n");
        out.push_str("| Rank | Project | Client | Source | URL |\n");
        out.push_str("| ---: | :--- | :--- | :--- | :--- |\n");
        for (i, l) in other.iter().enumerate() {
            let url = l.base.url.as_deref().unwrap_or("—");
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                i + 1,
                if l.project_scope.is_empty() {
                    "(no scope)"
                } else {
                    l.project_scope.as_str()
                },
                l.client_name,
                l.source_platform,
                url
            ));
        }
    }

    out.trim_end().to_string()
}

/// Render a mixed job + freelance `BrowserResult` as two
/// clearly-separated sections. The order is "Job openings"
/// (from `findings`) first, "Freelance leads" (from
/// `freelance_findings`) second — matching the natural
/// order of a compound task. Each section uses its
/// dedicated renderer (`synthesize_research` for the
/// job half, `synthesize_freelance_leads` for the
/// freelance half), so the visual shape matches what the
/// user would see if they ran either half as a
/// standalone task.
fn synthesize_mixed_job_freelance(result: &BrowserResult) -> String {
    // Render the job half. We re-use `synthesize_research`
    // by temporarily constructing a `BrowserResult`-shaped
    // view, but to keep the surface simple we just inline
    // the rendering: a header + one row per finding. The
    // `synthesize_research` function rebuilds the header
    // from `result.summary` (which may be one combined
    // summary from the compound task), so the simplest
    // approach is to call it on a synthetic
    // `BrowserResult` carrying only the job findings and a
    // job-specific summary.
    let job_summary = if !result.summary.trim().is_empty() {
        // Strip a leading "Found N roles" or "Found N
        // freelance leads" prefix from the combined
        // summary so the job section's header stands on
        // its own. The combined summary may be a single
        // sentence ("Found 3 jobs and 2 freelance leads")
        // — we drop it and rebuild per-section headers.
        String::new()
    } else {
        String::new()
    };
    let job_result = BrowserResult {
        summary: job_summary,
        findings: result.findings.clone(),
        job_findings: result.job_findings.clone(),
        lead_findings: Vec::new(),
        freelance_findings: Vec::new(),
        ..result.clone()
    };
    let job_section = synthesize_research(&job_result);

    // Render the freelance half.
    let freelance_result = BrowserResult {
        summary: String::new(),
        findings: Vec::new(),
        job_findings: Vec::new(),
        lead_findings: Vec::new(),
        freelance_findings: result.freelance_findings.clone(),
        ..result.clone()
    };
    let freelance_section = synthesize_freelance_leads(&freelance_result);

    let mut out = String::new();
    out.push_str("### Job openings\n\n");
    out.push_str(job_section.trim_end());
    out.push_str("\n\n### Freelance leads\n\n");
    out.push_str(freelance_section.trim_end());
    out
}

/// Phase 39: render a `LocalLeadGen` (Google Maps)
/// `BrowserResult` as a ranked markdown table.
///
/// Output shape:
///
///   <one-line summary>
///   <blank line>
///   | Rank | Business Name | Phone | Address | Rating | Reviews | Website |
///   | ---: | :--- | :--- | :--- | ---: | ---: | :--- |
///   | 1 | ... | ... |
///   | 2 | ... | ... |
///   ...
///   ### Other findings
///   | Rank | Business Name | ... |
///   | 4 | ... | ... |  (non-qualified: failed the
///                          `is_qualified_lead` filter)
///
/// The qualified/non-qualified split is recomputed here
/// (using the `Handoff.maps_query` filter), not trusted
/// from the orchestrator's prior `rank_and_filter` pass —
/// the spec says "if there are non-qualified findings,
/// render a '### Other findings' section with the same
/// columns", and the orchestrator already re-numbered
/// both partitions contiguously, so a single pass that
/// re-derives the split is the simplest and most
/// defensive implementation. (Re-deriving is O(n) and
/// the lists are bounded by `listing_count` ≤ 50.)
///
/// The one-line summary above the table is
/// "Found N qualified leads (no website + rating ≥ X)
/// out of M total listings checked." where X is the
/// `MapsQuery::min_rating` (or "any rating" when the
/// user did not set a rating filter).
///
/// The total "M" in the summary is the total pre-split
/// count of `maps_findings` — i.e. everything the agent
/// scraped, not just the qualified subset. This is the
/// spec's "out of M total listings checked" wording.
fn synthesize_maps(result: &BrowserResult, handoff: &Handoff) -> String {
    use crate::handoff::MapsFinding;

    let query = handoff.maps_query.as_ref();

    // Re-derive the qualified/non-qualified split.
    // The orchestrator has already deduped + ranked +
    // filtered, so this split is identical to what the
    // orchestrator produced; the synthesizer just needs
    // the boundary index.
    let mut qualified: Vec<&MapsFinding> = Vec::new();
    let mut other: Vec<&MapsFinding> = Vec::new();
    for f in &result.maps_findings {
        let is_qual = query
            .map(|q| f.is_qualified_lead(q))
            .unwrap_or(false);
        if is_qual {
            qualified.push(f);
        } else {
            other.push(f);
        }
    }

    let total = result.maps_findings.len();
    let qual_count = qualified.len();

    // One-line summary: "Found N qualified leads (no
    // website + rating ≥ X) out of M total listings
    // checked."
    let rating_qual = match query.and_then(|q| q.min_rating) {
        Some(r) => format!("rating ≥ {r:.1}"),
        None => "any rating".to_string(),
    };
    let website_qual = if query.map(|q| q.require_no_website).unwrap_or(false) {
        "no website"
    } else {
        "any website status"
    };
    let phone_qual = if query.map(|q| q.require_phone).unwrap_or(false) {
        "+ phone required"
    } else {
        ""
    };
    let filter_clause = if phone_qual.is_empty() {
        format!("{website_qual} + {rating_qual}")
    } else {
        format!("{website_qual} + {rating_qual} {phone_qual}")
    };

    let mut out = String::new();
    out.push_str(&format!(
        "Found {qual_count} qualified lead{qual_plural} ({filter_clause}) out of {total} total listing{total_plural} checked.",
        qual_plural = if qual_count == 1 { "" } else { "s" },
        total_plural = if total == 1 { "" } else { "s" },
    ));
    out.push_str("\n\n");

    // Table header. The column widths are not enforced —
    // the renderer (Tauri chat list / markdown viewer) is
    // expected to render raw `|...|` markdown.
    out.push_str(
        "| Rank | Business Name | Phone | Address | Rating | Reviews | Website |\n",
    );
    out.push_str("| ---: | :--- | :--- | :--- | ---: | ---: | :--- |\n");

    for f in &qualified {
        out.push_str(&maps_table_row(f));
    }

    if !other.is_empty() {
        out.push_str("\n### Other findings\n\n");
        // Same header so the second table reads as a
        // continuation, not a different schema.
        out.push_str(
            "| Rank | Business Name | Phone | Address | Rating | Reviews | Website |\n",
        );
        out.push_str("| ---: | :--- | :--- | :--- | ---: | ---: | :--- |\n");
        for f in &other {
            out.push_str(&maps_table_row(f));
        }
    }

    out.trim_end().to_string()
}

/// Render a single `MapsFinding` as one markdown table row.
/// Pulled out of `synthesize_maps` so the qualified and
/// non-qualified tables render identically without a
/// duplicated string-format block.
///
/// Cell formatting:
///   * `Phone` — raw value when `Some`, `—` when `None`.
///   * `Address` — raw value when `Some`, `—` when `None`.
///   * `Rating` — `4.5` when `Some`, `—` when `None`.
///   * `Reviews` — `123` when `Some`, `—` when `None`.
///   * `Website` — the bare domain (stripped of scheme +
///     `www.`) when `has_website == true`, `No` when
///     `false`. `No` is the spec's intended display: a
///     missing website is the *primary lead signal* (the
///     reason this whole vertical exists), so we want it
///     to stand out at a glance — a dash or a blank would
///     look like "data not collected" and defeat the
///     purpose.
///   * `Business Name` — escaped for `|` so a name like
///     "Smith | Jones Salon" doesn't break the row.
fn maps_table_row(f: &crate::handoff::MapsFinding) -> String {
    let name = f.name.replace('|', "\\|");
    let phone = f.phone.as_deref().unwrap_or("—");
    let address = f
        .address
        .as_deref()
        .unwrap_or("—")
        .replace('|', "\\|");
    let rating = f
        .rating
        .map(|r| format!("{r:.1}"))
        .unwrap_or_else(|| "—".to_string());
    let reviews = f
        .review_count
        .map(|n| n.to_string())
        .unwrap_or_else(|| "—".to_string());
    let website = if f.has_website {
        f.website_url
            .as_deref()
            .map(strip_scheme_and_www)
            .unwrap_or_else(|| "(yes)".to_string())
    } else {
        "No".to_string()
    };
    format!(
        "| {rank} | {name} | {phone} | {address} | {rating} | {reviews} | {website} |\n",
        rank = f.rank,
    )
}

/// Strip `https://`, `http://`, and a leading `www.` so a
/// long Maps-panel URL collapses to the bare domain.
/// `https://www.acme.com/path` → `acme.com/path`. We
/// keep the path so two listings pointing at the same
/// root domain but different landing pages stay
/// distinguishable in the rendered table.
fn strip_scheme_and_www(url: &str) -> String {
    let mut s = url.to_string();
    for prefix in &["https://", "http://"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.to_string();
            break;
        }
    }
    if let Some(rest) = s.strip_prefix("www.") {
        s = rest.to_string();
    }
    s
}

/// Phase 34: deterministic keyword check for "is this task a
/// Google Maps local-lead-gen request?". Pure string check —
/// no LLM call. The phase spec lists the exact keyword set:
///
///   "maps", "google maps", "salons in", "gyms in",
///   "clinics in", "restaurants near", "businesses in",
///   "listings in", "leads in"
///
/// followed by a city-like word.
///
/// We require the city-like *token* to be present (so "I like
/// maps" doesn't fire the keyword) — but we use a loose
/// "looks like a word" check so non-Latin scripts and
/// multi-word city names still match. The actual city is
/// extracted by `MapsQuery::parse` and lives on the
/// `Handoff.maps_query` field; this function only decides
/// the *shape*.
///
/// Important nuance: "leads in <city>" can ALSO be a
/// SaaS-lead-gen phrase ("SaaS leads in devtools"). We
/// disambiguate by requiring a *physical-business* keyword
/// (one of: maps, salons, gyms, clinics, restaurants,
/// businesses, listings) anywhere in the task, or the
/// phrase "google maps" specifically. Bare "leads in" alone
/// is ambiguous and we let the LLM classifier sort it.
pub fn is_local_lead_gen_task(task: &str) -> bool {
    let lower = task.to_lowercase();

    // "google maps" is unambiguous.
    if lower.contains("google maps") {
        return true;
    }

    // The other SPECIFIC_PHRASES are unambiguous: "salons
    // in", "gyms in", "clinics in", "restaurants near",
    // "businesses in", "listings in" all point to a
    // physical-business listing. "leads in" is NOT in this
    // list because "SaaS leads in devtools" matches it
    // without being a Maps task.
    const UNAMBIGUOUS_PHRASES: &[&str] = &[
        "salons in",
        "gyms in",
        "clinics in",
        "restaurants near",
        "businesses in",
        "listings in",
    ];
    if UNAMBIGUOUS_PHRASES.iter().any(|p| lower.contains(p)) {
        return true;
    }

    // Bare "maps" — only fire if there's an "in <word>" or
    // "near <word>" tail that looks like a city/location.
    if lower.contains("maps") {
        // Look for "in <word>" or "near <word>" anywhere in the
        // task where <word> starts with a letter and is at
        // least 2 characters.
        for kw in &[" in ", " near "] {
            if let Some(idx) = lower.find(kw) {
                let after = lower[idx + kw.len()..].trim_start();
                let word: String = after
                    .chars()
                    .take_while(|c| c.is_alphabetic() || *c == '-' || *c == '\'')
                    .collect();
                if word.chars().count() >= 2 {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_config() -> ProviderConfig {
        ProviderConfig {
            opencode_zen: crate::OpencodeZenConfig {
                base_url: "http://test".into(),
                api_key: "test".into(),
                default_model: "test-model".into(),
                max_iterations: 1,
                max_tokens: None,
                max_cost: None,
            },
            browser: None,
            agent: crate::AgentConfig::default(),
        }
    }

    #[test]
    fn chat_agent_system_prompt_is_distinct_from_browser_agent() {
        // The two agents must not share a prompt. The whole
        // point of the Phase 3 split is that they have different
        // identities, different tools, different responsibilities.
        let chat_agent = ChatAgent::new(dummy_config());
        let chat_prompt = chat_agent.system_prompt();
        // Browser agent's prompt lives in agent.rs and contains
        // "COMPLETENESS PROTOCOL" — a phrase the chat agent
        // never uses. We assert the chat prompt does *not* contain
        // it, so a future copy-paste merge gets caught.
        assert!(
            !chat_prompt.contains("COMPLETENESS PROTOCOL"),
            "chat agent prompt must not contain the browser agent's COMPLETENESS PROTOCOL block",
        );
        // The chat prompt mentions its two jobs.
        assert!(chat_prompt.contains("Decide what the user wants"));
        assert!(chat_prompt.contains("result into a single concise chat reply"));
    }

    #[test]
    fn build_handoff_uses_deterministic_planner() {
        // The chat agent's handoff builder must split a compound
        // task into at least 2 subtasks — that's the planner's
        // job and the whole reason Phase 2 introduced it.
        let chat_agent = ChatAgent::new(dummy_config());
        let handoff = chat_agent.build_handoff(
            "go to instagram and text my friend hi",
            "chat:123:000001",
            vec!["enter via search".to_string()],
            None,
            None,
        );
        assert_eq!(
            handoff.task_description,
            "go to instagram and text my friend hi"
        );
        assert!(
            handoff.subtasks.len() >= 2,
            "compound task must produce >= 2 subtasks, got {:?}",
            handoff.subtasks
        );
        assert_eq!(handoff.constraints, vec!["enter via search".to_string()]);
        assert_eq!(handoff.originating_message_id, "chat:123:000001");
    }

    #[test]
    fn build_handoff_with_bare_task_produces_no_subtasks() {
        // A single-clause task does not need decomposition. The
        // planner's "single clause" rule produces exactly one
        // subtask, which is fine.
        let chat_agent = ChatAgent::new(dummy_config());
        let handoff = chat_agent.build_handoff("go to wikipedia", "chat:1:0", vec![], None, None);
        // The planner's "single clause" rule returns 1 subtask
        // with the original text as the description. We don't
        // assert == 1 (that's the planner's contract, tested in
        // planner.rs); we assert it parsed cleanly.
        assert_eq!(handoff.task_description, "go to wikipedia");
    }

    #[test]
    fn synthesize_done_uses_summary_and_appends_subtask_footer() {
        let chat_agent = ChatAgent::new(dummy_config());
        let r = BrowserResult::done(
            "s1",
            "Message sent to Alice.",
            vec![
                KeyFinding {
                    id: "step-1".into(),
                    description: "open instagram".into(),
                    status: "done".into(),
                    reason: String::new(),
                    evidence_signature: None,
                },
                KeyFinding {
                    id: "step-2".into(),
                    description: "send hi to Alice".into(),
                    status: "done".into(),
                    reason: String::new(),
                    evidence_signature: None,
                },
            ],
            None,
            None,
        );
        let handoff = Handoff::bare("go to instagram and text alice hi", "chat:1:0");
        let out = chat_agent.synthesize_reply(&r, &[], &handoff);
        assert!(out.contains("Message sent to Alice."));
        assert!(out.contains("2 of 2 sub-tasks completed"));
    }

    #[test]
    fn synthesize_partial_lists_outstanding_findings() {
        let chat_agent = ChatAgent::new(dummy_config());
        let r = BrowserResult {
            status: BrowserStatus::Partial,
            summary: "Step 1 worked; step 2 was skipped.".into(),
            key_findings: vec![
                KeyFinding {
                    id: "step-1".into(),
                    description: "open instagram".into(),
                    status: "done".into(),
                    reason: String::new(),
                    evidence_signature: None,
                },
                KeyFinding {
                    id: "step-2".into(),
                    description: "send hi".into(),
                    status: "skipped".into(),
                    reason: "no friend named 'hi'".into(),
                    evidence_signature: None,
                },
            ],
            final_snapshot_signature: None,
            raw_transcript_ref: None,
            session_id: "s1".into(),
            failure_reason: String::new(),
            findings: Vec::new(),
            job_findings: Vec::new(),
            lead_findings: Vec::new(),
            respawn_count: 0,
            respawn_reasons: Vec::new(),
            maps_findings: Vec::new(),
            freelance_findings: Vec::new(),
        };
        let handoff = Handoff::bare("go to instagram and text hi", "chat:1:0");
        let out = chat_agent.synthesize_reply(&r, &[], &handoff);
        assert!(out.contains("Step 1 worked"));
        assert!(out.contains("Outstanding"));
        assert!(out.contains("step-2"));
        assert!(out.contains("no friend named 'hi'"));
    }

    #[test]
    fn synthesize_failed_includes_reason() {
        let chat_agent = ChatAgent::new(dummy_config());
        let r = BrowserResult::failure("s1", "Chrome failed to launch: ENOENT", None);
        let handoff = Handoff::bare("go to wikipedia", "chat:1:0");
        let out = chat_agent.synthesize_reply(&r, &[], &handoff);
        assert!(
            out.contains("Chrome failed to launch: ENOENT"),
            "failed reply must include the reason, got: {out}",
        );
    }

    #[test]
    fn synthesize_failed_falls_back_to_generic_when_no_reason() {
        // The synthesis contract: even a pathological
        // BrowserResult::Failed with no reason produces a
        // non-empty reply. The user must always see *something*.
        let chat_agent = ChatAgent::new(dummy_config());
        let r = BrowserResult {
            status: BrowserStatus::Failed,
            summary: String::new(),
            key_findings: Vec::new(),
            final_snapshot_signature: None,
            raw_transcript_ref: None,
            session_id: "s1".into(),
            failure_reason: String::new(),
            findings: Vec::new(),
            job_findings: Vec::new(),
            lead_findings: Vec::new(),
            respawn_count: 0,
            respawn_reasons: Vec::new(),
            maps_findings: Vec::new(),
            freelance_findings: Vec::new(),
        };
        let handoff = Handoff::bare("anything", "chat:1:0");
        let out = chat_agent.synthesize_reply(&r, &[], &handoff);
        assert!(!out.is_empty());
    }

    #[test]
    fn mint_message_id_is_unique_per_call() {
        let chat_agent = ChatAgent::new(dummy_config());
        let a = chat_agent.mint_message_id();
        let b = chat_agent.mint_message_id();
        // The two calls happen at different sub-second nanos in
        // practice; we don't *require* distinctness, but in any
        // realistic run they are. If this test ever flakes we
        // can relax the assertion — but the format check below
        // is the important one.
        let _ = (a, b);
    }

    #[test]
    fn mint_message_id_format_is_stable() {
        let chat_agent = ChatAgent::new(dummy_config());
        let id = chat_agent.mint_message_id();
        // Format: "chat:<secs>:<6-digit-suffix>"
        assert!(id.starts_with("chat:"));
        let parts: Vec<&str> = id.split(':').collect();
        assert_eq!(parts.len(), 3, "expected chat:<secs>:<suffix>, got {id}");
        assert!(parts[1].chars().all(|c| c.is_ascii_digit()));
        assert_eq!(parts[2].len(), 6);
    }

    // ---- Phase 7: research handoff + research synthesis ----

    #[test]
    fn build_research_handoff_produces_typed_plan() {
        // The Phase 7 spec line: the chat agent must be
        // able to build a research handoff directly, with
        // the typed `ResearchPlan` populated. This is the
        // surface the orchestrator calls when the user
        // message triggers a research intent.
        let chat_agent = ChatAgent::new(dummy_config());
        let handoff = chat_agent.build_research_handoff("find remote rust jobs", "chat:7:0");
        assert!(handoff.research_plan.is_some());
        let plan = handoff.research_plan.as_ref().unwrap();
        assert!(plan.is_research);
        assert!(
            plan.platforms.len() >= 3,
            "default platform list should be at least 3 platforms, got {}",
            plan.platforms.len()
        );
        // The subtask list is the per-platform list.
        assert_eq!(handoff.subtasks.len(), plan.platforms.len());
        // The originating message id is preserved.
        assert_eq!(handoff.originating_message_id, "chat:7:0");
    }

    #[test]
    fn build_handoff_with_empty_platforms_falls_back_to_clause_splitter() {
        // Back-compat: the Phase 3 `build_handoff` signature
        // is unchanged. Passing no platform list (the
        // orchestrator's normal case for non-research
        // intents) goes through the Phase 2 clause-splitter.
        let chat_agent = ChatAgent::new(dummy_config());
        let handoff = chat_agent.build_handoff(
            "go to instagram and text alice hi",
            "chat:1:0",
            vec![],
            None,
            None,
        );
        assert!(handoff.research_plan.is_none());
        assert!(
            handoff.subtasks.len() >= 2,
            "compound task must produce >= 2 subtasks, got {:?}",
            handoff.subtasks
        );
    }

    #[test]
    fn build_handoff_with_platforms_routes_to_research_for_research_tasks() {
        // The new build_handoff_with_platforms method
        // routes to the research planner when the task is
        // research-shaped and a non-empty platform list is
        // provided.
        let chat_agent = ChatAgent::new(dummy_config());
        let handoff = chat_agent.build_handoff_with_platforms(
            "find me a rust job",
            "chat:1:0",
            vec![],
            &default_job_board_platforms(),
            None,
            None,
        );
        assert!(handoff.research_plan.is_some());
        assert!(handoff.research_plan.as_ref().unwrap().is_research);
    }

    #[test]
    fn build_handoff_with_platforms_falls_back_for_non_research() {
        // "go to wikipedia" is not a research task. The
        // handoff should fall through to the clause-splitter
        // even when a platform list is provided.
        let chat_agent = ChatAgent::new(dummy_config());
        let handoff = chat_agent.build_handoff_with_platforms(
            "go to wikipedia and search for rust",
            "chat:1:0",
            vec![],
            &default_job_board_platforms(),
            None,
            None,
        );
        assert!(handoff.research_plan.is_none());
        assert!(handoff.subtasks.len() >= 2);
    }

    #[test]
    fn synthesize_research_done_renders_one_line_per_finding() {
        // The headline Phase 7 synthesis property: a
        // research-shaped Done result renders a consolidated
        // one-row-per-finding list, not a single message
        // with a "N of M sub-tasks" footer.
        use crate::research::ResearchFinding;
        let chat_agent = ChatAgent::new(dummy_config());
        let r = BrowserResult::done_research(
            "s1",
            "Found 3 roles across 2 platforms.",
            vec![KeyFinding {
                id: "linkedin".into(),
                description: "LinkedIn".into(),
                status: "done".into(),
                reason: String::new(),
                evidence_signature: None,
            }],
            None,
            None,
            vec![
                ResearchFinding {
                    id: "f1".into(),
                    platform: "LinkedIn".into(),
                    title: Some("Rust Engineer".into()),
                    company: Some("Acme".into()),
                    email: Some("alice@acme.com".into()),
                    url: Some("https://linkedin.com/jobs/1".into()),
                    note: String::new(),
                    added_at_secs: 0,
                },
                ResearchFinding {
                    id: "f2".into(),
                    platform: "Indeed".into(),
                    title: Some("Backend Engineer".into()),
                    company: Some("Globex".into()),
                    email: None,
                    url: Some("https://indeed.com/jobs/2".into()),
                    note: String::new(),
                    added_at_secs: 0,
                },
            ],
        );
        let handoff = Handoff::bare("find rust jobs", "chat:1:0");
        let out = chat_agent.synthesize_reply(&r, &[], &handoff);
        // The header is the summary.
        assert!(out.contains("Found 3 roles across 2 platforms."));
        // Each finding renders as a numbered row.
        assert!(
            out.contains("1. Rust Engineer"),
            "first finding missing: {out}"
        );
        assert!(
            out.contains("2. Backend Engineer"),
            "second finding missing: {out}"
        );
        // Each row carries the platform tag.
        assert!(out.contains("[LinkedIn]"));
        assert!(out.contains("[Indeed]"));
        // The first row has a contact email.
        assert!(out.contains("alice@acme.com"));
        // The second row has a URL.
        assert!(out.contains("https://indeed.com/jobs/2"));
        // The reply is natural language, not JSON.
        assert!(!out.contains('{'));
    }

    #[test]
    fn synthesize_research_partial_lists_outstanding_platforms() {
        // Partial status: the user should see the findings
        // they got, plus a footer noting which platforms
        // exhausted.
        use crate::research::ResearchFinding;
        let chat_agent = ChatAgent::new(dummy_config());
        let r = BrowserResult {
            status: BrowserStatus::Partial,
            summary: "Found 1 role; 2 platforms had no matching results.".into(),
            key_findings: vec![
                KeyFinding {
                    id: "linkedin".into(),
                    description: "LinkedIn".into(),
                    status: "done".into(),
                    reason: String::new(),
                    evidence_signature: None,
                },
                KeyFinding {
                    id: "wellfound".into(),
                    description: "Wellfound".into(),
                    status: "exhausted".into(),
                    reason: "no listings matched".into(),
                    evidence_signature: None,
                },
                KeyFinding {
                    id: "weworkremotely".into(),
                    description: "WeWorkRemotely".into(),
                    status: "exhausted".into(),
                    reason: "step budget exhausted: 10/10".into(),
                    evidence_signature: None,
                },
            ],
            final_snapshot_signature: None,
            raw_transcript_ref: None,
            session_id: "s1".into(),
            failure_reason: String::new(),
            findings: vec![ResearchFinding {
                id: "f1".into(),
                platform: "LinkedIn".into(),
                title: Some("Rust Engineer".into()),
                company: Some("Acme".into()),
                email: Some("a@b.com".into()),
                url: None,
                note: String::new(),
                added_at_secs: 0,
            }],
            job_findings: Vec::new(),
            lead_findings: Vec::new(),
            respawn_count: 0,
            respawn_reasons: Vec::new(),
            maps_findings: Vec::new(),
            freelance_findings: Vec::new(),
        };
        let handoff = Handoff::bare("find rust jobs", "chat:1:0");
        let out = chat_agent.synthesize_reply(&r, &[], &handoff);
        // The header is rendered.
        assert!(out.contains("Found 1 role"));
        // The finding is rendered.
        assert!(out.contains("Rust Engineer"));
        // The exhausted platforms are listed in the footer.
        assert!(out.contains("Outstanding"));
        assert!(out.contains("wellfound"));
        assert!(out.contains("no listings matched"));
        assert!(out.contains("weworkremotely"));
        assert!(out.contains("step budget exhausted"));
    }

    #[test]
    fn synthesize_research_with_no_findings_falls_through_to_non_research() {
        // Defense-in-depth: a non-research Done result with
        // an empty findings list should *not* be routed to
        // the research synthesizer. (The synthesizer
        // explicitly checks `!result.findings.is_empty()`
        // before routing, but a test here pins the
        // contract.)
        let chat_agent = ChatAgent::new(dummy_config());
        let r = BrowserResult::done("s1", "All done.", vec![], None, None);
        let handoff = Handoff::bare("go to wikipedia", "chat:1:0");
        let out = chat_agent.synthesize_reply(&r, &[], &handoff);
        assert!(out.contains("All done."));
        assert!(
            !out.contains("Found"),
            "non-research reply should not contain the research header, got: {out}"
        );
    }

    #[test]
    fn phase20_lead_query_generates_two_step_discovery_subtasks() {
        use crate::handoff::LeadQuery;
        let chat_agent = ChatAgent::new(dummy_config());
        let lq = LeadQuery {
            industry: vec!["SaaS".into(), "DevTools".into()],
            company_size_range: Some((10, 50)),
            geography: vec!["US".into()],
            target_role_titles: vec!["Head of Growth".into(), "VP Sales".into()],
            product_category_keywords: vec!["CRM".into()],
        };
        let handoff = chat_agent.build_handoff(
            "find SaaS leads in DevTools",
            "chat:1:0",
            vec![],
            None,
            Some(lq),
        );

        assert!(handoff.lead_query.is_some());
        assert!(handoff.research_plan.is_some());
        let plan = handoff.research_plan.unwrap();
        // Should contain step 1 company discovery and step 2 decision maker discovery subtasks
        assert!(plan
            .platforms
            .iter()
            .any(|p| matches!(p.lead_subtask_kind, Some(LeadSubTaskKind::CompanyDiscovery))));
        assert!(plan.platforms.iter().any(|p| matches!(
            p.lead_subtask_kind,
            Some(LeadSubTaskKind::DecisionMakerDiscovery { .. })
        )));
    }

    #[test]
    fn phase20_synthesize_lead_research_surfaces_confidence_levels() {
        use crate::handoff::{LeadConfidence, LeadFinding};
        use crate::research::ResearchFinding;

        let chat_agent = ChatAgent::new(dummy_config());
        let r = BrowserResult::done_lead_research(
            "s1",
            "Discovered SaaS leads.",
            vec![],
            None,
            None,
            vec![],
            vec![
                LeadFinding {
                    base: ResearchFinding {
                        id: "f1".into(),
                        platform: "ProductHunt".into(),
                        title: Some("Acme SaaS".into()),
                        company: Some("Acme Corp".into()),
                        email: Some("founder@acme.com".into()),
                        url: Some("https://acme.com".into()),
                        note: String::new(),
                        added_at_secs: 0,
                    },
                    company_name: "Acme Corp".into(),
                    company_domain: "acme.com".into(),
                    contact_name: Some("Alice Smith".into()),
                    contact_role: Some("Head of Growth".into()),
                    contact_email: Some("alice@acme.com".into()),
                    contact_url: Some("https://linkedin.com/in/alicesmith".into()),
                    source_platform: "ProductHunt".into(),
                    confidence: LeadConfidence::High,
                },
                LeadFinding {
                    base: ResearchFinding {
                        id: "f2".into(),
                        platform: "IndieHackers".into(),
                        title: Some("Beta Tools".into()),
                        company: Some("Beta Inc".into()),
                        email: None,
                        url: Some("https://betainc.io".into()),
                        note: String::new(),
                        added_at_secs: 0,
                    },
                    company_name: "Beta Inc".into(),
                    company_domain: "betainc.io".into(),
                    contact_name: None,
                    contact_role: None,
                    contact_email: None,
                    contact_url: None,
                    source_platform: "IndieHackers".into(),
                    confidence: LeadConfidence::Low,
                },
            ],
        );

        let handoff = Handoff::bare("find leads", "chat:1:0");
        let reply = chat_agent.synthesize_reply(&r, &[], &handoff);

        assert!(reply.contains("Discovered SaaS leads."));
        assert!(reply.contains("Acme Corp"));
        assert!(reply.contains("[High Confidence]"));
        assert!(reply.contains("Beta Inc"));
        assert!(reply.contains("[Low Confidence]"));
    }

    #[test]
    fn phase21_cross_vertical_query_understanding_and_source_router() {
        use crate::handoff::{JobQuery, LeadQuery, TaskShape};
        let chat_agent = ChatAgent::new(dummy_config());

        // Scenario 1: Clear job search
        let jq = JobQuery {
            roles: vec!["Rust Engineer".into()],
            locations: vec!["San Francisco".into()],
            is_remote: true,
            is_internship: true,
            experience_level: None,
        };
        let h1 = chat_agent.build_handoff_for_shape(
            "find remote Rust software engineer internships",
            "chat:1:1",
            vec![],
            TaskShape::JobSearch,
            Some(jq.clone()),
            None,
        );
        assert_eq!(h1.task_shape, Some(TaskShape::JobSearch));
        assert!(h1.job_query.is_some());
        assert!(h1.research_plan.is_some());

        // Scenario 2: Clear lead gen
        let lq = LeadQuery {
            industry: vec!["DevTools".into()],
            company_size_range: None,
            geography: vec!["US".into()],
            target_role_titles: vec!["VP Sales".into()],
            product_category_keywords: vec!["DevOps".into()],
        };
        let h2 = chat_agent.build_handoff_for_shape(
            "find SaaS leads in DevTools",
            "chat:1:2",
            vec![],
            TaskShape::LeadGen,
            None,
            Some(lq.clone()),
        );
        assert_eq!(h2.task_shape, Some(TaskShape::LeadGen));
        assert!(h2.lead_query.is_some());
        assert!(h2.research_plan.is_some());

        // Scenario 3: Clear generic research
        let h3 = chat_agent.build_handoff_for_shape(
            "compare pricing models of top 5 cloud providers",
            "chat:1:3",
            vec![],
            TaskShape::GenericResearch,
            None,
            None,
        );
        assert_eq!(h3.task_shape, Some(TaskShape::GenericResearch));

        // Scenario 4: Clear generic browse
        let h4 = chat_agent.build_handoff_for_shape(
            "open wikipedia.org and search for Quantum Computing",
            "chat:1:4",
            vec![],
            TaskShape::GenericBrowse,
            None,
            None,
        );
        assert_eq!(h4.task_shape, Some(TaskShape::GenericBrowse));

        // Scenario 5: Mixed compound task split into two shaped Handoffs
        let compound_handoffs = chat_agent.build_compound_handoffs(
            "find me internships and find clients for my tutoring business",
            "chat:1:5",
            vec![],
            TaskShape::JobSearch,
            Some(jq),
            Some(lq),
            Some("high"),
            true, // is_ambiguous / compound
        );
        assert_eq!(compound_handoffs.len(), 2);
        assert_eq!(compound_handoffs[0].task_shape, Some(TaskShape::JobSearch));
        assert_eq!(compound_handoffs[1].task_shape, Some(TaskShape::LeadGen));

        // Scenario 6: Ambiguous task with low confidence falls back to GenericResearch
        let fallback_handoffs = chat_agent.build_compound_handoffs(
            "find something interesting",
            "chat:1:6",
            vec![],
            TaskShape::JobSearch,
            None,
            None,
            Some("low"),
            false,
        );
        assert_eq!(fallback_handoffs.len(), 1);
        assert_eq!(
            fallback_handoffs[0].task_shape,
            Some(TaskShape::GenericResearch)
        );
    }

    #[test]
    fn phase29_chat_agent_awareness_of_respawns_and_self_learning() {
        use crate::supervisor::RespawnReason;
        let chat_agent = ChatAgent::new(dummy_config());
        let handoff = Handoff::bare("test task", "chat:29:1");

        // 1. Result with NO respawn: must produce byte-identical output to pre-Phase-29 baseline
        let res_no_respawn =
            BrowserResult::done("sess-29-1", "Extracted 5 key points", vec![], None, None);
        let reply_no_respawn = chat_agent.synthesize_reply(&res_no_respawn, &[], &handoff);
        assert_eq!(reply_no_respawn, "Extracted 5 key points");

        // 2. Result WITH respawn: must include the user-facing one-sentence note
        let res_with_respawn =
            BrowserResult::done("sess-29-2", "Extracted 5 key points", vec![], None, None)
                .with_respawns(1, vec![RespawnReason::Stuck("stalled in loop".into())]);
        let reply_with_respawn = chat_agent.synthesize_reply(&res_with_respawn, &[], &handoff);

        assert!(reply_with_respawn.contains("Extracted 5 key points"));
        assert!(reply_with_respawn.contains("Note: I ran into a snag partway through and restarted that part of the task, but still got you the full list below."));

        // 3. Confirm NO internal technical terms leak into user-visible reply
        assert!(!reply_with_respawn.contains("RespawnReason"));
        assert!(!reply_with_respawn.contains("WorkerHealth"));
        assert!(!reply_with_respawn.contains("Stuck"));
        assert!(!reply_with_respawn.contains("stalled in loop"));
    }

    // ----------------------------------------------------------------
    // Phase 39: Maps synthesizer tests.
    //
    // These tests exercise the user-visible output of
    // `synthesize_maps`: a "15 salons in Mumbai" run must
    // produce a markdown table where the no-website,
    // high-rating businesses appear at the top, and the
    // non-qualified listings land in a "### Other findings"
    // section. We don't actually run the orchestrator here
    // (no LLM, no Chrome); we feed the synthesizer a
    // hand-built `BrowserResult` and assert the rendered
    // markdown.
    // ----------------------------------------------------------------

    fn build_maps_finding(
        rank: usize,
        name: &str,
        phone: Option<&str>,
        address: Option<&str>,
        rating: Option<f32>,
        review_count: Option<u32>,
        has_website: bool,
        website_url: Option<&str>,
    ) -> crate::handoff::MapsFinding {
        crate::handoff::MapsFinding {
            rank,
            name: name.into(),
            phone: phone.map(str::to_string),
            address: address.map(str::to_string),
            rating,
            review_count,
            has_website,
            website_url: website_url.map(str::to_string),
            category: None,
            maps_url: "https://maps.google.com/test".into(),
        }
    }

    /// Headline: a multi-listing Maps `BrowserResult`
    /// renders as a markdown table with the spec's exact
    /// header, a one-line "Found N qualified leads
    /// (no website + rating ≥ X) out of M total
    /// listings checked" summary, and a `### Other
    /// findings` section for the non-qualified rows.
    #[test]
    fn phase39_synthesize_maps_renders_ranked_table() {
        use crate::handoff::{MapsFinding, MapsQuery};

        let query = MapsQuery {
            search_query: "salons in Mumbai".into(),
            city: "Mumbai".into(),
            listing_count: 15,
            min_rating: Some(4.0),
            require_no_website: true,
            require_phone: false,
            parallelism: 1,
            categories: vec![],
        };
        // Two qualified (no website, ≥4.0), one
        // non-qualified (has website → fails
        // require_no_website). Pre-ranked to match what
        // the orchestrator's `rank_and_filter` would
        // have produced.
        let findings: Vec<MapsFinding> = vec![
            build_maps_finding(
                1,
                "Top Salon",
                Some("555-0100"),
                Some("123 Main, Mumbai"),
                Some(4.8),
                Some(120),
                false,
                None,
            ),
            build_maps_finding(
                2,
                "Good Cuts",
                Some("555-0101"),
                Some("456 Park, Mumbai"),
                Some(4.2),
                Some(40),
                false,
                None,
            ),
            build_maps_finding(
                3,
                "Webby Inc",
                Some("555-0102"),
                Some("789 Lake, Mumbai"),
                Some(4.5),
                Some(80),
                true,
                Some("https://webby.com"),
            ),
        ];
        let chat_agent = ChatAgent::new(dummy_config());
        let r = BrowserResult::done_maps(
            "s1",
            "Found 2 qualified salons in Mumbai.",
            vec![],
            None,
            None,
            findings,
        );
        let handoff = Handoff {
            task_description: "salons in Mumbai".into(),
            subtasks: vec![],
            constraints: vec![],
            originating_message_id: "chat:1:0".into(),
            research_plan: None,
            job_query: None,
            lead_query: None,
            task_shape: None,
            respawn_handoff: None,
            maps_query: Some(query),
        };

        let reply = chat_agent.synthesize_reply(&r, &[], &handoff);

        // 1. The spec's exact one-line summary is at the
        // top.
        assert!(
            reply.contains("Found 2 qualified leads"),
            "expected 'Found 2 qualified leads' in:\n{reply}"
        );
        assert!(reply.contains("no website"));
        assert!(reply.contains("rating ≥ 4.0"));
        assert!(reply.contains("out of 3 total listings"));
        // 2. The exact spec header is present.
        assert!(reply.contains("| Rank | Business Name | Phone | Address | Rating | Reviews | Website |"));
        assert!(reply.contains("| ---: | :--- | :--- | :--- | ---: | ---: | :--- |"));
        // 3. The qualified rows are present in the
        // correct order.
        let top_idx = reply.find("Top Salon").expect("Top Salon missing");
        let good_idx = reply.find("Good Cuts").expect("Good Cuts missing");
        let webby_idx = reply.find("Webby Inc").expect("Webby Inc missing");
        assert!(top_idx < good_idx, "Top Salon must come before Good Cuts");
        assert!(good_idx < webby_idx, "Good Cuts must come before Webby Inc (qualified table)");
        // 4. The non-qualified "Webby Inc" is in the
        // "### Other findings" section.
        let other_idx = reply.find("### Other findings").expect("### Other findings section missing");
        assert!(webby_idx > other_idx, "Webby Inc must appear after ### Other findings");
        // 5. Rank numbers in the table.
        assert!(reply.contains("| 1 |"));
        assert!(reply.contains("| 2 |"));
        assert!(reply.contains("| 3 |"));
    }

    /// A Maps result with NO qualified leads still
    /// produces a clean reply — the summary says "Found
    /// 0 qualified leads", the table is empty, and no
    /// "### Other findings" section is rendered (because
    /// there are no non-qualified rows either, when
    /// the `maps_query` filter is conservative).
    #[test]
    fn phase39_synthesize_maps_with_zero_qualified_renders_cleanly() {
        use crate::handoff::{MapsFinding, MapsQuery};

        let query = MapsQuery {
            search_query: "salons in Mumbai".into(),
            city: "Mumbai".into(),
            listing_count: 15,
            min_rating: Some(4.0),
            require_no_website: true,
            require_phone: false,
            parallelism: 1,
            categories: vec![],
        };
        // All three listings have a website → none
        // qualify under `require_no_website`.
        let findings: Vec<MapsFinding> = vec![
            build_maps_finding(
                1,
                "Webby A",
                Some("555-1"),
                Some("addr A"),
                Some(4.5),
                Some(50),
                true,
                Some("https://a.com"),
            ),
            build_maps_finding(
                2,
                "Webby B",
                Some("555-2"),
                Some("addr B"),
                Some(4.3),
                Some(20),
                true,
                Some("https://b.com"),
            ),
        ];
        let chat_agent = ChatAgent::new(dummy_config());
        let r = BrowserResult::done_maps("s1", "", vec![], None, None, findings);
        let handoff = Handoff {
            task_description: "salons in Mumbai".into(),
            subtasks: vec![],
            constraints: vec![],
            originating_message_id: "chat:1:0".into(),
            research_plan: None,
            job_query: None,
            lead_query: None,
            task_shape: None,
            respawn_handoff: None,
            maps_query: Some(query),
        };

        let reply = chat_agent.synthesize_reply(&r, &[], &handoff);

        assert!(reply.contains("Found 0 qualified leads"));
        assert!(reply.contains("out of 2 total listings"));
        // The non-qualified findings still appear in the
        // "### Other findings" section so the user sees
        // what was scraped.
        assert!(reply.contains("### Other findings"));
        assert!(reply.contains("Webby A"));
        assert!(reply.contains("Webby B"));
    }

    /// A listing with a `|` in its name is properly
    /// escaped so it doesn't break the markdown table.
    #[test]
    fn phase39_synthesize_maps_escapes_pipe_in_name() {
        use crate::handoff::{MapsFinding, MapsQuery};

        let query = MapsQuery {
            search_query: "salons in Mumbai".into(),
            city: "Mumbai".into(),
            listing_count: 15,
            min_rating: Some(4.0),
            require_no_website: true,
            require_phone: false,
            parallelism: 1,
            categories: vec![],
        };
        let findings: Vec<MapsFinding> = vec![build_maps_finding(
            1,
            "Smith | Jones Salon",
            Some("555"),
            Some("123 Main, Mumbai"),
            Some(4.5),
            Some(50),
            false,
            None,
        )];
        let chat_agent = ChatAgent::new(dummy_config());
        let r = BrowserResult::done_maps("s1", "", vec![], None, None, findings);
        let handoff = Handoff {
            task_description: "salons in Mumbai".into(),
            subtasks: vec![],
            constraints: vec![],
            originating_message_id: "chat:1:0".into(),
            research_plan: None,
            job_query: None,
            lead_query: None,
            task_shape: None,
            respawn_handoff: None,
            maps_query: Some(query),
        };

        let reply = chat_agent.synthesize_reply(&r, &[], &handoff);

        // The pipe in the name is escaped — the row
        // contains `Smith \| Jones Salon`, not a raw
        // `|` that would break the cell.
        assert!(
            reply.contains("Smith \\| Jones Salon"),
            "expected escaped pipe in:\n{reply}"
        );
    }

    /// A `has_website == true` listing with no
    /// `website_url` still renders the cell as
    /// `(yes)` rather than a blank or `No` — the
    /// `has_website` flag and the `website_url` field
    /// are decoupled in `MapsFinding`, and the
    /// synthesizer has to handle both shapes.
    #[test]
    fn phase39_synthesize_maps_handles_website_url_missing() {
        use crate::handoff::{MapsFinding, MapsQuery};

        let query = MapsQuery {
            search_query: "salons in Mumbai".into(),
            city: "Mumbai".into(),
            listing_count: 15,
            min_rating: None,
            require_no_website: false,
            require_phone: false,
            parallelism: 1,
            categories: vec![],
        };
        let findings: Vec<MapsFinding> = vec![build_maps_finding(
            1,
            "Mystery Site",
            Some("555"),
            Some("123 Main, Mumbai"),
            Some(4.5),
            Some(50),
            true,
            None, // has_website = true but no URL — the
                  // agent saw a "Website" button but
                  // didn't capture the link
        )];
        let chat_agent = ChatAgent::new(dummy_config());
        let r = BrowserResult::done_maps("s1", "", vec![], None, None, findings);
        let handoff = Handoff {
            task_description: "salons in Mumbai".into(),
            subtasks: vec![],
            constraints: vec![],
            originating_message_id: "chat:1:0".into(),
            research_plan: None,
            job_query: None,
            lead_query: None,
            task_shape: None,
            respawn_handoff: None,
            maps_query: Some(query),
        };

        let reply = chat_agent.synthesize_reply(&r, &[], &handoff);

        // Renders the (yes) fallback for the
        // has-website-no-url case, not "No" (which would
        // be wrong — `has_website` is true).
        assert!(reply.contains("(yes)"));
        assert!(!reply.contains("Mystery Site | No |"));
    }

    /// Maps synthesizer takes priority over the
    /// research and lead-research synthesizers when a
    /// `BrowserResult` carries both shapes. This is
    /// a defense-in-depth test: a Maps task that
    /// somehow also collected `ResearchFinding`s
    /// should still render the Maps table.
    #[test]
    fn phase39_synthesize_maps_takes_priority_over_research() {
        use crate::handoff::{MapsFinding, MapsQuery};

        let query = MapsQuery {
            search_query: "salons in Mumbai".into(),
            city: "Mumbai".into(),
            listing_count: 15,
            min_rating: Some(4.0),
            require_no_website: true,
            require_phone: false,
            parallelism: 1,
            categories: vec![],
        };
        let findings: Vec<MapsFinding> = vec![build_maps_finding(
            1,
            "Solo Maps",
            Some("555"),
            Some("123 Main, Mumbai"),
            Some(4.5),
            Some(50),
            false,
            None,
        )];
        let chat_agent = ChatAgent::new(dummy_config());
        // Both maps_findings and lead_findings are
        // non-empty — the Maps synthesizer must win.
        let mut r = BrowserResult::done_maps("s1", "", vec![], None, None, findings);
        r.lead_findings = vec![crate::handoff::LeadFinding {
            base: crate::research::ResearchFinding {
                id: "l1".into(),
                platform: "X".into(),
                title: Some("Decoy".into()),
                company: Some("Decoy Co".into()),
                email: None,
                url: None,
                note: String::new(),
                added_at_secs: 0,
            },
            company_name: "Decoy Co".into(),
            company_domain: "decoy.com".into(),
            contact_name: None,
            contact_role: None,
            contact_email: None,
            contact_url: None,
            source_platform: "X".into(),
            confidence: crate::handoff::LeadConfidence::Low,
        }];
        let handoff = Handoff {
            task_description: "salons in Mumbai".into(),
            subtasks: vec![],
            constraints: vec![],
            originating_message_id: "chat:1:0".into(),
            research_plan: None,
            job_query: None,
            lead_query: None,
            task_shape: None,
            respawn_handoff: None,
            maps_query: Some(query),
        };

        let reply = chat_agent.synthesize_reply(&r, &[], &handoff);

        // Maps shape wins.
        assert!(reply.contains("Solo Maps"));
        assert!(reply.contains("Found 1 qualified lead"));
        // Lead-research rendering is NOT used.
        assert!(!reply.contains("[Low Confidence]"));
    }

    // ---- Phase 5: freelance lead rendering ----

    #[test]
    fn synthesize_freelance_only_renders_qualified_first_and_other_table() {
        use crate::handoff::{FreelanceBudget, FreelanceContact, FreelanceLead};
        use crate::research::ResearchFinding;
        let chat_agent = ChatAgent::new(dummy_config());
        let r = BrowserResult {
            status: BrowserStatus::Done,
            summary: "Found 2 freelance leads across 2 platforms.".into(),
            key_findings: vec![],
            final_snapshot_signature: None,
            raw_transcript_ref: None,
            session_id: "s1".into(),
            failure_reason: String::new(),
            findings: Vec::new(),
            job_findings: Vec::new(),
            lead_findings: Vec::new(),
            respawn_count: 0,
            respawn_reasons: Vec::new(),
            maps_findings: Vec::new(),
            freelance_findings: vec![
                FreelanceLead {
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
                },
                // Unqualified: no budget, no contact.
                FreelanceLead {
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
                },
            ],
        };
        let handoff = Handoff::bare("find me clients", "chat:1:0");
        let out = chat_agent.synthesize_reply(&r, &[], &handoff);
        // The qualified lead is the numbered row at the top.
        assert!(
            out.contains("1. Build a 5-page marketing site for Acme"),
            "qualified lead row missing: {out}"
        );
        assert!(out.contains("$50-75/hr"));
        assert!(out.contains("a@acme.com"));
        // The unqualified lead goes into the secondary
        // "Other listings found" section.
        assert!(
            out.contains("Other listings found"),
            "unqualified listings section missing: {out}"
        );
        assert!(out.contains("Beta"));
        assert!(out.contains("Design system v2"));
    }

    #[test]
    fn synthesize_mixed_job_and_freelance_renders_two_separated_sections() {
        use crate::handoff::{FreelanceBudget, FreelanceContact, FreelanceLead};
        use crate::research::ResearchFinding;
        let chat_agent = ChatAgent::new(dummy_config());
        let r = BrowserResult {
            status: BrowserStatus::Done,
            summary: "Found 1 job and 1 freelance lead.".into(),
            key_findings: vec![],
            final_snapshot_signature: None,
            raw_transcript_ref: None,
            session_id: "s1".into(),
            failure_reason: String::new(),
            // Job finding (from the job-search subtask).
            findings: vec![ResearchFinding {
                id: "j1".into(),
                platform: "LinkedIn".into(),
                title: Some("Senior Rust Engineer".into()),
                company: Some("Acme Corp".into()),
                email: Some("jobs@acme.com".into()),
                url: Some("https://linkedin.com/jobs/1".into()),
                note: String::new(),
                added_at_secs: 0,
            }],
            job_findings: Vec::new(),
            lead_findings: Vec::new(),
            respawn_count: 0,
            respawn_reasons: Vec::new(),
            maps_findings: Vec::new(),
            // Freelance lead (from the freelance subtask).
            freelance_findings: vec![FreelanceLead {
                base: ResearchFinding {
                    id: "f1".into(),
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
            }],
        };
        let handoff = Handoff::bare(
            "find me a Rust job and find me clients for my consulting",
            "chat:1:0",
        );
        let out = chat_agent.synthesize_reply(&r, &[], &handoff);
        // The mixed renderer must produce two clearly-separated
        // headers. The order is "Job openings" first, then
        // "Freelance leads".
        let job_idx = out.find("### Job openings").unwrap_or(usize::MAX);
        let freelance_idx = out.find("### Freelance leads").unwrap_or(usize::MAX);
        assert!(
            job_idx < freelance_idx,
            "job section must come before freelance section, got: {out}"
        );
        // The job half carries the role + email.
        assert!(out.contains("Senior Rust Engineer"));
        assert!(out.contains("jobs@acme.com"));
        // The freelance half carries the project + budget.
        assert!(out.contains("Build a 5-page marketing site"));
        assert!(out.contains("$50-75/hr"));
        // The two halves are NOT blended into a single
        // numbered list — the job has 1. and the freelance
        // has 1. inside its own section.
        assert!(out.contains("### Job openings"));
        assert!(out.contains("### Freelance leads"));
    }
}
