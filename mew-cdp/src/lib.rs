use anyhow::Result;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::dom::{BackendNodeId, FocusParams, ResolveNodeParams};
use chromiumoxide::cdp::js_protocol::runtime::CallFunctionOnParams;
use futures::StreamExt;
use thiserror::Error;
use chromiumoxide::cdp::browser_protocol::dom::GetBoxModelParams;
use chromiumoxide::cdp::browser_protocol::page::{CaptureScreenshotParams, CaptureScreenshotFormat, Viewport};

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
pub async fn launch(binary_path: Option<String>, visible_cursor: bool) -> Result<(Browser, Page, tokio::task::JoinHandle<()>)> {
    let profile_dir = std::env::current_dir()?.join("profile");

    let mut config_builder = BrowserConfig::builder()
        .with_head()
        .port(9222)
        .window_size(1280, 800)
        .user_data_dir(profile_dir);

    if let Some(path) = binary_path {
        config_builder = config_builder.chrome_executable(path);
    }

    let config = config_builder.build()
        .map_err(|e| anyhow::anyhow!("Failed to build BrowserConfig: {e}"))?;

    tracing::info!("Launching headed Chrome on remote debugging port 9222...");

    let (browser, mut handler) = Browser::launch(config).await?;

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

    Ok((browser, page, handle))
}

/// Cleanly closes the browser instance over CDP.
pub async fn shutdown(mut browser: Browser, handler_task: tokio::task::JoinHandle<()>) -> Result<()> {
    tracing::info!("Closing browser cleanly via CDP...");
    let close_res = browser.close().await;
    
    // Wait for the event loop to finish (which happens when the websocket disconnects / browser exits)
    let _ = handler_task.await;

    // Asynchronously wait for the spawned chromium instance to exit completely
    // to avoid zombie processes and the Drop warning.
    let _ = browser.wait().await;
    
    // Check if the close actually succeeded
    close_res?;

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
