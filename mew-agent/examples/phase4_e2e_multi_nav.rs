// Phase 4 (Bug 3 + Bug 4 fix): end-to-end multi-iteration test
// that exercises the fixed agent loop without the LLM in the
// loop. The test:
//
//   1. Launches a browser.
//   2. Navigates to page A.
//   3. Calls the fixed `wait_for_page_settled` + `extract_tree`.
//   4. Asserts the observation is non-trivial (>200 bytes) and
//      contains a recognizable content node.
//   5. Navigates to page B (a different site) — this exercises
//      the navigation detection + settle path on the *second*
//      navigation, not just the first.
//   6. Same assertions.
//   7. Shuts down.
//
// This is a faithful reproducer of the agent loop's perception
// path (sans the LLM). If it passes, the fix works for multi-step
// tasks. The Bug 3 fix is implicit: this example uses
// `mew_cdp::launch` directly (no Agent, no transcript). To
// confirm Bug 3 independently, see the build/launch behavior
// described in `phase4_evidence.md`.
//
// Run with:
//   cargo run --example phase4_e2e_multi_nav -p mew-agent
//
// (No URL arg; the URLs are hardcoded so the test is reproducible.)

use mew_agent::load_config;
use mew_cdp::{launch, navigate, wait_for_page_settled};

const PAGES: &[(&str, &str)] = &[
    ("https://example.com", "Example Domain"),
    ("https://en.wikipedia.org/wiki/Rust_(programming_language)", "Rust"),
];

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("[e2e] launching browser...");
    let config = load_config()?;
    let binary_path = config.browser.as_ref().and_then(|b| b.binary_path.clone());
    let (browser, page, handle, job) = launch(binary_path, false).await?;

    let mut failures: u32 = 0;
    for (idx, (url, must_contain)) in PAGES.iter().enumerate() {
        println!("\n[e2e] === nav {}/{}: {} ===", idx + 1, PAGES.len(), url);
        let nav = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            navigate(&page, url),
        )
        .await;
        match nav {
            Ok(Ok(_)) => println!("[e2e] navigation returned"),
            Ok(Err(e)) => eprintln!("[e2e] navigation error: {}", e),
            Err(_) => eprintln!("[e2e] navigation timeout"),
        }

        // The fixed settle path.
        let settle = wait_for_page_settled(&page).await;
        println!("[e2e] settled in {}ms ({} polls, settled={})",
                 settle.elapsed_ms, settle.polls, settle.settled);

        let obs = match mew_perception::extract_tree(&page, true).await {
            Ok((root, _refs, _dur)) => mew_perception::diff::serialize_full_tree(&root),
            Err(e) => {
                println!("[e2e] FAIL: extract_tree error: {}", e);
                failures += 1;
                continue;
            }
        };
        let bytes = obs.len();
        let contains = obs.contains(must_contain);
        println!("[e2e] observation: {} bytes, contains {:?}: {}",
                 bytes, must_contain, contains);
        if bytes < 200 {
            println!("[e2e] FAIL: observation too small (<200 bytes)");
            failures += 1;
        }
        if !contains {
            println!("[e2e] FAIL: observation missing expected content");
            failures += 1;
        }
    }

    let _ = mew_cdp::shutdown(browser, handle, job).await;
    if failures == 0 {
        println!("\n[e2e] ALL OK ({} navigations, 0 failures)", PAGES.len());
        Ok(())
    } else {
        println!("\n[e2e] FAILED with {} failure(s)", failures);
        std::process::exit(1);
    }
}
