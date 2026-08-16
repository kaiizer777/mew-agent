//! Phase 8 — Personal workflow presets.
//!
//! Background: the user wants a daily-use personal tool, not a
//! project they have to hand-configure every time. The first half of
//! that is a *named* preset they can trigger with a one-line CLI
//! command (`mew run-preset daily_remote_jobs`) and not have to
//! retype the same compound instruction every Monday morning.
//!
//! This module:
//!
//!   1. Defines the typed `TaskPreset` shape (a name, a description,
//!      a `PresetShape` that picks a default platform set, and a
//!      list of default subtasks).
//!   2. Ships three realistic seed presets:
//!      * `daily_remote_jobs` — job search on the four main remote
//!        boards (LinkedIn, Indeed, Wellfound, WeWorkRemotely).
//!        Built for the "I have 30 minutes on Monday morning" use
//!        case.
//!      * `daily_freelance_leads` — freelance client lead search
//!        on Upwork + Contra + WeWorkRemotely-Contract.
//!      * `weekly_full_search` — the union of both, with a per-run
//!        cap on applications submitted (caller can still override).
//!   3. Provides `resolve_preset(name, presets)` to look up a preset
//!      by name from the user's `config.yaml` map, and
//!      `build_handoff_for_preset(...)` to turn a resolved preset
//!      into the same `Handoff` struct the rest of the orchestrator
//!      already consumes — no new wiring downstream.
//!
//! Design notes:
//!   * The seed presets mirror the existing
//!     `default_job_board_platforms()` / `default_freelance_platforms()`
//!     lists so the system prompt and the per-platform budget guard
//!     behave identically to a `JobSearch` / `LeadGen` task shaped
//!     by the LLM classifier.
//!   * The user can override any preset in `config.yaml` by writing
//!     a new entry under `agent.task_presets: { <name>: "<task
//!     string>" }`. The CLI's `mew run --preset <name>` path is the
//!     legacy free-form-text path (Phase 17); the new
//!     `mew run-preset <name>` path here is structured, so the
//!     preset name in `config.yaml` is ignored for the
//!     `run-preset` subcommand (the preset is a first-class object
//!     in this module, not a `String`).
//!   * Every preset is hard-clamped: at most 6 platforms
//!     (the 4+2 job/freelance shape) so an operator can't fat-finger
//!     a 30-platform preset that burns 4 hours of token budget.
//!
//! ## Public surface
//!
//! ```ignore
//! pub fn default_presets() -> HashMap<String, TaskPreset>
//! pub fn resolve_preset<'a>(name: &str, presets: &'a HashMap<String, TaskPreset>) -> Option<&'a TaskPreset>
//! pub fn build_handoff_for_preset(preset: &TaskPreset, originating_message_id: &str) -> Handoff
//! ```
//!
//! See `mew-agent/tests/phase8_presets.rs` for the typed fixture
//! tests. See `docs/personal-use-setup.md` for the user-facing
//! setup flow.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::handoff::{Handoff, HandoffSubTask};
use crate::research::{
    default_freelance_platforms, default_job_board_platforms, ResearchSubTask,
};
use crate::router::TaskShape;

/// Hard cap on the number of platforms a single preset can
/// fan out to. The seed presets use 4 (jobs) and 3 (freelance)
/// with the union shape running 6. Operators can define a
/// smaller preset in `config.yaml` (any size ≤ 6) but a
/// misconfigured 30-platform preset is silently clamped to
/// `MAX_PLATFORMS_PER_PRESET` so a typo can't blow the
/// per-run token budget.
pub const MAX_PLATFORMS_PER_PRESET: usize = 6;

/// The shape a preset fans out to. Mirrors the existing
/// `TaskShape` taxonomy but is a discrete enum (not
/// `TaskShape::JobSearch` etc.) so future preset shapes don't
/// have to be a router-recognized task shape — `daily_local_map`
/// for example could be a preset-specific shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresetShape {
    /// A job-search fanout using `default_job_board_platforms()`.
    /// Subtasks = one per remote job board. `task_shape` field
    /// on the resulting `Handoff` is `Some(TaskShape::JobSearch)`.
    DailyRemoteJobs,
    /// A freelance lead search using `default_freelance_platforms()`
    /// (the Upwork / Contra / WeWorkRemotely-Contract / etc. list).
    /// `task_shape` is `Some(TaskShape::LeadGen)`.
    DailyFreelanceLeads,
    /// The union: a "weekly comprehensive sweep." Both platform
    /// lists are concatenated, then the first `MAX_PLATFORMS_PER_PRESET`
    /// rows are kept. `task_shape` is `None` (the chat surface
    /// treats this as a mixed vertical task).
    WeeklyFullSearch,
}

impl PresetShape {
    /// Resolve the platform list this preset shape fans out to.
    /// Returns a `Vec<ResearchSubTask>` ordered exactly the way
    /// the seed `default_*_platforms()` lists are — LinkedIn
    /// first, then Indeed, etc. so the synthesizer's
    /// platform-deduping is stable.
    pub fn platforms(&self) -> Vec<ResearchSubTask> {
        match self {
            PresetShape::DailyRemoteJobs => default_job_board_platforms(),
            PresetShape::DailyFreelanceLeads => default_freelance_platforms(),
            PresetShape::WeeklyFullSearch => {
                let mut combined: Vec<ResearchSubTask> = default_job_board_platforms();
                combined.extend(default_freelance_platforms());
                combined.truncate(MAX_PLATFORMS_PER_PRESET);
                combined
            }
        }
    }

    /// The `TaskShape` the orchestrator's `ChatAgent::build_handoff`
    /// paths will use to template the system prompt. The
    /// `WeeklyFullSearch` shape is mixed-vertical and returns
    /// `None` (the chat synthesizer's "mixed" branch handles it).
    pub fn task_shape(&self) -> Option<TaskShape> {
        match self {
            PresetShape::DailyRemoteJobs => Some(TaskShape::JobSearch),
            PresetShape::DailyFreelanceLeads => Some(TaskShape::LeadGen),
            PresetShape::WeeklyFullSearch => None,
        }
    }

    /// Short human description for the `mew list-presets` subcommand
    /// and the Tauri command that surfaces the preset catalog to
    /// the chat list.
    pub fn description(&self) -> &'static str {
        match self {
            PresetShape::DailyRemoteJobs => {
                "Daily Monday-morning remote job search across LinkedIn / Indeed / Wellfound / WeWorkRemotely / RemoteOK"
            }
            PresetShape::DailyFreelanceLeads => {
                "Daily freelance client lead search across Upwork / Contra / WeWorkRemotely-Contract / Freelancer.com / Guru"
            }
            PresetShape::WeeklyFullSearch => {
                "Weekly comprehensive sweep: both job and freelance verticals in one run"
            }
        }
    }
}

/// A named, structured preset the user can fire with
/// `mew run-preset <name>`. The seed presets in
/// `default_presets()` cover the three realistic weekly-use cases;
/// operators can also define custom presets in `config.yaml` but
/// the typed `TaskPreset` shape is not user-editable from
/// `config.yaml` — only the legacy free-form `task_presets` map
/// (the Phase 17 path) is user-editable, and `run-preset` is the
/// new structured path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPreset {
    /// Human-friendly name shown in `mew list-presets` and the
    /// Tauri command output. Stable, snake_case.
    pub name: String,
    /// Short sentence the user sees in the preset catalog.
    pub summary: String,
    /// The shape (drives the platform list and the `TaskShape`
    /// on the Handoff).
    pub shape: PresetShape,
    /// Per-preset overrides for the per-platform step and time
    /// budgets. Optional; when `None` the defaults from
    /// `ResearchSubTask` are used as-is. When `Some(map)`, the
    /// platform-id key (e.g. "linkedin") maps to `(step_budget,
    /// time_budget_secs)`. Platforms not in the override map
    /// keep their default budgets.
    ///
    /// Stored as `Option<HashMap<String, (u32, u64)>>` for
    /// forward-compat: a future "lightning" preset could halve
    /// every budget. For now the seed presets don't use it —
    /// keeping the field in the schema so a future operator
    /// override doesn't have to migrate the struct shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_overrides: Option<HashMap<String, (u32, u64)>>,
}

impl TaskPreset {
    /// One-line description used in the `mew list-presets`
    /// subcommand output and the Tauri command that surfaces
    /// the preset catalog. Stable so a chat-list reference
    /// ("available presets: ...") is byte-identical across runs.
    pub fn catalog_line(&self) -> String {
        format!("{}: {}", self.name, self.shape.description())
    }
}

/// The three seed presets a fresh `mew` install ships with.
/// Stable, never user-mutated from `config.yaml` (the
/// `task_presets: HashMap<String, String>` legacy map is the
/// override surface for free-form text). The names match
/// `work.md` Phase 8 spec verbatim.
pub fn default_presets() -> HashMap<String, TaskPreset> {
    let mut m = HashMap::new();
    m.insert(
        "daily_remote_jobs".into(),
        TaskPreset {
            name: "daily_remote_jobs".into(),
            summary: "Daily Monday-morning remote job search across LinkedIn / Indeed / \
                     Wellfound / WeWorkRemotely / RemoteOK."
                .into(),
            shape: PresetShape::DailyRemoteJobs,
            budget_overrides: None,
        },
    );
    m.insert(
        "daily_freelance_leads".into(),
        TaskPreset {
            name: "daily_freelance_leads".into(),
            summary: "Daily freelance client lead search across Upwork / Contra / \
                     WeWorkRemotely-Contract / Freelancer.com / Guru."
                .into(),
            shape: PresetShape::DailyFreelanceLeads,
            budget_overrides: None,
        },
    );
    m.insert(
        "weekly_full_search".into(),
        TaskPreset {
            name: "weekly_full_search".into(),
            summary: "Weekly comprehensive sweep: both job and freelance verticals in one run."
                .into(),
            shape: PresetShape::WeeklyFullSearch,
            budget_overrides: None,
        },
    );
    m
}

/// Look up a preset by name. Returns `None` when the name is
/// not in the map (caller can render "preset '<name>' not found;
/// available presets: ..." to the user).
///
/// Note: the lookup is exact-match on the preset name. The
/// legacy `task_presets: HashMap<String, String>` map in
/// `AgentConfig` is a separate surface (free-form task text
/// keyed by name) and is *not* consulted here — `run-preset`
/// is the structured path, `run --preset` is the legacy
/// free-form path.
pub fn resolve_preset<'a>(
    name: &str,
    presets: &'a HashMap<String, TaskPreset>,
) -> Option<&'a TaskPreset> {
    presets.get(name)
}

/// Merge the seed `default_presets()` with the operator's
/// `task_preset_overrides` map (from `AgentConfig`). Operator
/// overrides win on a per-name basis — any preset in both maps
/// uses the operator's `TaskPreset` (so an operator can change
/// the budget_overrides or summary without losing the seed
/// platform list — they just clone the seed and edit).
///
/// The merged map is what `mew run-preset <name>` and the Tauri
/// `list_presets` command should consult. The seed defaults
/// are never mutated.
pub fn merged_presets(overrides: &HashMap<String, TaskPreset>) -> HashMap<String, TaskPreset> {
    let mut merged = default_presets();
    for (name, preset) in overrides {
        merged.insert(name.clone(), preset.clone());
    }
    merged
}

/// Render the available-preset catalog as a single Markdown
/// string for the `mew list-presets` subcommand and the Tauri
/// command that surfaces the catalog to the chat list. Each
/// preset gets a one-line description plus the full
/// `shape.description()` blurb. Output is deterministic so
/// golden-transcript tests can diff against it.
pub fn render_preset_catalog(presets: &HashMap<String, TaskPreset>) -> String {
    let mut names: Vec<&str> = presets.keys().map(String::as_str).collect();
    names.sort_unstable();
    let mut out = String::from("Available presets:\n");
    for name in names {
        if let Some(preset) = presets.get(name) {
            out.push_str(&format!(
                "  - {} — {}\n",
                preset.name,
                preset.shape.description()
            ));
        }
    }
    out
}

/// Build a `Handoff` from a resolved `TaskPreset`. The returned
/// struct is the same shape `ChatAgent::build_handoff` produces
/// for a `JobSearch` / `LeadGen` task, so downstream wiring
/// (the `CompletenessTracker`, the per-platform budget guard,
/// the synthesizer) does not need to special-case "this came
/// from a preset."
///
/// The `Handoff` carries:
///   * `task_description` — a templated one-line description
///     that includes the preset name so the chat reply
///     ("Running preset `daily_remote_jobs` ...") is traceable
///     back to the user's command line.
///   * `subtasks` — one `HandoffSubTask` per platform, with a
///     descriptive `description` field that the LLM can read
///     in the system prompt and `mark_subtask_done` with.
///   * `task_shape` — `Some(TaskShape::JobSearch)` for
///     `DailyRemoteJobs`, `Some(TaskShape::LeadGen)` for
///     `DailyFreelanceLeads`, `None` for `WeeklyFullSearch`.
///   * `research_plan` — `None` for presets. The
///     `ResearchPlanner` is the LLM-driven long-horizon path;
///     presets are a deterministic "do this exact fanout"
///     surface. A future enhancement could populate this with
///     a synthesized `ResearchPlan` for presets; for now the
///     subtask list is the canonical source of truth.
pub fn build_handoff_for_preset(preset: &TaskPreset, originating_message_id: &str) -> Handoff {
    let platforms = preset.shape.platforms();
    let subtasks: Vec<HandoffSubTask> = platforms
        .iter()
        .map(|p| HandoffSubTask {
            id: p.id.clone(),
            description: p.to_declare_item().description,
        })
        .collect();

    let task_description = format!(
        "Running preset `{}` ({}). Visiting {} platform(s): {}",
        preset.name,
        preset.shape.description(),
        platforms.len(),
        platforms
            .iter()
            .map(|p| p.platform.as_str())
            .collect::<Vec<&str>>()
            .join(", ")
    );

    Handoff {
        task_description,
        subtasks,
        constraints: Vec::new(),
        originating_message_id: originating_message_id.to_string(),
        research_plan: None,
        job_query: None,
        lead_query: None,
        task_shape: preset.shape.task_shape(),
        respawn_handoff: None,
        maps_query: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_presets_has_three_named_seeds() {
        let p = default_presets();
        assert!(p.contains_key("daily_remote_jobs"));
        assert!(p.contains_key("daily_freelance_leads"));
        assert!(p.contains_key("weekly_full_search"));
        assert_eq!(p.len(), 3, "no extra seed presets allowed");
    }

    #[test]
    fn daily_remote_jobs_fans_out_to_five_job_boards() {
        let p = default_presets();
        let preset = p.get("daily_remote_jobs").unwrap();
        let platforms = preset.shape.platforms();
        assert_eq!(platforms.len(), 5, "daily_remote_jobs = 5 job boards");
        let ids: Vec<&str> = platforms.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"linkedin"));
        assert!(ids.contains(&"indeed"));
        assert!(ids.contains(&"wellfound"));
        assert!(ids.contains(&"weworkremotely"));
        assert!(ids.contains(&"remoteok"));
    }

    #[test]
    fn daily_freelance_leads_fans_out_to_freelance_boards() {
        let p = default_presets();
        let preset = p.get("daily_freelance_leads").unwrap();
        let platforms = preset.shape.platforms();
        assert!(
            platforms.len() >= 3,
            "daily_freelance_leads must include Upwork + Contra + WWR-Contract"
        );
        let ids: Vec<&str> = platforms.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"upwork"));
        assert!(ids.contains(&"contra"));
        assert!(ids.iter().any(|id| id.contains("wework") || id.contains("contract")));
    }

    #[test]
    fn weekly_full_search_combines_both_and_clamps_to_six() {
        let p = default_presets();
        let preset = p.get("weekly_full_search").unwrap();
        let platforms = preset.shape.platforms();
        assert!(
            platforms.len() <= MAX_PLATFORMS_PER_PRESET,
            "weekly_full_search must clamp to {} platforms (got {})",
            MAX_PLATFORMS_PER_PRESET,
            platforms.len()
        );
        // Union must contain at least one job-board and one
        // freelance-board id so the synthesizer's
        // "mixed vertical" branch actually fires.
        let ids: Vec<&str> = platforms.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"linkedin"));
        assert!(ids.contains(&"upwork") || ids.contains(&"contra"));
    }

    #[test]
    fn preset_shape_task_shape_for_orchestrator_dispatch() {
        // `daily_remote_jobs` => JobSearch, `daily_freelance_leads`
        // => LeadGen, `weekly_full_search` => None (mixed).
        let p = default_presets();
        assert_eq!(
            p.get("daily_remote_jobs").unwrap().shape.task_shape(),
            Some(TaskShape::JobSearch)
        );
        assert_eq!(
            p.get("daily_freelance_leads").unwrap().shape.task_shape(),
            Some(TaskShape::LeadGen)
        );
        assert_eq!(
            p.get("weekly_full_search").unwrap().shape.task_shape(),
            None
        );
    }

    #[test]
    fn resolve_preset_returns_some_for_known_and_none_for_unknown() {
        let p = default_presets();
        assert!(resolve_preset("daily_remote_jobs", &p).is_some());
        assert!(resolve_preset("daily_freelance_leads", &p).is_some());
        assert!(resolve_preset("weekly_full_search", &p).is_some());
        assert!(resolve_preset("does_not_exist", &p).is_none());
    }

    #[test]
    fn build_handoff_for_preset_populates_subtasks_and_task_shape() {
        let p = default_presets();
        let preset = p.get("daily_remote_jobs").unwrap();
        let h = build_handoff_for_preset(preset, "msg-1");
        assert_eq!(h.subtasks.len(), 5);
        assert_eq!(h.task_shape, Some(TaskShape::JobSearch));
        assert!(h.task_description.contains("daily_remote_jobs"));
        assert!(h.task_description.contains("LinkedIn"));
        assert!(h.task_description.contains("Indeed"));
        // The Handoff must NOT carry a research_plan —
        // presets are the deterministic fanout path; the
        // research_plan is the LLM-driven long-horizon path.
        assert!(h.research_plan.is_none());
        // Subtask ids are stable and match the platform ids.
        for (sub, plat) in h.subtasks.iter().zip(preset.shape.platforms().iter()) {
            assert_eq!(sub.id, plat.id);
        }
    }

    #[test]
    fn build_handoff_for_weekly_full_search_has_mixed_task_shape() {
        let p = default_presets();
        let preset = p.get("weekly_full_search").unwrap();
        let h = build_handoff_for_preset(preset, "msg-2");
        assert_eq!(h.task_shape, None, "weekly_full_search = mixed vertical");
        assert!(h.subtasks.len() > 4);
        assert!(h.subtasks.len() <= MAX_PLATFORMS_PER_PRESET);
    }

    #[test]
    fn render_preset_catalog_is_deterministic_and_lists_all_three() {
        let p = default_presets();
        let cat = render_preset_catalog(&p);
        assert!(cat.contains("daily_remote_jobs"));
        assert!(cat.contains("daily_freelance_leads"));
        assert!(cat.contains("weekly_full_search"));
        // Sorted order: daily_freelance_leads, daily_remote_jobs,
        // weekly_full_search (alphabetical).
        let pos_freelance = cat.find("daily_freelance_leads").unwrap();
        let pos_jobs = cat.find("daily_remote_jobs").unwrap();
        let pos_weekly = cat.find("weekly_full_search").unwrap();
        assert!(pos_freelance < pos_jobs);
        assert!(pos_jobs < pos_weekly);
    }

    #[test]
    fn catalog_line_is_stable_across_runs() {
        // The catalog line is what the user sees in `mew
        // list-presets` output and the Tauri command that
        // surfaces the catalog to the chat list. Stability
        // across runs is required for golden-transcript
        // diffs.
        let p = default_presets();
        let preset = p.get("daily_remote_jobs").unwrap();
        assert_eq!(
            preset.catalog_line(),
            "daily_remote_jobs: Daily Monday-morning remote job search across LinkedIn / Indeed / Wellfound / WeWorkRemotely / RemoteOK"
        );
    }

    #[test]
    fn unknown_preset_name_resolution_returns_none() {
        // Negative case: the chat surface / CLI surfaces a
        // "preset '<name>' not found" message; this confirms
        // the lookup itself is the gate.
        let p = default_presets();
        assert!(resolve_preset("", &p).is_none());
        assert!(resolve_preset("DAILY_REMOTE_JOBS", &p).is_none(), "case-sensitive");
    }

    #[test]
    fn merged_presets_operator_overrides_win() {
        // Operator adds a custom preset; the seed defaults
        // stay intact, and the merged map has both. An
        // operator can also override the summary on a
        // seed preset.
        let mut overrides = HashMap::new();
        let mut custom = TaskPreset {
            name: "my_custom".into(),
            summary: "operator-defined preset".into(),
            shape: PresetShape::DailyRemoteJobs,
            budget_overrides: None,
        };
        overrides.insert("my_custom".into(), custom.clone());
        let mut overridden = default_presets().get("daily_remote_jobs").unwrap().clone();
        overridden.summary = "operator-edited summary".into();
        overrides.insert("daily_remote_jobs".into(), overridden.clone());

        let merged = merged_presets(&overrides);
        assert_eq!(merged.len(), 4, "3 seeds + 1 custom");
        assert_eq!(merged.get("daily_remote_jobs").unwrap().summary, "operator-edited summary");
        assert_eq!(merged.get("my_custom").unwrap().summary, "operator-defined preset");
        // Non-overridden seeds survive.
        assert!(merged.contains_key("daily_freelance_leads"));
        assert!(merged.contains_key("weekly_full_search"));
        let _ = &mut custom;
    }

    #[test]
    fn merged_presets_empty_overrides_returns_seeds_intact() {
        // No operator overrides — the merged map is the
        // seed defaults, identical by value.
        let overrides = HashMap::new();
        let merged = merged_presets(&overrides);
        assert_eq!(merged, default_presets());
    }
}
