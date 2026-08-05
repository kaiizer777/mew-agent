// mew — Phase 10.4 unit tests for the end-of-task finish()
// passthrough heuristic.
//
// The heuristic lives on `mew_agent::agent::Agent` (the full
// struct has too many fields to construct in a unit test), so
// this module re-exports the relevant function as a thin
// pure-Rust wrapper and tests that. The agent's
// `try_passthrough_finish` calls into this same function under
// the hood — see the `cfg(test)`-only `passthrough_check` in
// `agent.rs`.

/// Pure form of the heuristic. See
/// `mew_agent::agent::Agent::try_passthrough_finish` for the
/// full rationale. Exposed here so unit tests can drive the
/// same rules without instantiating an `Agent` (which needs a
/// `ProviderConfig`, a `reqwest::Client`, a transcript dir,
/// etc. — all unnecessary for a string-shape check).
pub fn passthrough_check(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 280 {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    let templated_prefix = lower.starts_with("i clicked")
        || lower.starts_with("i typed")
        || lower.starts_with("i navigated")
        || lower.starts_with("i scrolled")
        || lower.starts_with("i performed")
        || lower.starts_with("i called")
        || lower.starts_with("i took")
        || lower.starts_with("step 1")
        || lower.starts_with("step 2");
    if templated_prefix {
        return None;
    }
    for line in trimmed.lines() {
        let t = line.trim_start();
        if t.starts_with('{') || t.starts_with('[') {
            return None;
        }
    }
    if lower.contains("tool call:") || lower.contains("[transcript]") {
        return None;
    }
    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_clean_text_passes_through() {
        let text = "I sent the message to John.";
        assert!(passthrough_check(text).is_some());
    }

    #[test]
    fn empty_text_does_not_pass_through() {
        assert!(passthrough_check("").is_none());
        assert!(passthrough_check("   \n  ").is_none());
    }

    #[test]
    fn over_280_chars_does_not_pass_through() {
        let text = "a".repeat(281);
        assert!(passthrough_check(&text).is_none());
    }

    #[test]
    fn templated_prefix_does_not_pass_through() {
        // These are the exact "I did tool X" shape the rewriter exists to clean up.
        for prefix in &[
            "I clicked the submit button",
            "I typed hello",
            "I navigated to instagram.com",
            "I scrolled down",
            "I performed a search",
            "I called finish() with the result",
            "I took a screenshot",
            "Step 1: open the page",
            "step 2: click the link",
        ] {
            assert!(
                passthrough_check(prefix).is_none(),
                "expected templated prefix to be rejected: {prefix:?}"
            );
        }
    }

    #[test]
    fn json_block_does_not_pass_through() {
        let with_object = "Here is the data:\n{ \"key\": \"value\" }";
        let with_array = "And the list:\n[1, 2, 3]";
        assert!(passthrough_check(with_object).is_none());
        assert!(passthrough_check(with_array).is_none());
    }

    #[test]
    fn embedded_json_object_inside_sentence_does_pass_through() {
        // Heuristic only flags JSON at *line start* — a stray
        // `{` mid-line is allowed. This is the right call
        // because chat text sometimes contains code-like
        // snippets (e.g. "I used the foo{x}bar trick") and
        // the rewriter is allowed to handle those.
        let text = "I used the foo{x}bar trick to format the message.";
        assert!(passthrough_check(text).is_some());
    }

    #[test]
    fn transcript_markers_do_not_pass_through() {
        for text in &[
            "Here is what I did: TOOL CALL: navigate",
            "see [TRANSCRIPT] for full log",
        ] {
            assert!(
                passthrough_check(text).is_none(),
                "expected transcript marker to be rejected: {text:?}"
            );
        }
    }

    #[test]
    fn leading_whitespace_is_trimmed() {
        let text = "  I sent it.  \n";
        let result = passthrough_check(text).unwrap();
        assert_eq!(result, "I sent it.");
    }
}
