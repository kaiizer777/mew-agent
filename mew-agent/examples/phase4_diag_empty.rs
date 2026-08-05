// Phase 4 (Bug 4 fix): diagnostic — see exactly what happens
// between the navigate+settle and the first extract_tree call.
// Prints the raw AX node count from each call so we can see
// whether the tree is genuinely empty (0 nodes) or just has a
// missing root (nodes but no RootWebArea / no parent=None).
//
// Run with:
//   cargo run --example phase4_diag_empty -p mew-agent

use chromiumoxide::cdp::browser_protocol::accessibility::GetFullAxTreeParams;
use mew_agent::load_config;
use mew_cdp::{launch, navigate, wait_for_page_settled};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = load_config()?;
    let binary_path = config.browser.as_ref().and_then(|b| b.binary_path.clone());
    let (browser, page, handle, job) = launch(binary_path, false).await?;

    println!("[diag] navigating to example.com");
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        navigate(&page, "https://example.com"),
    )
    .await;
    println!("[diag] navigate done");

    // Wait for page settled (the fix).
    let settle = wait_for_page_settled(&page).await;
    println!("[diag] wait_for_page_settled: {}ms ({} polls, settled={})",
             settle.elapsed_ms, settle.polls, settle.settled);

    // Now poll the AX tree every 200ms for 5 seconds, counting
    // nodes each time. We want to see when (or if) the tree
    // populates.
    for i in 1..=10 {
        let raw = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            page.execute(GetFullAxTreeParams::default()),
        )
        .await;
        match raw {
            Ok(Ok(r)) => {
                let has_root = r.nodes.iter().any(|n| n.parent_id.is_none());
                let has_root_web_area = r.nodes.iter().any(|n| {
                    if let Some(role) = n.role.as_ref() {
                        if let Some(val) = role.value.as_ref() {
                            if let Some(s) = val.as_str() {
                                return s == "RootWebArea";
                            }
                        }
                    }
                    false
                });
                println!("[diag] poll {}/10: {} nodes, has_root={}, has_RootWebArea={}",
                         i, r.nodes.len(), has_root, has_root_web_area);
            }
            Ok(Err(e)) => println!("[diag] poll {}/10: error {}", i, e),
            Err(_) => println!("[diag] poll {}/10: timeout", i),
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    let _ = mew_cdp::shutdown(browser, handle, job).await;
    Ok(())
}
