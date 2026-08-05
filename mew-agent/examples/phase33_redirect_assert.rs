// mew v3 — Phase 3.3 (regression) hard-assert: mid-task task-redirect
// steering must not cause a session teardown.
//
// The companion `phase33_redirect_steering.rs` is the
// "what-happened" record. This one is the hard-assert that
// `cargo run --example phase33_redirect_assert` should pass after
// the drain-timing fix is in place. A failure here means the
// regression is back.
//
// Assertions (all required, in order):
//   1. exactly one `session_id` is created during the run (i.e. no
//      second `Intent::BrowserTask` got spawned from a classifier
//      fallthrough).
//   2. the redirect message lands as a `USER:` line in the *same*
//      session's transcript (i.e. routed through `tx.send` and
//      drained by the agent loop, not reclassified).
//   3. the original baseline task string is still in the transcript
//      (i.e. history was not wiped or replaced).
//   4. the model in the same session either pivoted to the redirect
//      target (navigate tool) or addressed the redirect in a
//      `finish()` result string. A plain `finish()` on the
//      *original* task *without* seeing the redirect counts as a
//      failure, because that's the bug we just fixed.
//
// This test is *not* a substitute for the human-in-the-loop Tauri
// run, but it covers the routing-and-drain shape that the human
// reported broken. The Tauri re-run is captured separately in the
// Phase 3.3 evidence file.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio::time::sleep;

use mew_agent::agent::Agent;
use mew_agent::chat::UserMessage;
use mew_agent::session::SessionHandle;

#[allow(dead_code)]
struct ActiveSession {
    handle: SessionHandle,
    tx: tokio::sync::mpsc::Sender<UserMessage>,
}

type SessionSlot = Arc<Mutex<Option<ActiveSession>>>;

fn emit(label: &str) {
    println!("[agent-state] {}", label);
}

async fn run_browser_task_real(
    task_desc: String,
    config: mew_agent::ProviderConfig,
    slot: SessionSlot,
) -> anyhow::Result<(String, String, Vec<serde_json::Value>)> {
    emit("Started");

    let (browser, page, handler_task, job) = mew_cdp::launch(
        config.browser.as_ref().and_then(|b| b.binary_path.clone()),
        config.browser.as_ref().map(|b| b.visible_cursor).unwrap_or(false),
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to launch Chrome: {e}"))?;

    let mut agent = Agent::new(config, &task_desc, None);
    let chat_tx = agent.take_message_sender();
    let session_handle = agent.session_handle();
    let session_id = agent.session_id().to_string();

    {
        let mut lock = slot.lock().await;
        *lock = Some(ActiveSession {
            handle: session_handle,
            tx: chat_tx,
        });
    }
    println!(
        "[ui-wiring] ActiveSession installed in slot, session_id={}",
        session_id
    );

    let result = agent.run(&page).await;
    let final_state = match &result {
        Ok(_) => "Done",
        Err(_) => "Failed",
    };
    println!(
        "[ui-wiring] agent.run returned Ok?={}, final_state={}",
        result.is_ok(),
        final_state
    );

    {
        let mut lock = slot.lock().await;
        *lock = None;
    }
    println!("[ui-wiring] ActiveSession cleared from slot (terminal transition)");

    emit(final_state);

    let history = agent.history_snapshot();
    let _ = mew_cdp::shutdown(browser, handler_task, job).await;
    Ok((session_id, final_state.to_string(), history))
}

async fn try_route_to_running_session(
    slot: &SessionSlot,
    text: &str,
) -> Result<String, &'static str> {
    let mut lock = slot.lock().await;
    if let Some(session) = lock.as_ref() {
        let msg = UserMessage {
            text: text.to_string(),
            timestamp_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        };
        match session.tx.send(msg).await {
            Ok(()) => Ok(format!("[Routed to running agent] {}", text)),
            Err(_) => {
                *lock = None;
                Err("dead_session")
            }
        }
    } else {
        Err("no_active_session")
    }
}

fn read_transcript(session_id: &str) -> String {
    let path = std::path::PathBuf::from("transcripts")
        .join(format!("transcript_{}.log", session_id));
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        format!(
            "<could not read transcript at {}: {}>",
            path.display(),
            e
        )
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("PHASE33=redirect_assert — hard-assert regression for the redirect-during-task bug");

    let baseline_task = std::env::var("BASELINE_TASK")
        .unwrap_or_else(|_| "Navigate to https://example.com and report the main heading text.".to_string());
    let redirect_text = std::env::var("REDIRECT_TEXT").unwrap_or_else(|_| {
        "Actually, switch tasks: navigate to https://github.com/rust-lang/rust and report the repository description shown at the top of the README."
            .to_string()
    });
    let redirect_after_ms: u64 = std::env::var("REDIRECT_AFTER_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        // The exact value here is what the previous
        // phase33_redirect_steering run empirically needed to push
        // the redirect into the LLM-call-in-flight window for this
        // model + harness combo. 5s is enough to be inside the
        // first LLM call, late enough that the slot is populated.
        .unwrap_or(5_000);

    println!("baseline_task : {}", baseline_task);
    println!("redirect_text : {}", redirect_text);
    println!("redirect_after_ms : {}", redirect_after_ms);

    let config = mew_agent::load_config()?;
    let slot: SessionSlot = Arc::new(Mutex::new(None));

    let pre_existing: std::collections::HashSet<String> = {
        let dir = std::path::PathBuf::from("transcripts");
        let mut v: std::collections::HashSet<String> = std::collections::HashSet::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                if let Some(sid) = entry
                    .path()
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_prefix("transcript_"))
                    .and_then(|n| n.strip_suffix(".log"))
                {
                    v.insert(sid.to_string());
                }
            }
        }
        v
    };
    println!(
        "[harness] pre-existing transcript session_ids: {} file(s)",
        pre_existing.len()
    );

    // ----- Launch baseline task -----
    let task_handle = tokio::spawn({
        let slot = slot.clone();
        let config = config.clone();
        let task = baseline_task.clone();
        async move { run_browser_task_real(task, config, slot).await }
    });

    let mut slot_populated = false;
    for _ in 0..400 {
        sleep(Duration::from_millis(50)).await;
        if slot.lock().await.is_some() {
            slot_populated = true;
            break;
        }
    }
    assert!(
        slot_populated,
        "agent never installed its ActiveSession in the slot"
    );
    println!("[harness] session running; waiting {}ms before pushing redirect", redirect_after_ms);
    sleep(Duration::from_millis(redirect_after_ms)).await;

    let route_result = try_route_to_running_session(&slot, &redirect_text).await;
    match route_result {
        Ok(reply) => println!("[harness] redirect routed via live channel: {}", reply),
        Err(e) => {
            // If the push arrived after the session had already
            // ended (e.g. model was super fast and finished in
            // <5s), the routing will fail with `no_active_session`.
            // That's not the bug we're trying to surface — but it's
            // also not the desired scenario, so we hard-fail with
            // a clear message asking the human to increase
            // REDIRECT_AFTER_MS.
            panic!(
                "redirect FAILED to route via live channel ({}). \
                 Either the session ended too early, or the routing \
                 layer is broken. Try REDIRECT_AFTER_MS=2000 to push \
                 earlier, or REDIRECT_AFTER_MS=10000 if the model is \
                 slow.",
                e
            );
        }
    }

    let (sid, final_state, _history) = match task_handle.await {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => return Err(e),
        Err(e) => return Err(e.into()),
    };
    println!(
        "[harness] session done: sid={}, final_state={}",
        sid, final_state
    );

    // ----- Hard asserts -----
    let new_sids: Vec<String> = {
        let dir = std::path::PathBuf::from("transcripts");
        let mut v: Vec<String> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let p = entry.path();
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    if let Some(sid) = name
                        .strip_prefix("transcript_")
                        .and_then(|s| s.strip_suffix(".log"))
                    {
                        if !pre_existing.contains(sid) {
                            v.push(sid.to_string());
                        }
                    }
                }
            }
        }
        v.sort();
        v
    };
    println!("[harness] transcripts created by this run: {:?}", new_sids);

    // ASSERTION 1: exactly one session was created.
    assert_eq!(
        new_sids.len(),
        1,
        "Expected exactly 1 session to be created, got {} (sids={:?}). \
         A second session_id means the routing layer fell through to \
         the classifier and a second mew_cdp::launch was issued — \
         the bug is back.",
        new_sids.len(),
        new_sids
    );

    let body = read_transcript(&sid);

    // ASSERTION 2: the redirect message is in the transcript as a USER: line.
    let user_line: Option<String> = body
        .lines()
        .find(|l| l.contains("USER:") && l.contains(&redirect_text))
        .map(|s| s.to_string());
    assert!(
        user_line.is_some(),
        "Expected the redirect text to appear as a USER: line in the transcript, \
         but it was not found. Routing or drain is still missing the push.\n\
         Full transcript:\n{}",
        body
    );
    println!("[assert 2 PASS] USER: line for redirect present:");
    println!("    {}", user_line.unwrap());

    // ASSERTION 3: original baseline task is still in the transcript.
    assert!(
        body.contains(&baseline_task)
            || body.lines().any(|l| l.contains("Task:") && l.contains(&baseline_task)),
        "Original baseline task string was not in the transcript — history was \
         wiped or replaced. The drain fix may have broken the truncate path."
    );
    println!("[assert 3 PASS] original baseline task preserved in transcript");

    // ASSERTION 4: the model either pivoted to the redirect target
    // OR finished with content that references the redirect target.
    // A finish() on the *original* task with no mention of the
    // redirect means the LLM never saw it.
    let redirected_via_navigate = body
        .lines()
        .any(|l| l.contains("NAV-RESOLVE:") && l.contains("github.com/rust-lang/rust"));
    let finished_with_redirect = body.lines().any(|l| {
        l.contains("TOOL CALL: finish")
            && (l.contains("rust-lang") || l.contains("rust-lang/rust"))
    });
    assert!(
        redirected_via_navigate || finished_with_redirect,
        "Model did not address the redirect. Expected either a navigate to the \
         redirect target OR a finish() result that references the new target.\n\
         Full transcript:\n{}",
        body
    );
    println!(
        "[assert 4 PASS] model addressed the redirect: navigate={}, finish_with_redirect={}",
        redirected_via_navigate, finished_with_redirect
    );

    println!("\nALL PHASE33 REDIRECT ASSERTIONS PASSED");
    Ok(())
}
