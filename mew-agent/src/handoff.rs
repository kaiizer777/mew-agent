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

use crate::research::{ResearchFinding, ResearchPlan};
use crate::todo::{TodoId, TodoStatus};

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
        }
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    /// field existed (back-compat with the Phase 3 serializer
    /// contract).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub research_plan: Option<ResearchPlan>,
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
        }
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
            }],
            synthesis_hint: "One row per role".into(),
            overall_deadline_secs: Some(600),
            is_research: true,
            matched_pattern: "research_keyword".into(),
        };
        let h = Handoff::with_research_plan(
            "find rust jobs",
            "chat:1:0",
            plan.clone(),
        );
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
        assert!(!json.contains("research_plan"), "json leaked the optional field: {json}");
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
}
