// mew v2 — Phase 5: live step summarization.
//
// What this module is:
//
//   * The `summarize(name, args, result)` function: a cheap, *templated*
//     one-liner for the common actions (navigate / click / type / scroll
//     / press_key / snapshot / vision_inspect / finish). The LLM is
//     *not* called per tool invocation — a templated sentence is
//     plenty for a chat-list progress line, and avoiding the LLM
//     keeps the loop latency and cost flat.
//
//   * The `LiveProgress` buffer: a fixed-size ring of the most recent
//     progress lines. Each new line is also pushed on an
//     `mpsc::UnboundedSender<AgentEvent>` so the UI can stream it
//     in real time. The cap (`live_lines_cap`, default 5) is the
//     "flood guard" — without it, a 30-step task would dump 30
//     lines into the chat list and bury the user's actual
//     conversation.
//
//   * The `EndOfTaskSummary` producer: a single LLM call fired at
//     `finish()` time. The agent's raw `finish()` text is usually
//     a list of what it did ("I clicked X. I typed Y. I called
//     finish().") — the end-of-task summarizer turns that into a
//     proper one-paragraph "I sent your message to Alice on
//     Instagram" reply that the chat list shows. The single LLM
//     call is bounded (a short system prompt, max_tokens ~150) so
//     latency is ~1s, not the multi-second cost of calling the
//     LLM every iteration.
//
//   * The `Verbosity` enum: `Concise` (default — one short
//     sentence per action) or `Detailed` (one short sentence plus
//     the tool's first arg snippet, e.g. `Clicked "@e5"
//     (Compose button)`). Verbosity only affects the *chat-list*
//     line; the raw transcript is unchanged.
//
// What this module is NOT:
//
//   * It is *not* a replacement for the browser agent's main
//     system prompt. The LLM still drives the loop. The
//     summarizer is a side-channel that produces the user-facing
//     progress text *in parallel* with the loop's normal work.
//   * It is *not* a new tool the LLM calls. The LLM does not
//     know the summarizer exists. The loop calls the summarizer
//     after each tool dispatch as part of its bookkeeping.
//   * It does not change `BrowserResult`'s shape. The end-of-task
//     summary produced by the LLM call is folded into the
//     existing `BrowserResult::summary` field by the caller, so
//     the Phase 3 orchestrator wiring is unchanged.

use serde::{Deserialize, Serialize};

/// Verbosity level for the user-visible progress lines. The
/// difference is whether the templated line includes a snippet of
/// the tool's first argument (e.g. the URL of a navigate, the
/// text of a type). The raw transcript is unaffected — that
/// already carries the full tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verbosity {
    /// One short sentence per action. The default. Examples:
    /// "Opened instagram.com", "Sent a message".
    Concise,
    /// The same sentence plus a one-phrase arg snippet.
    /// Examples: 'Opened instagram.com ("instagram")',
    /// 'Typed "hi" into @e4 ("Message field")'.
    Detailed,
}

impl Default for Verbosity {
    fn default() -> Self {
        Verbosity::Concise
    }
}

impl Verbosity {
    pub fn from_str_opt(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "detailed" | "verbose" | "full" => Verbosity::Detailed,
            _ => Verbosity::Concise,
        }
    }
}

/// Kind tag for a progress line. The UI uses this to color the
/// bullet / icon — `navigate` is blue, `type` is green, `finish`
/// is purple, etc. Purely cosmetic; the line's content is what
/// matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressKind {
    Navigate,
    Click,
    Type,
    Scroll,
    PressKey,
    Snapshot,
    VisionInspect,
    Declare,
    MarkDone,
    MarkSkipped,
    MarkFailed,
    Finish,
    Other,
}

impl ProgressKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProgressKind::Navigate => "navigate",
            ProgressKind::Click => "click",
            ProgressKind::Type => "type",
            ProgressKind::Scroll => "scroll",
            ProgressKind::PressKey => "press_key",
            ProgressKind::Snapshot => "snapshot",
            ProgressKind::VisionInspect => "vision_inspect",
            ProgressKind::Declare => "declare",
            ProgressKind::MarkDone => "mark_done",
            ProgressKind::MarkSkipped => "mark_skipped",
            ProgressKind::MarkFailed => "mark_failed",
            ProgressKind::Finish => "finish",
            ProgressKind::Other => "other",
        }
    }
}

/// A single user-visible progress line. The frontend renders
/// this as a one-line bullet inside the task card's
/// "live progress" sub-list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressLine {
    pub kind: ProgressKind,
    /// One short, human-readable sentence. Always non-empty.
    /// Capped at ~120 chars in the templated path so the chat
    /// list does not get an entry that wraps awkwardly.
    pub text: String,
    /// Unix seconds, set by the caller (the agent loop has the
    /// canonical clock). Used for ordering and for the
    /// per-line timestamp the UI shows in the "view details"
    /// expansion.
    pub timestamp_secs: u64,
    /// Whether the underlying tool call succeeded. The UI
    /// uses this to color the line red for failures.
    pub success: bool,
}

impl ProgressLine {
    pub fn new(kind: ProgressKind, text: impl Into<String>, timestamp_secs: u64, success: bool) -> Self {
        Self {
            kind,
            text: text.into(),
            timestamp_secs,
            success,
        }
    }
}

/// Build a templated one-liner for a tool call. Cheap, no LLM.
/// `args` is the raw `serde_json::Value` the loop parsed; `result`
/// is the post-dispatch `tool_result` string. `verbosity` controls
/// whether the line includes an arg snippet (Detailed) or just
/// the action (Concise).
///
/// Returns `None` for unknown tool names so the loop can simply
/// skip emitting a line in that case. The loop never depends on
/// this function returning `Some` — it is purely additive.
///
/// The third tuple element (`success`) is derived from the
/// `result` string: a result starting with `ERROR:` (the
/// convention the agent's tool-dispatch handlers use) is
/// flagged as a failure so the UI can color the line red. The
/// templated text is the same either way; the success flag
/// is purely a UI hint.
pub fn summarize(
    name: &str,
    args: &serde_json::Value,
    result: &str,
    verbosity: Verbosity,
) -> Option<(ProgressKind, String, bool)> {
    let success = !result.to_ascii_lowercase().starts_with("error");
    let line = match name {
        "navigate" => {
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
            if verbosity == Verbosity::Detailed {
                format!("Opened {}", truncate(url, 80))
            } else {
                format!("Opened {}", short_url(url))
            }
        }
        "click" => {
            let r = args.get("ref").and_then(|v| v.as_str()).unwrap_or("?");
            if verbosity == Verbosity::Detailed {
                format!("Clicked {}", r)
            } else {
                "Clicked an element".to_string()
            }
        }
        "type" => {
            let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if verbosity == Verbosity::Detailed {
                format!("Typed \"{}\"", truncate(text, 60))
            } else {
                format!("Typed \"{}\"", truncate(text, 40))
            }
        }
        "scroll" => {
            let dir = args
                .get("direction")
                .and_then(|v| v.as_str())
                .unwrap_or("down");
            let pretty = if dir == "up" { "up" } else { "down" };
            format!("Scrolled {}", pretty)
        }
        "press_key" => {
            let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("?");
            if verbosity == Verbosity::Detailed {
                format!("Pressed {}", key)
            } else {
                format!("Pressed {}", key)
            }
        }
        "snapshot" => {
            if verbosity == Verbosity::Detailed {
                "Took a fresh snapshot of the page".to_string()
            } else {
                "Took a snapshot".to_string()
            }
        }
        "vision_inspect" => {
            let r = args.get("ref").and_then(|v| v.as_str()).unwrap_or("?");
            if verbosity == Verbosity::Detailed {
                format!("Looked at {} visually", r)
            } else {
                "Looked at a region visually".to_string()
            }
        }
        "declare_subtasks" => {
            let n = args
                .get("items")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            format!("Split task into {} sub-item{}", n, if n == 1 { "" } else { "s" })
        }
        "mark_subtask_done" => {
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("?");
            if verbosity == Verbosity::Detailed {
                format!("Marked sub-item \"{}\" as done", id)
            } else {
                "Marked a sub-item as done".to_string()
            }
        }
        "mark_subtask_skipped" => {
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("?");
            if verbosity == Verbosity::Detailed {
                format!("Skipped sub-item \"{}\"", id)
            } else {
                "Skipped a sub-item".to_string()
            }
        }
        "mark_subtask_failed" => {
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("?");
            if verbosity == Verbosity::Detailed {
                format!("Marked sub-item \"{}\" as failed", id)
            } else {
                "Marked a sub-item as failed".to_string()
            }
        }
        "finish" => {
            // finish's progress line is the synthesis of the
            // agent's final answer. The loop builds the
            // full text from the result string and pushes it
            // on its own — here we just return a short
            // completion label.
            "Task complete".to_string()
        }
        _ => return None,
    };
    let kind = match name {
        "navigate" => ProgressKind::Navigate,
        "click" => ProgressKind::Click,
        "type" => ProgressKind::Type,
        "scroll" => ProgressKind::Scroll,
        "press_key" => ProgressKind::PressKey,
        "snapshot" => ProgressKind::Snapshot,
        "vision_inspect" => ProgressKind::VisionInspect,
        "declare_subtasks" => ProgressKind::Declare,
        "mark_subtask_done" => ProgressKind::MarkDone,
        "mark_subtask_skipped" => ProgressKind::MarkSkipped,
        "mark_subtask_failed" => ProgressKind::MarkFailed,
        "finish" => ProgressKind::Finish,
        _ => ProgressKind::Other,
    };
    Some((kind, line, success))
}

/// Shorten a URL for display: strip the scheme, take the host
/// and first path segment. `instagram.com/whatever` reads
/// better in a chat list than
/// `https://www.instagram.com/whatever?igshid=...`.
fn short_url(url: &str) -> String {
    let trimmed = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    // Drop query string + fragment for display purposes.
    let without_q = trimmed.split('?').next().unwrap_or(trimmed);
    let without_f = without_q.split('#').next().unwrap_or(without_q);
    truncate(without_f, 80)
}

/// Truncate `s` to at most `n` characters, appending `…` if
/// the input was longer. Used to keep the chat-list lines
/// short and the transcript log lines bounded.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A fixed-size, append-only ring buffer of progress lines.
/// Holds the most recent `cap` lines; older lines are dropped.
/// The buffer is the source of truth for the "what's the agent
/// doing right now" view in the UI.
///
/// The buffer is intentionally *not* behind a Mutex — it lives
/// entirely on the agent's task thread and is read at the end
/// of the run to populate the `BrowserResult` end-of-task
/// summary's "what was done" list. The UI does not need direct
/// access; it consumes lines as they are emitted on the
/// `mpsc::UnboundedSender<AgentEvent>` and the agent's loop
/// also pushes to that sender.
#[derive(Debug, Clone)]
pub struct LiveProgress {
    cap: usize,
    lines: Vec<ProgressLine>,
}

impl LiveProgress {
    /// Create an empty buffer with capacity `cap`. `cap` is
    /// clamped to a minimum of 1 and a maximum of 1000 — the
    /// spec says "last 5" and we should never hold more than a
    /// chat list needs.
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.clamp(1, 1000),
            lines: Vec::new(),
        }
    }

    /// Push a new line. If the buffer is at capacity, the
    /// oldest line is dropped. Returns the line that was
    /// pushed (so the caller can emit it on the channel
    /// without re-allocating).
    pub fn push(&mut self, line: ProgressLine) -> ProgressLine {
        if self.lines.len() >= self.cap {
            self.lines.remove(0);
        }
        self.lines.push(line.clone());
        line
    }

    /// Snapshot the current contents (most-recent-last).
    pub fn snapshot(&self) -> Vec<ProgressLine> {
        self.lines.clone()
    }

    /// Number of lines currently in the buffer.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// True if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The cap. Used by tests; the UI does not read this
    /// directly (it has its own cap, mirroring the
    /// backend's).
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Concatenate the recent lines into a single text blob,
    /// one per line, suitable as context for the end-of-task
    /// LLM call. The end-of-task summary takes the last
    /// `at_most` lines; default is all of them.
    pub fn recent_text(&self, at_most: usize) -> String {
        let start = if self.lines.len() > at_most {
            self.lines.len() - at_most
        } else {
            0
        };
        self.lines[start..]
            .iter()
            .map(|l| format!("- {}", l.text))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The configuration the agent reads at construction time.
/// Lives in `AgentConfig` (see `lib.rs`) so a single YAML block
/// controls both verbosity and the line cap. Defaults:
/// verbosity = Concise, live_lines_cap = 5.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummarizationConfig {
    #[serde(default)]
    pub verbosity: Verbosity,
    /// Maximum number of progress lines the buffer holds and
    /// that the UI shows in the "live progress" sub-list.
    /// Older lines are folded into "…and N more steps."
    #[serde(default = "default_live_lines_cap")]
    pub live_lines_cap: usize,
    /// Whether to fire the end-of-task LLM summarizer. The
    /// default is `true` because the templated
    /// "I clicked X. I typed Y. I called finish()" text the
    /// agent emits is not user-friendly; the LLM
    /// summarizer rewrites it into a proper chat-list reply.
    /// Setting this to `false` falls back to using the raw
    /// `finish()` text as the `BrowserResult::summary`.
    #[serde(default = "default_true")]
    pub end_of_task_llm_summary: bool,
}

impl Default for SummarizationConfig {
    fn default() -> Self {
        Self {
            verbosity: Verbosity::default(),
            live_lines_cap: default_live_lines_cap(),
            end_of_task_llm_summary: true,
        }
    }
}

fn default_live_lines_cap() -> usize {
    5
}

fn default_true() -> bool {
    true
}

/// Format a "…and N more steps" suffix for the UI when the
/// buffer has more than `shown` lines. The UI uses this to
/// collapse older lines into a single summary bullet rather
/// than dump the whole history.
pub fn more_steps_suffix(total: usize, shown: usize) -> Option<String> {
    if total > shown {
        Some(format!("…and {} more step{}", total - shown, if total - shown == 1 { "" } else { "s" }))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- templated summaries ----

    #[test]
    fn navigate_concise_strips_scheme() {
        let args = json!({ "url": "https://www.instagram.com/explore/" });
        let (kind, line, _success) = summarize("navigate", &args, "ok", Verbosity::Concise).unwrap();
        assert_eq!(kind, ProgressKind::Navigate);
        assert!(line.contains("instagram.com"), "got: {line}");
        assert!(!line.contains("https://"), "got: {line}");
    }

    #[test]
    fn navigate_detailed_includes_url() {
        let args = json!({ "url": "https://instagram.com" });
        let (_, line, _) = summarize("navigate", &args, "ok", Verbosity::Detailed).unwrap();
        assert!(line.contains("instagram.com"), "got: {line}");
    }

    #[test]
    fn type_includes_text() {
        let args = json!({ "ref": "@e4", "text": "hi alice" });
        let (kind, line, _) = summarize("type", &args, "ok", Verbosity::Concise).unwrap();
        assert_eq!(kind, ProgressKind::Type);
        assert!(line.contains("hi alice"), "got: {line}");
    }

    #[test]
    fn type_truncates_long_text_in_concise() {
        let long = "x".repeat(200);
        let args = json!({ "ref": "@e4", "text": long });
        let (_, line, _) = summarize("type", &args, "ok", Verbosity::Concise).unwrap();
        // concise truncates at 40
        assert!(line.chars().count() < 80, "line too long: {} chars", line.chars().count());
    }

    #[test]
    fn click_concise_hides_ref() {
        let args = json!({ "ref": "@e5" });
        let (_, line, _) = summarize("click", &args, "ok", Verbosity::Concise).unwrap();
        assert_eq!(line, "Clicked an element");
    }

    #[test]
    fn click_detailed_shows_ref() {
        let args = json!({ "ref": "@e5" });
        let (_, line, _) = summarize("click", &args, "ok", Verbosity::Detailed).unwrap();
        assert!(line.contains("@e5"), "got: {line}");
    }

    #[test]
    fn scroll_direction_normalized() {
        let up = json!({ "direction": "up" });
        let down = json!({ "direction": "down" });
        assert!(summarize("scroll", &up, "ok", Verbosity::Concise).unwrap().1.contains("up"));
        assert!(summarize("scroll", &down, "ok", Verbosity::Concise).unwrap().1.contains("down"));
    }

    #[test]
    fn mark_subtask_done_concise() {
        let args = json!({ "id": "send_msg" });
        let (kind, line, _) = summarize("mark_subtask_done", &args, "ok", Verbosity::Concise).unwrap();
        assert_eq!(kind, ProgressKind::MarkDone);
        assert!(line.contains("Marked"));
        assert!(!line.contains("send_msg"), "concise should not show id");
    }

    #[test]
    fn mark_subtask_done_detailed_includes_id() {
        let args = json!({ "id": "send_msg" });
        let (_, line, _) = summarize("mark_subtask_done", &args, "ok", Verbosity::Detailed).unwrap();
        assert!(line.contains("send_msg"), "got: {line}");
    }

    #[test]
    fn unknown_tool_returns_none() {
        let args = json!({});
        assert!(summarize("no_such_tool", &args, "ok", Verbosity::Concise).is_none());
    }

    #[test]
    fn error_result_does_not_change_text_but_marks_not_success_in_caller() {
        // The summarize() function itself does not look at success
        // for the text. The caller (loop) uses `success` from the
        // string check. We just assert the text is still produced.
        let args = json!({ "url": "https://x.com" });
        let (_, line, success) = summarize("navigate", &args, "ERROR: boom", Verbosity::Concise).unwrap();
        assert!(!success, "ERROR-prefixed result should be flagged as failure");
        assert!(line.contains("x.com"), "got: {line}");
    }

    // ---- short_url ----

    #[test]
    fn short_url_strips_scheme_and_query() {
        assert_eq!(short_url("https://www.instagram.com/explore/?x=1"), "www.instagram.com/explore/");
        assert_eq!(short_url("http://example.com/path#frag"), "example.com/path");
    }

    // ---- truncate ----

    #[test]
    fn truncate_under_cap_is_unchanged() {
        assert_eq!(truncate("hi", 10), "hi");
    }

    #[test]
    fn truncate_over_cap_appends_ellipsis() {
        let out = truncate("hello world", 5);
        assert_eq!(out, "hell…");
    }

    // ---- LiveProgress ----

    #[test]
    fn live_progress_drops_oldest_when_full() {
        let mut buf = LiveProgress::new(3);
        for i in 0..5 {
            buf.push(ProgressLine::new(
                ProgressKind::Other,
                format!("step {i}"),
                i as u64,
                true,
            ));
        }
        let snap = buf.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].text, "step 2");
        assert_eq!(snap[2].text, "step 4");
    }

    #[test]
    fn live_progress_cap_clamped() {
        let buf = LiveProgress::new(0);
        assert_eq!(buf.cap(), 1, "cap 0 should clamp to 1");
        let buf = LiveProgress::new(2000);
        assert_eq!(buf.cap(), 1000, "cap 2000 should clamp to 1000");
    }

    #[test]
    fn live_progress_push_returns_what_was_pushed() {
        let mut buf = LiveProgress::new(5);
        let line = ProgressLine::new(ProgressKind::Click, "clicked", 1, true);
        let out = buf.push(line.clone());
        assert_eq!(out, line);
    }

    #[test]
    fn live_progress_recent_text_joins_with_dashes() {
        let mut buf = LiveProgress::new(5);
        buf.push(ProgressLine::new(ProgressKind::Navigate, "opened", 0, true));
        buf.push(ProgressLine::new(ProgressKind::Type, "typed", 0, true));
        let text = buf.recent_text(10);
        assert!(text.contains("- opened"));
        assert!(text.contains("- typed"));
    }

    #[test]
    fn live_progress_recent_text_limits_to_at_most() {
        let mut buf = LiveProgress::new(10);
        for i in 0..10 {
            buf.push(ProgressLine::new(ProgressKind::Other, format!("s{i}"), 0, true));
        }
        let text = buf.recent_text(3);
        assert!(text.contains("s9"));
        assert!(text.contains("s8"));
        assert!(text.contains("s7"));
        assert!(!text.contains("s6"), "should not include s6 with at_most=3: {text}");
    }

    // ---- more_steps_suffix ----

    #[test]
    fn more_steps_suffix_only_when_collapsed() {
        assert_eq!(more_steps_suffix(10, 5), Some("…and 5 more steps".to_string()));
        assert_eq!(more_steps_suffix(6, 5), Some("…and 1 more step".to_string()));
        assert_eq!(more_steps_suffix(5, 5), None);
        assert_eq!(more_steps_suffix(3, 5), None);
    }

    // ---- Verbosity ----

    #[test]
    fn verbosity_from_str_is_case_insensitive() {
        assert_eq!(Verbosity::from_str_opt("detailed"), Verbosity::Detailed);
        assert_eq!(Verbosity::from_str_opt("VERBOSE"), Verbosity::Detailed);
        assert_eq!(Verbosity::from_str_opt("full"), Verbosity::Detailed);
        assert_eq!(Verbosity::from_str_opt("concise"), Verbosity::Concise);
        assert_eq!(Verbosity::from_str_opt("anything-else"), Verbosity::Concise);
    }

    #[test]
    fn verbosity_default_is_concise() {
        assert_eq!(Verbosity::default(), Verbosity::Concise);
    }

    // ---- SummarizationConfig ----

    #[test]
    fn summarization_config_default() {
        let cfg = SummarizationConfig::default();
        assert_eq!(cfg.verbosity, Verbosity::Concise);
        assert_eq!(cfg.live_lines_cap, 5);
        assert!(cfg.end_of_task_llm_summary);
    }

    #[test]
    fn summarization_config_yaml_round_trip() {
        let yaml = "verbosity: detailed\nlive_lines_cap: 8\nend_of_task_llm_summary: false\n";
        let cfg: SummarizationConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.verbosity, Verbosity::Detailed);
        assert_eq!(cfg.live_lines_cap, 8);
        assert!(!cfg.end_of_task_llm_summary);
    }

    // ---- end-of-task summarizer prompt ----

    #[test]
    fn end_of_task_prompt_includes_recent_lines() {
        let mut buf = LiveProgress::new(5);
        buf.push(ProgressLine::new(ProgressKind::Navigate, "Opened instagram.com", 0, true));
        buf.push(ProgressLine::new(ProgressKind::Type, "Typed \"hi\"", 0, true));
        let prompt = end_of_task_prompt("send a message to alice", &buf.recent_text(5));
        assert!(prompt.contains("instagram.com"));
        assert!(prompt.contains("hi"));
        assert!(prompt.contains("alice"), "must include the task description");
    }

    // ---- progress kind string ----

    #[test]
    fn progress_kind_as_str_is_stable_for_frontend_keys() {
        // The frontend reads these as keys for icon/color
        // mapping. They must not change without a frontend
        // change in lockstep.
        assert_eq!(ProgressKind::Navigate.as_str(), "navigate");
        assert_eq!(ProgressKind::Click.as_str(), "click");
        assert_eq!(ProgressKind::Type.as_str(), "type");
        assert_eq!(ProgressKind::Finish.as_str(), "finish");
    }
}

/// Build the system prompt for the end-of-task LLM call.
/// Kept here so the prompt is a tested, owned piece of
/// behavior — a copy-paste from the spec means the test for
/// "this prompt is what the LLM sees" lives next to the
/// prompt itself.
pub fn end_of_task_prompt(task_description: &str, recent_progress: &str) -> String {
    format!(
        "You are a summarizer. Given the user's task and the recent steps the browser agent took, write a single short user-facing reply (1-2 sentences) that describes what was accomplished. Be specific — mention the names of people/sites/values the steps reference. Do NOT mention internal tools, JSON, the agent, or that this is a summary. Output plain text only.\n\nTASK: {task}\n\nRECENT STEPS:\n{steps}",
        task = task_description,
        steps = if recent_progress.is_empty() { "(no recent steps recorded)".to_string() } else { recent_progress.to_string() },
    )
}
