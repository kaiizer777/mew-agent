use mew_cdp::{launch, shutdown, click_ref, StaleRefError};
use mew_perception::extract_tree;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let (browser, page, handler_task) = launch().await?;
    
    // Convert absolute path to file URI
    let html_path = std::env::current_dir()?.join("test.html");
    let file_url = format!("file:///{}", html_path.to_string_lossy().replace('\\', "/"));
    
    page.goto(&file_url).await?.wait_for_navigation().await?;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    
    let (tree, ref_map, _) = extract_tree(&page, true).await?;
    println!("--- Initial Tree ---");
    tree.print(0);
    
    // Find button 2 specifically
    let b2_ref = ref_map.keys().find(|r| {
        let b_id = ref_map.get(*r).unwrap();
        // Just find the button named "Button 2"
        let mut found = false;
        fn search(node: &mew_perception::TreeNode, r: &str, f: &mut bool) {
            if node.ref_id.as_deref() == Some(r) && node.name == "Button 2" { *f = true; }
            for child in &node.children { search(child, r, f); }
        }
        search(&tree, r, &mut found);
        found
    }).unwrap();
    
    println!("Found Button 2 with ref {}", b2_ref);
    let b2_id = ref_map.get(b2_ref).unwrap().clone();
    
    // Click button 2
    println!("Clicking Button 2...");
    click_ref(&page, b2_id).await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    
    // Verify it was clicked
    let (tree2, _, _) = extract_tree(&page, true).await?;
    println!("--- Tree after clicking Button 2 ---");
    tree2.print(0);

    // Now let's test stale ref handling on Button 1
    // We will extract tree again to get a fresh ref for button 1
    let (tree3, ref_map3, _) = extract_tree(&page, true).await?;
    let b1_ref = ref_map3.keys().find(|r| {
        let mut found = false;
        fn search(node: &mew_perception::TreeNode, r: &str, f: &mut bool) {
            if node.ref_id.as_deref() == Some(r) && node.name == "Button 1" { *f = true; }
            for child in &node.children { search(child, r, f); }
        }
        search(&tree3, r, &mut found);
        found
    }).unwrap();
    let b1_id = ref_map3.get(b1_ref).unwrap().clone();
    
    println!("Clicking Button 1 (which removes itself from DOM)...");
    click_ref(&page, b1_id.clone()).await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    
    println!("Attempting to click Button 1 again using the SAME old ref...");
    match click_ref(&page, b1_id).await {
        Ok(_) => println!("WARNING: Click succeeded unexpectedly on a stale ref!"),
        Err(e) => println!("SUCCESS: Caught expected stale ref error: {:?}", e),
    }

    shutdown(browser, handler_task).await?;
    Ok(())
}
