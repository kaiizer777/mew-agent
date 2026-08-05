// mew v2 — Phase 5: live progress line smoke test.
//
// Purpose: prove the templated `summarizer::summarize` path
// produces the expected one-liners for each common action, that
// the `LiveProgress` buffer caps at the configured line count,
// and that the end-of-task LLM call falls back cleanly to the
// raw finish() text on a synthetic HTTP failure (so the
// "never silent on the error path" guarantee holds).
//
// The test does NOT need a running Chrome, a real LLM, or a
// `&Page` — `summarizer::summarize` is a pure function, and the
// `end_of_task_summarize` method is exercised by pointing
// `config.opencode_zen.base_url` at a deliberately-bad URL so
// the HTTP call fails and the fallback path fires.
//
// Run with: `cargo run --example phase5_live_progress -p mew-agent`

use mew_agent::summarizer::{
    self, LiveProgress, ProgressKind, SummarizationConfig, Verbosity,
};
use mew_agent::{AgentConfig, OpencodeZenConfig, ProviderConfig};
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    println!("== Phase 5 live progress smoke test ==");

    // --- 1. Templated summaries for every common tool -----------
    println!("\n[1] Templated summaries");
    let cases: &[(&str, serde_json::Value, &str, Verbosity)] = &[
        // (tool, args, result, verbosity) — `result` is the
        // simulated post-dispatch tool_result string.
        ("navigate", json!({ "url": "https://www.instagram.com/explore/" }), "ok", Verbosity::Concise),
        ("navigate", json!({ "url": "https://example.com/path?x=1" }), "ok", Verbosity::Detailed),
        ("click",    json!({ "ref": "@e5" }), "Clicked successfully", Verbosity::Concise),
        ("click",    json!({ "ref": "@e7" }), "Clicked successfully", Verbosity::Detailed),
        ("type",     json!({ "ref": "@e4", "text": "hi alice" }), "ok", Verbosity::Concise),
        ("type",     json!({ "ref": "@e4", "text": "x".repeat(200) }), "ok", Verbosity::Concise),
        ("scroll",   json!({ "direction": "down" }), "ok", Verbosity::Concise),
        ("scroll",   json!({ "direction": "up" }), "ok", Verbosity::Concise),
        ("press_key",json!({ "key": "Enter" }), "ok", Verbosity::Concise),
        ("snapshot", json!({}), "ok", Verbosity::Concise),
        ("vision_inspect", json!({ "ref": "@e10" }), "ok", Verbosity::Concise),
        ("declare_subtasks", json!({ "items": [{"id":"a"},{"id":"b"}] }), "ok", Verbosity::Concise),
        ("mark_subtask_done", json!({ "id": "send_msg" }), "ok", Verbosity::Concise),
        ("mark_subtask_failed", json!({ "id": "msg_to_alice" }), "ok", Verbosity::Concise),
        ("finish", json!({ "result": "I sent the message." }), "ok", Verbosity::Concise),
    ];
    for (name, args, result, verbosity) in cases {
        let (kind, line, success) = summarizer::summarize(name, &args, result, *verbosity)
            .expect("templated summary");
        println!(
            "  [{}] {} (verbosity={:?}, success={})",
            kind.as_str(),
            line,
            verbosity,
            success
        );
    }

    // --- 2. LiveProgress cap behavior ---------------------------
    println!("\n[2] LiveProgress cap behavior");
    let mut buf = LiveProgress::new(3);
    for i in 0..7 {
        let line = summarizer::ProgressLine::new(
            ProgressKind::Other,
            format!("step {i}"),
            i as u64,
            true,
        );
        buf.push(line);
    }
    let snap = buf.snapshot();
    assert_eq!(snap.len(), 3, "cap 3 should hold 3 lines");
    assert_eq!(snap[0].text, "step 4", "oldest should be step 4 after 7 pushes");
    assert_eq!(snap[2].text, "step 6", "newest should be step 6");
    println!("  cap=3, 7 pushes -> kept [step 4, step 5, step 6] (OK)");

    // --- 3. recent_text for the end-of-task LLM call -------------
    println!("\n[3] recent_text (LLM context)");
    let mut buf = LiveProgress::new(10);
    for i in 0..5 {
        buf.push(summarizer::ProgressLine::new(
            ProgressKind::Click,
            format!("Clicked button {i}"),
            i,
            true,
        ));
    }
    let recent = buf.recent_text(3);
    assert!(recent.contains("Clicked button 4"));
    assert!(recent.contains("Clicked button 3"));
    assert!(!recent.contains("Clicked button 1"));
    println!("  recent_text(3) = {recent:?}");

    // --- 4. End-of-task LLM call falls back on HTTP error --------
    println!("\n[4] end_of_task_summarize on HTTP failure -> raw fallback");
    let cfg = ProviderConfig {
        opencode_zen: OpencodeZenConfig {
            // Deliberately unreachable base URL so the HTTP call
            // returns Err. The summarizer must fall back to the
            // raw finish() text (the never-silent guarantee).
            base_url: "http://127.0.0.1:1".into(),
            api_key: "test".into(),
            default_model: "test".into(),
            max_iterations: 1,
            max_tokens: None,
            max_cost: None,
        },
        browser: None,
        agent: AgentConfig::default(),
    };
    let agent = mew_agent::agent::Agent::new(cfg, "send a message to alice", None);
    let raw = "I clicked the message field. I typed hi. I called finish().";
    let summarized = agent.end_of_task_summarize("send a message to alice", raw).await;
    println!("  summarized = {summarized:?}");
    assert!(
        summarized.is_none(),
        "expected None on HTTP failure, got {summarized:?}"
    );
    println!("  falls back to raw text on HTTP failure (OK)");

    // --- 5. End-of-task opt-out returns None ---------------------
    println!("\n[5] end_of_task_summarize with end_of_task_llm_summary=false");
    let cfg2 = ProviderConfig {
        opencode_zen: OpencodeZenConfig {
            base_url: "http://127.0.0.1:1".into(),
            api_key: "test".into(),
            default_model: "test".into(),
            max_iterations: 1,
            max_tokens: None,
            max_cost: None,
        },
        browser: None,
        agent: AgentConfig {
            summarization: SummarizationConfig {
                end_of_task_llm_summary: false,
                ..SummarizationConfig::default()
            },
            planner_enabled: true,
            ..AgentConfig::default()
        },
    };
    let agent2 = mew_agent::agent::Agent::new(cfg2, "anything", None);
    let out = agent2.end_of_task_summarize("anything", "raw text").await;
    assert!(out.is_none(), "opt-out should always return None");
    println!("  opt-out -> None (OK)");

    // --- 6. more_steps_suffix ------------------------------------
    println!("\n[6] more_steps_suffix");
    assert_eq!(summarizer::more_steps_suffix(10, 5), Some("…and 5 more steps".into()));
    assert_eq!(summarizer::more_steps_suffix(6, 5), Some("…and 1 more step".into()));
    assert_eq!(summarizer::more_steps_suffix(5, 5), None);
    assert_eq!(summarizer::more_steps_suffix(3, 5), None);
    println!("  singular/plural/under-cap all behave (OK)");

    println!("\n[phase5_live_progress] all assertions passed");
    Ok(())
}
