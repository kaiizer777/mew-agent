// mew v3 — Phase 3.3 (regression): Mid-task task-redirecting steering.
//
// Background:
//   Phase 3.2 sub-test B proved that a *benign* steering message
//   ("also tell me the page's <title> tag") correctly landed as a
//   `USER:` line in the running session's transcript and the model
//   answered both the original and the steered question in a single
//   `finish()`. That is not the bug.
//
//   The real human scenario reported on the 3.2 review was different:
//   the human typed "open rust github instead" while the agent was
//   mid-task on a different site (tokio GitHub). Visually: the entire
//   Chromium window closed, there was a brief lag, and only on a
//   SECOND message did rust-lang.github.io open. The hypothesis
//   behind this test is that the model, given the redirect-style
//   message, called `finish()` to end the in-progress task — which
//   clears the `AppState.active_session` slot and `mew_cdp::shutdown`
//   runs — and only then does the user's *next* message get
//   classified as a fresh `Intent::BrowserTask("open rust github")`
//   which spawns a brand new `run_browser_task`, a new
//   `mew_cdp::launch`, and a new Chromium process tree.
//
// What this harness does:
//   Re-implements the same code path as `phase3_2_session_lifecycle`
//   (slot = `Arc<Mutex<Option<ActiveSession>>>`,
//   `tokio::spawn` instead of `tauri::async_runtime::spawn`),
//   but uses a *task-redirecting* steering message instead of a
//   benign one. Then reads the on-disk transcript to determine
//   whether the redirect was incorporated into the same session
//   (good) or whether the model `finish()`-ed the original task and
//   the redirect became the seed of a second session (bug).
//
// The harness is intentionally non-asserting on the "is this a bug?"
// question — it prints the transcript evidence and the verdict and
// leaves the call to the human reading the output. After the fix is
// applied, a follow-up `cargo run --example phase33_fixed_assert
// -p mew-agent` will do the asserts (kept separate so this initial
// run is the unambiguous "what was the actual behavior?" record).
//
// Run with: cargo run --example phase33_redirect_steering -p mew-agent
//
// Environment:
//   REDIRECT_TEXT  — text of the mid-task steering message
//                    (default: "actually, navigate to https://www.rust-lang.org
//                     instead and report the page's main heading text")
//   REDIRECT_AFTER_MS — how long to wait after the session is
//                    running before pushing the redirect. Must be
//                    long enough that the agent has reached the
//                    "mid-task" state (iteration >= 2, not
//                    pre-navigate). Default: 12000 (12s).
//   BASELINE_TASK  — the original seed task. Default matches the
//                    A/B/E sub-tests: example.com heading.

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
                println!("[ui-wiring] Sender.send() returned Err — agent loop is dead. Clearing slot.");
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

/// List all transcript files in the transcripts/ directory, sorted
/// by session-id timestamp ascending.
fn list_all_transcripts() -> Vec<(String, std::path::PathBuf)> {
    let dir = std::path::PathBuf::from("transcripts");
    let mut out: Vec<(String, std::path::PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if let Some(sid) = name
                    .strip_prefix("transcript_")
                    .and_then(|s| s.strip_suffix(".log"))
                {
                    out.push((sid.to_string(), p));
                }
            }
        }
    }
    // Sort by session-id string (which is "session_<unix_secs>") ascending.
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn print_transcript_block(label: &str, body: &str) {
    println!("\n--- TRANSCRIPT: {} ---", label);
    if body.is_empty() {
        println!("(empty)");
    } else {
        for line in body.lines() {
            println!("  {}", line);
        }
    }
    println!("--- END TRANSCRIPT ({}) ---\n", label);
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("PHASE33=redirect_steering — Phase 3.3 mid-task-redirect reproduction");

    // Read environment-driven knobs so the same harness can replay
    // different steer messages without a code change.
    let baseline_task = std::env::var("BASELINE_TASK")
        .unwrap_or_else(|_| "Navigate to https://example.com and report the main heading text.".to_string());
    let redirect_text = std::env::var("REDIRECT_TEXT").unwrap_or_else(|_| {
        "Actually, switch tasks: navigate to https://www.rust-lang.org and report the page's main heading text."
            .to_string()
    });
    let redirect_after_ms: u64 = std::env::var("REDIRECT_AFTER_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12_000);

    println!("baseline_task : {}", baseline_task);
    println!("redirect_text : {}", redirect_text);
    println!("redirect_after_ms : {}", redirect_after_ms);

    let config = mew_agent::load_config()?;
    let slot: SessionSlot = Arc::new(Mutex::new(None));

    // Snapshot the existing transcripts *before* the run so we can
    // tell which files were created by this harness vs. prior runs.
    let pre_existing: std::collections::HashSet<String> = list_all_transcripts()
        .into_iter()
        .map(|(sid, _p)| sid)
        .collect();
    println!(
        "[harness] pre-existing transcript session_ids: {} file(s)",
        pre_existing.len()
    );

    // ----- Phase 1: launch the baseline task -----
    let slot_for_run = slot.clone();
    let config_for_run = config.clone();
    let task_for_run = baseline_task.clone();
    let task_handle = tokio::spawn(async move {
        run_browser_task_real(task_for_run, config_for_run, slot_for_run).await
    });

    // Wait for the slot to be populated (i.e. the agent has launched
    // chromium and installed the ActiveSession).
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

    // Sleep so the agent is genuinely "mid-task" (past the first
    // navigate), not pre-navigate.
    sleep(Duration::from_millis(redirect_after_ms)).await;

    // ----- Phase 2: push the redirect-style steering message -----
    let route_result = try_route_to_running_session(&slot, &redirect_text).await;
    match route_result {
        Ok(reply) => println!("[harness] redirect pushed via live routing: {}", reply),
        Err(e) => {
            eprintln!(
                "[harness] redirect FAILED to route via live channel ({}). \
                 This is itself a routing regression; aborting.",
                e
            );
            // Don't return an error so the harness still leaves the
            // session in a clean state, but make the failure loud.
        }
    }

    // Wait for the agent task to terminate naturally.
    let (sid, final_state, _history) = match task_handle.await {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            eprintln!("[harness] run_browser_task_real returned error: {}", e);
            return Err(e);
        }
        Err(e) => {
            eprintln!("[harness] task join error: {}", e);
            return Err(e.into());
        }
    };
    println!(
        "[harness] session 1 done: sid={}, final_state={}",
        sid, final_state
    );

    // ----- Phase 3: read every transcript this harness created -----
    let post_all = list_all_transcripts();
    let new_sids: Vec<String> = post_all
        .iter()
        .map(|(sid, _p)| sid.clone())
        .filter(|sid| !pre_existing.contains(sid))
        .collect();
    println!(
        "[harness] transcripts created by this run: {} file(s)",
        new_sids.len()
    );
    for sid_new in &new_sids {
        let body = read_transcript(sid_new);
        print_transcript_block(sid_new, &body);
    }

    // ----- Phase 4: verdict, based on what's actually in the transcripts -----
    // (Not a hard assert — this run is the "what happened?" record.
    // The fixed-run asserts are a separate example to keep this
    // evidence pure.)
    println!("\n=== VERDICT (evidence-based, not asserted) ===");
    println!("sessions created by this run: {}", new_sids.len());
    if new_sids.len() == 1 {
        let only_sid = &new_sids[0];
        let body = read_transcript(only_sid);
        let has_baseline_task = body.contains(&baseline_task)
            || body.lines().any(|l| l.contains("Task:") && l.contains(&baseline_task));
        let has_user_redirect = body
            .lines()
            .any(|l| l.contains("USER:") && l.contains(&redirect_text));
        let has_finish_tool_call = body.lines().any(|l| l.contains("TOOL CALL: finish"));
        // The model can pivot via the `navigate` tool. The tool
        // dispatch prints `TOOL CALL: navigate ...`. The
        // `NAV-RESOLVE` line is written regardless of whether the
        // tool dispatch eventually succeeded (a timeout still
        // prints NAV-RESOLVE first, then a tool-result error). So
        // the most reliable signal of "the model pivoted to the
        // new URL" is a NAV-RESOLVE input matching the redirect
        // target.
        let redirect_target_hint = "rust-lang.org";
        let has_rust_navigate = body.lines().any(|l| {
            (l.contains("TOOL CALL: navigate") || l.contains("NAV-RESOLVE:"))
                && l.contains(redirect_target_hint)
        });

        println!("  baseline task present  : {}", has_baseline_task);
        println!("  redirect as USER: line : {}", has_user_redirect);
        println!("  finish() tool called   : {}", has_finish_tool_call);
        println!("  navigate to rust-lang  : {}", has_rust_navigate);

        if has_user_redirect && has_rust_navigate {
            println!(
                "  OBSERVATION: single session; the redirect was incorporated as an in-place \
                 navigate() within the same session. This is the desired behavior."
            );
        } else if has_user_redirect && has_finish_tool_call && !has_rust_navigate {
            println!(
                "  OBSERVATION: single session, redirect was captured as a USER: line, but the \
                 model still called finish() without ever pivoting to the new target. The model \
                 may be too eager to finish; consider a system-prompt or completeness-gate \
                 refinement as a follow-up. (Out of scope for the routing-drain fix.)"
            );
        } else if !has_user_redirect {
            println!(
                "  OBSERVATION: single session, but the redirect never landed as a USER: line. \
                 This means the routing or the drain missed the push. The fix is incomplete."
            );
        } else {
            println!(
                "  OBSERVATION: single session, redirect present, but no rust-lang navigation \
                 happened. Could be (a) the model chose a different path, (b) the model ran out \
                 of iterations, or (c) something else."
            );
        }
    } else {
        println!(
            "  OBSERVATION: more than one session was created during the run. \
             This is consistent with the 'Chromium window closed + new window opened' report. \
             To see the session IDs and their first USER/TASK lines, inspect the transcripts above."
        );
    }

    println!("\n=== Phase 3.3 reproduction run finished ===");
    Ok(())
}
