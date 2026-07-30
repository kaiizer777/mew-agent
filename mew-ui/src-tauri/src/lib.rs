use mew_agent::{load_config, ProviderConfig};

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

#[tauri::command]
fn send_message(text: String) -> String {
    text
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
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
