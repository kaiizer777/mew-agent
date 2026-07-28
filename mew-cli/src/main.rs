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
    println!("1. Navigating to Wikipedia...");
    if let Err(e) = mew_cdp::navigate(&page, "https://www.wikipedia.org/").await {
        eprintln!("Navigation failed: {e}");
    }

    // Wait a bit for page to be fully interactive
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    println!("2. Extracting tree to find refs...");
    let (tree, ref_map, _) = match mew_perception::extract_tree(&page, true).await {
        Ok(res) => res,
        Err(e) => {
            eprintln!("Failed to extract tree: {e}");
            return Err(e.into());
        }
    };

    fn find_ref(node: &mew_perception::TreeNode, role: &str) -> Option<String> {
        if node.role.eq_ignore_ascii_case(role) {
            if let Some(r) = &node.ref_id {
                return Some(r.clone());
            }
        }
        for child in &node.children {
            if let Some(r) = find_ref(child, role) {
                return Some(r);
            }
        }
        None
    }

    let searchbox_ref = find_ref(&tree, "searchbox").or_else(|| find_ref(&tree, "combobox"));
    let mut search_button_backend = None;

    if let Some(r) = searchbox_ref {
        if let Some(backend_id) = ref_map.get(&r) {
            println!("3. Typing text into search box using ref {}...", r);
            if let Err(e) = mew_cdp::type_ref(&page, backend_id.clone(), "Rust programming language").await {
                eprintln!("Type ref failed: {}", e);
            }
        }
    } else {
        println!("Could not find searchbox ref in tree!");
    }

    // Small delay to make it visually clear
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // Find a button to click. Wikipedia has a search button
    let button_ref = find_ref(&tree, "button");
    if let Some(r) = button_ref {
        if let Some(backend_id) = ref_map.get(&r) {
            search_button_backend = Some(backend_id.clone());
            println!("4. Clicking search button using ref {}...", r);
            if let Err(e) = mew_cdp::click_ref(&page, backend_id.clone()).await {
                eprintln!("Click ref failed: {}", e);
            }
        }
    } else {
        println!("Could not find a button ref in tree to click!");
    }

    // Wait for search results
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

    println!("5. Scrolling down the results...");
    if let Err(e) = mew_cdp::scroll(&page, mew_cdp::ScrollDirection::Down, 800).await {
        eprintln!("Scroll failed: {e}");
    }

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    println!("6. Pressing 'PageDown' key...");
    if let Err(e) = mew_cdp::press_key(&page, "PageDown").await {
        eprintln!("Press key failed: {e}");
    }

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    println!("7. Testing deliberate failure on stale ref...");
    if let Some(backend_id) = search_button_backend {
        match mew_cdp::click_ref(&page, backend_id).await {
            Ok(_) => println!("WARNING: Click succeeded unexpectedly on a stale ref!"),
            Err(e) => println!("SUCCESS: Caught expected stale ref error: {}", e),
        }
    }

    println!("\n=== Step 3.1: Extracting Accessibility Tree ===");
    println!("1. Navigating to a complex page (e.g. GitHub login)...");
    if let Err(e) = mew_cdp::navigate(&page, "https://github.com/login").await {
        eprintln!("Navigation failed: {e}");
    }

    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

    println!("2. Extracting Accessibility Tree...");
    match mew_perception::extract_tree(&page, true).await {
        Ok((tree, _ref_map, duration)) => {
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
