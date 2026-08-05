// Phase 4 (Bug 4 fix): pre-fix reproducer for the "no root found"
// perception failure on JS-heavy pages.
//
// Mirrors the agent loop's perception path exactly as it existed
// before the fix:
//
//   1. mew_cdp::navigate(page, url) -> page.goto().wait_for_navigation()
//   2. tokio::time::sleep(2s)  (the fixed sleep in agent.rs)
//   3. mew_perception::extract_tree(page, true)
//
// On a JS-heavy page (e.g. github.com) the AX tree returned at step
// 3 can be near-empty or have no root, even though the page has fully
// loaded. extract_tree then errors with
// `Failed to build tree: no root found`, and the loop substitutes the
// "Error: Failed to load page state" RootWebArea placeholder. The user
// sees a 70-byte observation and the model has nothing to act on.
//
// What this reproducer does:
//   - Navigates to a target URL (default: github.com).
//   - Tries extract_tree, dumping the raw CDP `nodes.len()` and the
//     first error if any, plus observation byte count.
//   - Tries up to 4 attempts with a 500ms gap to see if the tree
//     self-heals (it usually doesn't on the early ones).
//   - Writes the raw CDP `GetFullAxTree` response JSON to disk so we
//     can inspect exactly what the browser returned.
//
// Run with:
//   cargo run --example phase4_bug4_repro -p mew-agent -- <url>
//
// Default URL is github.com. Override with e.g. -- https://example.com
// to confirm the failure is page-specific (example.com should pass).

use mew_agent::load_config;
use mew_cdp::launch;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://github.com".to_string());
    println!("[repro] target url: {}", url);

    let config = load_config()?;
    let binary_path = config.browser.as_ref().and_then(|b| b.binary_path.clone());

    // All artifacts land in tests-output/phase4_bug4_repro/ so the
    // project root stays clean. The folder is gitignored.
    let out_dir = std::path::PathBuf::from("tests-output").join("phase4_bug4_repro");
    let _ = std::fs::create_dir_all(&out_dir);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let safe_url = url
        .replace("https://", "")
        .replace("http://", "")
        .replace(['/', '.', ':'], "_");
    let run_dir = out_dir.join(format!("{}_{}", stamp, safe_url));
    let _ = std::fs::create_dir_all(&run_dir);

    let (browser, page, handle, job) = launch(binary_path, false).await?;

    // 1. navigate + wait_for_navigation (matches mew_cdp::navigate).
    println!("[repro] navigating...");
    let nav_result = tokio::time::timeout(
        Duration::from_secs(20),
        page.goto(&url),
    )
    .await;
    match nav_result {
        Ok(Ok(_)) => {
            let _ = page.wait_for_navigation().await;
            println!("[repro] navigation returned");
        }
        Ok(Err(e)) => {
            eprintln!("[repro] navigation error (continuing): {}", e);
        }
        Err(_) => {
            eprintln!("[repro] navigation timeout (continuing)");
        }
    }

    // 2. The 2s fixed sleep currently in agent.rs (line ~1286 of
    //    pre-fix code). This is the line the fix removes/replaces.
    println!("[repro] sleeping 2s (matches pre-fix agent.rs fixed sleep)...");
    sleep(Duration::from_secs(2)).await;

    // 3. extract_tree, exactly as the loop calls it. Capture both
    //    the raw CDP response and the observation byte count.
    for attempt in 1..=4 {
        println!("[repro] --- attempt {} ---", attempt);
        let raw = match tokio::time::timeout(
            Duration::from_secs(10),
            page.execute(chromiumoxide::cdp::browser_protocol::accessibility::GetFullAxTreeParams::default()),
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                println!("[repro] CDP execute error: {}", e);
                sleep(Duration::from_millis(500)).await;
                continue;
            }
            Err(_) => {
                println!("[repro] CDP execute timeout");
                sleep(Duration::from_millis(500)).await;
                continue;
            }
        };

        // Dump the raw CDP response as JSON so we can see what the
        // browser actually returned (node count, root role, etc).
        // `raw` doesn't impl Serialize directly, so we serialize
        // node-level fields we care about into a JSON object.
        let raw_value = serde_json::json!({
            "nodes": raw.nodes,
        });
        let raw_json = serde_json::to_string_pretty(&raw_value).unwrap_or_default();
        let raw_path = run_dir.join(format!("raw_ax_response_attempt{}.json", attempt));
        let _ = std::fs::write(&raw_path, &raw_json);
        println!("[repro] raw CDP response: {} nodes, dumped to {}",
                 raw.nodes.len(), raw_path.display());
        if raw.nodes.is_empty() {
            println!("[repro] ZERO NODES returned by Accessibility.getFullAXTree");
        } else {
            // Find the node with no parent_id (the AX tree's root).
            let mut root_candidate: Option<&chromiumoxide::cdp::browser_protocol::accessibility::AxNode> = None;
            for n in &raw.nodes {
                if n.parent_id.is_none() {
                    root_candidate = Some(n);
                    break;
                }
            }
            match root_candidate {
                Some(n) => {
                    let role = n
                        .role
                        .as_ref()
                        .and_then(|v| v.value.as_ref())
                        .and_then(|j| j.as_str())
                        .unwrap_or("<no role>");
                    let name = n
                        .name
                        .as_ref()
                        .and_then(|v| v.value.as_ref())
                        .and_then(|j| j.as_str())
                        .unwrap_or("<no name>");
                    println!("[repro] root candidate: role={:?} name={:?}", role, name);
                }
                None => {
                    println!("[repro] NO node has parent_id=None -> build_tree will fail");
                }
            }
        }

        // Now call the production perception path to see if it
        // builds a tree from the same payload.
        match mew_perception::extract_tree(&page, true).await {
            Ok((root, ref_map, dur)) => {
                println!(
                    "[repro] extract_tree OK in {:?} (root role={}, name={:?}, refs={}, children={})",
                    dur,
                    root.role,
                    root.name,
                    ref_map.len(),
                    root.children.len()
                );
                // Dump the serialized observation so we can read the
                // exact bytes the LLM would have seen.
                let obs = mew_perception::diff::serialize_full_tree(&root);
                let _ = std::fs::write(run_dir.join(format!("observation_attempt{}.txt", attempt)), &obs);
                println!("[repro] observation bytes: {}", obs.len());
                if attempt == 1 {
                    println!("[repro] EARLY EXIT on first success (debug)");
                    break;
                }
            }
            Err(e) => {
                println!("[repro] extract_tree FAILED: {}", e);
            }
        }

        if attempt < 4 {
            sleep(Duration::from_millis(500)).await;
        }
    }

    let _ = mew_cdp::shutdown(browser, handle, job).await;
    println!("[repro] done. artifacts in {}", run_dir.display());
    Ok(())
}
