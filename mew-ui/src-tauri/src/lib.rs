use mew_agent::{load_config, ProviderConfig};
use mew_agent::session::SessionHandle;
use mew_agent::chat::UserMessage;
use std::sync::Arc;
use tokio::sync::Mutex;
use tauri::{AppHandle, Manager, State, Emitter};

/// Phase 1.2: real proof that mew-agent compiles in and runs.
/// Loads config.yaml from the workspace root, returns a tiny summary of
/// fields pulled from the actual parsed config — not a hardcoded string.
#[tauri::command]
fn get_config_summary() -> Result<String, String> {
    let cfg: ProviderConfig = load_config().map_err(|e| format!("load_config failed: {e}"))?;
    Ok(format!(
        "model={} base_url={} max_iter={} browser_binary={:?}",
        cfg.opencode_zen.default_model,
        cfg.opencode_zen.base_url,
        cfg.opencode_zen.max_iterations,
        cfg.browser
            .as_ref()
            .and_then(|b| b.binary_path.as_deref())
    ))
}

#[derive(serde::Deserialize)]
pub struct FrontendMessage {
    role: String,
    content: String,
}

// Phase 3.1: Tauri managed state to hold the running agent session
struct ActiveSession {
    #[allow(dead_code)] // Will be used in Phase 6 for pause/resume via UI buttons
    handle: SessionHandle,
    tx: tokio::sync::mpsc::Sender<UserMessage>,
}

#[derive(Default)]
struct AppState {
    active_session: Arc<Mutex<Option<ActiveSession>>>,
}

#[tauri::command]
async fn send_message(
    text: String, 
    history: Vec<FrontendMessage>,
    app_handle: AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    // Step 3.1: Route subsequent messages to running agent if active
    let mut session_lock = state.active_session.lock().await;
    if let Some(session) = session_lock.as_ref() {
        let msg = UserMessage {
            text: text.clone(),
            timestamp_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        // Try to send. If the channel is closed (agent loop exited but state wasn't cleared),
        // we'll get an error and fall through.
        if session.tx.send(msg).await.is_ok() {
            return Ok(format!("[Routed to running agent] {}", text));
        } else {
            // The agent loop is dead. Clear it from state and fall through to classification.
            *session_lock = None;
        }
    }
    
    // Release lock before async classification
    drop(session_lock);

    let cfg = load_config().map_err(|e| format!("load_config failed: {e}"))?;
    
    let context: Vec<mew_agent::router::ConversationMessage> = history.into_iter().map(|msg| {
        mew_agent::router::ConversationMessage {
            role: msg.role,
            content: msg.content,
        }
    }).collect();
    
    match mew_agent::router::classify(&text, &context, &cfg).await {
        Ok(mew_agent::router::Intent::Chat(reply)) => Ok(reply),
        Ok(mew_agent::router::Intent::BrowserTask(task)) => {
            // Spawn the agent task
            let state_clone = state.active_session.clone();
            let app_clone = app_handle.clone();
            let cfg_clone = cfg.clone();
            let task_clone = task.clone();
            
            tauri::async_runtime::spawn(async move {
                if let Err(e) = run_browser_task(task_clone, cfg_clone, state_clone, app_clone).await {
                    eprintln!("Browser task failed: {}", e);
                }
            });
            
            Ok(format!("[Browser Task Started] {}", task))
        }
        Err(e) => Err(format!("Classification failed: {e}")),
    }
}

async fn run_browser_task(
    task_desc: String,
    config: ProviderConfig,
    state: Arc<Mutex<Option<ActiveSession>>>,
    app_handle: AppHandle,
) -> anyhow::Result<()> {
    // Surface minimal status ping
    let _ = app_handle.emit("agent-state", "Started");

    let (browser, page, handler_task, job) = mew_cdp::launch(
        config.browser.as_ref().and_then(|b| b.binary_path.clone()),
        config.browser.as_ref().map(|b| b.visible_cursor).unwrap_or(false),
    ).await.map_err(|e| anyhow::anyhow!("Failed to launch Chrome: {e}"))?;

    // Phase 4 (Bug 3 fix): resolve a transcript directory that lives
    // OUTSIDE the Tauri source tree. Tauri's dev-mode file watcher
    // (see `tauri-cli/src/interface/rust.rs::run_dev_watcher` and
    // `tauri-cli/src/interface/rust.rs::get_watch_folders`) recursively
    // watches the entire `src-tauri/` directory and treats any change
    // in it as a reason to rebuild + restart the app. Writing the
    // agent's on-disk transcript under `src-tauri/transcripts/` was
    // triggering that watcher on every iteration, which printed
    // `File ... changed. Rebuilding application...`, killed the
    // running agent and its Chrome window, and started a fresh chat
    // — the Bug 3 restart loop.
    //
    // We use Tauri's `app_data_dir()` (Tauri 2.11.x API on
    // `app.path()`) so the file lands at the OS-appropriate
    // per-user app-data location:
    //   Windows: %APPDATA%/<bundle_identifier>/transcripts/
    //   macOS:   ~/Library/Application Support/<bundle_identifier>/transcripts/
    //   Linux:   $XDG_DATA_HOME/<bundle_identifier>/transcripts/
    //
    // If the resolution fails (very unusual on desktop) we log a
    // warning and let `Agent::new` fall back to the relative
    // `transcripts/` path. The second layer of defense — a
    // `.taurignore` in `mew-ui/src-tauri/` excluding the folder
    // even if the path ever lands there again — is set up in
    // `mew-ui/src-tauri/.taurignore`.
    let transcript_dir = match app_handle.path().app_data_dir() {
        Ok(dir) => {
            let transcripts = dir.join("transcripts");
            eprintln!(
                "[mew-ui] Bug 3 fix: writing transcripts to {} (outside src-tauri/)",
                transcripts.display()
            );
            Some(transcripts)
        }
        Err(e) => {
            eprintln!(
                "[mew-ui] WARNING: app_data_dir() failed ({}); falling back to default transcript path",
                e
            );
            None
        }
    };

    let mut agent = mew_agent::agent::Agent::new(config, &task_desc, transcript_dir);
    let chat_tx = agent.take_message_sender();
    let session_handle = agent.session_handle();

    {
        let mut lock = state.lock().await;
        *lock = Some(ActiveSession {
            handle: session_handle,
            tx: chat_tx,
        });
    }

    let result = agent.run(&page).await;

    // Clear session on end so the next message goes back to classifier
    {
        let mut lock = state.lock().await;
        *lock = None;
    }

    let status = match result {
        Ok(_) => "Done",
        Err(_) => "Failed",
    };
    let _ = app_handle.emit("agent-state", status);

    let _ = mew_cdp::shutdown(browser, handler_task, job).await;

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .manage(AppState::default())
    .invoke_handler(tauri::generate_handler![send_message, get_config_summary])
    .setup(|app| {
      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }
      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
