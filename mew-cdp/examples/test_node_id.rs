use chromiumoxide::browser::{Browser, BrowserConfig};
// use chromiumoxide::cdp::browser_protocol::dom::Node;
use futures::StreamExt;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // tracing_subscriber::fmt()
    //     .with_max_level(tracing::Level::TRACE)
    //     .init();

    // Use stock chrome by not specifying binary_path
    let config = BrowserConfig::builder()
        .with_head()
        .build()
        .unwrap();

    let (mut browser, mut handler) = Browser::launch(config).await?;

    tokio::spawn(async move {
        while let Some(h) = handler.next().await {
            if h.is_err() {
                break;
            }
        }
    });

    let page = browser.new_page("about:blank").await?;
    
    for i in 1..=10 {
        println!("--- Iteration {} ---", i);
        page.goto("https://en.wikipedia.org/wiki/Main_Page").await?;
        page.wait_for_navigation_response().await?;
        // wait a bit for DOM to settle
        tokio::time::sleep(Duration::from_millis(1000)).await;
        
        // Find the search input
        if let Ok(node) = page.find_element("input[name='search']").await {
            // Get the backend node id
            if let Ok(dom_node) = node.description().await {
                println!("Iteration {}: Search box BackendNodeId: {:?}", i, dom_node.backend_node_id);
            }
        }
    }
    
    Ok(())
}
