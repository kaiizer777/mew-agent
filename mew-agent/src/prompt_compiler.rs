//! Phase 36: Agent-Optimized Task Prompt Compiler.
//!
//! Users write task prompts for human readers: "narrate as you go", "move
//! quickly but visibly", "don't batch or skip the click-through". These
//! directives are structurally incompatible with the agent's tool-calling
//! model — there is no narration tool, speed is a config concern, and
//! "don't batch" conflicts with the Phase 32 batch dispatch optimization.
//!
//! This module is a **pre-flight prompt compiler** that normalizes the
//! raw user task into an agent-executable form before it enters
//! `Agent::messages`. It is a pure-Rust, deterministic rule set:
//!
//!   * No LLM call (zero cost; runs on the agent's own thread).
//!   * No `regex` crate dependency (we use `str::contains` + `str::replace`).
//!   * The user's original intent is preserved; only the phrasing is
//!     adapted. Counts ("first 15", "each of the 15") are left intact so
//!     `agent::detect_enumeration_task` can still pick them up (Phase 35).
//!
//! The compiler returns three artifacts:
//!
//!   * `agent_task`        — the cleaned task string the LLM actually sees.
//!   * `execution_hints`   — per-vertical execution-strategy lines that
//!                            are appended to the system prompt as an
//!                            `EXECUTION HINTS:` block (mirrors the
//!                            `PLAN:` block style from Phase 2 / 34).
//!   * `strip_warnings`    — human-readable audit notes for the
//!                            transcript: `[ts] [session_id]
//!                            PROMPT-COMPILE: <warning>`. Each warning
//!                            corresponds to a normalized phrase so a
//!                            future operator can trace exactly what the
//!                            compiler did to the user's input.
//!
//! ## Rule set (deterministic, in this order)
//!
//! 1. **Narration directives** — "narrate as you go", "narrate briefly",
//!    "describe what you see", "say out loud", "tell me what you're doing",
//!    "explain each step". Stripped; `"stripped: narration directive"`
//!    added to `strip_warnings`.
//! 2. **Speed directives** — "move quickly", "be fast", "move slowly",
//!    "take your time". Stripped; speed is a config concern.
//! 3. **Anti-batch directives** — "don't batch", "don't skip the
//!    click-through", "one at a time". Stripped; the agent always
//!    goes one-at-a-time within a batch anyway (Phase 32).
//! 4. **`declare_subtasks` mention** — if the user wrote "use
//!    declare_subtasks" or "call declare_subtasks", the phrase is
//!    stripped and an execution hint is added telling the LLM
//!    explicitly *not* to call it.
//! 5. **Count normalization** — counts ("first 15", "each of the 15")
//!    are not touched. The rule set is intentionally narrow so these
//!    survive intact for `agent::detect_enumeration_task`.
//!
//! ## Per-vertical hints
//!
//! `TaskShape::LocalLeadGen` always injects the Maps execution hints
//! regardless of what the user wrote, because the Phase 34 acceptance
//! scenario proved the LLM needs them as a baseline. Other shapes
//! (JobSearch, LeadGen, GenericResearch, GenericBrowse) inject no
//! hints by default — their vertical packs already carry the
//! necessary guidance via the `PLAN:` block.

use crate::handoff::MapsQuery;
use crate::router::TaskShape;

/// The normalized output of [`compile`]. See the module docstring
/// for the field semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledTask {
    /// The cleaned task string the LLM actually sees. Human-oriented
    /// directives (narration, speed, anti-batch, declare_subtasks
    /// mentions) are stripped; counts are preserved.
    pub agent_task: String,
    /// Per-vertical execution-strategy lines. Injected into the
    /// system prompt as an `EXECUTION HINTS:` block. Empty for
    /// shapes that don't have a vertical pack.
    pub execution_hints: Vec<String>,
    /// Human-readable audit notes, one per normalization applied.
    /// Written to the transcript as
    /// `[ts] [session_id] PROMPT-COMPILE: <strip_warning>`.
    pub strip_warnings: Vec<String>,
}

// --------------------------------------------------------------------------
// Rule data
// --------------------------------------------------------------------------

/// Phrases to strip as narration directives. Compared case-insensitively.
const NARRATION_PHRASES: &[&str] = &[
    "narrate as you go",
    "narrate briefly",
    "narrate each step",
    "describe what you see",
    "say out loud",
    "tell me what you're doing",
    "explain each step",
];

/// Phrases to strip as speed directives. Speed is a config concern;
/// the agent's pacing guard (Phase 17.1) already controls the actual
/// inter-action delay.
const SPEED_PHRASES: &[&str] = &[
    "move quickly",
    "be fast",
    "move slowly",
    "take your time",
];

/// Phrases to strip as anti-batch directives. The agent always
/// goes one-at-a-time within a batch anyway (Phase 32
/// `max_tools_per_turn`); the user-visible "batching" is a
/// re-frame of the LLM round-trip cost, not a deviation from
/// per-step correctness.
const ANTI_BATCH_PHRASES: &[&str] = &[
    "don't batch",
    "do not batch",
    "don't skip the click-through",
    "do not skip the click-through",
    "one at a time",
];

/// Phrase variants for "use declare_subtasks". The user sometimes
/// writes this hoping to force a planning step; the agent's own
/// `run_preflight_plan` (Phase 2) already handles that and the
/// `LocalLeadGen` shape *forbids* declare_subtasks (Phase 34).
const DECLARE_SUBTASKS_PHRASES: &[&str] = &[
    "use declare_subtasks",
    "call declare_subtasks",
    "use declare subtasks",
    "call declare subtasks",
];

// --------------------------------------------------------------------------
// The compiler
// --------------------------------------------------------------------------

/// Normalize a raw user task into an agent-executable form.
///
/// This is the only public entry point of the module. The function
/// is total — empty input, all-whitespace input, and any pathological
/// input compile cleanly to a `CompiledTask` with no panics.
///
/// # Arguments
///
/// * `task`       — the raw user task string.
/// * `shape`      — the `TaskShape` already inferred by
///                  `agent::infer_task_shape_from_task` (or the
///                  chat-side router for a chat-originated task).
/// * `_query`     — the parsed `MapsQuery` if available. Reserved
///                  for future per-vertical hint customization; the
///                  current implementation does not depend on it
///                  because the spec requires Maps hints to be
///                  injected "regardless of what the user wrote."
///                  Accepting the parameter today locks the
///                  public API so a future revision can extend
///                  without breaking call sites.
///
/// # Returns
///
/// A [`CompiledTask`] with `agent_task` cleaned, `execution_hints`
/// populated for `LocalLeadGen` (empty for other shapes), and
/// `strip_warnings` containing one entry per normalization that
/// actually fired (empty when the input was already clean).
pub fn compile(task: &str, shape: TaskShape, _query: Option<&MapsQuery>) -> CompiledTask {
    let mut warnings: Vec<String> = Vec::new();
    let mut out = task.to_string();

    // Lowercased view for case-insensitive contains. We do *not*
    // mutate the original casing in `out` — we only use `lower` to
    // detect matches. The replacement itself targets the original
    // casing via `str::replace` with a case-insensitive search, so
    // the user's text style is preserved (e.g. "Check 15 Salons
    // in Mumbai" stays title-cased).
    let lower = out.to_lowercase();

    // (1) Narration directives.
    for phrase in NARRATION_PHRASES {
        if lower.contains(phrase) {
            out = strip_phrase_ci(&out, phrase);
            warnings.push("stripped: narration directive".to_string());
        }
    }
    // Re-derive the lowercased view after each pass because the
    // string may have changed (a strip could change the surrounding
    // word boundaries in subtle ways — rare but safer than assuming).
    let lower = out.to_lowercase();

    // (2) Speed directives.
    for phrase in SPEED_PHRASES {
        if lower.contains(phrase) {
            out = strip_phrase_ci(&out, phrase);
            warnings.push("stripped: speed directive (config concern, not prompt concern)".to_string());
        }
    }
    let lower = out.to_lowercase();

    // (3) Anti-batch directives.
    for phrase in ANTI_BATCH_PHRASES {
        if lower.contains(phrase) {
            out = strip_phrase_ci(&out, phrase);
            warnings.push("stripped: anti-batch directive (per-step by design)".to_string());
        }
    }
    let lower = out.to_lowercase();

    // (4) declare_subtasks mention. Different category: a hint is
    // emitted (not just a warning) so the LLM sees explicit
    // instructions to NOT use that tool.
    let mut declare_subtasks_seen = false;
    for phrase in DECLARE_SUBTASKS_PHRASES {
        if lower.contains(phrase) {
            out = strip_phrase_ci(&out, phrase);
            declare_subtasks_seen = true;
        }
    }
    if declare_subtasks_seen {
        warnings.push("stripped: declare_subtasks mention (forbidden by vertical hint)".to_string());
    }

    // (5) Count normalization. Explicitly a no-op — preserved here as
    // a comment so the rule-set numbering matches the spec. Counts
    // like "first 15" or "each of the 15" do not match any of the
    // four strip categories above, so they survive intact. This is
    // the requirement called out in the phase 36 spec work item 2(e)
    // and is the contract `agent::detect_enumeration_task` (Phase
    // 35) relies on.

    // Tidy up the cleaned string: collapse runs of whitespace into a
    // single space, then trim leading/trailing whitespace. Empty
    // input is a valid compilation result (the agent will receive
    // an empty task; that's a degenerate case but we must not panic).
    out = collapse_whitespace(&out);

    // Per-vertical execution hints.
    let mut execution_hints: Vec<String> = Vec::new();
    if shape == TaskShape::LocalLeadGen {
        // Phase 34 acceptance scenario: the LLM needs every one of
        // these hints to avoid the regressions Phase 34 was written
        // to fix. Injected "regardless of what the user wrote"
        // because the user may not know the Maps SPA semantics.
        execution_hints.push(
            "Use extract_fields(fields=['phone','address','rating','website','name']) immediately after clicking each listing.".to_string(),
        );
        execution_hints.push(
            "Use press_key(key='Escape') to close business panels and return to search results.".to_string(),
        );
        if declare_subtasks_seen {
            // Only push this hint when the user actually mentioned
            // declare_subtasks — otherwise it's noise on a clean
            // task. The spec work item 4 lists this as one of the
            // four required Maps hints; pairing it with the
            // declare_subtasks mention keeps the hint density
            // proportional to the noise.
            execution_hints.push(
                "Do NOT call declare_subtasks for this task — use extract_fields and finish() with structured output directly.".to_string(),
            );
        } else {
            // Even on a clean LocalLeadGen task, the agent should
            // never reach for declare_subtasks. The phase 34 maps
            // pack already says this, but a one-line reminder in
            // the EXECUTION HINTS block (which the LLM sees fresh
            // every turn) is cheap insurance.
            execution_hints.push("Do NOT call declare_subtasks.".to_string());
        }
        execution_hints.push(
            "Call finish() with a markdown table (columns: Name | Phone | Address | Rating | Website) after all listings are checked."
                .to_string(),
        );
    }

    CompiledTask {
        agent_task: out,
        execution_hints,
        strip_warnings: warnings,
    }
}

// --------------------------------------------------------------------------
// String helpers (private)
// --------------------------------------------------------------------------

/// Strip the first case-insensitive occurrence of `phrase` from `s`
/// along with any immediately adjacent whitespace/comma, and collapse
/// the result back to a single space where the strip occurred.
///
/// This is deliberately a *targeted* replace (not a blanket regex
/// sweep) because:
///   1. We want to preserve the surrounding prose's naturalness — a
///      blanket regex would mangle "narrate as you go" but also any
///      user phrase that happens to contain those words.
///   2. The spec mandates no `regex` dependency. Using `str::replace`
///      for case-insensitive matching is achieved by lowercasing the
///      haystack and computing the index, then `splice`-ing in the
///      original-cased substring.
///
/// Multiple occurrences of the same phrase in a single task are
/// handled: the function loops until no more occurrences are found.
/// This is the right behavior because a user could conceivably write
/// "narrate as you go, and then later narrate as you go again" —
/// the second instance is also a directive.
fn strip_phrase_ci(s: &str, phrase: &str) -> String {
    let mut result = s.to_string();
    loop {
        let lower = result.to_lowercase();
        let Some(idx) = lower.find(phrase) else {
            break;
        };
        let end = idx + phrase.len();
        // Extend the strip range to include an immediately trailing
        // comma, period, or whitespace so we don't leave a stranded
        // punctuation token like ", " or " ." floating in the
        // cleaned string. We do NOT eat a leading character (the
        // user may have written "and narrate as you go" — we want
        // the "and" to survive).
        let mut cut_end = end;
        let bytes = result.as_bytes();
        while cut_end < bytes.len()
            && (bytes[cut_end] == b' '
                || bytes[cut_end] == b'\t'
                || bytes[cut_end] == b','
                || bytes[cut_end] == b'.')
        {
            // If we just consumed a period or comma, do not also
            // consume following whitespace — the user's sentence
            // boundary may have been intentional. We only consume
            // one trailing punctuation or whitespace run.
            if bytes[cut_end] == b',' || bytes[cut_end] == b'.' {
                cut_end += 1;
                break;
            }
            cut_end += 1;
        }
        result = format!("{}{}", &result[..idx], &result[cut_end..]);
    }
    result
}

/// Collapse runs of whitespace (spaces, tabs) into a single space
/// and trim leading/trailing whitespace. Multi-line input is
/// flattened to a single line.
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff::MapsQuery;

    /// Convenience: build a `MapsQuery` with the search query and a
    /// sensible default listing count. The compiler doesn't actually
    /// use the query today, but the test fixtures exercise the
    /// `Option<&MapsQuery>` parameter to lock the API.
    fn maps_query_fixture() -> MapsQuery {
        MapsQuery {
            search_query: "salons in Mumbai".to_string(),
            city: "Mumbai".to_string(),
            listing_count: 15,
            min_rating: None,
            require_no_website: false,
            require_phone: false,
            parallelism: 1,
            categories: Vec::new(),
        }
    }

    // (a) "Check 15 salons, narrate as you go" with `LocalLeadGen`
    //     -> `strip_warnings` contains "narration",
    //        `agent_task` does not contain "narrate",
    //        `execution_hints` contains the Maps-specific hints.
    #[test]
    fn compile_local_lead_gen_strips_narration_and_injects_maps_hints() {
        let task = "Check 15 salons in Mumbai, narrate as you go";
        let shape = TaskShape::LocalLeadGen;
        let q = maps_query_fixture();
        let compiled = compile(task, shape, Some(&q));

        // Narration must be gone from the cleaned task.
        assert!(
            !compiled.agent_task.to_lowercase().contains("narrate"),
            "agent_task should not contain 'narrate', got: {:?}",
            compiled.agent_task
        );

        // The "15" count must survive intact (Phase 35 contract).
        assert!(
            compiled.agent_task.contains("15"),
            "agent_task should preserve the count, got: {:?}",
            compiled.agent_task
        );

        // A narration warning must be recorded.
        assert!(
            compiled
                .strip_warnings
                .iter()
                .any(|w| w.contains("narration")),
            "expected a narration strip warning, got: {:?}",
            compiled.strip_warnings
        );

        // All four Maps-specific hints must be present.
        assert!(
            compiled
                .execution_hints
                .iter()
                .any(|h| h.contains("extract_fields")),
            "expected extract_fields hint, got: {:?}",
            compiled.execution_hints
        );
        assert!(
            compiled
                .execution_hints
                .iter()
                .any(|h| h.contains("press_key") && h.contains("Escape")),
            "expected Escape-to-close hint, got: {:?}",
            compiled.execution_hints
        );
        assert!(
            compiled
                .execution_hints
                .iter()
                .any(|h| h.contains("declare_subtasks")),
            "expected declare_subtasks hint, got: {:?}",
            compiled.execution_hints
        );
        assert!(
            compiled
                .execution_hints
                .iter()
                .any(|h| h.contains("finish()") && h.contains("markdown table")),
            "expected finish() markdown table hint, got: {:?}",
            compiled.execution_hints
        );
    }

    // (b) "Search gyms in Mumbai, use declare_subtasks"
    //     -> `strip_warnings` contains "declare_subtasks",
    //        `execution_hints` contains the "do NOT call
    //        declare_subtasks" hint.
    #[test]
    fn compile_strips_declare_subtasks_and_adds_specific_hint() {
        let task = "Search gyms in Mumbai, use declare_subtasks";
        let shape = TaskShape::LocalLeadGen;
        let q = maps_query_fixture();
        let compiled = compile(task, shape, Some(&q));

        // The "use declare_subtasks" phrase must be gone.
        assert!(
            !compiled.agent_task.to_lowercase().contains("declare_subtasks"),
            "agent_task should not contain 'declare_subtasks', got: {:?}",
            compiled.agent_task
        );

        // A declare_subtasks strip warning must be recorded.
        assert!(
            compiled
                .strip_warnings
                .iter()
                .any(|w| w.contains("declare_subtasks")),
            "expected a declare_subtasks strip warning, got: {:?}",
            compiled.strip_warnings
        );

        // The hint must tell the agent NOT to call declare_subtasks.
        assert!(
            compiled
                .execution_hints
                .iter()
                .any(|h| h.contains("Do NOT") && h.contains("declare_subtasks")),
            "expected the 'do NOT call declare_subtasks' hint, got: {:?}",
            compiled.execution_hints
        );
    }

    // (c) "Find businesses in Delhi" with `GenericBrowse`
    //     -> `strip_warnings` is empty, `agent_task` equals
    //        original (modulo whitespace collapse if any), and
    //        `execution_hints` is empty.
    #[test]
    fn compile_generic_browse_is_passthrough() {
        let task = "Find businesses in Delhi";
        let shape = TaskShape::GenericBrowse;
        let compiled = compile(task, shape, None);

        // No warnings — nothing in the task matches a strip rule.
        assert!(
            compiled.strip_warnings.is_empty(),
            "expected no strip warnings for clean GenericBrowse task, got: {:?}",
            compiled.strip_warnings
        );

        // Task is unchanged.
        assert_eq!(compiled.agent_task, task);

        // No execution hints for a non-Maps shape.
        assert!(
            compiled.execution_hints.is_empty(),
            "expected no execution hints for GenericBrowse, got: {:?}",
            compiled.execution_hints
        );
    }

    // (d) Empty string -> compiles cleanly, no panics.
    #[test]
    fn compile_empty_string_is_clean() {
        let compiled = compile("", TaskShape::GenericResearch, None);
        assert_eq!(compiled.agent_task, "");
        assert!(compiled.strip_warnings.is_empty());
        assert!(compiled.execution_hints.is_empty());
    }

    // ---- additional regression tests below; the four above are the
    // ---- spec-mandated acceptance tests, the rest are safety nets
    // ---- for the implementation details the spec did not pin down.

    /// Anti-batch directives get stripped on a non-Maps task and a
    /// warning is recorded. This exercises rule (c) and confirms the
    /// `execution_hints` stays empty for a non-Maps shape.
    #[test]
    fn compile_strips_anti_batch_with_warning() {
        let compiled = compile(
            "Open the docs site, don't batch the steps",
            TaskShape::GenericResearch,
            None,
        );
        assert!(!compiled.agent_task.to_lowercase().contains("don't batch"));
        assert!(!compiled.agent_task.to_lowercase().contains("do not batch"));
        assert!(
            compiled
                .strip_warnings
                .iter()
                .any(|w| w.contains("anti-batch"))
        );
        assert!(compiled.execution_hints.is_empty());
    }

    /// Speed directives get stripped and a warning is recorded.
    #[test]
    fn compile_strips_speed_with_warning() {
        let compiled = compile(
            "Open the docs site, move quickly",
            TaskShape::GenericResearch,
            None,
        );
        assert!(!compiled.agent_task.to_lowercase().contains("move quickly"));
        assert!(compiled.strip_warnings.iter().any(|w| w.contains("speed")));
    }

    /// Counts like "first 15" and "each of the 15" must survive.
    /// This is the Phase 35 enumeration-detector contract.
    #[test]
    fn compile_preserves_enumeration_counts() {
        let compiled = compile(
            "Check the first 15 listings, narrate as you go",
            TaskShape::LocalLeadGen,
            None,
        );
        assert!(compiled.agent_task.contains("15"));
        assert!(!compiled.agent_task.to_lowercase().contains("narrate"));
    }

    /// Whitespace is collapsed: a string with multiple spaces and a
    /// leading/trailing comma is normalized to a single-space form
    /// with the comma-eating behavior of `strip_phrase_ci`.
    #[test]
    fn compile_collapses_whitespace() {
        let compiled = compile(
            "Check  15   salons   in Mumbai,   narrate as you go",
            TaskShape::LocalLeadGen,
            None,
        );
        // No double spaces.
        assert!(!compiled.agent_task.contains("  "));
        // Count preserved.
        assert!(compiled.agent_task.contains("15"));
    }

    /// Case-insensitive matching: the directive is matched regardless
    /// of the user's casing. The original casing of the *surrounding*
    /// text is preserved.
    #[test]
    fn compile_matches_case_insensitively() {
        let compiled = compile(
            "Check 15 salons, NARRATE AS YOU GO",
            TaskShape::LocalLeadGen,
            None,
        );
        // The narration phrase is gone (matched case-insensitively).
        assert!(!compiled.agent_task.to_lowercase().contains("narrate as you go"));
        assert!(compiled.strip_warnings.iter().any(|w| w.contains("narration")));
    }

    /// The non-Maps shapes do not get Maps hints even when the user
    /// mentions Maps. The hint injection is keyed off the shape, not
    /// the task text.
    #[test]
    fn compile_non_maps_shape_does_not_inject_maps_hints() {
        let compiled = compile(
            "Open Google Maps and check 15 salons",
            TaskShape::GenericBrowse,
            None,
        );
        assert!(compiled.execution_hints.is_empty());
    }
}
