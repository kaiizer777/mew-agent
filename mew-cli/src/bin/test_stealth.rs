use std::time::Duration;
use tokio::time::sleep;
use mew_cdp::launch;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("mew_cdp=info,chromiumoxide=trace")
        .try_init().ok();

    let config = mew_agent::load_config()?;

    // Test Stock Chrome
    println!("=== Testing STOCK Chrome (10 Navigations) ===");
    let (browser1, page1, handle1, job1) = launch(None, false).await?;

    for i in 1..=10 {
        page1.goto("https://en.wikipedia.org/wiki/Main_Page").await?.wait_for_navigation().await?;
        sleep(Duration::from_millis(500)).await;

        if let Ok(node) = page1.find_element("input[name='search']").await {
            if let Ok(dom_node) = node.description().await {
                println!("Stock Iteration {}: Search box BackendNodeId: {:?}", i, dom_node.backend_node_id);
            }
        }
    }
    let _ = mew_cdp::shutdown(browser1, handle1, job1).await;


    // Test Stealth Chrome
    println!("=== Testing STEALTH Chrome (10 Navigations) ===");
    let binary_path = config.browser.as_ref().and_then(|b| b.binary_path.clone());
    let (browser2, page2, handle2, job2) = launch(binary_path, false).await?;

    for i in 1..=10 {
        page2.goto("https://en.wikipedia.org/wiki/Main_Page").await?.wait_for_navigation().await?;
        sleep(Duration::from_millis(500)).await;

        if let Ok(node) = page2.find_element("input[name='search']").await {
            if let Ok(dom_node) = node.description().await {
                println!("Stealth Iteration {}: Search box BackendNodeId: {:?}", i, dom_node.backend_node_id);
            }
        }
    }
    let _ = mew_cdp::shutdown(browser2, handle2, job2).await;
    
    Ok(())
}
