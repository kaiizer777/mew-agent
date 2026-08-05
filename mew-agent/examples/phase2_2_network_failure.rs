// Phase 2.2 review & testing — network failure surfacing.
//
// The 2.2 checklist explicitly asks: "Deliberately break the network (or
// point base_url at something invalid) mid-session and confirm the
// classification failure surfaces as a real visible error in the chat UI,
// not a silently dropped message or a frozen input box."
//
// Headless test: we can't render the UI, but we CAN exercise the same
// `classify()` call with a deliberately broken base_url, read the
// returned error, and confirm:
//   1. it returns Err (not silently Ok)
//   2. the error message is informative (names "Network error" or
//      "API returned error status" or similar — not a useless empty string)
//   3. the same path is what `send_message` would propagate as
//      `Err(format!("Classification failed: {e}"))` to the frontend.
//
// We do this in two modes:
//   A) base_url points at a port nothing is listening on
//      → expect reqwest connection error → "Network error during
//        classification" wrapper
//   B) base_url points at a real but non-OpenAI endpoint (httpbin /status/500)
//      → expect non-2xx status → "API returned error status" wrapper
//
// Run: `cargo run --example phase2_2_network_failure -p mew-agent`

use mew_agent::router::{classify, ConversationMessage, Intent};
use mew_agent::{load_config, OpencodeZenConfig, ProviderConfig};

#[derive(Debug)]
struct Outcome {
    label: String,
    err: String,
    _saw_classification_prefix: bool,
    // The classifier wraps with "Network error during classification: ..."
    // or "API returned error status ..." or "Failed to parse ..." etc.
    saw_meaningful_substring: bool,
}

fn classify_failure_text(e: &anyhow::Error) -> (bool, bool) {
    let s = format!("{e}");
    let has_classification_prefix = false; // prefix is added in send_message
    let has_meaningful = s.contains("Network error during classification")
        || s.contains("API returned error status")
        || s.contains("Failed to parse classification JSON")
        || s.contains("No tool_calls in response")
        || s.contains("Failed to read classification response body")
        || s.contains("Tool call arguments is not a string")
        || s.contains("Empty tool_calls array")
        || s.contains("Failed to parse tool call arguments");
    (has_classification_prefix, has_meaningful)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Mode A: nothing is listening on this port.
    let bad_url_a = OpencodeZenConfig {
        base_url: "http://127.0.0.1:1".to_string(), // port 1 = nothing
        api_key: "irrelevant".to_string(),
        default_model: "irrelevant".to_string(),
        max_iterations: 1,
        max_tokens: None,
        max_cost: None,
    };
    let cfg_a = ProviderConfig {
        opencode_zen: bad_url_a,
        browser: None,
        agent: Default::default(),
    };

    // Mode B: real HTTP endpoint, but not a chat-completions endpoint that
    // will return success. We use the real /v1/ root of the same provider
    // (we KNOW that exists), but post a malformed body to a path that
    // returns 404. Easier alternative: use a different host that's
    // guaranteed-reachable but won't return 200 to a /chat/completions
    // request. httpbin.org's /status/500 does exactly that.
    let bad_url_b = OpencodeZenConfig {
        base_url: "https://httpbin.org".to_string(),
        api_key: "irrelevant".to_string(),
        default_model: "irrelevant".to_string(),
        max_iterations: 1,
        max_tokens: None,
        max_cost: None,
    };
    let cfg_b = ProviderConfig {
        opencode_zen: bad_url_b,
        browser: None,
        agent: Default::default(),
    };

    let cases: Vec<(&str, &ProviderConfig, &str)> = vec![
        ("A: port-1 connection refused", &cfg_a, "check that for me"),
        ("B: httpbin /status/500", &cfg_b, "open wikipedia"),
    ];

    let ctx: Vec<ConversationMessage> = vec![];
    let mut outcomes: Vec<Outcome> = Vec::new();

    for (label, cfg, msg) in &cases {
        println!("=== {} ===", label);
        let result = classify(msg, &ctx, cfg).await;
        match result {
            Ok(i) => {
                println!("  UNEXPECTED Ok: {:?}", i);
                outcomes.push(Outcome {
                    label: label.to_string(),
                    err: "<no error>".into(),
                    _saw_classification_prefix: false,
                    saw_meaningful_substring: false,
                });
            }
            Err(e) => {
                let (prefix, meaningful) = classify_failure_text(&e);
                println!("  err: {e}");
                println!("  wraps with one of the documented classifier error strings: {meaningful}");
                outcomes.push(Outcome {
                    label: label.to_string(),
                    err: format!("{e}"),
                    _saw_classification_prefix: prefix,
                    saw_meaningful_substring: meaningful,
                });
            }
        }
        println!();
    }

    // Mode C: same broken endpoint, but exercise the EXACT same wrap that
    // `send_message` does. We don't import the Tauri fn (it's in another
    // crate); we replicate the wrap in-process and confirm it produces a
    // string the frontend would actually display.
    println!("=== C: simulating send_message's wrap on the broken-URL error ===");
    let fake_send = |e: anyhow::Error| format!("Classification failed: {e}");
    let res = classify("hey", &[], &cfg_a).await;
    match res {
        Ok(_) => println!("  UNEXPECTED Ok on broken URL"),
        Err(e) => {
            let wrapped = fake_send(e);
            println!("  frontend-bound string: {wrapped}");
            let visible = !wrapped.is_empty() && wrapped.contains("Classification failed");
            println!("  has visible error prefix for UI: {visible}");
        }
    }

    // Mode D: also confirm a working config (the real one) does NOT trip
    // the failure path on a real chat message — this is the negative test
    // that the failure path isn't accidentally triggered in normal use.
    println!();
    println!("=== D: negative — real config + plain chat should succeed ===");
    let real = load_config()?;
    let res = classify("hello there", &[], &real).await;
    match res {
        Ok(Intent::Chat(reply)) => {
            println!("  Ok(Chat({:?}))", reply);
        }
        Ok(Intent::BrowserTask(t)) => {
            println!("  UNEXPECTED BrowserTask({:?}) on a plain greeting", t);
        }
        Err(e) => {
            println!("  UNEXPECTED Err on real config: {e}");
        }
    }

    // Final report
    println!();
    println!("==== summary ====");
    let all_meaningful = outcomes
        .iter()
        .all(|o| o.saw_meaningful_substring || o.err == "<no error>");
    let any_unexpected_ok = outcomes.iter().any(|o| o.err == "<no error>");
    for o in &outcomes {
        println!(
            "  {:<40} err_meaningful={}  unexpected_ok={}",
            o.label, o.saw_meaningful_substring, any_unexpected_ok
        );
    }
    println!();
    println!("verdict: all errors are informative = {all_meaningful}");

    if any_unexpected_ok {
        eprintln!("FAIL: classify returned Ok on a deliberately broken base_url");
        std::process::exit(2);
    }
    if !all_meaningful {
        eprintln!("FAIL: at least one error string is not informative");
        std::process::exit(2);
    }
    Ok(())
}
