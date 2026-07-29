use mew_cdp::{launch, shutdown, type_ref};
use mew_perception::extract_tree;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let (browser, page, handler_task) = launch(None, false).await?;
    
    // Create an HTML file that has an input text field and something else
    let html = r#"
<!DOCTYPE html>
<html>
<body>
  <h1>Hello</h1>
  <input type="text" id="i1" value="" oninput="if(this.value.includes('destroy')) this.remove();" />
</body>
</html>
    "#;
    
    let html_path = std::env::current_dir()?.join("test_type.html");
    std::fs::write(&html_path, html)?;
    let file_url = format!("file:///{}", html_path.to_string_lossy().replace('\\', "/"));
    
    page.goto(&file_url).await?.wait_for_navigation().await?;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    
    let (tree, ref_map, _) = extract_tree(&page, true).await?;
    println!("--- Initial Tree (Type Test) ---");
    tree.print(0);
    
    // Find the input text field ref
    let i1_ref = ref_map.keys().find(|r| {
        let mut found = false;
        fn search(node: &mew_perception::TreeNode, r: &str, f: &mut bool) {
            if node.ref_id.as_deref() == Some(r) && node.role.eq_ignore_ascii_case("textbox") { *f = true; }
            for child in &node.children { search(child, r, f); }
        }
        search(&tree, r, &mut found);
        found
    }).unwrap();
    
    let i1_id = ref_map.get(i1_ref).unwrap().clone();
    
    // Type into it
    println!("Typing 'hello ' into Textbox...");
    type_ref(&page, i1_id.clone(), "hello ").await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    
    // Verify it was typed
    let (tree2, _, _) = extract_tree(&page, true).await?;
    println!("--- Tree after typing 'hello ' ---");
    tree2.print(0);

    // Type 'destroy' which will remove it
    println!("Typing 'destroy' to trigger self-removal...");
    type_ref(&page, i1_id.clone(), "destroy").await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Verify it was removed
    let (tree3, _, _) = extract_tree(&page, true).await?;
    println!("--- Tree after typing 'destroy' ---");
    tree3.print(0);

    // Try typing into the same old ref
    println!("Attempting to type into the removed Textbox using the SAME old ref...");
    match type_ref(&page, i1_id.clone(), " more").await {
        Ok(_) => println!("WARNING: Type succeeded unexpectedly on a stale ref!"),
        Err(e) => println!("SUCCESS: Caught expected stale ref error: {:?}", e),
    }

    shutdown(browser, handler_task).await?;
    Ok(())
}
