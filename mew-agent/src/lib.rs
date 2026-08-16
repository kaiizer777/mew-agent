pub mod agent;
pub mod application_dedup;
pub mod auth;
pub mod auto_apply;
pub mod budget;
pub mod captcha_solver;
pub mod captcha_telemetry;
pub mod chat;
pub mod chat_agent;
pub mod classify_cache;
pub mod completeness;
pub mod end_of_task_passthrough;
pub mod handoff;
pub mod lead_scorer;
pub mod orchestrator;
// Phase 41: lead-gen result export. Writes the ranked
// `MapsFinding` list (the output of `LocalLeadScorer`) to
// disk in the operator's chosen format (CSV / JSON /
// Markdown) at task completion, and surfaces the file path
// as a `file://` link in the chat reply. Pure-Rust
// post-processing — no LLM calls, no async, no network.
// Backed by the `csv` crate for the CSV writer; JSON goes
// through `serde_json::to_string_pretty`; Markdown uses the
// same hand-rolled table format as `chat_agent::synthesize_maps`
// (the in-chat table and the on-disk Markdown export are
// byte-equivalent for the same input). See
// `docs/phase41-benchmarks.md` for the design rationale and
// `exporter::export_findings` for the call surface.
pub mod exporter;
pub mod pacing;
pub mod planner;
// Phase 36: agent-optimized task prompt compiler. Pure-Rust
// pre-flight normalization of the raw user task into an
// agent-executable form (no LLM call, no `regex` crate).
// Public surface: `mew_agent::prompt_compiler::compile`. Wired
// into `Agent::new_with_tracing` to clean the task string and
// inject per-vertical `EXECUTION HINTS:` blocks into the system
// prompt. See `docs/phase36-prompt-compiler.md` for the design
// rationale and the deterministic rule set.
pub mod prompt_compiler;
pub use planner::{Plan, Planner};
pub mod research;
pub mod resilience;
pub mod respawn_policy;
pub mod router;
pub mod session;
pub mod session_summary;
pub mod summarizer;
pub mod supervisor;
pub mod todo;
pub mod tracing_layer;
pub mod worker;
pub mod worker_pool;

#[cfg(feature = "eval")]
pub mod eval;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpencodeZenConfig {
    pub base_url: String,
    pub api_key: String,
    pub default_model: String,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
    #[serde(default)]
    pub max_tokens: Option<usize>,
    #[serde(default)]
    pub max_cost: Option<f64>,
}

fn default_max_iterations() -> usize {
    15
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BrowserConfig {
    pub binary_path: Option<String>,
    /// Phase 16.1: when true, a fixed-position ghost cursor is injected on
    /// every navigation and visibly moves to each click target before the
    /// real click fires. Default false — zero overhead, zero CDP calls.
    #[serde(default)]
    pub visible_cursor: bool,
}

/// Phase 4: Google credential for single-credential authenticated sessions.
/// The user supplies env var names; the actual values are read at runtime
/// and never stored in config or written to trace logs.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GoogleCredential {
    /// Env var name holding the Google email address (e.g. "GOOGLE_EMAIL").
    pub email_env: String,
    /// Env var name holding the Google password (e.g. "GOOGLE_PASSWORD").
    pub password_env: String,
}

impl GoogleCredential {
    /// Read the actual email/password from the environment.
    /// Returns `None` if either env var is missing or empty.
    pub fn read_from_env(&self) -> Option<(String, String)> {
        let email = std::env::var(&self.email_env).ok().filter(|s| !s.is_empty())?;
        let password = std::env::var(&self.password_env).ok().filter(|s| !s.is_empty())?;
        Some((email, password))
    }
}

/// Phase 4: credential config block. All supported platforms
/// authenticate through the single Google account.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CredentialsConfig {
    #[serde(default)]
    pub google: Option<GoogleCredential>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub task_presets: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub allowed_domains: Option<Vec<String>>,
    /// Phase 4: single-credential auth. The user supplies one Gmail
    /// account; the agent uses "Sign in with Google" wherever it
    /// appears. Credentials are read from env vars at runtime,
    /// never stored in plaintext in config.
    #[serde(default)]
    pub credentials: CredentialsConfig,
    /// Phase 17.1: per-action-type pacing guard. When `enabled` is
    /// true, consecutive identical actions in a tight loop (no
    /// different action type between them) are spaced out by a
    /// random delay in `[min_delay_ms, max_delay_ms]`. Default is
    /// off — pacing is opt-in so existing task patterns aren't
    /// silently slowed.
    #[serde(default)]
    pub pacing: pacing::PacingConfig,
    /// Phase 5: live step summarization. Controls verbosity
    /// (concise vs. detailed), the live progress buffer cap,
    /// and whether the end-of-task LLM summarizer is on.
    /// Default: concise, 5 lines, LLM on.
    #[serde(default)]
    pub summarization: summarizer::SummarizationConfig,
    /// Phase 7: long-horizon research loop config. When
    /// `enabled` is true, the orchestrator's handoff builder
    /// asks the `ResearchPlanner` first and the synthesizer
    /// routes research-shaped `BrowserResult`s to the
    /// consolidated renderer. The `platforms` list is
    /// operator-editable; when empty, the agent falls back
    /// to `default_job_board_platforms()` so a default
    /// `mew-agent` install still works out of the box.
    #[serde(default)]
    pub research: research::ResearchConfig,
    /// Phase 8: CAPTCHA / challenge page handling. The
    /// default is fully disabled — the agent pauses the
    /// session and messages the user when a challenge is
    /// detected. Setting `enabled: true` enables an
    /// opt-in path to delegate the challenge to a solving
    /// service; **the `mew-agent` build today ships with
    /// no solver implementation**, so even with
    /// `enabled: true` the runtime falls back to the
    /// pause-and-message path with a warning trace
    /// event. See `mew_agent::captcha_solver` for the
    /// extension point and
    /// `docs/phase8-captcha-handling.md` for the
    /// cost / ToS caveat.
    #[serde(default)]
    pub captcha: captcha_solver::CaptchaConfig,
    /// Phase 14 / 16: when true, user browser tasks flow through the new
    /// outer Planner loop (classify -> decompose to todos -> supervisor ->
    /// evidence gate). Default false — legacy single ReAct loop path active.
    #[serde(default)]
    pub planner_enabled: bool,
    /// Phase 24: Episodic memory retrieval & prompt injection configuration.
    /// `enabled` defaults to true, `max_lessons_per_task` defaults to 5,
    /// `min_lesson_age_secs` defaults to 0.
    #[serde(default)]
    pub memory: MemoryConfig,
    /// Phase 32: maximum number of tool calls dispatched per LLM turn.
    /// The LLM API supports returning multiple tool calls in one
    /// response. When `max_tools_per_turn > 1`, the loop iterates
    /// over `calls[0..max_tools_per_turn]` and dispatches each
    /// before the next LLM round-trip. Default 1 = single-action
    /// per turn (byte-for-byte equivalent to pre-Phase-32 behavior).
    /// For enumeration / lead-gen tasks on SPA sites (Maps, Yelp),
    /// setting this to 5 cuts LLM round-trips by 2.5-5x — the
    /// single biggest wall-clock speedup available.
    #[serde(default = "default_max_tools_per_turn")]
    pub max_tools_per_turn: usize,
    /// Phase 35: how many recent assistant+tool message pairs the
    /// loop keeps in `self.messages` after a `NoChange` perception
    /// (the per-iteration `trim_in_page_history` call). The default
    /// of 5 reproduces the pre-Phase-35 hardcoded value byte-for-byte.
    /// The `run_preflight_plan` step auto-overrides this to
    /// `min(detected_count + 5, 50)` when the task string matches an
    /// enumeration pattern like "check 15 listings" or "top 20
    /// results" — the +5 headroom lets the agent see the last
    /// listing plus the current one plus a few context messages.
    /// The config loader hard-clamps to `[3, 50]`: values below 3
    /// starve the agent of context, values above 50 risk ballooning
    /// the prompt beyond the model's effective window.
    #[serde(default = "default_in_page_history_cap")]
    pub in_page_history_cap: usize,
    /// Phase 38: optional cap on the size of the AX tree the
    /// perception block will serialize. Default `None` = unlimited,
    /// matching the pre-Phase-38 behavior. Set to a positive value
    /// (recommended: 800 for Maps tasks) to truncate the serialized
    /// tree at that node count — the diff and the LLM only care
    /// about the interactive / content-bearing nodes, and a Maps
    /// panel's full tree typically exceeds 2000 nodes (most are map
    /// tiles, canvas elements, and irrelevant viewport geometry).
    /// Truncation happens during serialization, so the underlying
    /// `TreeNode` used by the diff and the resilience hook is
    /// untouched.
    #[serde(default)]
    pub max_ax_tree_nodes: Option<usize>,
    /// Phase 41: lead-gen result export. When `enabled` is
    /// true, the orchestrator's Maps completion path writes
    /// the ranked findings to disk in the requested format
    /// (`csv` / `json` / `markdown`) under `output_dir`
    /// (default: `<cwd>/exports/` — the agent is intentionally
    /// filesystem-agnostic, the Tauri UI calls
    /// `open_export_directory` to surface the OS file
    /// manager). The exported file path is appended to the
    /// chat reply as a `file://` link so the user can open
    /// it from the chat bubble. Default disabled — a
    /// no-export mode matches the pre-Phase-41 chat-only
    /// delivery byte-for-byte. The `enabled: false` path
    /// is a hard no-op: no file written, no link in the
    /// chat reply, no observable change in the user
    /// experience.
    #[serde(default)]
    pub export: ExportConfig,
    /// Phase 6: application / proposal / connection-request
    /// auto-apply action layer. When `enabled` is true, the
    /// agent will draft application / pitch text on `Paused`
    /// confirmation checkpoints and track submitted
    /// applications in a local dedup store at
    /// `<data_dir>/applications_sent.json`. Default disabled
    /// — even with `enabled: true`, the agent does not
    /// auto-submit: the existing `Paused`-checkpoint
    /// irreversible-action gate is the *only* path to
    /// submission, and the user must explicitly confirm
    /// unless `require_confirmation_per_application: false`
    /// is also set. The per-run cap (`max_applications_per_run`)
    /// is enforced independently of any other budget so a
    /// runaway loop cannot fire hundreds of applications in
    /// one session. See `mew_agent::auto_apply` for the
    /// implementation and `docs/phase6-auto-apply.md` for
    /// the design rationale.
    #[serde(default)]
    pub auto_apply: AutoApplyConfig,
}

/// Phase 41: configuration for the lead-gen result export path.
/// The agent writes the ranked `MapsFinding` list to disk in
/// the operator's chosen format. The default format is `csv`
/// (the operator's most likely next step is to open the file
/// in a spreadsheet); `json` and `markdown` are supported
/// for operators who want the raw structured form or a
/// copy-pasteable view.
///
/// `output_dir: None` resolves to `<cwd>/exports/` at the
/// call site. The Tauri UI overrides this at runtime via
/// `app_handle.path().app_data_dir()` if it wants the file
/// to land in the per-user app-data tree; the agent itself
/// never depends on Tauri, so the default works in CLI and
/// test contexts too.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExportConfig {
    #[serde(default = "default_export_enabled")]
    pub enabled: bool,
    #[serde(default = "default_export_format")]
    pub format: ExportFormat,
    /// `None` => agent-side default of `<cwd>/exports/`.
    /// Tauri overrides this to `<app_data_dir>/exports/`.
    #[serde(default)]
    pub output_dir: Option<std::path::PathBuf>,
}

fn default_export_enabled() -> bool {
    false
}

fn default_export_format() -> ExportFormat {
    ExportFormat::Csv
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            enabled: default_export_enabled(),
            format: default_export_format(),
            output_dir: None,
        }
    }
}

/// Phase 41: output format for the lead-gen export. The
/// serde representations are intentionally lowercase
/// (`csv`, `json`, `markdown`) to match the config.yaml
/// shape the operator writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    Csv,
    Json,
    Markdown,
}

impl std::fmt::Display for ExportFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Json => "json",
            ExportFormat::Markdown => "markdown",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for ExportFormat {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "csv" => Ok(ExportFormat::Csv),
            "json" => Ok(ExportFormat::Json),
            "markdown" | "md" => Ok(ExportFormat::Markdown),
            other => Err(format!(
                "unknown export format '{other}'; expected one of: csv, json, markdown"
            )),
        }
    }
}

/// Phase 6: configuration for the auto-apply action layer.
///
/// The auto-apply layer extends the existing
/// `irreversible_actions` classifier with three new typed
/// actions — `apply_to_job` (job board submission),
/// `send_proposal` (Upwork / Contra proposal), and
/// `send_connection_request` (LinkedIn connect) — and adds:
///   * local dedup store at `<data_dir>/applications_sent.json`
///     keyed by `(platform, posting_identifier)`, so the agent
///     never double-applies to the same posting across runs.
///   * a per-run submission cap (`max_applications_per_run`)
///     enforced independently of any other budget, to prevent
///     runaway submission loops on flaky platforms.
///   * a draft-text step (reusing the existing summarizer
///     LLM call) presented at the `Paused` checkpoint for
///     user approval.
///
/// The config block is **default-disabled** to match the
/// pre-Phase-6 read-only / report-only behavior byte-for-byte.
/// The `enabled: false` path is a hard no-op: the
/// `irreversible_actions` classifier still fires and still
/// pauses for confirmation, but the agent does not draft
/// application text and does not consult the dedup store —
/// those are pure add-ons gated on `enabled: true`.
///
/// Even with `enabled: true`, the agent does NOT
/// auto-submit. The existing `Paused`-checkpoint
/// irreversible-action gate is the *only* path to
/// submission. The user must explicitly confirm each
/// application unless `require_confirmation_per_application:
/// false` is explicitly set (the `false` value is the
/// "fire-and-forget" path — operators are expected to read
/// `docs/phase6-auto-apply.md` before flipping it).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AutoApplyConfig {
    /// Master switch. When `false` (the default), the
    /// auto-apply layer is a no-op and the agent falls
    /// back to the pre-Phase-6 "find and report" behavior
    /// (job / lead findings surface as chat messages;
    /// no application text is drafted; the dedup store
    /// is not consulted). When `true`, the agent drafts
    /// application text and presents it at the `Paused`
    /// checkpoint for user approval before submission.
    #[serde(default = "default_auto_apply_enabled")]
    pub enabled: bool,
    /// When `true` (the default and the safe-by-default
    /// value), the agent pauses for the user to confirm
    /// each application / proposal / connection request
    /// individually. When `false`, the agent auto-submits
    /// any draft the summarizer LLM call produces — the
    /// dedup store and per-run cap still apply, but the
    /// user is not in the loop. Operators are expected to
    /// read `docs/phase6-auto-apply.md` before flipping
    /// this to `false` (the spec calls it out as the
    /// "fire-and-forget" path).
    #[serde(default = "default_require_confirmation")]
    pub require_confirmation_per_application: bool,
    /// Hard cap on submissions per agent run. Enforced
    /// independently of `max_iterations` / `max_cost` so a
    /// runaway loop on a flaky platform cannot fire
    /// hundreds of applications. The default is 10 — a
    /// reasonable upper bound for a single personal-use
    /// session on a Monday morning, well above the typical
    /// "weekly_full_search" preset's natural output. The
    /// hard upper clamp is 1000 (config loader will not
    /// accept a higher value; this matches the existing
    /// `live_lines_cap` clamp pattern).
    #[serde(default = "default_max_applications_per_run")]
    pub max_applications_per_run: usize,
}

fn default_auto_apply_enabled() -> bool {
    false
}

fn default_require_confirmation() -> bool {
    true
}

fn default_max_applications_per_run() -> usize {
    10
}

impl Default for AutoApplyConfig {
    fn default() -> Self {
        Self {
            enabled: default_auto_apply_enabled(),
            require_confirmation_per_application: default_require_confirmation(),
            max_applications_per_run: default_max_applications_per_run(),
        }
    }
}

fn default_max_tools_per_turn() -> usize {
    1
}

fn default_in_page_history_cap() -> usize {
    5
}

impl Default for AgentConfig {
    /// Custom `Default` impl so `max_tools_per_turn` defaults to
    /// 1 (backward-compatible single-action-per-turn behavior).
    /// The `#[derive(Default)]` would have set the `usize` field
    /// to 0, which would make the dispatch loop's
    /// `take_n = calls.len().min(max.max(1))` clamp to 1 every
    /// time — equivalent, but the explicit value makes the
    /// intent obvious to readers and matches the `serde` default.
    fn default() -> Self {
        Self {
            task_presets: std::collections::HashMap::new(),
            allowed_domains: None,
            credentials: CredentialsConfig::default(),
            pacing: crate::pacing::PacingConfig::default(),
            summarization: crate::summarizer::SummarizationConfig::default(),
            research: crate::research::ResearchConfig::default(),
            captcha: crate::captcha_solver::CaptchaConfig::default(),
            planner_enabled: false,
            memory: MemoryConfig::default(),
            max_tools_per_turn: default_max_tools_per_turn(),
            in_page_history_cap: default_in_page_history_cap(),
            max_ax_tree_nodes: None,
            export: ExportConfig::default(),
            auto_apply: AutoApplyConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MemoryConfig {
    #[serde(default = "default_memory_enabled")]
    pub enabled: bool,
    #[serde(default = "default_max_lessons")]
    pub max_lessons_per_task: usize,
    #[serde(default)]
    pub min_lesson_age_secs: u64,
    /// Phase 25: Minimum confidence score threshold for lesson decay (default: 0.3).
    #[serde(default = "default_min_confidence")]
    pub min_confidence_score: f32,
    /// Phase 25: Minimum number of sightings required before decay threshold applies (default: 2).
    #[serde(default = "default_min_times_seen_decay")]
    pub min_times_seen_decay: u32,
}

fn default_memory_enabled() -> bool {
    true
}

fn default_max_lessons() -> usize {
    5
}

fn default_min_confidence() -> f32 {
    0.3
}

fn default_min_times_seen_decay() -> u32 {
    2
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_lessons_per_task: 5,
            min_lesson_age_secs: 0,
            min_confidence_score: 0.3,
            min_times_seen_decay: 2,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    pub opencode_zen: OpencodeZenConfig,
    pub browser: Option<BrowserConfig>,
    #[serde(default)]
    pub agent: AgentConfig,
}

pub fn load_config() -> anyhow::Result<ProviderConfig> {
    let mut current_dir = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("Failed to get current directory: {e}"))?;

    let mut config_path = None;

    loop {
        let potential_path = current_dir.join("config.yaml");
        if potential_path.exists() {
            config_path = Some(potential_path);
            break;
        }

        if !current_dir.pop() {
            break;
        }
    }

    let config_path = config_path.ok_or_else(|| {
        anyhow::anyhow!("Could not find config.yaml in current or any parent directory")
    })?;
    let path_str = config_path.display().to_string();

    let content = std::fs::read_to_string(&config_path)
        .map_err(|e| anyhow::anyhow!("Failed to read config file '{path_str}': {e}"))?;
    let mut config: ProviderConfig = serde_yaml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse config file '{path_str}': {e}"))?;
    // Phase 35: hard-clamp the per-iteration in-page-history window.
    // Values below 3 starve the agent of context (the loop trims to
    // `k * 2` assistant+tool slots, so k<3 means at most 4 messages
    // remain — too few for a 3-step tool chain). Values above 50
    // risk ballooning the prompt past the model's effective context
    // window on long enumeration tasks. The default of 5 is the
    // pre-Phase-35 hardcoded value; operators can lift it via
    // `agent.in_page_history_cap` in config.yaml.
    config.agent.in_page_history_cap = config.agent.in_page_history_cap.clamp(3, 50);
    // Phase 38: treat a 0 cap as "operator wanted a cap but set it to
    // the wrong value" and disable the cap rather than silently
    // truncate every tree to its root. A positive cap is left as-is.
    if matches!(config.agent.max_ax_tree_nodes, Some(0)) {
        config.agent.max_ax_tree_nodes = None;
    }
    // Phase 6: hard-clamp `max_applications_per_run` to a
    // sensible upper bound. The default is 10 (the typical
    // "personal-use Monday morning" cap). A 0 value is
    // interpreted as "operator wanted the cap but set it to
    // the wrong value" and is bumped to the default 10.
    // Values above 1000 are clamped to 1000 — a run cannot
    // reasonably submit more than 1000 applications in a
    // single session, and an unbounded cap is exactly the
    // runaway-loop failure mode the field exists to prevent.
    if config.agent.auto_apply.max_applications_per_run == 0 {
        config.agent.auto_apply.max_applications_per_run =
            default_max_applications_per_run();
    }
    if config.agent.auto_apply.max_applications_per_run > 1000 {
        config.agent.auto_apply.max_applications_per_run = 1000;
    }
    Ok(config)
}

pub async fn smoke_test(config: &ProviderConfig) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let url = format!("{}/chat/completions", config.opencode_zen.base_url);

    let body = serde_json::json!({
        "model": config.opencode_zen.default_model,
        "messages": [
            {
                "role": "user",
                "content": "Reply with exactly one word: hello"
            }
        ]
    });

    let res = client
        .post(&url)
        .header(
            "Authorization",
            format!("Bearer {}", config.opencode_zen.api_key),
        )
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("HTTP request failed to {url}: {e}"))?;

    let status = res.status();
    let text = res
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read response body: {e}"))?;

    if !status.is_success() {
        println!("{text}");
        anyhow::bail!("API returned error status {status}: {text}");
    }

    println!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase 38: `max_ax_tree_nodes` must default to `None` so the
    /// pre-Phase-38 unbounded-serialization behavior is preserved
    /// for every existing operator that hasn't set the field.
    #[test]
    fn agent_config_default_max_ax_tree_nodes_is_none() {
        let cfg = AgentConfig::default();
        assert!(
            cfg.max_ax_tree_nodes.is_none(),
            "default AgentConfig.max_ax_tree_nodes must be None, got {:?}",
            cfg.max_ax_tree_nodes
        );
    }

    /// Phase 38: round-trip the new field through serde_yaml so a
    /// regression in the field name (or its `Option` shape) gets
    /// caught at `cargo test` time rather than at first deploy.
    #[test]
    fn agent_config_yaml_round_trip_max_ax_tree_nodes() {
        let yaml = "max_ax_tree_nodes: 800\n";
        let cfg: AgentConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.max_ax_tree_nodes, Some(800));

        // `null` (or omitted) -> `None`.
        let cfg: AgentConfig = serde_yaml::from_str("max_ax_tree_nodes: null\n").unwrap();
        assert_eq!(cfg.max_ax_tree_nodes, None);
    }

    /// Phase 38: a positive `max_ax_tree_nodes` round-trips
    /// through the loader. We can't drive `load_config()`
    /// directly (it walks up from CWD looking for a `config.yaml`
    /// in the workspace root), but the field's serde behavior is
    /// the same — the loader adds the `Some(0) -> None` clamp on
    /// top.
    #[test]
    fn agent_config_max_ax_tree_nodes_accepts_various_values() {
        let cfg: AgentConfig = serde_yaml::from_str("max_ax_tree_nodes: 1\n").unwrap();
        assert_eq!(cfg.max_ax_tree_nodes, Some(1));
        let cfg: AgentConfig = serde_yaml::from_str("max_ax_tree_nodes: 10000\n").unwrap();
        assert_eq!(cfg.max_ax_tree_nodes, Some(10000));
    }

    // ---- Phase 40: `expand_allowlist` unit tests ----
    //
    // The spec's Acceptance Criteria call out two specific
    // assertions:
    //   (a) `expand_allowlist(TaskShape::LocalLeadGen, &mut vec![],
    //       &maps_config)` -> vec now contains `maps.google.com`.
    //   (b) `expand_allowlist(TaskShape::GenericBrowse, &mut vec![],
    //       &empty_config)` -> vec unchanged (empty).
    //
    // Both are below as `expand_allowlist_adds_maps_domains_to_empty_vec`
    // and `expand_allowlist_generic_browse_is_noop`. Three
    // additional tests cover the dedup invariant (an
    // already-present domain is not re-appended), the
    // `JobSearch` / `LeadGen` -> `Research` variant path,
    // and the empty-string guard (a blank
    // `required_domains` entry must not silently disable
    // the allowlist).
    //
    // The tests use the default `MapsVerticalConfig` /
    // `ResearchPlatforms` rather than reading the on-disk
    // TOML — the on-disk loader is exercised by the
    // round-trip test in the production code path; these
    // tests focus on the function's contract.

    /// (a) LocalLeadGen + empty seed + default Maps config
    /// -> the Maps default domains appear in the vec.
    #[test]
    fn expand_allowlist_adds_maps_domains_to_empty_vec() {
        let mut allowed: Vec<String> = Vec::new();
        let maps = crate::research::MapsVerticalConfig::default();
        let cfg = VerticalConfig::LocalLeadGen(maps);
        crate::expand_allowlist(mew_memory::TaskShape::LocalLeadGen, &mut allowed, &cfg);
        // Default Maps config seeds three hosts; all
        // three must be present after the call.
        assert!(
            allowed.iter().any(|d| d == "maps.google.com"),
            "expected maps.google.com in {allowed:?}"
        );
        assert!(
            allowed.iter().any(|d| d == "maps.googleapis.com"),
            "expected maps.googleapis.com in {allowed:?}"
        );
        assert!(
            allowed.iter().any(|d| d == "www.google.com"),
            "expected www.google.com in {allowed:?}"
        );
        // And the vec grew by exactly 3 entries (no
        // duplicates within the source list).
        assert_eq!(allowed.len(), 3, "got {allowed:?}");
    }

    /// (b) GenericBrowse + empty seed + Empty config
    /// -> vec is unchanged.
    #[test]
    fn expand_allowlist_generic_browse_is_noop() {
        let mut allowed: Vec<String> = Vec::new();
        let cfg = VerticalConfig::Empty;
        crate::expand_allowlist(mew_memory::TaskShape::GenericBrowse, &mut allowed, &cfg);
        assert!(allowed.is_empty(), "GenericBrowse must not seed the allowlist");
    }

    /// Idempotency: an already-present domain is not
    /// re-appended. The function is called twice; the
    /// second call is a no-op.
    #[test]
    fn expand_allowlist_is_idempotent_on_duplicate() {
        let mut allowed: Vec<String> = vec!["maps.google.com".to_string()];
        let maps = crate::research::MapsVerticalConfig::default();
        let cfg = VerticalConfig::LocalLeadGen(maps);
        crate::expand_allowlist(mew_memory::TaskShape::LocalLeadGen, &mut allowed, &cfg);
        // The pre-existing `maps.google.com` is not
        // re-appended; the other two Maps defaults are
        // added once.
        let count = allowed.iter().filter(|d| *d == "maps.google.com").count();
        assert_eq!(count, 1, "duplicate append: {allowed:?}");
        assert_eq!(allowed.len(), 3, "got {allowed:?}");
    }

    /// JobSearch + empty seed + Research config with
    /// `required_domains` set -> the research platforms
    /// are appended. (Mirrors the spec's local-lead-gen
    /// test for the research half of the matrix.)
    #[test]
    fn expand_allowlist_job_search_seeds_research_domains() {
        let mut allowed: Vec<String> = Vec::new();
        let mut research = crate::research::ResearchPlatforms::default();
        research.required_domains = vec![
            "linkedin.com".to_string(),
            "www.linkedin.com".to_string(),
        ];
        let cfg = VerticalConfig::Research(research);
        crate::expand_allowlist(mew_memory::TaskShape::JobSearch, &mut allowed, &cfg);
        assert!(
            allowed.iter().any(|d| d == "linkedin.com"),
            "expected linkedin.com in {allowed:?}"
        );
        assert!(
            allowed.iter().any(|d| d == "www.linkedin.com"),
            "expected www.linkedin.com in {allowed:?}"
        );
        assert_eq!(allowed.len(), 2, "got {allowed:?}");
    }

    /// Empty-string entries in `required_domains` are
    /// skipped, not appended. A blank entry would match
    /// every host against the allowlist's
    /// `ends_with("." + d)` check, silently disabling it.
    #[test]
    fn expand_allowlist_skips_blank_entries() {
        let mut allowed: Vec<String> = Vec::new();
        let mut research = crate::research::ResearchPlatforms::default();
        research.required_domains = vec![
            "linkedin.com".to_string(),
            "  ".to_string(),   // whitespace
            "".to_string(),     // empty
        ];
        let cfg = VerticalConfig::Research(research);
        crate::expand_allowlist(mew_memory::TaskShape::LeadGen, &mut allowed, &cfg);
        // Only the non-blank entry should be appended.
        assert_eq!(allowed, vec!["linkedin.com".to_string()]);
    }

    /// GenericResearch shape with `Empty` config: the
    /// expansion is a no-op, mirroring the GenericBrowse
    /// case. Belt-and-suspenders — the call site in
    /// `run_preflight_plan` maps `GenericResearch` to
    /// `VerticalConfig::Empty`, but a future refactor
    /// could route the shape differently. The
    /// function's contract is "Empty config = no
    /// auto-expansion", independent of the shape.
    #[test]
    fn expand_allowlist_empty_config_is_noop_for_any_shape() {
        let mut allowed: Vec<String> = Vec::new();
        let cfg = VerticalConfig::Empty;
        // Cycle through every shape — every call must
        // be a no-op.
        let shapes = [
            mew_memory::TaskShape::JobSearch,
            mew_memory::TaskShape::LeadGen,
            mew_memory::TaskShape::GenericResearch,
            mew_memory::TaskShape::GenericBrowse,
            mew_memory::TaskShape::LocalLeadGen,
        ];
        for shape in shapes {
            crate::expand_allowlist(shape, &mut allowed, &cfg);
        }
        assert!(
            allowed.is_empty(),
            "Empty config must not seed the allowlist (got {allowed:?})"
        );
    }

    // ---- Phase 41: `ExportConfig` unit tests ----
    //
    // The spec's Acceptance Criteria call out two
    // properties:
    //   (a) the default `AgentConfig.export` is
    //       `enabled = false` (backward compat with
    //       pre-Phase-41 behavior),
    //   (b) the YAML `export:` block round-trips through
    //       serde (so a typo in the field name gets
    //       caught at `cargo test` time, not at first
    //       deploy).
    //
    // We assert both, plus a Display / FromStr round-trip
    // on `ExportFormat` to pin the operator-facing
    // string forms.

    /// (a) `AgentConfig::default().export.enabled`
    /// must be `false` — the pre-Phase-41 chat-only
    /// delivery must be the byte-equivalent default
    /// for every existing operator who hasn't set
    /// the field.
    #[test]
    fn agent_config_default_export_is_disabled() {
        let cfg = AgentConfig::default();
        assert!(
            !cfg.export.enabled,
            "default AgentConfig.export.enabled must be false, got {}",
            cfg.export.enabled
        );
    }

    /// (a2) The default format is `Csv` — the
    /// operator's most likely next step is to open
    /// the file in a spreadsheet.
    #[test]
    fn agent_config_default_export_format_is_csv() {
        let cfg = AgentConfig::default();
        assert_eq!(
            cfg.export.format,
            ExportFormat::Csv,
            "default AgentConfig.export.format must be Csv"
        );
    }

    /// (b) The full YAML shape round-trips through
    /// serde. A typo in `enabled` / `format` /
    /// `output_dir` would manifest as a missing
    /// field on the parse side; this test pins the
    /// field names.
    #[test]
    fn agent_config_yaml_round_trip_export() {
        let yaml = r#"
export:
  enabled: true
  format: markdown
  output_dir: /tmp/exports
"#;
        let cfg: AgentConfig = serde_yaml::from_str(yaml).expect("parse export yaml");
        assert!(cfg.export.enabled);
        assert_eq!(cfg.export.format, ExportFormat::Markdown);
        assert_eq!(
            cfg.export.output_dir,
            Some(std::path::PathBuf::from("/tmp/exports"))
        );
    }

    /// (b2) An omitted `export:` block must round-trip
    /// to the default config (`enabled = false`,
    /// `format = csv`, `output_dir = None`). Backward
    /// compat for operators who never edited the
    /// block.
    #[test]
    fn agent_config_yaml_omitted_export_uses_default() {
        let yaml = r#"
pacing:
  enabled: false
"#;
        let cfg: AgentConfig = serde_yaml::from_str(yaml).expect("parse yaml without export");
        assert!(!cfg.export.enabled, "omitted export must default to disabled");
        assert_eq!(cfg.export.format, ExportFormat::Csv);
        assert!(cfg.export.output_dir.is_none());
    }

    /// Display / FromStr round-trip on the three
    /// operator-facing format names. The serde
    /// `rename_all = "lowercase"` representation
    /// matches `Display` for every variant.
    #[test]
    fn export_format_display_matches_serde() {
        for fmt in [
            ExportFormat::Csv,
            ExportFormat::Json,
            ExportFormat::Markdown,
        ] {
            // Display form.
            let s = fmt.to_string();
            // Serde round-trip via YAML. The format
            // field lives under the `export:` block
            // in the operator's config.yaml, so we
            // wrap the test fixture in the same
            // shape to pin the field path.
            let yaml = format!("export:\n  format: {s}\n");
            let parsed: AgentConfig =
                serde_yaml::from_str(&yaml).expect("parse export.format yaml");
            assert_eq!(
                parsed.export.format, fmt,
                "Display / serde mismatch for {s}"
            );
        }
    }
}

// ----------------------------------------------------------------------
// Phase 40: Domain Allowlist Auto-Expansion
//
// Each task shape can declare a list of `required_domains` —
// hosts the shape is known to visit, which should be appended to
// the session-local `allowed_domains` list at session start. The
// data lives in the per-vertical config files (Maps vertical
// declares its hosts; research platforms declare theirs); the
// `expand_allowlist` function below is the single chokepoint the
// agent uses to apply the expansion. This module is the spec's
// "`config module (or chat_agent.rs)`" home — it lives here
// because the `VerticalConfig` it consumes is data, not
// behavior, and `lib.rs` is already where the
// `AgentConfig::allowed_domains` field is defined.
//
// The expansion is *session-local*: only the in-memory vec on
// `AgentConfig.allowed_domains` is mutated, never the on-disk
// `config.yaml` or any per-vertical TOML. The spec is explicit
// on this: "the vec is cloned from the parsed config at session
// start — not persisted to disk." The reasoning is that an
// operator who edits `config.yaml` to add a domain is making a
// long-lived trust decision; an auto-expansion is just a
// per-session convenience, and silently mutating the on-disk
// list would make a one-off Maps task permanently open a domain
// the operator never vetted.
// --------------------------------------------------------------------//

/// Per-shape vertical config. The variant tells
/// `expand_allowlist` which TOML file's `required_domains` to
/// pull. `Empty` is the "no auto-expansion" sentinel used by
/// the `GenericBrowse` / `GenericResearch` shapes — when the
/// `VerticalConfig` is `Empty`, the call is a no-op regardless
/// of the shape. This keeps the call site uniform (always
/// pass a `VerticalConfig`) without forcing every call site to
/// know which shape is "interesting."
///
/// The variant design also future-proofs the spec's
/// "`shape: TaskShape, allowed_domains: &mut Vec<String>,
/// vertical_config: &VerticalConfig`" signature: a new
/// `JobSearch(ResearchPlatforms)` / `LeadGen(ResearchPlatforms)`
/// variant doesn't break any existing caller — they just keep
/// passing `VerticalConfig::Empty` and the new auto-expansion
/// paths live in `expand_allowlist` only.
#[derive(Debug, Clone, Default)]
pub enum VerticalConfig {
    /// No required domains for this shape. The expansion is a
    /// no-op. Use for `GenericBrowse` / `GenericResearch`.
    #[default]
    Empty,
    /// Phase 34: Google Maps local-business lead gen. The
    /// `required_domains` field on `MapsVerticalConfig` is the
    /// source of truth; `expand_allowlist` reads
    /// `config.required_domains` and appends to the caller's
    /// vec.
    LocalLeadGen(crate::research::MapsVerticalConfig),
    /// Phase 40: the long-horizon research loop
    /// (`JobSearch` / `LeadGen`). The
    /// `ResearchPlatforms::required_domains` field is the
    /// source of truth; `expand_allowlist` reads
    /// `config.required_domains` and appends to the caller's
    /// vec.
    Research(crate::research::ResearchPlatforms),
}

/// Phase 40: append a shape's `required_domains` to the
/// session-local allowlist. Idempotent (already-present
/// domains are not re-appended), and silent on an empty
/// `required_domains` vec — the function is a no-op for the
/// `GenericBrowse` / `GenericResearch` shapes via the
/// `VerticalConfig::Empty` variant.
///
/// Signature is the spec's: `pub fn expand_allowlist(shape,
/// &mut Vec<String>, &VerticalConfig)`. The `shape` parameter
/// is reserved for future shape-specific logic (e.g. refusing
/// to auto-expand for an explicit `ForbidNetworkTasks` shape
/// the operator might add later); today every variant keys off
/// the `vertical_config` only, so `shape` is currently
/// diagnostic — it's used only in the optional log line a
/// caller may choose to emit. The parameter is kept so the
/// spec'd signature is honored verbatim and a future shape
/// filter doesn't break existing callers.
///
/// Per-domain appends: the function appends each new domain
/// exactly once. The dedup check is O(n) on the existing vec
/// — fine for the small allowlist sizes (typically <30
/// entries) we expect. A `HashSet`-backed allowlist would be
/// overkill at this scale and would force a `HashSet` →
/// `Vec` materialization at every check site.
pub fn expand_allowlist(
    _shape: mew_memory::TaskShape,
    allowed_domains: &mut Vec<String>,
    vertical_config: &VerticalConfig,
) {
    let to_add: Vec<String> = match vertical_config {
        // No-op for shapes that don't declare required
        // domains. Using an empty `to_add` vec lets the
        // post-match dedup loop run without a special case
        // — cheaper than branching here.
        VerticalConfig::Empty => Vec::new(),
        VerticalConfig::LocalLeadGen(cfg) => cfg.required_domains.clone(),
        VerticalConfig::Research(cfg) => cfg.required_domains.clone(),
    };
    for d in to_add {
        // Skip empties defensively. A blank entry in
        // `required_domains` would silently disable the
        // allowlist (the existing `ends_with` check
        // matches every host that ends in the empty
        // string — which is every host). Skip rather
        // than reject so a typo doesn't brick a session
        // on first contact; the next `cargo test` will
        // surface a regression.
        if d.trim().is_empty() {
            continue;
        }
        if !allowed_domains.iter().any(|existing| existing == &d) {
            allowed_domains.push(d);
        }
    }
}
