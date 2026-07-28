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

/// Launches a headed Chrome instance via CDP using chromiumoxide.
/// Configured with a fixed remote debugging port (9222) and persistent user data directory (`./profile`).
pub async fn launch(binary_path: Option<String>) -> Result<(Browser, Page, tokio::task::JoinHandle<()>)> {
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
