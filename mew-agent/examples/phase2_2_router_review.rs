// Phase 2.2 review & testing — primary harness.
//
// Goal: drive `mew_agent::router::classify` with a varied corpus of real
// messages and read the actual classification decision for each one
// against what we'd expect. The 2.2 checklist is:
//
//   - 10+ varied real messages (plain chat, clear browser tasks, deliberately
//     ambiguous ones like "check that for me" / "open it" / "what about the
//     other one")
//   - Two-turn exchange where turn 1 names a site and turn 2 says "open it"
//     — confirm context is actually used
//   - Read the raw request/response for a few calls ourselves
//
// This calls the EXACT same `classify()` function `send_message` (the Tauri
// command) calls, against the EXACT same provider/model `mew-agent` is
// configured for. The only thing not exercised here is the final hop from
// the Rust fn to the Tauri command's `Result<String, String>` boundary —
// and the unit test below in `tests/` covers that wrap.
//
// Run: `cargo run --example phase2_2_router_review -p mew-agent`

use mew_agent::router::{classify, ConversationMessage, Intent};
use mew_agent::{load_config, ProviderConfig};
use std::time::{Duration, Instant};

/// Per-case fixture: the message, the conversation context to send alongside,
/// and a short human-readable expectation for the report.
struct Case {
    label: &'static str,
    msg: &'static str,
    ctx: Vec<ConversationMessage>,
    /// "chat", "browser_task", or "either" (genuinely ambiguous to a human).
    expect: &'static str,
}

fn corpus() -> Vec<Case> {
    vec![
        // --- Plain chat (clearly no browser intent) ---
        Case {
            label: "plain-greeting",
            msg: "hey, how's it going?",
            ctx: vec![],
            expect: "chat",
        },
        Case {
            label: "thanks",
            msg: "thanks, that's all I needed",
            ctx: vec![],
            expect: "chat",
        },
        Case {
            label: "meta-about-model",
            msg: "what model are you running on?",
            ctx: vec![],
            expect: "chat",
        },
        Case {
            label: "explanatory-question",
            msg: "why is rust's borrow checker so strict?",
            ctx: vec![],
            expect: "chat",
        },
        // --- Clear browser tasks ---
        Case {
            label: "clear-wiki",
            msg: "open wikipedia and search for Rust programming language",
            ctx: vec![],
            expect: "browser_task",
        },
        Case {
            label: "clear-github",
            msg: "go to github.com and find the rust-lang/rust repo",
            ctx: vec![],
            expect: "browser_task",
        },
        Case {
            label: "clear-shop",
            msg: "search amazon for noise-cancelling headphones under $200",
            ctx: vec![],
            expect: "browser_task",
        },
        // --- Deliberately ambiguous on its own ---
        Case {
            label: "ambiguous-no-ctx",
            msg: "check that for me",
            ctx: vec![],
            expect: "either", // genuinely ambiguous without prior context
        },
        Case {
            label: "ambiguous-other-one",
            msg: "what about the other one?",
            ctx: vec![],
            expect: "either",
        },
        // --- Pronoun-heavy without context (should be chat, since there's
        //     no site to act on) ---
        Case {
            label: "pronoun-only-no-ctx",
            msg: "open it",
            ctx: vec![],
            expect: "chat", // nothing to open, model should ask for clarification
        },
        // --- Two-turn exchange: turn 1 names a site, turn 2 says only
        //     "open it". The critical test that conversation context is
        //     genuinely used. ---
        Case {
            label: "two-turn-open-it",
            msg: "open it",
            ctx: vec![
                ConversationMessage {
                    role: "user".into(),
                    content: "search wikipedia for Rust".into(),
                },
                ConversationMessage {
                    role: "assistant".into(),
                    content: "Sure, navigating to wikipedia now.".into(),
                },
            ],
            expect: "browser_task", // MUST route as browser task, not chat
        },
        // --- Another two-turn, different pronoun pattern ---
        Case {
            label: "two-turn-check-it",
            msg: "can you check it for me?",
            ctx: vec![
                ConversationMessage {
                    role: "user".into(),
                    content: "go to github.com/tokio-rs/tokio".into(),
                },
                ConversationMessage {
                    role: "assistant".into(),
                    content: "On the Tokio repo page now.".into(),
                },
            ],
            expect: "browser_task",
        },
        // --- Multi-turn then a chat follow-up (context-dependent classification) ---
        Case {
            label: "chat-after-task",
            msg: "thanks, that was quick!",
            ctx: vec![
                ConversationMessage {
                    role: "user".into(),
                    content: "open wikipedia".into(),
                },
                ConversationMessage {
                    role: "assistant".into(),
                    content: "Wikipedia is open.".into(),
                },
            ],
            expect: "chat",
        },
    ]
}

fn render_intent(i: &Intent) -> (&'static str, String) {
    match i {
        Intent::Chat(r) => ("Chat", r.clone()),
        Intent::BrowserTask(t) => ("BrowserTask", t.clone()),
    }
}

/// Strip non-essential whitespace for a compact log line.
fn compact(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg: ProviderConfig = load_config()?;
    println!(
        "phase2.2 router review — model={} base={}",
        cfg.opencode_zen.default_model, cfg.opencode_zen.base_url
    );
    println!();

    let cases = corpus();
    let mut passed = 0usize;
    let mut mismatched = 0usize;
    let mut errors = 0usize;
    let mut mismatches: Vec<String> = Vec::new();
    let mut errs: Vec<String> = Vec::new();

    let started = Instant::now();
    for (i, c) in cases.iter().enumerate() {
        print!("[{:02}] {:<22} ", i + 1, c.label);
        let t0 = Instant::now();
        let outcome = classify(c.msg, &c.ctx, &cfg).await;
        let dt = t0.elapsed();

        match outcome {
            Ok(intent) => {
                let (variant, payload) = render_intent(&intent);
                let verdict = match (c.expect, variant) {
                    ("chat", "Chat") => "OK",
                    ("browser_task", "BrowserTask") => "OK",
                    ("either", _) => "OK (ambiguous, either fine)",
                    (exp, got) => {
                        mismatched += 1;
                        mismatches.push(format!(
                            "[{}] expected={} got={} msg={:?} ctx.len={}",
                            c.label, exp, got, c.msg, c.ctx.len()
                        ));
                        "MISMATCH"
                    }
                };
                println!(
                    "→ {:<13} ({}ms) [{}]  reply={}",
                    variant,
                    dt.as_millis(),
                    verdict,
                    compact(&payload)
                );
                if verdict == "OK" {
                    passed += 1;
                }
            }
            Err(e) => {
                errors += 1;
                errs.push(format!("[{}] classify error: {e}", c.label));
                println!("→ ERR  ({}ms) {}", dt.as_millis(), e);
            }
        }

        // Be polite to the API and avoid blowing through any rate limit.
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let total = cases.len();
    println!();
    println!("==== summary ====");
    println!("total:     {}", total);
    println!("passed:    {}", passed);
    println!("mismatch:  {}", mismatched);
    println!("errors:    {}", errors);
    println!("elapsed:   {:?}", started.elapsed());

    if !mismatches.is_empty() {
        println!();
        println!("---- mismatches ----");
        for m in &mismatches {
            println!("{m}");
        }
    }
    if !errs.is_empty() {
        println!();
        println!("---- errors ----");
        for e in &errs {
            println!("{e}");
        }
    }

    // Exit non-zero on any mismatch or unhandled error so this binary is
    // useful as a CI gate. (Phase 1.2's example didn't fail-fast; the 2.2
    // step is the one that has to actually prove behavior, not just log.)
    if mismatched > 0 || errors > 0 {
        std::process::exit(2);
    }
    Ok(())
}
