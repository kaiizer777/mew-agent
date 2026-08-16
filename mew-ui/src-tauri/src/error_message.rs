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
    if detail.contains("Failed to launch Chrome")
        || detail.contains("Failed to build BrowserConfig")
    {
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
///
/// The `router::classify` failure path bails with a status-bearing
/// string — `"API returned error status {status}: {text}"` — for
/// every non-2xx response from the upstream provider. Different
/// status codes mean very different things to the user, so we
/// pull the status out of the detail and produce copy that points
/// at the right knob:
///
///   * 401 / 403 — the API key is wrong or has been revoked.
///     Telling the user "try again in a moment" is wrong; the
///     key needs to be fixed in `config.yaml`.
///   * 402 — the account is out of credit or on a plan tier that
///     does not cover the configured `default_model`. This is the
///     single most common "chat just stopped working" symptom on
///     the xiaomimimo / OpenCode-Zen style providers; we name it
///     explicitly so the user knows to top up or switch plan.
///   * 404 — the configured `default_model` is not available to
///     this account. Most providers' `/v1/models` endpoint
///     enumerates what the key can use.
///   * 429 — rate-limited; transient, the user can retry.
///   * 5xx — upstream service hiccup; transient, the user can
///     retry in a moment.
///
/// Everything else that lands here (parse failure, network
/// timeout, malformed body) is the existing generic copy — the
/// user can't act on it, but they should know their message
/// was not lost.
fn match_classify(detail: &str) -> Option<String> {
    if detail.contains("Classification failed")
        || detail.contains("Failed to read classification response body")
        || detail.contains("Failed to parse classification JSON")
        || detail.contains("Failed to parse tool call arguments")
        || detail.contains("API returned error status")
    {
        // Pull the status code out of the anyhow chain. The
        // classifier bails with the exact shape
        // `API returned error status 402 Payment Required: <body>`,
        // so a regex-style scan for `status <3-digit>` is the
        // cheapest correct parse. If the status isn't there
        // (e.g. the failure is a network error or a parse error
        // and not a non-2xx response), fall back to the generic
        // copy that doesn't promise a retry.
        if let Some(status) = api_error_status(detail) {
            return Some(match status {
                401 | 403 => "The chat service rejected the API key. Check `opencode_zen.api_key` in config.yaml — it may be wrong or revoked.".to_string(),
                402 => "The chat service account is out of credit or on a plan that doesn't cover the configured model. Top up the account or switch `default_model` in config.yaml.".to_string(),
                404 => "The chat service doesn't recognize the configured model. Check `opencode_zen.default_model` in config.yaml — the model name may be wrong or not available to this account.".to_string(),
                429 => "The chat service is rate-limiting requests. Please wait a moment and try again.".to_string(),
                500..=599 => "The chat service is having a temporary problem. Please try again in a moment.".to_string(),
                // Other 4xx (e.g. 400 from a malformed tool-call
                // shape) — the user can't really fix this from the
                // UI, so we don't promise a retry.
                400 | 4..=499 => "The chat service rejected the request. Please try again — and if it keeps happening, the prompt or model config may need a tweak.".to_string(),
                // Unknown non-2xx status — fall through to the
                // generic message below; we don't have a
                // confident copy for it.
                _ => return None,
            });
        }
        // No status code in the detail (network error, parse
        // error, empty body, etc.) — keep the original generic
        // message: user can't act on it, but the message was
        // not lost.
        return Some(
            "I couldn't reach the chat service. Please try again in a moment."
                .to_string(),
        );
    }
    None
}

/// Extract the HTTP status code from an anyhow detail string
/// shaped like `"API returned error status 402 Payment Required: ..."`,
/// or `None` if no 3-digit status is present. We only accept
/// 3-digit codes to keep this from accidentally matching other
/// 3-digit numbers in a body (e.g. tokens-used counts in a
/// response body that the provider echoes back).
fn api_error_status(detail: &str) -> Option<u16> {
    let needle = "API returned error status ";
    let start = detail.find(needle)? + needle.len();
    let rest = &detail[start..];
    // Take the longest leading run of ASCII digits, up to 3.
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).take(3).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
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

/// Phase 18: Map a Todo rejection into a user-facing sentence.
pub fn todo_rejected(mismatch: &mew_agent::todo::EvidenceMismatch) -> String {
    format!(
        "The agent's evidence did not match the planner's signature ({}). Please try again.",
        mismatch.planner_signature
    )
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

    /// Pin the 402 (Payment Required) mapping. This is the single
    /// most common "chat suddenly stopped" symptom on
    /// xiaomimimo / OpenCode-Zen style providers when the API
    /// key has no plan attached — a regression to the generic
    /// "I couldn't understand your message" copy here is a
    /// real-world UX bug, not a stylistic one.
    #[test]
    fn classify_402_payment_required_yields_actionable_message() {
        let err = anyhow!("Classification failed: API returned error status 402 Payment Required: ");
        let msg = for_user(&err, "classifying intent");
        assert!(
            msg.contains("out of credit") || msg.contains("plan"),
            "402 message should name the billing/plan cause; got: {msg}"
        );
        assert!(
            !msg.contains("try again"),
            "402 is not transient — must not promise a retry; got: {msg}"
        );
        assert!(!msg.contains("Classification failed"));
        assert!(!msg.contains("402"));
    }

    /// Pin the 401 / 403 (unauthorized) mapping. Telling the user
    /// to "try again" hides a config error they need to fix.
    #[test]
    fn classify_401_unauthorized_yields_actionable_message() {
        let err = anyhow!("Classification failed: API returned error status 401 Unauthorized: invalid api key");
        let msg = for_user(&err, "classifying intent");
        assert!(
            msg.contains("api_key") || msg.contains("API key"),
            "401 message should point at config.yaml api_key; got: {msg}"
        );
        assert!(!msg.contains("try again"));
    }

    /// Pin the 404 mapping — usually a wrong `default_model` name.
    #[test]
    fn classify_404_model_not_found_yields_actionable_message() {
        let err =
            anyhow!("Classification failed: API returned error status 404 Not Found: model 'gpt-9000'");
        let msg = for_user(&err, "classifying intent");
        assert!(
            msg.contains("default_model") || msg.contains("model"),
            "404 message should point at default_model in config.yaml; got: {msg}"
        );
    }

    /// Pin the 429 mapping — this *is* transient, so the message
    /// must explicitly invite a retry.
    #[test]
    fn classify_429_rate_limited_yields_retry_message() {
        let err = anyhow!("Classification failed: API returned error status 429 Too Many Requests");
        let msg = for_user(&err, "classifying intent");
        assert!(
            msg.contains("rate") || msg.contains("wait"),
            "429 message should mention rate-limiting or waiting; got: {msg}"
        );
    }

    /// A network error (no status code in the detail) should
    /// still produce a sensible message — the user can't act on
    /// it, but they should know the message wasn't lost.
    #[test]
    fn classify_network_error_yields_generic_message() {
        let err = anyhow!(
            "Classification failed: Network error during classification: connection refused"
        );
        let msg = for_user(&err, "classifying intent");
        assert!(!msg.is_empty());
        assert!(!msg.contains("Classification failed"));
        assert!(!msg.contains("connection refused"));
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

    #[test]
    fn todo_rejected_yields_plain_language_sentence() {
        let mismatch = mew_agent::todo::EvidenceMismatch {
            worker_signature: "len:0123abcd".to_string(),
            planner_signature: "len:0123abce".to_string(),
            reason: "stale iteration".to_string(),
        };
        let msg = super::todo_rejected(&mismatch);
        assert!(msg.contains("len:0123abce"));
        assert!(msg.contains("did not match"));
        assert!(!msg.starts_with("{"));
        assert!(!msg.contains("EvidenceMismatch"));
    }
}
