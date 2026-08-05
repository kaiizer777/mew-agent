// Phase 1.2 critical evidence: confirms OpenCode Zen + mimo-v2.5-pro
// actually returns a well-formed `classify_intent` tool call when forced
// via `tool_choice: required`.
//
// This is the assumption Step 2 of work.md depends on:
//   "route intent through a classify_intent(intent, reply) tool call
//    (schema-guaranteed via the mechanism already proven to work),
//    not through response_format."
//
// If this test prints a well-formed tool_call, we're good. If the model
// returns free text, refuses, or returns a malformed call, we have to
// decide NOW (before Step 2 is built) how to fall back.

use mew_agent::{load_config, OpencodeZenConfig};
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = load_config()?;
    let zen: &OpencodeZenConfig = &cfg.opencode_zen;
    let url = format!("{}/chat/completions", zen.base_url);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .build()?;

    // The exact tool definition Step 2 will use.
    let tools = json!([{
        "type": "function",
        "function": {
            "name": "classify_intent",
            "description": "Classify a user chat message as either a normal conversational reply (chat) or a request to do something in the browser (browser_task). Return the decision in `intent` and a short text in `reply` (a conversational answer for chat, or a rephrased task statement for browser_task).",
            "parameters": {
                "type": "object",
                "properties": {
                    "intent": {
                        "type": "string",
                        "enum": ["chat", "browser_task"]
                    },
                    "reply": {
                        "type": "string",
                        "description": "If intent=chat, a short conversational reply. If intent=browser_task, a rephrased task description that an agent could execute."
                    }
                },
                "required": ["intent", "reply"]
            }
        }
    }]);

    // Four test cases:
    //  1) clearly chat
    //  2) clearly browser task
    //  3) ambiguous that requires context (we won't include prior context
    //     here, but we'll see how the model handles a vague one on its own)
    //  4) "open it" — needs the prior turn's context to route as a browser task
    let standalone: &[&str] = &[
        "hey, how's it going?",
        "open wikipedia and search for Rust programming language",
        "check that for me",
    ];

    for (i, msg) in standalone.iter().enumerate() {
        println!("\n=== CASE {} ===", i + 1);
        println!("user: {msg}");

        let body = json!({
            "model": zen.default_model,
            "messages": [
                { "role": "user", "content": *msg }
            ],
            "tools": tools,
            // Force the model to call the tool — no free-text escape hatch.
            "tool_choice": "required"
        });

        let res = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", zen.api_key))
            .json(&body)
            .send()
            .await?;

        let status = res.status();
        let raw = res.text().await?;

        if !status.is_success() {
            println!("HTTP {}: {}", status, raw);
            continue;
        }

        // Print the raw response so we can inspect it ourselves.
        let v: Value = serde_json::from_str(&raw)?;
        println!("raw response (truncated to 800 chars):");
        let s = serde_json::to_string_pretty(&v)?;
        if s.len() > 800 {
            println!("{}... <truncated, total {} chars>", &s[..800], s.len());
        } else {
            println!("{}", s);
        }

        // Walk the OpenAI-shape response and extract the tool call.
        let choice = &v["choices"][0];
        let finish = choice["finish_reason"].as_str().unwrap_or("<none>");
        println!("finish_reason: {}", finish);

        if let Some(tool_calls) = choice["message"]["tool_calls"].as_array() {
            println!("tool_calls.len = {}", tool_calls.len());
            for (j, tc) in tool_calls.iter().enumerate() {
                let name = tc["function"]["name"].as_str().unwrap_or("<none>");
                let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
                println!("  [{}] name={} args={}", j, name, args_str);
                // Parse arguments and verify schema.
                match serde_json::from_str::<Value>(args_str) {
                    Ok(args) => {
                        let intent = args["intent"].as_str().unwrap_or("<missing>");
                        let reply = args["reply"].as_str().unwrap_or("<missing>");
                        println!("    parsed: intent={} reply={:?}", intent, reply);
                        match intent {
                            "chat" | "browser_task" => println!("    SCHEMA OK"),
                            other => println!("    SCHEMA FAIL: unexpected intent value `{}`", other),
                        }
                    }
                    Err(e) => println!("    SCHEMA FAIL: arguments not valid JSON: {e}"),
                }
            }
        } else {
            println!("NO tool_calls at all in response.message — model did not call the tool");
            if let Some(content) = choice["message"]["content"].as_str() {
                println!("content fallback: {:?}", content);
            }
        }
    }

    // Case 4: two-turn exchange. Turn 1 names a site, turn 2 is "open it".
    // The model should use the prior context to route as a browser_task.
    println!("\n=== CASE 4 (with prior context) ===");
    println!("user[0]: search wikipedia for Rust");
    println!("user[1]: open it");
    let body4 = json!({
        "model": zen.default_model,
        "messages": [
            { "role": "user", "content": "search wikipedia for Rust" },
            { "role": "assistant", "content": "Sure, navigating to wikipedia now." },
            { "role": "user", "content": "open it" }
        ],
        "tools": tools,
        "tool_choice": "required"
    });
    let res4 = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", zen.api_key))
        .json(&body4)
        .send()
        .await?;
    let raw4 = res4.text().await?;
    let v4: Value = serde_json::from_str(&raw4)?;
    let s4 = serde_json::to_string_pretty(&v4)?;
    println!("raw response (truncated):");
    if s4.len() > 800 {
        println!("{}... <truncated, total {} chars>", &s4[..800], s4.len());
    } else {
        println!("{}", s4);
    }
    let choice4 = &v4["choices"][0];
    println!("finish_reason: {}", choice4["finish_reason"]);
    if let Some(tcs) = choice4["message"]["tool_calls"].as_array() {
        for (j, tc) in tcs.iter().enumerate() {
            let name = tc["function"]["name"].as_str().unwrap_or("<none>");
            let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
            println!("  [{}] name={} args={}", j, name, args_str);
            if let Ok(args) = serde_json::from_str::<Value>(args_str) {
                let intent = args["intent"].as_str().unwrap_or("<missing>");
                let reply = args["reply"].as_str().unwrap_or("<missing>");
                println!("    parsed: intent={} reply={:?}", intent, reply);
            }
        }
    } else {
        println!("NO tool_calls at all");
    }

    Ok(())
}
