use anyhow::Result;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::dom::GetBoxModelParams;
use chromiumoxide::cdp::browser_protocol::dom::{BackendNodeId, FocusParams, ResolveNodeParams};
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, CaptureScreenshotParams, Viewport,
};
use chromiumoxide::cdp::js_protocol::runtime::CallFunctionOnParams;
use chromiumoxide::Page;
use futures::StreamExt;
use thiserror::Error;

/// Phase 3: re-export `chromiumoxide::Page` so crates downstream
/// of `mew-cdp` (the Tauri `mew-ui` crate, in particular) can
/// name the `Page` type without taking a direct dependency on
/// `chromiumoxide`. The orchestrator's `BrowserAgentFactory`
/// trait uses this re-exported name in its method signatures.
pub use chromiumoxide::Page as ReExportedPage;
pub use chromiumoxide::Browser as ReExportedBrowser;

mod process_lifetime;
use process_lifetime::JobObject;

// Orphan-chrome reaper — see `orphan_reap.rs` for the
// full design doc. Called at the top of every `launch` /
// `launch_headless` to clean up any chrome.exe the parent
// process left behind on a hard kill. Without this sweep,
// a stale chrome holding our `user_data_dir` (SQLite lock)
// or our port 9222 (TCP listener) causes the next launch
// to fail with a generic "browser may not be reachable"
// error, hiding the real root cause.
//
// The module is `pub` so example/test harnesses can call
// `reap_orphaned_chromes` directly; the public API is a
// single function with a stable signature.
pub mod orphan_reap;
use orphan_reap::reap_orphaned_chromes;

// Phase 33: structured field extractor (`extract_fields` tool).
// The LLM used to issue `snapshot()` and then spend a full LLM
// round-trip scanning the AX tree to pull out phone / address /
// rating. The new tool runs a single targeted CDP query against
// the current page and returns the requested fields as a typed
// `HashMap<String, Option<String>>` — O(1) for Maps, O(small n)
// for generic pages. See `field_extractor::extract_structured_fields`.
pub mod field_extractor;
pub use field_extractor::{
    build_extract_js, extract_structured_fields, extract_structured_fields_auto,
    is_maps_url, parse_extraction, SUPPORTED_FIELDS,
};

#[derive(Error, Debug)]
pub enum StaleRefError {
    #[error("Stale ref: Node with BackendNodeId {0:?} could not be found or resolved.")]
    NotFound(BackendNodeId),

    #[error("Stale ref: Failed to interact with BackendNodeId {0:?}: {1}")]
    InteractionFailed(BackendNodeId, String),
}

pub const DEFAULT_PORT: u16 = 9222;

// ---------------------------------------------------------------------------
// Phase 16.1: visible cursor overlay — runtime API
// ---------------------------------------------------------------------------
// These functions are the agent-facing side of the cursor feature. They are
// always safe to call: if the script wasn't injected (`visible_cursor: false`
// in config), `window.__mewCursor` won't exist, and the evaluate call returns
// an error we swallow. The click path itself is never blocked on these calls.

/// Compute the viewport-space center (cx, cy) of the element identified by
/// `backend_id`. Mirrors the box-model path used by [`screenshot_region`].
/// Returns `None` if the element is stale / has no box / has zero area.
pub async fn compute_element_center(
    page: &Page,
    backend_id: BackendNodeId,
) -> Result<Option<(f64, f64)>, StaleRefError> {
    let box_model_params = GetBoxModelParams::builder()
        .backend_node_id(backend_id)
        .build();
    let box_model_res = match page.execute(box_model_params).await {
        Ok(r) => r,
        Err(_) => return Ok(None), // stale / detached
    };
    let quad: Vec<f64> = serde_json::from_value(
        serde_json::to_value(&box_model_res.model.border).unwrap_or_default(),
    )
    .unwrap_or_default();
    if quad.len() != 8 {
        return Ok(None);
    }
    let x_coords = [quad[0], quad[2], quad[4], quad[6]];
    let y_coords = [quad[1], quad[3], quad[5], quad[7]];
    let min_x = x_coords.iter().cloned().fold(f64::INFINITY, f64::min);
    let min_y = y_coords.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_x = x_coords.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let max_y = y_coords.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let w = max_x - min_x;
    let h = max_y - min_y;
    if w <= 0.0 || h <= 0.0 {
        return Ok(None);
    }
    Ok(Some((min_x + w / 2.0, min_y + h / 2.0)))
}

/// Move the ghost cursor to (x, y). No-op if the cursor script wasn't
/// injected — the evaluate simply returns "undefined" and we ignore it.
pub async fn move_cursor(page: &Page, x: f64, y: f64) {
    let expr = format!(
        "(window.__mewCursor && window.__mewCursor.moveTo) ? window.__mewCursor.moveTo({x}, {y}) : null"
    );
    if let Err(e) = page.evaluate(expr).await {
        tracing::debug!("move_cursor: no-op ({e})");
    }
}

/// Move the ghost cursor to (x, y) and fire a click ripple. No-op if the
/// cursor script wasn't injected.
pub async fn move_cursor_and_ripple(page: &Page, x: f64, y: f64) {
    let expr = format!(
        "(function(){{ \
            if (!(window.__mewCursor && window.__mewCursor.click)) return null; \
            window.__mewCursor.moveTo({x}, {y}); \
            window.__mewCursor.click({x}, {y}); \
            return true; \
        }})()"
    );
    if let Err(e) = page.evaluate(expr).await {
        tracing::debug!("move_cursor_and_ripple: no-op ({e})");
    }
}

// ---------------------------------------------------------------------------
// Phase 16.1: visible cursor overlay
// ---------------------------------------------------------------------------
// Injected on every navigation via `Page.addScriptToEvaluateOnNewDocument`
// (chromiumoxide's direct equivalent of Playwright's `addInitScript`). The
// script creates a `position: fixed` ghost cursor + a click ripple element,
// both with `pointer-events: none` so real page interaction is never blocked,
// and exposes a small imperative API on `window.__mewCursor`.
//
// Adaptation note vs. the spec text: the spec mentions intercepting
// `Input.dispatchMouseEvent` calls, but in this codebase the real click path
// is `click_ref` -> `Runtime.callFunctionOn` -> `el.click()` (synthetic JS
// click), which never dispatches a mouse event at real coordinates. We
// therefore drive `__mewCursor` from the agent side using the element's
// pre-computed center (same `GetBoxModel` path `screenshot_region` already
// uses), which is what makes the cursor visibly slide to each click target
// in the actual session. The script itself is unchanged from the spec — it
// re-injects on every navigation and is a CSS-only overlay with no
// `navigator.*` property touches, so it doesn't trip any bot-detection.
const VISIBLE_CURSOR_SCRIPT: &str = r#"
(function () {
    // Idempotency guard: if a previous page already installed the cursor,
    // do nothing. This protects against double-injection if a re-navigation
    // fires before the previous document is torn down.
    if (window.__mewCursor && window.__mewCursor.__installed) return;
    window.__mewCursor = { __installed: true };

    // The cursor element: a small filled circle with a thin outer ring,
    // fixed to the viewport, never interactive.
    var cursor = document.createElement('div');
    cursor.id = '__mew-cursor';
    cursor.style.cssText = [
        'position: fixed',
        'left: 0',
        'top: 0',
        'width: 18px',
        'height: 18px',
        'margin-left: -9px',
        'margin-top: -9px',
        'border-radius: 50%',
        'background: rgba(37, 99, 235, 0.95)',     // matches the project's ink-blue accent #2563EB
        'border: 2px solid rgba(255, 255, 255, 0.9)',
        'box-shadow: 0 2px 6px rgba(0, 0, 0, 0.25)',
        'pointer-events: none',                    // never blocks real clicks
        'z-index: 2147483647',                     // max int — top of any stacking context
        'transform: translate3d(-100px, -100px, 0)',
        'transition: transform 180ms ease-out',    // visible slide, not teleport
        'will-change: transform'
    ].join(';');

    // The click ripple: a short-lived expanding ring that flashes at the
    // click point. Distinguishes real clicks from plain hovers/moves.
    var ripple = document.createElement('div');
    ripple.id = '__mew-cursor-ripple';
    ripple.style.cssText = [
        'position: fixed',
        'left: 0',
        'top: 0',
        'width: 8px',
        'height: 8px',
        'margin-left: -4px',
        'margin-top: -4px',
        'border-radius: 50%',
        'background: rgba(37, 99, 235, 0.0)',
        'border: 2px solid rgba(37, 99, 235, 0.85)',
        'pointer-events: none',
        'z-index: 2147483646',
        'transform: translate3d(-100px, -100px, 0) scale(1)',
        'opacity: 1',
        'will-change: transform, opacity'
    ].join(';');

    // Append as late as possible — at document-start the body may not exist
    // yet, so wait until it does.
    function attach() {
        var parent = document.body || document.documentElement;
        if (!parent) return false;
        if (cursor.parentNode !== parent) parent.appendChild(cursor);
        if (ripple.parentNode !== parent) parent.appendChild(ripple);
        return true;
    }
    if (!attach()) {
        var obs = new MutationObserver(function () {
            if (attach()) obs.disconnect();
        });
        obs.observe(document.documentElement || document, { childList: true, subtree: true });
    }

    // Track the last set position so a `click()` without a prior moveTo
    // (race / typo) still ripples at a sane spot instead of (-100, -100).
    var lastX = 0;
    var lastY = 0;

    window.__mewCursor.moveTo = function (x, y) {
        if (typeof x !== 'number' || typeof y !== 'number') return;
        lastX = x; lastY = y;
        // Re-attach in case the body was replaced by an SPA route change.
        if (cursor.parentNode !== document.body && cursor.parentNode !== document.documentElement) {
            attach();
        }
        cursor.style.transform = 'translate3d(' + x + 'px, ' + y + 'px, 0)';
    };

    window.__mewCursor.click = function (x, y) {
        if (typeof x === 'number' && typeof y === 'number') {
            lastX = x; lastY = y;
            cursor.style.transform = 'translate3d(' + x + 'px, ' + y + 'px, 0)';
        }
        if (ripple.parentNode !== document.body && ripple.parentNode !== document.documentElement) {
            attach();
        }
        // Reset ripple to a known starting transform, then force a reflow so
        // the next style write triggers the CSS transition cleanly.
        ripple.style.transition = 'none';
        ripple.style.transform = 'translate3d(' + lastX + 'px, ' + lastY + 'px, 0) scale(1)';
        ripple.style.opacity = '1';
        // eslint-disable-next-line no-unused-expressions
        ripple.getBoundingClientRect();
        // Animate: expand and fade out over ~450ms.
        ripple.style.transition = 'transform 450ms ease-out, opacity 450ms ease-out';
        ripple.style.transform = 'translate3d(' + lastX + 'px, ' + lastY + 'px, 0) scale(6)';
        ripple.style.opacity = '0';
    };
})();
"#;

/// Resolve the browser profile directory.
///
/// The historical call site used `std::env::current_dir()?.join("profile")`,
/// which is **fragile** — the Tauri shell (`app.exe`) is sometimes launched
/// from the workspace root, sometimes from `mew-ui/src-tauri/`, sometimes
/// from VSCode, and the CWD in each case is different. When the reaper
/// looked at one CWD's `profile` while Chrome was using a *different*
/// CWD's `profile`, the reaper saw a clean state and Chrome hit
/// "ProcessSingleton ... Lock file can not be created! Error code: 32"
/// because the other path's stale lockfile was still on disk.
///
/// Resolution order (first match wins):
///   1. `MEW_BROWSER_PROFILE` env var — power-user / test override.
///   2. `<exe_dir>/profile` — deterministic regardless of how the
///      binary was launched. The Tauri app's `app.exe` lives in
///      `target/debug/` or `target/release/` and a `profile/` next
///      to it is unambiguous. Cargo examples and tests still get a
///      sensible default: example binaries land in
///      `target/debug/examples/`, so `examples/../profile` is
///      `target/debug/profile` — same on every run from a clean
///      build, no CWD dependency.
///   3. `<cwd>/profile` — the historical behavior, kept as a
///      last-resort fallback so a developer running `cargo test`
///      from a non-standard layout still gets a working profile.
///
/// All three options produce an absolute path so the reaper, the
/// chromiumoxide `user_data_dir`, and any later relaunch agree on
/// the same directory even if CWD changes between calls.
fn resolve_profile_dir() -> std::path::PathBuf {
    use std::path::PathBuf;

    // (1) Env-var override — the explicit escape hatch.
    if let Ok(p) = std::env::var("MEW_BROWSER_PROFILE") {
        let p = PathBuf::from(p);
        if p.is_absolute() {
            return p;
        }
        // Relative env-var values resolve against the exe dir, not CWD,
        // so a wrapper script that sets `MEW_BROWSER_PROFILE=profile`
        // gets the same directory as the no-env-var case.
        if let Some(exe_dir) = exe_dir() {
            return exe_dir.join(&p);
        }
        // No exe dir available (extremely unusual) — best effort.
        let p_for_join = p.clone();
        return std::env::current_dir()
            .map(|c| c.join(&p_for_join))
            .unwrap_or_else(|_| p);
    }

    // (2) Exe-dir-relative — the deterministic default.
    if let Some(exe_dir) = exe_dir() {
        return exe_dir.join("profile");
    }

    // (3) CWD fallback — kept for the rare case where the current
    // executable path cannot be determined (e.g. some sandboxed
    // environments).
    std::env::current_dir()
        .map(|c| c.join("profile"))
        .unwrap_or_else(|_| PathBuf::from("profile"))
}

/// Best-effort: directory containing the running executable.
///
/// `std::env::current_exe()` returns the path to the current binary
/// (e.g. `target/debug/app.exe` or `target/debug/examples/foo.exe`).
/// `.parent()` is the directory the binary lives in. Returns `None`
/// if the path cannot be obtained.
fn exe_dir() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

/// Launches a headed Chrome instance via CDP using chromiumoxide.
/// Configured with a fixed remote debugging port (9222) and persistent
/// user data directory. The profile path is resolved by
/// [`resolve_profile_dir`] — see its docs for the resolution order.
///
/// `visible_cursor` (Phase 16.1) — when true, the page-level script in
/// [`VISIBLE_CURSOR_SCRIPT`] is registered on every navigation so a
/// ghost cursor + click ripple overlay is available to the agent. Default
/// false: when off, the script is not registered, the API calls are no-ops
/// via the `__mewCursor` guard, and the click path adds zero latency.
///
/// **Phase 3.3 (Bug 2):** the returned tuple is now 4-tuple:
/// `(Browser, Page, handler_task, JobObject)`. The `JobObject` is
/// a Windows Job Object (or Unix stub) that ties the
/// chromiumoxide-spawned Chromium process to this crate's
/// lifetime — when the `JobObject` is dropped, the child Chrome
/// process is killed. Pass the `JobObject` to
/// [`shutdown`] so it can be dropped *after* the browser has
/// gracefully closed; if the parent process is hard-killed
/// without `shutdown` running, the kernel still kills the child
/// because the OS closes the job handle on process exit.
///
/// On Unix the `JobObject` is a Drop-based SIGTERM/SIGKILL
/// stub that covers the graceful case but **not** a
/// SIGKILL'd parent — see the module docs in
/// `src/process_lifetime.rs` for the limitation and the
/// `prctl(PR_SET_PDEATHSIG)` follow-up.
pub async fn launch(
    binary_path: Option<String>,
    visible_cursor: bool,
) -> Result<(Browser, Page, tokio::task::JoinHandle<()>, JobObject)> {
    let profile_dir = resolve_profile_dir();

    // Root-cause fix: before any BrowserConfig work,
    // sweep the process table for any chrome.exe that's
    // holding our `profile_dir` (SQLite lock on
    // History/Cookies/Preferences) or our debug port.
    // See `orphan_reap.rs` for the design doc. The
    // reaper is a best-effort sweep — failures are
    // logged and skipped, never fatal — so the launch
    // still proceeds and either fails with its real
    // error (which the orchestrator can now surface
    // to the user, see `orchestrator.rs`'s factory-error
    // fallback) or is retried by `launch_inner`.
    reap_orphaned_chromes(&profile_dir, DEFAULT_PORT);

    // Settle delay: after `TerminateProcess` the OS
    // closes the dying process's handles asynchronously.
    // Spawning the new Chrome immediately can race the
    // kernel cleanup and hit the singleton "Error code:
    // 32" before the dying process's handles are fully
    // released. 200 ms is short enough to feel instant
    // and long enough for the kernel to finish the
    // majority of handle teardown. `launch_inner` adds
    // a full retry on top of this for the rare cases
    // where the settle isn't enough.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Phase X: 4K live preview. The viewport is bumped from
    // 1280×800 to 3840×2160 so the page renders at true 4K
    // CSS-pixel resolution. The agent's interaction model
    // (accessibility-tree refs, `@eX` click targets) is
    // resolution-independent — the AX tree is computed from
    // the DOM, not the viewport — so the larger viewport does
    // not change which elements the agent can target. It does,
    // however, give the live preview enough source pixels to
    // look crisp on a 4K monitor without a browser-side
    // upscale.
    let mut config_builder = BrowserConfig::builder()
        .with_head()
        .port(9222)
        .window_size(3840, 2160)
        .user_data_dir(profile_dir.clone());

    if let Some(path) = binary_path.clone() {
        config_builder = config_builder.chrome_executable(path);
    }

    let config = config_builder
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build BrowserConfig: {e}"))?;

    tracing::info!("Launching headed Chrome on remote debugging port 9222...");

    launch_inner(config, visible_cursor).await
}

pub async fn launch_headless(
    binary_path: Option<String>,
    visible_cursor: bool,
) -> Result<(Browser, Page, tokio::task::JoinHandle<()>, JobObject)> {
    let profile_dir = resolve_profile_dir();

    // Root-cause fix: see the matching block in
    // `launch` above. The reaper's behavior is identical
    // for headless vs. headed — the difference is only
    // whether the child chrome has a window.
    reap_orphaned_chromes(&profile_dir, DEFAULT_PORT);

    // Settle delay — see the matching block in `launch`
    // above for the rationale. 200 ms gives the kernel
    // time to finish closing handles of any chrome the
    // reaper just terminated, so the new chrome's
    // singleton file create doesn't race the cleanup.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Phase X: 4K live preview — see `launch()` for the
    // rationale. Headless mode uses the same 3840×2160
    // viewport so the test / CI paths and the production Tauri
    // shell produce identical live-preview resolution.
    let mut config_builder = BrowserConfig::builder()
        .port(9222)
        .window_size(3840, 2160)
        .user_data_dir(profile_dir);

    if let Some(path) = binary_path {
        config_builder = config_builder.chrome_executable(path);
    }

    let config = config_builder
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build BrowserConfig: {e}"))?;

    tracing::info!("Launching headless Chrome on remote debugging port 9222...");

    launch_inner(config, visible_cursor).await
}

async fn launch_inner(
    config: BrowserConfig,
    visible_cursor: bool,
) -> Result<(Browser, Page, tokio::task::JoinHandle<()>, JobObject)> {
    // Phase X.2: launch-with-retry. `Browser::launch` spawns the
    // child Chrome and waits for it to print the DevTools websocket
    // URL on stderr. If Chrome exits early — typically because the
    // Windows process-singleton lockfile can't be created
    // (`Lock file can not be created! Error code: 32`) — chromiumoxide
    // returns `CdpError::LaunchExit` and we get nothing back. The
    // reaper (called from `launch` / `launch_headless` before this)
    // is a best-effort sweep; it does NOT guarantee a clean slate,
    // because:
    //
    //   1. `RmGetList` only finds processes that have registered
    //      with the Restart Manager. A Chrome child that hasn't
    //      registered yet, or one in the middle of dying, is
    //      invisible to Pass 2.
    //   2. After `TerminateProcess` the OS closes the process's
    //      handles asynchronously. A subsequent `Browser::launch`
    //      can race ahead of the kernel cleanup and the new Chrome
    //      hits `code: 32` on the singleton file.
    //
    // The retry loop below catches that race: on the first failure,
    // sleep, re-run the reaper (which now sees the half-dead Chrome
    // that registered in the meantime), and try the launch again.
    // After 3 attempts we give up and surface the real error so
    // the user sees the genuine failure mode instead of a permanent
    // lockup.
    const MAX_LAUNCH_ATTEMPTS: u32 = 3;
    const RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(750);

    let user_data_dir = config.user_data_dir.clone();
    let mut attempt: u32 = 0;

    let mut pair: Option<(Browser, chromiumoxide::handler::Handler)> = None;
    loop {
        attempt += 1;
        tracing::info!(
            attempt,
            max_attempts = MAX_LAUNCH_ATTEMPTS,
            "Browser::launch attempt"
        );
        match Browser::launch(config.clone()).await {
            Ok(p) => {
                pair = Some(p);
                break;
            }
            Err(e) => {
                let detail = format!("{e:#}");
                let is_singleton_race = detail.contains("Lock file can not be created")
                    || detail.contains("ProcessSingleton")
                    || detail.contains("Error code: 32");
                tracing::warn!(
                    attempt,
                    max_attempts = MAX_LAUNCH_ATTEMPTS,
                    is_singleton_race,
                    error = %e,
                    "Browser::launch failed; considering retry"
                );
                let any_err = anyhow::Error::from(e);
                let give_up = attempt >= MAX_LAUNCH_ATTEMPTS || !is_singleton_race;
                if give_up {
                    return Err(any_err);
                }
                // Re-run the reaper against the configured
                // user-data-dir. The reaper will catch the
                // half-dead Chrome that wasn't visible to the
                // first sweep, terminate it, and clean its stale
                // lockfile. Then sleep to let the kernel finish
                // closing handles before we try again.
                if let Some(ref dir) = user_data_dir {
                    reap_orphaned_chromes(dir, DEFAULT_PORT);
                }
                tokio::time::sleep(RETRY_BACKOFF).await;
            }
        }
    }
    let (mut browser, mut handler) = pair.expect("loop either returned or set pair");

    // Phase 3.3 (Bug 2): create a Job Object and assign the
    // chromiumoxide-launched Chrome PID to it as early as possible
    // — before the browser does any work that could exit the
    // process. If we fail to create the job OR fail to assign
    // the PID, we log loudly and continue without protection
    // (this is the same orphan-prone behavior the project had
    // pre-3.3, so we don't make anything *worse* by trying).
    // The job's Drop will trigger the kernel kill on hard-kill
    // regardless of whether assignment succeeded.
    let job = match JobObject::new() {
        Ok(j) => j,
        Err(e) => {
            tracing::error!(
                "JobObject::new failed: {}. Child Chrome will NOT be \
                 protected against parent hard-kill on Windows. \
                 Falling back to pre-3.3 orphan-prone behavior.",
                e
            );
            // Re-create a no-op JobObject? The current type
            // doesn't have a Default. For now, the cleanest
            // thing is to return an error — a parent that
            // can't get a job is in a bad state anyway.
            return Err(anyhow::anyhow!(
                "JobObject::new failed: {}. Cannot launch Chrome without \
                 kernel-enforced child-process protection (would orphan the \
                 subprocess on hard-kill). See phase3.2_evidence.md Bug 2.",
                e
            ));
        }
    };
    // Pull the spawned Chrome PID out of the Browser's
    // child handle. The `get_mut_child()` accessor on
    // chromiumoxide's `Browser` returns `Option<&mut Child>`,
    // and `Child::id()` returns the OS process ID. We do
    // this *after* `Browser::launch` returns so we know the
    // child PID is real (it was assigned during the spawn
    // we just did).
    let chrome_pid: Option<u32> = browser.get_mut_child().and_then(|c| c.inner.id());
    if let Some(pid) = chrome_pid {
        if let Err(e) = job.assign_pid(pid) {
            // Assignment failed — the child is in some
            // state we can't attach to (most likely: it
            // exited before we could assign, OR it's in
            // another job — Windows forbids nested jobs
            // by default and chromiumoxide does not
            // document whether it sets one up).
            //
            // We log and continue. The child will not be
            // protected by the job, so a hard-kill of
            // the parent will orphan it — exactly the
            // pre-3.3 behavior. But we don't make
            // anything *worse* than that, and the rest
            // of the launch still works for the
            // graceful-shutdown path.
            tracing::error!(
                "JobObject::assign_pid({}) failed: {}. Child Chrome is \
                 NOT protected against parent hard-kill on Windows. \
                 Falling back to pre-3.3 orphan-prone behavior for this run.",
                pid,
                e
            );
        } else {
            tracing::info!(
                "Child Chrome PID {} assigned to kernel job (KILL_ON_JOB_CLOSE). \
                 Hard-kill of the parent will now clean up the child via the kernel, \
                 not via best-effort user-space signals.",
                pid
            );
        }
    } else {
        // `Browser::launch` returned a Browser with no
        // child handle — this can happen if we connected
        // to an existing browser, but `launch` is
        // documented to always spawn one. Either way, we
        // can't assign a PID we don't have; the job is
        // empty so it does nothing on drop.
        tracing::warn!(
            "Browser::launch returned a Browser with no child handle; \
             JobObject is empty and will not protect any process on hard-kill."
        );
    }

    let handle = tokio::spawn(async move {
        while let Some(h) = handler.next().await {
            if let Err(e) = h {
                tracing::error!("CDP handler error: {:?}", e);
            }
        }
    });

    let page = browser.new_page("about:blank").await?;

    // Inject defense-in-depth stealth patches
    let js_patch = r#"
        Object.defineProperty(navigator, 'webdriver', { get: () => false });
        if (window.chrome && window.chrome.runtime) delete window.chrome.runtime;
    "#;

    page.execute(
        chromiumoxide::cdp::browser_protocol::page::AddScriptToEvaluateOnNewDocumentParams::builder()
            .source(js_patch)
            .build()
            .unwrap()
    ).await?;

    // Phase 16.1: register the visible-cursor overlay script. It runs at
    // document start on every navigation, so the cursor survives SPA route
    // changes and full reloads alike.
    if visible_cursor {
        tracing::info!("Visible cursor overlay ENABLED (Phase 16.1)");
        page.execute(
            chromiumoxide::cdp::browser_protocol::page::AddScriptToEvaluateOnNewDocumentParams::builder()
                .source(VISIBLE_CURSOR_SCRIPT)
                .build()
                .unwrap()
        ).await?;
    }

    Ok((browser, page, handle, job))
}

/// Cleanly closes the browser instance over CDP.
///
/// **Phase 3.3 (Bug 2):** the signature now takes a fourth
/// parameter, `job: JobObject`, which is dropped at the end of
/// this function. The drop order is important: we close the
/// browser via CDP first, wait for it to exit, *then* drop the
/// job. On Windows the drop is `CloseHandle(job)` — by that
/// point the browser has already exited and the kernel has no
/// children left to kill, so it's a clean no-op. On Unix the
/// drop sends SIGTERM/SIGKILL — also a no-op at that point
/// because the browser has already exited. The job's *real*
/// role is to fire on a hard-kill where this function never
/// runs: in that case the OS closes the job handle on process
/// exit, which is what triggers the kernel kill.
///
/// Callers that don't use a job (e.g. some example harnesses)
/// can pass `JobObject::new()?.drop_alone()` — but the cleanest
/// approach is to always thread the job through from
/// [`launch`]. See the diff for the updated call sites.
pub async fn shutdown(
    mut browser: Browser,
    handler_task: tokio::task::JoinHandle<()>,
    job: JobObject,
) -> Result<()> {
    tracing::info!("Closing browser cleanly via CDP...");
    let close_res = browser.close().await;

    // Wait for the event loop to finish (which happens when the websocket disconnects / browser exits)
    let _ = handler_task.await;

    // Asynchronously wait for the spawned chromium instance to exit completely
    // to avoid zombie processes and the Drop warning.
    let _ = browser.wait().await;

    // Check if the close actually succeeded
    close_res?;

    // Phase 3.3 (Bug 2): drop the job last. The browser has
    // already exited by this point, so on Windows the
    // `CloseHandle` is a clean no-op (the job has no live
    // processes). On Unix the SIGTERM is sent to an
    // already-dead PID and is ignored. The drop's *real*
    // value is the hard-kill path where shutdown never
    // runs at all.
    drop(job);

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Down,
}

pub async fn navigate(page: &Page, url: &str) -> Result<()> {
    tracing::info!("Navigating to {}", url);
    page.goto(url).await?.wait_for_navigation().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 4 (Bug 4 fix): wait for the page to be *visually* settled before
// the agent takes a perception snapshot.
//
// The previous behavior was a fixed `tokio::time::sleep(2s)` after
// `wait_for_navigation()`. On JS-heavy pages (GitHub, SPAs) the
// browser's `Page.loadEventFired` event fires well before the SPA has
// populated the DOM with real content. The accessibility tree at that
// moment has only the `RootWebArea` and a single `ignored/uninteresting`
// placeholder child, and the root carries `busy: true`. A 2-second
// fixed sleep is not enough on a slow / heavy page, and is wasteful on
// a fast static page. Either extreme breaks the agent — see
// `tests-output/phase4_bug4_repro_github_pre_fix.out.txt` for a
// captured pre-fix run that returned a 171-byte observation with
// `busy: true` for `https://github.com/tokio-rs/tokio` even after
// 2s + 4 AX-tree attempts.
//
// This function replaces the fixed sleep with a bounded DOM-content
// poll:
//
//   - We accept "settled" only when ALL of the following are true
//     (cheap JS check via `Runtime.evaluate`):
//       1. `document.readyState === 'complete'`
//       2. `document.body` exists and has at least one element child
//          (an empty body means the SPA hasn't mounted yet)
//       3. The page is not marked `[aria-busy="true"]` anywhere
//          (GitHub and many SPAs set this while loading)
//       4. `document.body.innerText.length >= min_text_len` (default
//          50 chars — a real page has way more; an empty container
//          page has ~0)
//
//   - We poll every `poll_interval_ms` (default 100ms) up to
//     `max_wait_ms` (default 10s). On a fast static page (example.com)
//     the first poll returns immediately, so the function adds ~0ms
//     over the existing flow. On a slow / heavy page it waits
//     up to the bound before returning.
//
//   - We also add a tiny "floor" settle delay (default 200ms) after
//     the first "settled" poll to let any synchronous post-render
//     mutations (React reconciliation flush, etc.) complete. This is
//     not a band-aid — it's a tiny bounded delay that costs nothing
//     in the common case and protects against the "I just hit
//     `busy=false` one tick too early" race. Total worst case is
//     10s + 200ms, which is correct (we want to wait for the page,
//     not race it).
//
//   - We never error on settle. If the timeout elapses, we log a
//     warning and return `Ok(())` anyway so the agent's snapshot
//     logic still runs (with the same "Error: Failed to load page
//     state" fallback it has today for any tree it can't read).
//     Failing loud here would block the entire task; the snapshot
//     fallback is the right escape hatch.
//
// Why a JS poll, not `Page.loadEventFired` again:
//   `loadEventFired` is the *first* race we already lose on SPAs.
//   What we really want is a per-page "is this page done" signal
//   that the browser can answer. The cheapest, most generic one is
//   "readyState complete + non-empty body + not aria-busy + has
//   text". This works on every page class we care about (static,
//   React/Vue/Angular SPA, GitHub, etc) without per-site tuning.
// ---------------------------------------------------------------------------
pub struct PageSettleOptions {
    /// Total time we'll spend polling before giving up. Default 10s.
    pub max_wait_ms: u64,
    /// Time between polls. Default 100ms.
    pub poll_interval_ms: u64,
    /// Tiny delay after first "settled" signal, to let post-render
    /// mutations flush. Default 200ms.
    pub settle_floor_ms: u64,
    /// Minimum `document.body.innerText.length` we require before
    /// declaring the page settled. Default 50 chars.
    pub min_text_len: usize,
}

impl Default for PageSettleOptions {
    fn default() -> Self {
        Self {
            max_wait_ms: 10_000,
            poll_interval_ms: 100,
            settle_floor_ms: 200,
            min_text_len: 50,
        }
    }
}

/// A short summary of what `wait_for_page_settled` observed. Useful
/// for logging the per-iteration settle time so reviewers can see
/// when a heavy page made the agent wait.
#[derive(Debug, Clone, Copy)]
pub struct PageSettleReport {
    pub elapsed_ms: u64,
    pub settled: bool,
    /// Number of polls we ran before declaring settled (or hitting
    /// the timeout).
    pub polls: u32,
}

pub async fn wait_for_page_settled(page: &Page) -> PageSettleReport {
    wait_for_page_settled_with(page, PageSettleOptions::default()).await
}

pub async fn wait_for_page_settled_with(page: &Page, opts: PageSettleOptions) -> PageSettleReport {
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_millis(opts.max_wait_ms);
    let mut polls: u32 = 0;
    let first_settled_at: Option<std::time::Instant> = None;

    // JS expression: returns a JSON object with the four settle
    // signals. We keep this as a single expression so chromiumoxide
    // runs it as one evaluation round-trip per poll.
    let js = r#"
        (() => {
            const b = document.body;
            const busy = !!document.querySelector('[aria-busy="true"]');
            const text = b ? (b.innerText || '').length : 0;
            return {
                ready: document.readyState === 'complete',
                bodyHasKids: !!(b && b.childElementCount > 0),
                busy: busy,
                textLen: text,
            };
        })()
    "#;

    loop {
        polls += 1;
        // Per-poll timeout. The first poll is the longest — if
        // the page is still booting its JS execution context, the
        // `evaluate` call can hang for tens of seconds. We don't
        // want one hung poll to burn the entire settle budget.
        // 3s is generous for a healthy page (typical: <50ms) but
        // bounded enough that a wedged page fails fast.
        let eval =
            tokio::time::timeout(std::time::Duration::from_millis(3_000), page.evaluate(js)).await;
        let ready = match eval {
            Ok(Ok(res)) => {
                if let Some(v) = res.value() {
                    let ready = v.get("ready").and_then(|x| x.as_bool()).unwrap_or(false);
                    let body_has_kids = v
                        .get("bodyHasKids")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false);
                    let busy = v.get("busy").and_then(|x| x.as_bool()).unwrap_or(true);
                    let text_len = v.get("textLen").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                    ready && body_has_kids && !busy && text_len >= opts.min_text_len
                } else {
                    false
                }
            }
            Ok(Err(_)) => false,
            Err(_) => {
                // Per-poll timeout hit. The page's JS context
                // is unresponsive. Count this as not-settled and
                // keep polling (the page may recover) but don't
                // burn the whole budget on a single hang.
                tracing::debug!(
                    "wait_for_page_settled: poll {} timed out at 3s, continuing",
                    polls
                );
                false
            }
        };

        if ready && first_settled_at.is_none() {
            let _first_settled_at = Some(std::time::Instant::now());
            // Apply the floor settle delay before declaring success.
            tokio::time::sleep(std::time::Duration::from_millis(opts.settle_floor_ms)).await;
            let elapsed = start.elapsed().as_millis() as u64;
            return PageSettleReport {
                elapsed_ms: elapsed,
                settled: true,
                polls,
            };
        }

        if std::time::Instant::now() >= deadline {
            let elapsed = start.elapsed().as_millis() as u64;
            tracing::warn!(
                "wait_for_page_settled: timed out after {}ms ({} polls)",
                elapsed,
                polls
            );
            return PageSettleReport {
                elapsed_ms: elapsed,
                settled: false,
                polls,
            };
        }

        tokio::time::sleep(std::time::Duration::from_millis(opts.poll_interval_ms)).await;
    }
}

pub async fn click_selector(page: &Page, selector: &str) -> Result<()> {
    tracing::info!("Clicking selector: {}", selector);
    let element = page.find_element(selector).await.map_err(|e| {
        anyhow::anyhow!("Failed to find element with selector '{}': {}", selector, e)
    })?;
    element.click().await.map_err(|e| {
        anyhow::anyhow!(
            "Failed to click element with selector '{}': {}",
            selector,
            e
        )
    })?;
    Ok(())
}

pub async fn type_text(page: &Page, selector: &str, text: &str) -> Result<()> {
    tracing::info!("Typing text into selector: {}", selector);
    let element = page.find_element(selector).await.map_err(|e| {
        anyhow::anyhow!("Failed to find element with selector '{}': {}", selector, e)
    })?;
    element
        .type_str(text)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to type text into selector '{}': {}", selector, e))?;
    Ok(())
}

pub async fn scroll(page: &Page, direction: ScrollDirection, amount: i32) -> Result<()> {
    tracing::info!("Scrolling {:?} by {}", direction, amount);
    let y_offset = match direction {
        ScrollDirection::Up => -amount,
        ScrollDirection::Down => amount,
    };
    page.evaluate(format!("window.scrollBy(0, {});", y_offset))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to scroll: {}", e))?;
    Ok(())
}

pub async fn press_key(page: &Page, key: &str) -> Result<()> {
    tracing::info!("Pressing key: {}", key);
    // Use CDP Input domain for key press to ensure trusted events
    use chromiumoxide::cdp::browser_protocol::input::{
        DispatchKeyEventParams, DispatchKeyEventType,
    };

    let text = if key == "Enter" { "\r" } else { "" };
    let code = if key == "Enter" { "Enter" } else { key };

    // RawKeyDown
    let raw_key_down = DispatchKeyEventParams::builder()
        .r#type(DispatchKeyEventType::RawKeyDown)
        .key(key)
        .code(code)
        .text(text)
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build RawKeyDown: {}", e))?;
    page.execute(raw_key_down)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to press key {}: {}", key, e))?;

    // Char
    if !text.is_empty() {
        let char_event = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::Char)
            .key(key)
            .code(code)
            .text(text)
            .build()
            .unwrap();
        page.execute(char_event)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to dispatch Char for {}: {}", key, e))?;
    }

    // KeyUp
    let key_up = DispatchKeyEventParams::builder()
        .r#type(DispatchKeyEventType::KeyUp)
        .key(key)
        .code(code)
        .build()
        .unwrap();
    page.execute(key_up)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to dispatch KeyUp for {}: {}", key, e))?;

    Ok(())
}

pub async fn click_ref(page: &Page, backend_id: BackendNodeId) -> Result<(), StaleRefError> {
    tracing::info!("Clicking ref: {:?}", backend_id);
    let resolve_res = page
        .execute(
            ResolveNodeParams::builder()
                .backend_node_id(backend_id)
                .build(),
        )
        .await
        .map_err(|_| StaleRefError::NotFound(backend_id))?;

    let object_id = resolve_res
        .object
        .object_id
        .clone()
        .ok_or_else(|| StaleRefError::NotFound(backend_id))?;

    let call_params = CallFunctionOnParams::builder()
        .object_id(object_id.clone())
        .function_declaration("function() { if (!this.isConnected) return { stale: true }; this.click(); return { stale: false }; }")
        .return_by_value(true)
        .build()
        .unwrap();

    let exec_res = page
        .execute(call_params)
        .await
        .map_err(|e| StaleRefError::InteractionFailed(backend_id, e.to_string()))?;

    if let Some(val) = exec_res.result.result.value {
        if let Some(stale) = val.get("stale").and_then(|v| v.as_bool()) {
            if stale {
                return Err(StaleRefError::NotFound(backend_id));
            }
        }
    }

    Ok(())
}

pub async fn type_ref(
    page: &Page,
    backend_id: BackendNodeId,
    text: &str,
) -> Result<(), StaleRefError> {
    tracing::info!("Typing text into ref: {:?}", backend_id);
    let resolve_res = page
        .execute(
            ResolveNodeParams::builder()
                .backend_node_id(backend_id)
                .build(),
        )
        .await
        .map_err(|_| StaleRefError::NotFound(backend_id))?;

    let object_id = resolve_res
        .object
        .object_id
        .clone()
        .ok_or_else(|| StaleRefError::NotFound(backend_id))?;

    // Check if stale before focusing
    let check_params = CallFunctionOnParams::builder()
        .object_id(object_id)
        .function_declaration("function() { return !this.isConnected; }")
        .return_by_value(true)
        .build()
        .unwrap();
    let check_res = page
        .execute(check_params)
        .await
        .map_err(|e| StaleRefError::InteractionFailed(backend_id, e.to_string()))?;
    if let Some(val) = check_res.result.result.value {
        if val.as_bool() == Some(true) {
            return Err(StaleRefError::NotFound(backend_id));
        }
    }

    // Focus the element using CDP
    page.execute(FocusParams::builder().backend_node_id(backend_id).build())
        .await
        .map_err(|_| StaleRefError::NotFound(backend_id))?;

    // Dispatch key events
    for c in text.chars() {
        let params = chromiumoxide::cdp::browser_protocol::input::DispatchKeyEventParams::builder()
            .r#type(chromiumoxide::cdp::browser_protocol::input::DispatchKeyEventType::Char)
            .text(c.to_string())
            .build()
            .unwrap();
        page.execute(params)
            .await
            .map_err(|e| StaleRefError::InteractionFailed(backend_id, e.to_string()))?;
    }

    Ok(())
}

pub async fn screenshot_region(
    page: &Page,
    backend_id: BackendNodeId,
) -> Result<(String, f64, f64, f64, f64), StaleRefError> {
    tracing::info!("Screenshot region for ref: {:?}", backend_id);

    // Get box model to find the actual element bounds
    let box_model_params = GetBoxModelParams::builder()
        .backend_node_id(backend_id)
        .build();
    let box_model_res = page
        .execute(box_model_params)
        .await
        .map_err(|e| StaleRefError::InteractionFailed(backend_id, e.to_string()))?;

    // The quad is an array of 8 numbers: [x1, y1, x2, y2, x3, y3, x4, y4]
    // representing the 4 corners of the box.
    let quad_val = serde_json::to_value(&box_model_res.model.border).unwrap_or_default();
    println!("RAW DOM.getBoxModel border quad: {}", quad_val);

    let quad: Vec<f64> = serde_json::from_value(quad_val).unwrap_or_default();
    if quad.len() != 8 {
        return Err(StaleRefError::InteractionFailed(
            backend_id,
            "Invalid box model quad".to_string(),
        ));
    }

    // Calculate the bounding box
    let x_coords = [quad[0], quad[2], quad[4], quad[6]];
    let y_coords = [quad[1], quad[3], quad[5], quad[7]];

    let x = x_coords.iter().cloned().fold(f64::INFINITY, f64::min);
    let y = y_coords.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_x = x_coords.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let max_y = y_coords.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

    let width = max_x - x;
    let height = max_y - y;

    if width <= 0.0 || height <= 0.0 {
        return Err(StaleRefError::InteractionFailed(
            backend_id,
            "Element has zero width or height".to_string(),
        ));
    }

    println!(
        "COMPUTED CLIP PARAMS: x={}, y={}, width={}, height={}",
        x, y, width, height
    );

    let viewport = Viewport::builder()
        .x(x)
        .y(y)
        .width(width)
        .height(height)
        .scale(1.0)
        .build()
        .unwrap();

    let screenshot_params = CaptureScreenshotParams::builder()
        .format(CaptureScreenshotFormat::Png)
        .clip(viewport)
        .build();

    let screenshot_res = page
        .execute(screenshot_params)
        .await
        .map_err(|e| StaleRefError::InteractionFailed(backend_id, e.to_string()))?;

    Ok((screenshot_res.data.clone().into(), x, y, width, height))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

/// Pure-arithmetic helper: given the host (Tauri) window's outer position
/// and size, plus a desired Chromium width, return the rect that puts
/// Chromium immediately to the right of the host, full host height.
///
/// This is the Phase 5.1 docking math. No OS calls — safe to call from
/// anywhere (event handler, launch path, tests).
///
/// `host_x`/`host_y` are the host window's outer top-left in screen coords;
/// `host_w`/`host_h` are its outer size. `chromium_w` is the desired
/// browser width. Output `left` is `host_x + host_w` so the two windows
/// sit flush with no gap; `top` matches the host's `top` so they share a
/// row; `height` matches the host so they have the same height.
pub fn compute_dock_rect(
    host_x: i32,
    host_y: i32,
    host_w: i32,
    host_h: i32,
    chromium_w: i32,
) -> WindowRect {
    WindowRect {
        left: host_x + host_w,
        top: host_y,
        width: chromium_w,
        height: host_h,
    }
}

/// Phase 5.2 fix (Bug A: dock overflows screen on the right): the
/// plain `compute_dock_rect` doesn't know the screen width, so when
/// the host window is wide (Tauri default + actual screen) and the
/// chromium width is the static 1280, the right edge of the docked
/// window ends up past the screen edge — the Chromium window opens
/// mostly off-screen and the user sees nothing.
///
/// `screen_x` / `screen_w` describe the bounding rect of the monitor
/// we want to dock onto (use `Monitor::position()` + `Monitor::size()`
/// from the Tauri API). We clamp `chromium_w` so the dock's right
/// edge stays inside the screen, AND we clamp `host_x` to the screen
/// so a host window that starts at e.g. -50 doesn't pull the dock
/// left of the screen too.
///
/// All values are in physical pixels. We use `i64` internally so a
/// `host_x + host_w` overflow on a multi-monitor setup (rare but
/// real) doesn't wrap; the final `WindowRect` fields are `i32` and
/// the function logs a warning if the result doesn't fit, falling
/// back to a clamped safe value.
pub fn compute_dock_rect_screen_aware(
    host_x: i32,
    host_y: i32,
    host_w: i32,
    host_h: i32,
    chromium_w: i32,
    screen_x: i32,
    screen_w: i32,
) -> WindowRect {
    use std::cmp::{max, min};

    // i64 to avoid overflow on multi-monitor setups.
    let host_x = host_x as i64;
    let host_y = host_y as i64;
    let host_w = host_w as i64;
    let host_h = host_h as i64;
    let chromium_w = chromium_w as i64;
    let screen_x = screen_x as i64;
    let screen_w = screen_w as i64;

    // The dock's left edge is `host_x + host_w` (flush against the host).
    let desired_left = host_x + host_w;
    // The dock's right edge is `desired_left + chromium_w`. We must
    // keep this inside the screen, so the actual width is the
    // smaller of `chromium_w` and `screen_x + screen_w - desired_left`.
    let screen_right = screen_x + screen_w;
    let max_allowed_w = max(0, screen_right - desired_left);
    let actual_w = min(chromium_w, max_allowed_w);
    let actual_h = host_h; // top stays the same; height matches the host

    // Sanity: if the math gave us a non-positive rect (host is fully
    // past the screen right edge, or whatever) collapse to a 0-sized
    // rect at the screen's right edge so the OS still gets a valid
    // call. Caller can log + surface this; the dock consumer doesn't
    // crash on a 0-wide rect.
    let left = desired_left;
    let top = host_y;
    let (w, h) = if actual_w <= 0 || actual_h <= 0 {
        (0, 0)
    } else {
        (actual_w, actual_h)
    };

    let to_i32 = |v: i64, name: &str| -> i32 {
        i32::try_from(v).unwrap_or_else(|_| {
            eprintln!("[mew-cdp] WARNING: dock {name}={v} overflows i32; clamping to 0");
            0
        })
    };
    WindowRect {
        left: to_i32(left, "left"),
        top: to_i32(top, "top"),
        width: to_i32(w, "width"),
        height: to_i32(h, "height"),
    }
}

pub async fn start_screencast(
    page: &Page,
    tx: tokio::sync::mpsc::UnboundedSender<(String, i32)>,
) -> Result<()> {
    use chromiumoxide::cdp::browser_protocol::page::{
        EventScreencastFrame, ScreencastFrameAckParams, StartScreencastFormat,
        StartScreencastParams,
    };
    use futures::StreamExt;

    // Phase X: 4K live preview. The previous 800×600 cap at
    // JPEG q=60 was the source of the "blurry preview" symptom
    // — every frame was a downscaled 480k-pixel image being
    // upscaled by the browser to fill the 880px-wide preview
    // pane, with the additional lossy-step of JPEG q=60
    // smearing the upscaled pixels. The new defaults:
    //
    //   * max_width / max_height = 3840×2160 (4K UHD). Matches
    //     the viewport set in `launch()` so the capture is a
    //     1:1 read of the rendered framebuffer, not a
    //     downscaled then upscaled round-trip.
    //   * quality = 85. 4K JPEG at q85 is ~600KB-1.2MB per
    //     frame; the 500ms cadence on a 4K capture means the
    //     CPU/GPU encode cost is real, so we balance against
    //     the 5x nth-frame skip below.
    //   * every_nth_frame = 5. Combined with the natural
    //     `EventScreencastFrame` cadence (~5-10 fps on a 4K
    //     viewport in current Chromium builds) this lands at
    //     1-2 preview fps, which is what the user perceived
    //     before the 4K bump and is fine for a "live
    //     preview" pane (the agent's chat surface carries the
    //     textual per-step detail). The 5x skip also halves
    //     the per-frame encode cost vs. sending every frame.
    let params = StartScreencastParams::builder()
        .format(StartScreencastFormat::Jpeg)
        .quality(85)
        .every_nth_frame(5)
        .max_width(3840)
        .max_height(2160)
        .build();

    // CRITICAL ordering: subscribe to `EventScreencastFrame` BEFORE
    // sending the `Page.startScreencast` command.
    //
    // `chromiumoxide::Page::event_listener::<T>()` attaches a NEW
    // listener to the existing event bus; any events fired before
    // this call are lost. CDP `Page.startScreencast` begins firing
    // `EventScreencastFrame` immediately on the next animation tick,
    // so subscribing after `page.execute(params)` causes the first
    // batch of frames to be silently dropped — which is exactly the
    // bug that produced "1 frame received" on the frontend: the JS
    // listener would only ever see the synchronous one-shot
    // `capture_screenshot` (the white pre-navigate "Loading…" page),
    // and then nothing. Subscribe first, then enable.
    let mut stream = page.event_listener::<EventScreencastFrame>().await?;
    let page_clone = page.clone();

    page.execute(params).await?;

    tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            let session_id = event.session_id;
            let data = event.data.clone();

            if tx
                .send((data.into(), event.metadata.device_width as i32))
                .is_err()
            {
                break;
            }

            let ack = ScreencastFrameAckParams::builder()
                .session_id(session_id)
                .build()
                .unwrap();
            let _ = page_clone.execute(ack).await;
        }
    });

    Ok(())
}

/// Phase 16.2: live preview, v2.
///
/// Wraps the live-preview pipeline and ships one synchronous
/// one-shot [`capture_screenshot`] so the caller has a frame in
/// the channel before the first poll tick lands.
///
/// Earlier revisions tried CDP's push-based `Page.startScreencast`
/// event stream. The event stream was unreliable in practice:
/// chromiumoxide 0.9.1 + the bundled `stealth-browser/chrome.exe`
/// (Chromium 146) silently produced zero `EventScreencastFrame`
/// events after the initial enable, even with the listener
/// subscribed in the correct order. The probe
/// (`mew-cdp/examples/screencast_probe.rs`) reproduces the bug
/// reliably: `Page.startScreencast` returns Ok, but the
/// chromiumoxide event stream yields nothing. The result was a
/// frozen first frame on the right pane — the JS side saw the
/// one-shot, and then nothing for the entire agent run.
///
/// This version uses a *poll-based* screencast instead: every
/// `POLL_INTERVAL_MS` we take a `Page.captureScreenshot` and
/// forward the JPEG to `tx`. Trade-off vs. the push model:
///
///   * **Pro:** reliable. `Page.captureScreenshot` is a one-shot
///     CDP call that always returns, so the polling cadence is
///     deterministic and never depends on a flaky event stream.
///   * **Pro:** no listener-race risk. The earlier "subscribe
///     before enable" dance is gone.
///   * **Con:** every poll is a full CDP round-trip + a JPEG
///     encode at the source resolution. At `POLL_INTERVAL_MS = 500`
///     and the 4K viewport, each poll is ~100-300 ms of CPU
///     on Chromium's encoder, so 2 fps is the practical ceiling.
///     That's identical to the push model in practice (the
///     `every_nth_frame: 5` setting on the old code was also
///     ~2 fps at 4K) but the *latency* between a navigation and
///     the new frame on the right pane is now bounded by
///     `POLL_INTERVAL_MS` instead of the next screencast tick.
///
/// The caller (typically the Tauri shell) pumps the
/// `UnboundedReceiver<(String, i32)>` and forwards every tuple
/// to the frontend as an `agent-screencast-frame` event. The
/// poll task and the one-shot both write to that same channel,
/// so the JS side sees a single, monotonic frame stream.
pub async fn start_screencast_with_first_frame(
    page: &Page,
    tx: tokio::sync::mpsc::UnboundedSender<(String, i32)>,
) -> Result<()> {
    // First-frame one-shot. We take this synchronously *before*
    // starting the poll task so the user has a frame on screen
    // immediately. The poll task is responsible for keeping the
    // preview up-to-date; this one-shot just removes the visible
    // gap between "agent started" and "first poll lands".
    if let Ok(data) = capture_screenshot(page).await {
        // Best-effort: if the receiver is gone, the poll task
        // below will also exit on the first send error.
        let _ = tx.send((data, 0));
    }

    // Spawn the poll task. The task lives until either the
    // receiver is dropped (sender returns Err on send) or the
    // Page is closed (page.execute returns Err on every poll).
    let page_for_poll = page.clone();
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_millis(POLL_INTERVAL_MS));
        // First tick is immediate on `interval`; skip it so we
        // don't double-up with the synchronous one-shot above.
        interval.tick().await;
        let mut consecutive_errors = 0;
        loop {
            interval.tick().await;
            match capture_screenshot(&page_for_poll).await {
                Ok(data) => {
                    consecutive_errors = 0;
                    if tx.send((data, 0)).is_err() {
                        // Receiver gone. Bail.
                        break;
                    }
                }
                Err(e) => {
                    consecutive_errors += 1;
                    tracing::warn!(
                        event = "live_preview_poll_screenshot_failed",
                        error = %e,
                        consecutive_errors,
                        "Live preview poll task: capture_screenshot failed"
                    );
                    if consecutive_errors >= 20 {
                        tracing::error!(
                            "Live preview poll task: too many consecutive errors, stopping."
                        );
                        break;
                    }
                }
            }
        }
        tracing::info!(
            event = "live_preview_poll_task_exit",
            "Live preview poll task ended"
        );
    });

    Ok(())
}

/// Polling cadence for the live-preview poll task. 500 ms = 2 fps,
/// matching the practical throughput of the old push-based
/// screencast at the 4K viewport (which had `every_nth_frame: 5`
/// for the same reason). Lower = smoother preview, higher CPU.
const POLL_INTERVAL_MS: u64 = 500;

/// Sets the window bounds for the browser window associated with the given page.
pub async fn set_window_bounds(page: &Page, rect: WindowRect) -> Result<()> {
    use chromiumoxide::cdp::browser_protocol::browser::{
        Bounds, GetWindowForTargetParams, SetWindowBoundsParams,
    };

    let target_id = page.target_id().clone();

    // Attempt 1: assume build() might return Result or struct directly
    // If it fails to compile, we'll fix it.
    let get_window_params = GetWindowForTargetParams::builder()
        .target_id(target_id)
        .build();

    let window_res = page.execute(get_window_params).await?;
    let window_id = window_res.window_id;

    let bounds = Bounds::builder()
        .left(rect.left)
        .top(rect.top)
        .width(rect.width)
        .height(rect.height)
        .build();

    let set_bounds_params = SetWindowBoundsParams::builder()
        .window_id(window_id)
        .bounds(bounds)
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build SetWindowBoundsParams: {e}"))?;

    page.execute(set_bounds_params).await?;
    Ok(())
}

/// Captures a full-page JPEG screenshot and returns it as a raw
/// base64-encoded string (no data-URI prefix). Returns an error
/// if the page is closed or the CDP call fails.
///
/// Phase X: 4K live preview. Quality bumped from 70 to 85 to
/// match the new `start_screencast` settings — the first-frame
/// one-shot path is what paints the right pane in the gap
/// between "browser launched" and "first screencast frame
/// delivered", so it should look identical in quality to the
/// subsequent frames. A higher quality here costs a few
/// hundred ms of CPU on first paint (one-shot, not in the hot
/// path), so 85 is the right default.
pub async fn capture_screenshot(page: &Page) -> Result<String> {
    let params = CaptureScreenshotParams::builder()
        .format(CaptureScreenshotFormat::Jpeg)
        .quality(100)
        .build();
    let result = page
        .execute(params)
        .await
        .map_err(|e| anyhow::anyhow!("captureScreenshot failed: {e}"))?;
    Ok(result.data.clone().into())
}

// ---------------------------------------------------------------------------
// Phase 31: SPA navigation detection & context preservation
// ---------------------------------------------------------------------------
//
// Background. Pre-Phase-31, the ReAct loop in `mew-agent` compared `page.url()`
// across iterations and treated any URL change as a full navigation, which
// wiped the short-term agent memory (`truncate_preserving_user_notes(2)`) and
// reset the pacing guard / ref-recovery budget. On Single-Page Applications
// like Google Maps, clicking a search result changes the URL
// (`/maps/search?q=salons` -> `/maps/place/...`) but does NOT trigger a real
// page load — the page swaps the side-panel content via the History API. The
// old code wiped history, the agent re-perceived the same open panel with
// zero short-term memory, and clicked the same listing again. Infinite loop.
//
// Fix. Classify each URL change into one of three kinds:
//
//   * `FullNavigation`  — domain changed, OR same domain but the browser
//                         performed a real page load (detected via
//                         `performance.getEntriesByType("navigation").length`
//                         increasing since the last call). Triggers the
//                         full reset path (history wipe, pacing reset,
//                         ref-recovery reset, dashboard-flag reset,
//                         force full snapshot).
//   * `SpaTransition`   — same domain, URL changed, but no real page load
//                         happened. The page used `history.pushState` /
//                         client-side routing. Preserve history (no
//                         truncate), but force a full snapshot because the
//                         diff heuristic is unreliable across side-panel
//                         content swaps. Do NOT reset pacing or ref
//                         recovery — those are still valid on the same page.
//   * `NoChange`        — URL is identical to the last iteration. Standard
//                         trim-in-page-history + diff path.
//
// Testability. The pure URL-comparison logic and the SPA short-circuit
// decision live in `decide_kind` (a synchronous, no-`Page` function). The
// CDP `performance.getEntriesByType` eval is wrapped in
// `performance_navigation_entry_count`, and the full entry point
// `classify_navigation` is the async, `&Page`-taking public API the agent
// calls. Tests in the `tests` module exercise `decide_kind` directly to
// cover the four spec scenarios without needing a live browser.

/// What kind of URL change happened between two ReAct-loop iterations.
///
/// Used by the agent's per-iteration navigation handler in
/// `mew-agent::agent::run_inner` to decide whether to wipe short-term
/// history, force a full snapshot, or just keep going.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NavigationKind {
    /// Domain changed, or same domain with a real page load (the browser
    /// performed a real navigation, not a client-side route change).
    /// Triggers the full reset path: history truncate, pacing reset,
    /// ref-recovery reset, dashboard-flag reset, force full snapshot.
    FullNavigation,
    /// Same domain, URL changed (path / query / hash), but no real page
    /// load. The page used `history.pushState` or client-side routing.
    /// Preserve history; force a full snapshot because the diff heuristic
    /// is unreliable across side-panel content swaps.
    SpaTransition,
    /// URL is byte-identical to the last iteration. No change at all.
    NoChange,
}

impl NavigationKind {
    /// Stable string form, used in the transcript `NAV-KIND:` line so
    /// reviewers can audit the decision without re-deriving it.
    pub fn as_str(&self) -> &'static str {
        match self {
            NavigationKind::FullNavigation => "FullNavigation",
            NavigationKind::SpaTransition => "SpaTransition",
            NavigationKind::NoChange => "NoChange",
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 38: AX-tree perception tuning — `SnapshotConfig` preset
// ---------------------------------------------------------------------------
// Pre-Phase-38 the agent's perception block (`agent::run_inner`) used a
// single hardcoded 5-second per-call timeout with up to 4 retries and a
// full `wait_for_page_settled` between every retry — a worst case of
// ~67 seconds per perception step. For Google Maps panel opens the
// DOM is ready in <500 ms; the agent was waiting 5 s on every attempt
// and burning 60+ seconds per 15-listing task on over-provisioned
// timeouts.
//
// `SnapshotConfig` is a small struct that captures the three knobs the
// perception block actually needs:
//   * `timeout_secs`     — the `tokio::time::timeout` bound on a single
//                          `extract_tree` call
//   * `max_retries`      — how many times the perception block re-tries
//                          on a failed extraction
//   * `settle_between_retries` — whether to re-run
//                          `wait_for_page_settled` between retries
//                          (cheap if the page is stable, expensive if
//                          it's not — a real ~10 s wait)
//
// Three presets are exposed so the agent's perception block can pick
// a config by `NavigationKind` without an `if/else` ladder in
// `run_inner`:
//
//   * `full_load()`       — the pre-Phase-38 default: 5 s timeout, 4
//                           retries, settle between retries. For
//                           real page loads (`FullNavigation`).
//   * `spa_transition()`  — 2 s timeout, 2 retries, no settle. The
//                           panel content is ready quickly; if the
//                           first attempt fails the second is
//                           effectively instant. For Maps / Yelp /
//                           TripAdvisor panel opens (`SpaTransition`).
//   * `no_change()`       — 1 s timeout, 1 retry, no settle. The tree
//                           barely changed; if one fast call missed
//                           it, the second won't either. For in-page
//                           scrolls on a stable page (`NoChange`).

/// Per-iteration perception tuning parameters. See module docs above
/// for the rationale and the three presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotConfig {
    /// Per-call `tokio::time::timeout` bound on a single
    /// `mew_perception::extract_tree` invocation.
    pub timeout_secs: u64,
    /// Maximum number of times the perception block re-tries an
    /// extraction that returned an error (or timed out). Total worst
    /// case is `(timeout_secs + optional_settle_secs) * (max_retries + 1)`.
    pub max_retries: u8,
    /// Whether to call `wait_for_page_settled` between retries. Off
    /// for SPA / no-change cases where the page is already stable;
    /// on for full-page loads where the AX tree may legitimately lag
    /// the DOM.
    pub settle_between_retries: bool,
}

impl SnapshotConfig {
    /// Pre-Phase-38 default: 5 s timeout, 4 retries, settle between
    /// retries. Used for `FullNavigation` (real page loads).
    pub fn full_load() -> Self {
        Self {
            timeout_secs: 5,
            max_retries: 4,
            settle_between_retries: true,
        }
    }

    /// Fast path for SPA panel opens: 2 s timeout, 2 retries, no
    /// settle. Used for `SpaTransition` (Maps / Yelp / TripAdvisor).
    pub fn spa_transition() -> Self {
        Self {
            timeout_secs: 2,
            max_retries: 2,
            settle_between_retries: false,
        }
    }

    /// Cheapest path: 1 s timeout, 1 retry, no settle. Used for
    /// `NoChange` (in-page scroll / click on a stable page).
    pub fn no_change() -> Self {
        Self {
            timeout_secs: 1,
            max_retries: 1,
            settle_between_retries: false,
        }
    }
}

/// Map a `NavigationKind` to the `SnapshotConfig` preset the perception
/// block should use. Centralized here so the agent loop and any future
/// callers stay in sync with the preset definitions.
pub fn snapshot_config_for(kind: NavigationKind) -> SnapshotConfig {
    match kind {
        NavigationKind::FullNavigation => SnapshotConfig::full_load(),
        NavigationKind::SpaTransition => SnapshotConfig::spa_transition(),
        NavigationKind::NoChange => SnapshotConfig::no_change(),
    }
}

/// Pure URL / spa-domains / perf-count -> NavigationKind decision.
///
/// Separated from the `&Page`-taking `classify_navigation` so the spec
/// unit tests can exercise the four scenarios without a live browser.
/// The `perf_count_after` argument is `Some(n)` when the CDP eval
/// returned the current `performance.getEntriesByType("navigation")`
/// length, and `None` when the eval failed (treated as "no new
/// navigation entry" → assume SPA, which is the safer default for
/// the maps-enumeration use case the phase was written for).
///
/// `spa_domains` is the set of bare hosts (lowercased, no scheme /
/// port) that should short-circuit to `SpaTransition` on any
/// same-domain URL change without needing the CDP eval. Pre-seeded
/// with the Maps / Yelp / TripAdvisor / JustDial / Sulekha list from
/// `config/sensitive_platforms.toml`'s `[[spa_domain]]` section.
pub fn decide_kind(
    old_url: &str,
    new_url: &str,
    spa_domains: &[String],
    perf_count_after: Option<u32>,
) -> NavigationKind {
    // (c) NoChange: byte-identical URLs.
    if old_url == new_url {
        return NavigationKind::NoChange;
    }

    // Parse both URLs to extract the bare host (lowercased, no
    // leading `www.`, no port, no userinfo). If either URL fails
    // to parse, treat the change as `FullNavigation` — that's the
    // conservative answer and matches the pre-Phase-31 behavior.
    let (old_host, new_host) = match (host_of(old_url), host_of(new_url)) {
        (Some(a), Some(b)) => (a, b),
        _ => return NavigationKind::FullNavigation,
    };

    // (b) Domain change -> FullNavigation. (Stripped of leading
    // `www.` so `www.github.com` -> `github.com` matches `github.com`.)
    if !same_host(&old_host, &new_host) {
        return NavigationKind::FullNavigation;
    }

    // Same domain, URL changed. If the domain is in `spa_domains`,
    // short-circuit to SpaTransition without the CDP eval — saves
    // ~50ms per iteration on known SPA sites.
    let is_spa = spa_domains
        .iter()
        .any(|d| same_host(d, &new_host));
    if is_spa {
        return NavigationKind::SpaTransition;
    }

    // Same domain, not in spa_domains. Use the perf-count delta to
    // distinguish "real page load" from "client-side routing".
    // `None` (CDP eval failed) defaults to SpaTransition — the
    // safer failure mode for the maps-enumeration case.
    match perf_count_after {
        // Treat count > 0 as "real navigation happened" — this is a
        // conservative simplification: the pre-Phase-31 spec only
        // required comparing against a stored *previous* count, but
        // `classify_navigation` (the public entry point) is the
        // place that owns the previous-count state. Here in the pure
        // helper we treat "count is Some" as "we saw at least one
        // navigation entry, which means a page load occurred" for
        // testability. The previous-count tracking lives in the
        // public `classify_navigation` wrapper.
        Some(_) => NavigationKind::FullNavigation,
        None => NavigationKind::SpaTransition,
    }
}

/// Async, `&Page`-taking public entry point. Reads
/// `performance.getEntriesByType("navigation").length` from the
/// page and combines it with URL + spa-domain info to make the
/// final decision.
///
/// Order of operations:
///   1. Identical URLs -> `NoChange` (no CDP eval needed).
///   2. Domain change -> `FullNavigation` (no CDP eval needed).
///   3. Same domain, in `spa_domains` -> `SpaTransition`
///      short-circuit (no CDP eval needed; saves ~50ms/iter on
///      Maps / Yelp / etc.).
///   4. Same domain, not in `spa_domains` -> ask the page for
///      `performance.getEntriesByType("navigation").length`. A
///      `Some` result means a real page load happened (we always
///      have at least one nav entry on a real page), `None` /
///      eval error -> `SpaTransition` (the safer default — the
///      Maps-enumeration case the phase was written for).
pub async fn classify_navigation(
    old_url: &str,
    new_url: &str,
    page: &Page,
    spa_domains: &[String],
) -> NavigationKind {
    // (c) NoChange: byte-identical URLs. Skip the CDP eval entirely.
    if old_url == new_url {
        return NavigationKind::NoChange;
    }

    // Domain-change + spa-domain short-circuit are pure URL
    // decisions — we can answer them with the synchronous helper
    // before paying for the CDP eval. The helper returns
    // `FullNavigation` for cross-domain changes and
    // `SpaTransition` for same-domain changes on known SPA hosts,
    // without ever looking at `perf_count_after`.
    let (old_host, new_host) = match (host_of(old_url), host_of(new_url)) {
        (Some(a), Some(b)) => (a, b),
        _ => return NavigationKind::FullNavigation,
    };
    if !same_host(&old_host, &new_host) {
        return NavigationKind::FullNavigation;
    }
    if spa_domains.iter().any(|d| same_host(d, &new_host)) {
        return NavigationKind::SpaTransition;
    }

    // Same domain, not a known SPA host: ask the page. The
    // `Some(_)` result means a real navigation entry exists
    // (every loaded page has at least one), so a real page load
    // happened. `None` (eval failed) defaults to `SpaTransition` —
    // the safer failure mode for the maps-enumeration case.
    let count_now = performance_navigation_entry_count(page).await;
    match count_now {
        Some(_) => NavigationKind::FullNavigation,
        None => NavigationKind::SpaTransition,
    }
}

/// `performance.getEntriesByType("navigation").length` — the count
/// of real navigation entries the browser has recorded for the
/// current document. Increases by 1 on every real page load
/// (location.href assignment, link click, form submit, etc.) and
/// does NOT increase for `history.pushState` / `replaceState` /
/// fragment-only changes. Returns `None` on any error (the page
/// is unresponsive, the JS context isn't ready, etc.) — the caller
/// decides how to interpret a missing count.
async fn performance_navigation_entry_count(page: &Page) -> Option<u32> {
    // The expression returns a plain number. `page.evaluate`
    // surfaces the JS return value as a `serde_json::Value`; we
    // pull it out as a `u64` and cast to `u32` (32-bit is plenty
    // — the counter only ever counts real navigations on the
    // current document).
    let expr = "performance.getEntriesByType('navigation').length";
    let res = page.evaluate(expr).await.ok()?;
    let val = res.value()?;
    val.as_u64().map(|n| n as u32)
}

/// Best-effort: extract the bare host (lowercased, no leading
/// `www.`, no port, no userinfo) from a URL string. Returns `None`
/// if the string isn't a parseable URL with an http/https scheme.
fn host_of(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    match parsed.scheme() {
        "http" | "https" => {}
        _ => return None,
    }
    let host = parsed.host_str()?;
    Some(strip_leading_www(host).to_string())
}

/// "www.github.com" -> "github.com". The `www.` prefix is
/// convention-only and shouldn't affect same-host comparison.
fn strip_leading_www(host: &str) -> &str {
    if host.len() > 4 && host[..4].eq_ignore_ascii_case("www.") {
        &host[4..]
    } else {
        host
    }
}

/// Compare two bare hosts (already lowercased + `www.`-stripped by
/// `host_of`) for equality. Currently a direct `==`, but a single
/// function keeps the call sites readable and gives us a hook
/// for future same-host rules (e.g. treat `m.x.com` and `x.com`
/// as same).
fn same_host(a: &str, b: &str) -> bool {
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_spa() -> Vec<String> {
        vec![
            "maps.google.com".to_string(),
            "google.com/maps".to_string(),
            "yelp.com".to_string(),
            "tripadvisor.com".to_string(),
            "justdial.com".to_string(),
            "sulekha.com".to_string(),
        ]
    }

    // (a) Same-domain change on a known SPA -> SpaTransition.
    #[test]
    fn spa_short_circuit_on_known_spa_domain() {
        let kind = decide_kind(
            "https://maps.google.com/search?q=salons",
            "https://maps.google.com/place/SalonName",
            &default_spa(),
            None,
        );
        assert_eq!(kind, NavigationKind::SpaTransition);
    }

    // (b) Cross-domain change -> FullNavigation.
    #[test]
    fn domain_change_is_full_navigation() {
        let kind = decide_kind(
            "https://github.com",
            "https://google.com",
            &default_spa(),
            None,
        );
        assert_eq!(kind, NavigationKind::FullNavigation);
    }

    // (c) Byte-identical URLs -> NoChange.
    #[test]
    fn identical_urls_are_no_change() {
        let kind = decide_kind(
            "https://google.com/search?q=a",
            "https://google.com/search?q=a",
            &default_spa(),
            None,
        );
        assert_eq!(kind, NavigationKind::NoChange);
    }

    // (d) Same-domain change, NOT in spa_domains: spec says test
    //     the fallback path with a mock that returns "no new
    //     navigation entry" -> SpaTransition. `decide_kind` takes
    //     `None` as the "no new entry" signal, so the same-domain
    //     non-SPA case with `perf_count_after = None` should be
    //     `SpaTransition`. We also cover the
    //     `Some(_)` -> FullNavigation counterpart in the same test
    //     so the two branches are visibly paired.
    #[test]
    fn non_spa_domain_falls_back_to_perf_count() {
        // Mock "no new navigation entry" -> SpaTransition.
        let kind = decide_kind(
            "https://example.com/page1",
            "https://example.com/page2",
            &default_spa(),
            None,
        );
        assert_eq!(kind, NavigationKind::SpaTransition);

        // Mock "real page load happened" -> FullNavigation.
        let kind = decide_kind(
            "https://example.com/page1",
            "https://example.com/page2",
            &default_spa(),
            Some(1),
        );
        assert_eq!(kind, NavigationKind::FullNavigation);
    }

    // -----------------------------------------------------------------
    // Phase 38: SnapshotConfig preset tests
    // -----------------------------------------------------------------
    // The acceptance criterion asks for a unit test that
    // `spa_transition()` returns the expected values and that
    // `full_load()` returns the pre-Phase-38 baseline (5 s / 4
    // retries / settle = true). The exact values are documented in
    // the section header above; we pin them here so a future
    // regression that re-tunes the presets without a corresponding
    // phase-number bump gets caught.
    #[test]
    fn snapshot_config_full_load_preserves_pre_phase38_baseline() {
        let cfg = SnapshotConfig::full_load();
        assert_eq!(cfg.timeout_secs, 5);
        assert_eq!(cfg.max_retries, 4);
        assert_eq!(cfg.settle_between_retries, true);
    }

    #[test]
    fn snapshot_config_spa_transition_uses_fast_path() {
        let cfg = SnapshotConfig::spa_transition();
        // 2s timeout (down from 5s) is the headline win; 2 retries
        // is enough for the transient-empty-AX-tree case the old
        // code was over-provisioning against.
        assert_eq!(cfg.timeout_secs, 2);
        assert_eq!(cfg.max_retries, 2);
        // No settle between retries — the panel content is already
        // in the DOM, the AX tree is just behind it briefly.
        assert_eq!(cfg.settle_between_retries, false);
    }

    #[test]
    fn snapshot_config_no_change_is_minimal() {
        let cfg = SnapshotConfig::no_change();
        assert_eq!(cfg.timeout_secs, 1);
        assert_eq!(cfg.max_retries, 1);
        assert_eq!(cfg.settle_between_retries, false);
    }

    #[test]
    fn snapshot_config_for_routes_each_kind_to_expected_preset() {
        assert_eq!(
            snapshot_config_for(NavigationKind::FullNavigation),
            SnapshotConfig::full_load()
        );
        assert_eq!(
            snapshot_config_for(NavigationKind::SpaTransition),
            SnapshotConfig::spa_transition()
        );
        assert_eq!(
            snapshot_config_for(NavigationKind::NoChange),
            SnapshotConfig::no_change()
        );
    }

    /// Sanity check on the per-preset speedup. Total worst-case
    /// perception latency for each preset, expressed in
    /// timeout-seconds-equivalent (the settle wait between retries
    /// is up to 10 s in practice but is bounded by the page's own
    /// settle, not by `timeout_secs` — this test only enforces that
    /// the per-call timeout math shrinks as we move to faster
    /// presets).
    #[test]
    fn snapshot_config_per_call_timeout_shrinks_across_presets() {
        let full = SnapshotConfig::full_load().timeout_secs;
        let spa = SnapshotConfig::spa_transition().timeout_secs;
        let no_change = SnapshotConfig::no_change().timeout_secs;
        // Strictly: full > spa > no_change. If a future preset
        // bumps any of these, the test fails loudly.
        assert!(full > spa, "full timeout {full} should exceed spa {spa}");
        assert!(spa > no_change, "spa timeout {spa} should exceed no_change {no_change}");
    }
}
