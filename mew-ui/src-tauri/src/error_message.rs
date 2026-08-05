// mew-ui — Phase 10.1: user-facing error messages.
//
// Background. The Tauri command surface returns `Result<T, String>`
// to the frontend. Pre-Phase 10 every error path was a raw
// `format!("<verb> failed: {e}")` — the user saw the original
// anyhow chain, which leaks internals (file paths, "BackendNodeId
// {…}", and Rust debug-print formatting).
//
// This module is the *one* place that converts an error chain into
// a plain-language user-facing message. The contract is:
//
//   * Never include anyhow::Error debug-formatting in the user
//     message. Internal ids, file paths, and stack traces are
//     strictly for the trace log.
//   * Match on the message body's substrings to map common failure
//     modes to short, actionable copy. When nothing matches, fall
//     back to a generic-but-true line. The generic line is the
//     same shape across every error path so the user is never
//     surprised by "what does this mean?"
//   * Always return a non-empty `String`. The frontend's
//     `chat-reply` listener pushes the payload straight into the
//     chat list — a blank error is a regression on the Phase 3
//     "user always sees something" guarantee.
//
// The `for_user` entry point is the function every Tauri command
// and Tauri sink should call. Helpers below are split by
// failure-mode family so unit tests can lock each mapping in
// place — a regression in copy is a regression in UX.

/// Convert an anyhow error into a one- or two-sentence user-facing
/// message. The full debug chain is *not* included; the caller
/// should `tracing::error!` it separately for the trace log.
///
/// `context` is a short verb phrase the caller supplies —
/// "loading config", "classifying intent", "launching the browser",
/// "pausing the session", "running the task". It is woven into
/// both the matched message and the generic fallback so the user
/// always knows *which step* failed.
pub fn for_user(err: &anyhow::Error, context: &str) -> String {
    let detail = format!("{err:#}");
    for_user_from_detail(&detail, context)
}

/// Same as `for_user` but takes anything that implements
/// `std::error::Error`. Used for the typed errors the Tauri
/// command surface returns (e.g. `mew_agent::session::SessionError`,
/// which is a `thiserror::Error` rather than an `anyhow::Error`).
/// Walking the cause chain via `std::error::Error::source` keeps
/// the matched-substring logic intact.
pub fn for_std_error(err: &dyn std::error::Error, context: &str) -> String {
    // Compose a single detail string with the top-level error
    // and every cause joined by `: ` — same shape anyhow's `{:#}`
    // produces. This is the input the match arms substring-test
    // against, so the matchers below work unchanged.
    let mut detail = err.to_string();
    let mut current = err.source();
    while let Some(c) = current {
        detail.push_str(": ");
        detail.push_str(&c.to_string());
        current = c.source();
    }
    for_user_from_detail(&detail, context)
}

fn for_user_from_detail(detail: &str, context: &str) -> String {
    if let Some(msg) = match_load_config(detail) {
        return msg;
    }
    if let Some(msg) = match_launch_chrome(detail) {
        return msg;
    }
    if let Some(msg) = match_classify(detail) {
        return msg;
    }
    if let Some(msg) = match_run_browser_task(detail) {
        return msg;
    }
    if let Some(msg) = match_pause_resume(detail) {
        return msg;
    }
    if let Some(msg) = match_shutdown(detail) {
        return msg;
    }
    if let Some(msg) = match_screenshot(detail) {
        return msg;
    }
    if let Some(msg) = match_planner(detail) {
        return msg;
    }

    // Generic fallback. Never empty, never internal, never blames
    // the user. The action is implied by the verb phrase the
    // caller passed in.
    format!("Couldn't {context}. Please try again — and if it keeps happening, restart the app.")
}

/// Map load_config failures. The most common case is a missing or
/// malformed `config.yaml` next to the running binary.
fn match_load_config(detail: &str) -> Option<String> {
    if detail.contains("Could not find config.yaml")
        || detail.contains("Failed to read config file")
        || detail.contains("Failed to parse config file")
        || detail.contains("load_config failed")
    {
        return Some(
            "Couldn't load the configuration file. Make sure config.yaml exists next to the app and is valid YAML."
                .to_string(),
        );
    }
    None
}

/// Map Chrome-launch failures. The two most common shapes:
///   * Binary path missing or wrong.
fn match_launch_chrome(detail: &str) -> Option<String> {
    if detail.contains("Failed to launch Chrome") || detail.contains("Failed to build BrowserConfig") {
        return Some(
            "Couldn't start the browser. Check the browser.binary_path in config.yaml and that the file exists."
                .to_string(),
        );
    }
    None
}

/// Map intent-classification failures. The user typed something,
/// the LLM call failed. The user can't fix the network or the
/// model, but they should know the message was not lost.
fn match_classify(detail: &str) -> Option<String> {
    if detail.contains("Classification failed")
        || detail.contains("Failed to read classification response body")
        || detail.contains("Failed to parse classification JSON")
        || detail.contains("Failed to parse tool call arguments")
        || detail.contains("API returned error status")
    {
        return Some(
            "I couldn't understand your message. The chat service didn't respond — please try again in a moment."
                .to_string(),
        );
    }
    None
}

/// Map the catch-all from the spawned browser-task future. By the
/// time `run_browser_task` returns Err, we've already lost the
/// typed `BrowserResult` we want to show the user — we have to
/// synthesize something. Keep it short and tell them how to
/// recover.
fn match_run_browser_task(detail: &str) -> Option<String> {
    if detail.contains("run_browser_task")
        || detail.contains("browser task could not start")
        || detail.contains("PrefabricatedAgentFactory")
        || detail.contains("agent already consumed")
    {
        return Some(
            "The browser task couldn't run. Try again — if it keeps failing, the browser may need to be restarted."
                .to_string(),
        );
    }
    None
}

/// Map pause/resume command failures. The session probably already
/// ended; nothing the user can fix.
fn match_pause_resume(detail: &str) -> Option<String> {
    if detail.contains("Failed to pause") || detail.contains("Failed to resume") {
        return Some(
            "Couldn't change the session state. The task may have already finished or been cancelled."
                .to_string(),
        );
    }
    if detail.contains("No active session") {
        // The user pressed pause/resume with no agent running —
        // the previous UI hid the buttons, but the keyboard path
        // could still hit the command. "Nothing to do" is the
        // honest answer.
        return Some("There's no running task to pause or resume.".to_string());
    }
    None
}

/// Map Chrome-shutdown failures. The session is over; the user
/// can ignore this.
fn match_shutdown(detail: &str) -> Option<String> {
    if detail.contains("shutdown") || detail.contains("Browser already closed") {
        return Some(
            "The browser didn't close cleanly. It's safe to ignore — the next run will start a fresh window."
                .to_string(),
        );
    }
    None
}

/// Map screenshot-poll failures. The chat list still works; the
/// screencast popover just won't refresh. Quiet, not alarming.
fn match_screenshot(detail: &str) -> Option<String> {
    if detail.contains("screenshot poll stopped")
        || detail.contains("capture_screenshot")
        || detail.contains("Failed to take screenshot")
    {
        return Some(
            "The live preview stopped updating. The chat task is still running — try opening the preview again."
                .to_string(),
        );
    }
    None
}

/// Map Phase 14 planner and todo command failures.
fn match_planner(detail: &str) -> Option<String> {
    if detail.contains("not yet implemented") {
        return Some("This action is not yet implemented.".to_string());
    }
    if detail.contains("worker pool is busy") || detail.contains("max in-flight capacity") {
        return Some(
            "The agent is currently busy with another task. Please wait for it to complete or stop it before starting a new one."
                .to_string(),
        );
    }
    if detail.contains("worker pool is shutting down") {
        return Some("The system is shutting down. Please restart the application.".to_string());
    }
    if detail.contains("task not found") || detail.contains("unknown task_id") {
        return Some("The requested task could not be found or has already finished.".to_string());
    }
    if detail.contains("todo not found") || detail.contains("unknown todo_id") {
        return Some("The specified subtask was not found.".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn load_config_missing_yields_user_facing_message() {
        let err = anyhow!("Could not find config.yaml in current or any parent directory");
        let msg = for_user(&err, "loading config");
        assert!(msg.contains("config.yaml"));
        assert!(!msg.contains("Failed to read"));
    }

    #[test]
    fn launch_chrome_yields_user_facing_message() {
        let err = anyhow!("Failed to launch Chrome: file not found");
        let msg = for_user(&err, "launching the browser");
        assert!(msg.contains("browser"));
        assert!(!msg.contains("Failed to launch"));
    }

    #[test]
    fn classify_failure_yields_user_facing_message() {
        let err = anyhow!("Classification failed: API returned error status 502");
        let msg = for_user(&err, "classifying intent");
        assert!(msg.contains("try again"));
        assert!(!msg.contains("Classification failed"));
        assert!(!msg.contains("502"));
    }

    #[test]
    fn browser_task_factory_consumed_yields_user_facing_message() {
        let err = anyhow!("PrefabricatedAgentFactory: agent already consumed");
        let msg = for_user(&err, "running the task");
        assert!(msg.contains("task"));
        assert!(!msg.contains("agent already consumed"));
    }

    #[test]
    fn pause_no_active_session_is_quiet() {
        let err = anyhow!("No active session");
        let msg = for_user(&err, "pausing");
        assert!(msg.contains("no running task"));
    }

    #[test]
    fn unknown_error_falls_back_to_generic() {
        let err = anyhow!("some entirely new failure: 0xdeadbeef");
        let msg = for_user(&err, "doing the thing");
        assert!(!msg.is_empty());
        assert!(!msg.contains("0xdeadbeef"));
        assert!(msg.contains("doing the thing"));
    }

    #[test]
    fn planner_busy_error_yields_user_facing_message() {
        let err = anyhow!("worker pool is busy (max in-flight capacity reached)");
        let msg = for_user(&err, "submit todo");
        assert!(msg.contains("currently busy"));
        assert!(!msg.contains("max in-flight capacity"));
    }

    #[test]
    fn planner_not_implemented_error_yields_user_facing_message() {
        let err = anyhow!("not yet implemented");
        let msg = for_user(&err, "stop task");
        assert!(msg.contains("not yet implemented"));
        assert!(!msg.contains("{"));
    }

    #[test]
    fn error_message_never_empty_and_never_json_dump() {
        let err = anyhow!("{{\"code\": 500, \"details\": \"internal failure\"}}");
        let msg = for_user(&err, "execute command");
        assert!(!msg.is_empty());
        assert!(!msg.starts_with("{"));
    }
}

