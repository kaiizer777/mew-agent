use anyhow::Result;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::Page;
use futures::StreamExt;

pub const DEFAULT_PORT: u16 = 9222;

/// Launches a headed Chrome instance via CDP using chromiumoxide.
/// Configured with a fixed remote debugging port (9222) and persistent user data directory (`./profile`).
pub async fn launch() -> Result<(Browser, Page, tokio::task::JoinHandle<()>)> {
    let profile_dir = std::env::current_dir()?.join("profile");

    let config = BrowserConfig::builder()
        .with_head()
        .port(9222)
        .user_data_dir(profile_dir)
        .build()
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

    let page = browser.new_page("https://example.com").await?;

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
    
    let params = DispatchKeyEventParams::builder()
        .r#type(DispatchKeyEventType::KeyDown)
        .key(key)
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build key event: {}", e))?;
        
    page.execute(params).await
        .map_err(|e| anyhow::anyhow!("Failed to press key {}: {}", key, e))?;
        
    Ok(())
}

