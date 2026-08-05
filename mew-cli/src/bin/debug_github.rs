use mew_agent::load_config;
use mew_cdp::launch;
use mew_perception::extract_tree;
use tokio::time::sleep;
use std::time::Duration;
use chromiumoxide::cdp::browser_protocol::accessibility::GetFullAxTreeParams;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("mew_cdp=info")
        .try_init().ok();
        
    let config = load_config()?;
    let binary_path = config.browser.as_ref().and_then(|b| b.binary_path.clone());

    // All debug artifacts land in tests-output/debug_github/ so the
    // project root stays clean. The folder is gitignored.
    let out_dir = std::path::PathBuf::from("tests-output").join("debug_github");
    let _ = std::fs::create_dir_all(&out_dir);

    let (browser, page, handle, job) = launch(binary_path, false).await?;
    
    println!("Navigating to https://github.com...");
    let _ = tokio::time::timeout(
        Duration::from_secs(15),
        page.goto("https://github.com/")
    ).await;
    
    println!("goto returned. Sleeping 5s...");
    sleep(Duration::from_secs(5)).await;
    
    println!("Taking screenshot...");
    let screenshot_data = page.pdf(chromiumoxide::cdp::browser_protocol::page::PrintToPdfParams::default()).await;
    if let Ok(data) = screenshot_data {
        let _ = std::fs::write(out_dir.join("github_screenshot.pdf"), data);
    }
    
    println!("Executing GetFullAxTreeParams...");
    let ax_res_result = tokio::time::timeout(
        Duration::from_secs(30),
        page.execute(GetFullAxTreeParams::default())
    ).await;
    
    let ax_res = match ax_res_result {
        Ok(Ok(res)) => res,
        Ok(Err(e)) => {
            println!("Error executing AxTree: {:?}", e);
            return Ok(());
        }
        Err(_) => {
            println!("Timeout executing AxTree.");
            return Ok(());
        }
    };
    println!("Ax tree extracted.");
    
    let (tree, ref_map) = mew_perception::build_tree(ax_res.nodes.clone(), false).unwrap();
    
    // Part 1 complete. Dump tree to string.
    fn tree_to_string(node: &mew_perception::TreeNode, depth: usize, out: &mut String) {
        let indent = "  ".repeat(depth);
        let mut parts = vec![format!("[{:?}]", node.category), node.role.clone()];
        if let Some(r) = &node.ref_id {
            parts.push(format!("ref: {}", r));
        }
        if !node.name.is_empty() {
            parts.push(format!("name: {:?}", node.name));
        }
        if !node.value.is_empty() {
            parts.push(format!("value: {:?}", node.value));
        }
        out.push_str(&format!("{}{}\n", indent, parts.join(" | ")));
        
        for child in &node.children {
            tree_to_string(child, depth + 1, out);
        }
    }
    
    let mut tree_str = String::new();
    tree_to_string(&tree, 0, &mut tree_str);
    std::fs::write(out_dir.join("tree_dashboard.txt"), &tree_str)?;
    
    println!("--- GREP RESULTS FOR SNAPSHOT 1 ---");
    for line in tree_str.lines() {
        let l = line.to_lowercase();
        if l.contains("new") || l.contains("create") || l.contains("repo") || l.contains("+") {
            println!("{}", line);
        }
    }
    
    // Search for button
    fn find_target(node: &mew_perception::TreeNode) -> Option<String> {
        let lower = node.name.to_lowercase();
        if (lower.contains("create new") || lower == "new" || lower.contains("new repo")) 
            && (node.role == "button" || node.role == "link" || node.role == "menuitem") {
            if node.ref_id.is_some() {
                return node.ref_id.clone();
            }
        }
        for child in &node.children {
            if let Some(r) = find_target(child) {
                return Some(r);
            }
        }
        None
    }
    
    if let Some(target_ref) = find_target(&tree) {
        println!("Found target to click: {}", target_ref);
        if let Some(backend_id) = ref_map.get(&target_ref) {
            println!("Clicking backend id {:?}", backend_id);
            let _ = mew_cdp::click_ref(&page, backend_id.clone()).await;
            println!("Clicked! Sleeping 5s...");
            sleep(Duration::from_secs(5)).await;
            
            // Extract second tree
            let ax_res_result2 = tokio::time::timeout(
                Duration::from_secs(30),
                page.execute(GetFullAxTreeParams::default())
            ).await;
            
            if let Ok(Ok(res2)) = ax_res_result2 {
                let (tree2, _ref_map2) = mew_perception::build_tree(res2.nodes.clone(), false).unwrap();
                let mut tree_str2 = String::new();
                tree_to_string(&tree2, 0, &mut tree_str2);
                std::fs::write(out_dir.join("tree_after_click.txt"), &tree_str2)?;
                println!("--- GREP RESULTS FOR SNAPSHOT 2 ---");
                for line in tree_str2.lines() {
                    let l = line.to_lowercase();
                    if l.contains("new") || l.contains("create") || l.contains("repo") || l.contains("+") {
                        println!("{}", line);
                    }
                }
            } else {
                println!("Timeout/Error on second tree extraction.");
            }
        }
    } else {
        println!("No 'Create new' or 'New' interactive node found in first snapshot.");
    }
    
    mew_cdp::shutdown(browser, handle, job).await?;
    Ok(())
}
