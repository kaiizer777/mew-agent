use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .try_init();

    println!("=== Step 0: API Smoke Test ===");
    let config = mew_agent::load_config()?;
    mew_agent::smoke_test(&config).await?;
    println!("API Smoke Test Passed!");

    println!("\n=== Step 1.1: Launching Visible Chrome via CDP ===");
    println!("Launching headed Chrome on remote debugging port {}...", mew_cdp::DEFAULT_PORT);

    let (browser, page, handler_task) = match mew_cdp::launch().await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Failed to launch Chrome: {e}");
            return Err(e.into());
        }
    };

    println!("Chrome process launched successfully!");
    println!("CDP listening on port: 9222");

    if let Ok(url) = page.url().await {
        println!("Page active and navigated to: {:?}", url);
    }

    println!("\n=== Step 2.1: Testing Action Primitives ===");
    println!("1. Navigating to DuckDuckGo...");
    if let Err(e) = mew_cdp::navigate(&page, "https://duckduckgo.com/").await {
        eprintln!("Navigation failed: {e}");
    }

    // Wait a bit for page to be fully interactive
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    println!("2. Typing text into search box...");
    if let Err(e) = mew_cdp::type_text(&page, "input[name='q']", "Rust programming language").await {
        eprintln!("Type text failed: {e}");
    }

    // Small delay to make it visually clear
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    println!("3. Clicking search button...");
    if let Err(e) = mew_cdp::click_selector(&page, "button[type='submit']").await {
        eprintln!("Click selector failed: {e}");
    }

    // Wait for search results
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

    println!("4. Scrolling down the results...");
    if let Err(e) = mew_cdp::scroll(&page, mew_cdp::ScrollDirection::Down, 800).await {
        eprintln!("Scroll failed: {e}");
    }

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    println!("5. Pressing 'PageDown' key...");
    if let Err(e) = mew_cdp::press_key(&page, "PageDown").await {
        eprintln!("Press key failed: {e}");
    }

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    println!("6. Testing deliberate failure on non-existent selector...");
    match mew_cdp::click_selector(&page, "button#this-id-surely-does-not-exist-123").await {
        Ok(_) => println!("WARNING: Click succeeded unexpectedly on a non-existent selector!"),
        Err(e) => println!("SUCCESS: Caught expected error: {}", e),
    }

    println!("\n=== Step 3.1: Extracting Accessibility Tree ===");
    println!("1. Navigating to a complex page (e.g. GitHub login)...");
    if let Err(e) = mew_cdp::navigate(&page, "https://github.com/login").await {
        eprintln!("Navigation failed: {e}");
    }

    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

    println!("2. Extracting Accessibility Tree...");
    match mew_perception::extract_tree(&page, true).await {
        Ok((tree, duration)) => {
            println!("Extraction took: {:?}", duration);
            println!("--- Extracted Tree ---");
            tree.print(0);
            println!("----------------------");
        },
        Err(e) => {
            eprintln!("Failed to extract accessibility tree: {e}");
        }
    }

    println!("\nTest flow complete. Waiting 3 seconds before closing...");
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

    println!("Shutting down browser cleanly...");
    if let Err(e) = mew_cdp::shutdown(browser, handler_task).await {
        eprintln!("Error during browser shutdown: {e}");
    } else {
        println!("Browser process closed cleanly.");
    }

    Ok(())
}
