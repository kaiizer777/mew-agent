// Phase 4 (Bug 4 fix): post-fix reproducer. This is the
// complement to `phase4_bug4_repro.rs` — that one mirrors the
// *pre-fix* agent loop (2s fixed sleep after wait_for_navigation);
// this one mirrors the *post-fix* agent loop
// (`wait_for_page_settled` after wait_for_navigation).
//
// Both reproducers navigate to the same URL (default: a heavy
// GitHub page). Side-by-side they prove the fix:
//
//   - Pre-fix:  171-byte observation, root only, child is
//               `ignored/uninteresting`, `busy: true` in the
//               properties — model has nothing to act on.
//   - Post-fix: multi-thousand-byte observation, real nodes,
//               refs, links, headings, etc. — model can act.
//
// Run with:
//   cargo run --example phase4_bug4_post_fix -p mew-agent -- <url>
//
// Default URL is https://github.com/tokio-rs/tokio. Override with
// e.g. -- https://example.com to confirm both reproducers behave
// correctly on a simple static page (settles in ~0ms, observation
// has the page content).

use mew_agent::load_config;
use mew_cdp::{launch, wait_for_page_settled};
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://github.com/tokio-rs/tokio".to_string());
    println!("[post-fix] target url: {}", url);

    let config = load_config()?;
    let binary_path = config.browser.as_ref().and_then(|b| b.binary_path.clone());

    let out_dir = std::path::PathBuf::from("tests-output").join("phase4_bug4_post_fix");
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

    println!("[post-fix] navigating...");
    let nav_result = tokio::time::timeout(
        Duration::from_secs(20),
        page.goto(&url),
    )
    .await;
    match nav_result {
        Ok(Ok(_)) => {
            let _ = page.wait_for_navigation().await;
            println!("[post-fix] navigation returned");
        }
        Ok(Err(e)) => {
            eprintln!("[post-fix] navigation error (continuing): {}", e);
        }
        Err(_) => {
            eprintln!("[post-fix] navigation timeout (continuing)");
        }
    }

    // POST-FIX: bounded DOM-content poll, replaces the 2s fixed
    // sleep. This is the function the agent loop now calls after
    // every successful navigation.
    println!("[post-fix] calling wait_for_page_settled (replaces 2s sleep)...");
    let settle = wait_for_page_settled(&page).await;
    println!(
        "[post-fix] settled in {}ms ({} polls, settled={})",
        settle.elapsed_ms, settle.polls, settle.settled
    );

    match mew_perception::extract_tree(&page, true).await {
        Ok((root, ref_map, dur)) => {
            println!(
                "[post-fix] extract_tree OK in {:?} (root role={}, name={:?}, refs={}, children={})",
                dur,
                root.role,
                root.name,
                ref_map.len(),
                root.children.len()
            );
            let obs = mew_perception::diff::serialize_full_tree(&root);
            let _ = std::fs::write(run_dir.join("observation.txt"), &obs);
            println!("[post-fix] observation bytes: {}", obs.len());
            // Print the first 20 lines so the user can read what
            // the agent now sees (vs. the pre-fix 171-byte empty
            // tree).
            println!("[post-fix] --- first 20 lines of observation ---");
            for line in obs.lines().take(20) {
                println!("[post-fix]   {}", line);
            }
            println!("[post-fix] --- end ---");
        }
        Err(e) => {
            println!("[post-fix] extract_tree FAILED: {}", e);
        }
    }

    let _ = mew_cdp::shutdown(browser, handle, job).await;
    println!("[post-fix] done. artifacts in {}", run_dir.display());
    Ok(())
}
