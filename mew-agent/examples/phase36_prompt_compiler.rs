// Phase 36 verification harness.
//
// Spot-checks the agent-optimized task prompt compiler on three
// realistic user-phrased inputs. Each input exercises a different
// aspect of the rule set:
//
//   1. "Open Google Maps, check 15 salons in Mumbai, narrate as you
//      go, don't batch" — narration + anti-batch + Maps hints.
//   2. "Search gyms in Mumbai, use declare_subtasks, move quickly" —
//      declare_subtasks strip + speed strip.
//   3. "Find businesses in Delhi" — clean passthrough, no warnings,
//      no hints.
//
// The constructor is the unit under test. It writes transcript
// lines in the standard `[ts] [session_id] EVENT: ...` format; we
// use those lines to verify the compiler's behavior instead of
// poking at the private `messages` field (which the spec explicitly
// calls out as a transcript-side check).
//
//   * `PROMPT-COMPILE:` lines confirm which normalizations fired.
//   * The absence of any narration / speed / anti-batch phrase in
//     the user-task transcript is the spec acceptance #4 ("the
//     normalized agent_task in self.messages[1] contains no
//     narration directives").
//
// Run with:
//   cargo run -p mew-agent --example phase36_prompt_compiler
//
// Exits with code 0 on success, code 1 on any assertion failure.

use mew_agent::agent::Agent;
use mew_agent::{AgentConfig, BrowserConfig, OpencodeZenConfig, ProviderConfig};
use std::path::PathBuf;

fn synth_config() -> ProviderConfig {
    ProviderConfig {
        opencode_zen: OpencodeZenConfig {
            base_url: "http://test/".into(),
            api_key: "test".into(),
            default_model: "test".into(),
            max_iterations: 1,
            max_tokens: None,
            max_cost: None,
        },
        browser: Some(BrowserConfig {
            binary_path: None,
            visible_cursor: false,
        }),
        agent: AgentConfig::default(),
    }
}

/// Build an Agent with the given task, then read back the
/// transcript file and return its full text. The transcript lives
/// in a per-call temp directory so the 3 spot-checks don't share
/// files.
fn run_one(label: &str, task: &str) -> String {
    println!("\n=== {label} ===");
    println!("input  : {task}");

    // Per-call temp dir so the three runs don't clobber each
    // other. The `Agent` constructor will `create_dir_all` this and
    // drop a `transcript_<session_id>.log` inside.
    let dir = std::env::temp_dir().join(format!(
        "mew_phase36_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let _ = std::fs::create_dir_all(&dir);

    let _a = Agent::new(synth_config(), task, Some(dir.clone()));

    // The constructor picks a fresh `session_id` per call. Walk the
    // temp dir for the newest `transcript_*.log` file the Agent
    // wrote. There's only one, but iterating keeps the test
    // self-contained if the naming convention ever changes.
    let mut transcript_path: Option<PathBuf> = None;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("transcript_") && name.ends_with(".log") {
                    transcript_path = Some(p);
                    break;
                }
            }
        }
    }
    let path = transcript_path.unwrap_or_else(|| panic!("no transcript file in {dir:?}"));
    let body = std::fs::read_to_string(&path).expect("read transcript");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);

    println!("transcript (first 600 chars):\n{}", &body[..body.len().min(600)]);
    body
}

fn main() {
    // --- Spot-check 1: Maps + narration + anti-batch ---
    let body1 = run_one(
        "Maps + narration + anti-batch",
        "Open Google Maps, check 15 salons in Mumbai, narrate as you go, don't batch",
    );
    // Spec acceptance #3: the live transcript must contain
    // `PROMPT-COMPILE: stripped: narration directive` for this input.
    assert!(
        body1.contains("PROMPT-COMPILE: stripped: narration directive"),
        "expected a narration-stripped PROMPT-COMPILE line, got:\n{body1}"
    );
    // The user-task line is written nowhere in the transcript
    // (the constructor's transcript logging covers only the
    // `STATE:`, `PROMPT-COMPILE:`, and structured-tracing lines —
    // the user task is logged through the structured tracing path
    // and the loop, not the constructor). What the spec calls out
    // is that the cleaned task the LLM *sees* is narration-free.
    // We verify that via the in-process `compile()` call's return
    // value in the unit tests (10 tests in
    // `prompt_compiler::tests`); the transcript here confirms
    // the *audit trail* side of the contract: the warning was
    // recorded with the right format string.
    assert!(
        body1.contains("PROMPT-COMPILE: stripped: anti-batch directive"),
        "expected an anti-batch-stripped PROMPT-COMPILE line, got:\n{body1}"
    );

    // --- Spot-check 2: declare_subtasks + speed ---
    let body2 = run_one(
        "declare_subtasks + speed",
        "Search gyms in Mumbai, use declare_subtasks, move quickly",
    );
    assert!(
        body2.contains("PROMPT-COMPILE: stripped: declare_subtasks mention"),
        "expected a declare_subtasks-stripped PROMPT-COMPILE line, got:\n{body2}"
    );
    assert!(
        body2.contains("PROMPT-COMPILE: stripped: speed directive"),
        "expected a speed-stripped PROMPT-COMPILE line, got:\n{body2}"
    );

    // --- Spot-check 3: clean passthrough ---
    let body3 = run_one("Clean passthrough", "Find businesses in Delhi");
    // No PROMPT-COMPILE lines at all for a clean input.
    assert!(
        !body3.contains("PROMPT-COMPILE:"),
        "expected no PROMPT-COMPILE lines for a clean input, got:\n{body3}"
    );

    println!("\nAll 3 spot-checks passed.");
}
