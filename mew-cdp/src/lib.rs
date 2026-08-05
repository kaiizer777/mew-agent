use anyhow::Result;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::dom::{BackendNodeId, FocusParams, ResolveNodeParams};
use chromiumoxide::cdp::js_protocol::runtime::CallFunctionOnParams;
use futures::StreamExt;
use thiserror::Error;
use chromiumoxide::cdp::browser_protocol::dom::GetBoxModelParams;
use chromiumoxide::cdp::browser_protocol::page::{CaptureScreenshotParams, CaptureScreenshotFormat, Viewport};

/// Phase 3: re-export `chromiumoxide::Page` so crates downstream
/// of `mew-cdp` (the Tauri `mew-ui` crate, in particular) can
/// name the `Page` type without taking a direct dependency on
/// `chromiumoxide`. The orchestrator's `BrowserAgentFactory`
/// trait uses this re-exported name in its method signatures.
pub use chromiumoxide::Page as ReExportedPage;

mod process_lifetime;
use process_lifetime::JobObject;

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
        .backend_node_id(backend_id.clone())
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

/// Launches a headed Chrome instance via CDP using chromiumoxide.
/// Configured with a fixed remote debugging port (9222) and persistent user data directory (`./profile`).
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
    let profile_dir = std::env::current_dir()?.join("profile");

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

    let config = config_builder.build()
        .map_err(|e| anyhow::anyhow!("Failed to build BrowserConfig: {e}"))?;

    tracing::info!("Launching headed Chrome on remote debugging port 9222...");

    launch_inner(config, visible_cursor).await
}

pub async fn launch_headless(
    binary_path: Option<String>,
    visible_cursor: bool,
) -> Result<(Browser, Page, tokio::task::JoinHandle<()>, JobObject)> {
    let profile_dir = std::env::current_dir()?.join("profile");

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

    let config = config_builder.build()
        .map_err(|e| anyhow::anyhow!("Failed to build BrowserConfig: {e}"))?;

    tracing::info!("Launching headless Chrome on remote debugging port 9222...");

    launch_inner(config, visible_cursor).await
}

async fn launch_inner(
    config: BrowserConfig,
    visible_cursor: bool,
) -> Result<(Browser, Page, tokio::task::JoinHandle<()>, JobObject)> {

    let (mut browser, mut handler) = Browser::launch(config).await?;

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
    let chrome_pid: Option<u32> = browser
        .get_mut_child()
        .and_then(|c| c.inner.id());
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

pub async fn wait_for_page_settled_with(
    page: &Page,
    opts: PageSettleOptions,
) -> PageSettleReport {
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_millis(opts.max_wait_ms);
    let mut polls: u32 = 0;
    let mut first_settled_at: Option<std::time::Instant> = None;

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
        let eval = tokio::time::timeout(
            std::time::Duration::from_millis(3_000),
            page.evaluate(js),
        )
        .await;
        let ready = match eval {
            Ok(Ok(res)) => {
                if let Some(v) = res.value() {
                    let ready = v.get("ready").and_then(|x| x.as_bool()).unwrap_or(false);
                    let body_has_kids = v
                        .get("bodyHasKids")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false);
                    let busy = v.get("busy").and_then(|x| x.as_bool()).unwrap_or(true);
                    let text_len = v
                        .get("textLen")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0) as usize;
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
    let element = page.find_element(selector).await
        .map_err(|e| anyhow::anyhow!("Failed to find element with selector '{}': {}", selector, e))?;
    element.click().await
        .map_err(|e| anyhow::anyhow!("Failed to click element with selector '{}': {}", selector, e))?;
    Ok(())
}

pub async fn type_text(page: &Page, selector: &str, text: &str) -> Result<()> {
    tracing::info!("Typing text into selector: {}", selector);
    let element = page.find_element(selector).await
        .map_err(|e| anyhow::anyhow!("Failed to find element with selector '{}': {}", selector, e))?;
    element.type_str(text).await
        .map_err(|e| anyhow::anyhow!("Failed to type text into selector '{}': {}", selector, e))?;
    Ok(())
}

pub async fn scroll(page: &Page, direction: ScrollDirection, amount: i32) -> Result<()> {
    tracing::info!("Scrolling {:?} by {}", direction, amount);
    let y_offset = match direction {
        ScrollDirection::Up => -amount,
        ScrollDirection::Down => amount,
    };
    page.evaluate(format!("window.scrollBy(0, {});", y_offset)).await
        .map_err(|e| anyhow::anyhow!("Failed to scroll: {}", e))?;
    Ok(())
}

pub async fn press_key(page: &Page, key: &str) -> Result<()> {
    tracing::info!("Pressing key: {}", key);
    // Use CDP Input domain for key press to ensure trusted events
    use chromiumoxide::cdp::browser_protocol::input::{DispatchKeyEventParams, DispatchKeyEventType};
    
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
    page.execute(raw_key_down).await
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
        page.execute(char_event).await
            .map_err(|e| anyhow::anyhow!("Failed to dispatch Char for {}: {}", key, e))?;
    }

    // KeyUp
    let key_up = DispatchKeyEventParams::builder()
        .r#type(DispatchKeyEventType::KeyUp)
        .key(key)
        .code(code)
        .build()
        .unwrap();
    page.execute(key_up).await
        .map_err(|e| anyhow::anyhow!("Failed to dispatch KeyUp for {}: {}", key, e))?;
        
    Ok(())
}

pub async fn click_ref(page: &Page, backend_id: BackendNodeId) -> Result<(), StaleRefError> {
    tracing::info!("Clicking ref: {:?}", backend_id);
    let resolve_res = page.execute(
        ResolveNodeParams::builder().backend_node_id(backend_id.clone()).build()
    ).await.map_err(|_| StaleRefError::NotFound(backend_id.clone()))?;

    let object_id = resolve_res.object.object_id.clone().ok_or_else(|| StaleRefError::NotFound(backend_id.clone()))?;

    let call_params = CallFunctionOnParams::builder()
        .object_id(object_id.clone())
        .function_declaration("function() { if (!this.isConnected) return { stale: true }; this.click(); return { stale: false }; }")
        .return_by_value(true)
        .build()
        .unwrap();

    let exec_res = page.execute(call_params).await.map_err(|e| StaleRefError::InteractionFailed(backend_id.clone(), e.to_string()))?;
    
    if let Some(val) = exec_res.result.result.value {
        if let Some(stale) = val.get("stale").and_then(|v| v.as_bool()) {
            if stale {
                return Err(StaleRefError::NotFound(backend_id));
            }
        }
    }

    Ok(())
}

pub async fn type_ref(page: &Page, backend_id: BackendNodeId, text: &str) -> Result<(), StaleRefError> {
    tracing::info!("Typing text into ref: {:?}", backend_id);
    let resolve_res = page.execute(
        ResolveNodeParams::builder().backend_node_id(backend_id.clone()).build()
    ).await.map_err(|_| StaleRefError::NotFound(backend_id.clone()))?;

    let object_id = resolve_res.object.object_id.clone().ok_or_else(|| StaleRefError::NotFound(backend_id.clone()))?;

    // Check if stale before focusing
    let check_params = CallFunctionOnParams::builder()
        .object_id(object_id)
        .function_declaration("function() { return !this.isConnected; }")
        .return_by_value(true)
        .build()
        .unwrap();
    let check_res = page.execute(check_params).await.map_err(|e| StaleRefError::InteractionFailed(backend_id.clone(), e.to_string()))?;
    if let Some(val) = check_res.result.result.value {
        if val.as_bool() == Some(true) {
            return Err(StaleRefError::NotFound(backend_id));
        }
    }
    
    // Focus the element using CDP
    page.execute(
        FocusParams::builder().backend_node_id(backend_id.clone()).build()
    ).await.map_err(|_| StaleRefError::NotFound(backend_id.clone()))?;

    // Dispatch key events
    for c in text.chars() {
        let params = chromiumoxide::cdp::browser_protocol::input::DispatchKeyEventParams::builder()
            .r#type(chromiumoxide::cdp::browser_protocol::input::DispatchKeyEventType::Char)
            .text(c.to_string())
            .build()
            .unwrap();
        page.execute(params).await.map_err(|e| StaleRefError::InteractionFailed(backend_id.clone(), e.to_string()))?;
    }
    
    Ok(())
}

pub async fn screenshot_region(page: &Page, backend_id: BackendNodeId) -> Result<(String, f64, f64, f64, f64), StaleRefError> {
    tracing::info!("Screenshot region for ref: {:?}", backend_id);
    
    // Get box model to find the actual element bounds
    let box_model_params = GetBoxModelParams::builder().backend_node_id(backend_id.clone()).build();
    let box_model_res = page.execute(box_model_params).await.map_err(|e| StaleRefError::InteractionFailed(backend_id.clone(), e.to_string()))?;
    
    // The quad is an array of 8 numbers: [x1, y1, x2, y2, x3, y3, x4, y4]
    // representing the 4 corners of the box. 
    let quad_val = serde_json::to_value(&box_model_res.model.border).unwrap_or_default();
    println!("RAW DOM.getBoxModel border quad: {}", quad_val);

    let quad: Vec<f64> = serde_json::from_value(quad_val).unwrap_or_default();
    if quad.len() != 8 {
        return Err(StaleRefError::InteractionFailed(backend_id.clone(), "Invalid box model quad".to_string()));
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
        return Err(StaleRefError::InteractionFailed(backend_id.clone(), "Element has zero width or height".to_string()));
    }
    
    println!("COMPUTED CLIP PARAMS: x={}, y={}, width={}, height={}", x, y, width, height);
    
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
        
    let screenshot_res = page.execute(screenshot_params).await.map_err(|e| StaleRefError::InteractionFailed(backend_id.clone(), e.to_string()))?;
    
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
    use chromiumoxide::cdp::browser_protocol::page::{StartScreencastParams, EventScreencastFrame, ScreencastFrameAckParams, StartScreencastFormat};
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

    page.execute(params).await?;

    let mut stream = page.event_listener::<EventScreencastFrame>().await?;
    let page_clone = page.clone();

    tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            let session_id = event.session_id.clone();
            let data = event.data.clone();

            if tx.send((data.into(), event.metadata.device_width as i32)).is_err() {
                break;
            }

            let ack = ScreencastFrameAckParams::builder().session_id(session_id).build().unwrap();
            let _ = page_clone.execute(ack).await;
        }
    });

    Ok(())
}

/// Phase 16.2: live preview, v2.
///
/// Wraps [`start_screencast`] and immediately takes a synchronous
/// one-shot [`capture_screenshot`] so the caller has a frame
/// available *before* the first `EventScreencastFrame` arrives.
/// Without this, callers see a 0–500 ms blank gap between
/// "browser launched" and "first frame painted" — that gap is
/// what was making the Live Preview pane feel "late" relative
/// to the agent's first ReAct step.
///
/// The caller (typically the Tauri shell) pumps the
/// `UnboundedReceiver<(String, i32)>` and forwards every tuple
/// to the frontend as an `agent-screencast-frame` event. The
/// first item out of the channel is always the synchronous
/// `capture_screenshot` result, so the UI can paint a frame
/// the instant `start_screencast_with_first_frame` returns.
///
/// `max_width` and `max_height` are forwarded to the
/// `Page.startScreencast` params so the screencast matches the
/// Tauri window aspect ratio (the synchronous one-shot
/// screenshot uses the page's actual content size and may be
/// larger; the UI down-scales).
pub async fn start_screencast_with_first_frame(
    page: &Page,
    tx: tokio::sync::mpsc::UnboundedSender<(String, i32)>,
) -> Result<()> {
    // Synchronous first frame. Failure here is non-fatal: the
    // screencast stream will deliver a frame within a few hundred
    // ms anyway, and we don't want a transient screenshot
    // failure (e.g. page is still painting) to break the
    // entire live-preview pipeline.
    if let Ok(data) = capture_screenshot(page).await {
        // Best-effort: if the receiver is gone, the screencast
        // task below will also exit on the first send error.
        let _ = tx.send((data, 0));
    }

    start_screencast(page, tx).await
}

/// Sets the window bounds for the browser window associated with the given page.
pub async fn set_window_bounds(page: &Page, rect: WindowRect) -> Result<()> {
    use chromiumoxide::cdp::browser_protocol::browser::{
        GetWindowForTargetParams, SetWindowBoundsParams, Bounds,
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
        .quality(85)
        .build();
    let result = page
        .execute(params)
        .await
        .map_err(|e| anyhow::anyhow!("captureScreenshot failed: {e}"))?;
    Ok(result.data.clone().into())
}
