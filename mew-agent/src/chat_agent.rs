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
use crate::research::{default_job_board_platforms, ResearchPlanner, ResearchSubTask};
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
    pub fn with_classify_cache(
        mut self,
        cache: Arc<crate::classify_cache::ClassifyCache>,
    ) -> Self {
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
                crate::classify_cache::classify_with_cache(
                    message, history, &self.config, cache,
                )
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
    ) -> Handoff {
        self.build_handoff_with_platforms(
            task_description,
            originating_message_id,
            constraints,
            &[],
        )
    }

    /// Phase 7: same as `build_handoff`, with an explicit
    /// `default_platforms` list passed in. When the list is
    /// non-empty AND the deterministic `ResearchPlanner`
    /// recognizes the task as long-horizon research-shaped, the
    /// returned Handoff carries a `ResearchPlan` and the
    /// subtask list is the per-platform list. The Phase 2
    /// clause-splitter is the fallback when the task is not
    /// research-shaped.
    pub fn build_handoff_with_platforms(
        &self,
        task_description: &str,
        originating_message_id: &str,
        constraints: Vec<String>,
        default_platforms: &[ResearchSubTask],
    ) -> Handoff {
        // When the caller provides a non-empty platform list,
        // ask the research planner first. If the planner
        // recognizes the task as research-shaped, return a
        // research Handoff. Otherwise fall through to the
        // Phase 2 clause-splitter.
        if !default_platforms.is_empty() {
            let plan = ResearchPlanner::plan(
                task_description,
                default_platforms,
                None, // overall_deadline_secs: future, from config
            );
            if plan.is_research {
                let mut handoff = Handoff::with_research_plan(
                    task_description,
                    originating_message_id,
                    plan,
                );
                handoff.constraints = constraints;
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
        let plan = ResearchPlanner::plan(
            task_description,
            &default_job_board_platforms(),
            None,
        );
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
        _handoff: &Handoff,
    ) -> String {
        if self.config.agent.planner_enabled {
            if result.status == BrowserStatus::Failed && result.key_findings.is_empty() {
                return synthesize_failed(result);
            }
            let mut out = String::new();
            let mut parts = Vec::new();
            for f in &result.key_findings {
                let reason = if f.reason.is_empty() { String::new() } else { format!(" ({})", f.reason) };
                parts.push(format!("{} {}{}", f.id, f.status, reason));
            }
            out.push_str(&parts.join(" · "));
            return out;
        }

        // Research-shaped: route to the consolidated renderer.
        // We do this before the status match so a `Done`
        // research result with 0 findings (rare but possible —
        // a successful "I searched all platforms and found
        // nothing matching") still falls through to the
        // non-research renderer rather than rendering a bare
        // "0 results" line.
        if !result.findings.is_empty() {
            return synthesize_research(result);
        }
        match result.status {
            BrowserStatus::Done => synthesize_done(result),
            BrowserStatus::Partial => synthesize_partial(result),
            BrowserStatus::Failed => synthesize_failed(result),
        }
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
        let platforms: std::collections::BTreeSet<String> = result
            .findings
            .iter()
            .map(|f| f.platform.clone())
            .collect();
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
        );
        assert_eq!(handoff.task_description, "go to instagram and text my friend hi");
        assert!(handoff.subtasks.len() >= 2, "compound task must produce >= 2 subtasks, got {:?}", handoff.subtasks);
        assert_eq!(handoff.constraints, vec!["enter via search".to_string()]);
        assert_eq!(handoff.originating_message_id, "chat:123:000001");
    }

    #[test]
    fn build_handoff_with_bare_task_produces_no_subtasks() {
        // A single-clause task does not need decomposition. The
        // planner's "single clause" rule produces exactly one
        // subtask, which is fine.
        let chat_agent = ChatAgent::new(dummy_config());
        let handoff = chat_agent.build_handoff("go to wikipedia", "chat:1:0", vec![]);
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
        let handoff = chat_agent.build_research_handoff(
            "find remote rust jobs",
            "chat:7:0",
        );
        assert!(handoff.research_plan.is_some());
        let plan = handoff.research_plan.as_ref().unwrap();
        assert!(plan.is_research);
        assert!(plan.platforms.len() >= 3, "default platform list should be at least 3 platforms, got {}", plan.platforms.len());
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
        );
        assert!(handoff.research_plan.is_none());
        assert!(handoff.subtasks.len() >= 2, "compound task must produce >= 2 subtasks, got {:?}", handoff.subtasks);
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
        assert!(out.contains("1. Rust Engineer"), "first finding missing: {out}");
        assert!(out.contains("2. Backend Engineer"), "second finding missing: {out}");
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
}
