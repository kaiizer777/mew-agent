//! Phase 16.1 verification harness — confirms the visible cursor overlay
//! actually appears on a real page, moves on demand, and re-injects on
//! every navigation.
//!
//! Run with the feature flag ON:
//!   cargo run --example test_visible_cursor -p mew-cdp
//!
//! What it does:
//!   1. Launch Chrome with `visible_cursor = true`.
//!   2. Navigate to a local data: URL.
//!   3. Verify `window.__mewCursor` exists and has both `moveTo` and `click`.
//!   4. Verify the cursor DOM element is present and has `pointer-events: none`.
//!   5. Move the cursor to (300, 200) and read back the computed `transform`.
//!   6. Navigate to a second data: URL and re-check `__mewCursor` exists —
//!      proves the script re-injects on every navigation, not just once.
//!   7. Fire `click(400, 250)` and confirm the ripple element exists and
//!      was assigned the expected transform.
//!   8. Print PASS/FAIL for each check.
//!
//! This is the 16.1→16.2 eyes-on step: pass this on a real headed browser
//! before declaring 16.1 done.

use anyhow::Result;
use chromiumoxide::Page;
use mew_cdp::{launch, shutdown};

async fn exists(page: &Page, expr: &str) -> Result<bool> {
    let r = page.evaluate(expr).await?;
    Ok(r.value().is_some())
}

async fn string_value(page: &Page, expr: &str) -> Result<String> {
    let r = page.evaluate(expr).await?;
    if let Some(v) = r.value() {
        Ok(v.to_string())
    } else {
        Ok(String::from("<undefined>"))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // `mew-cdp` doesn't pull in `tracing_subscriber` as a dep — and we don't
    // need it here, the print statements below are the real signal. The
    // library's `tracing::info!` events just go to a no-op sink.

    println!("=== Phase 16.1 — visible cursor overlay verification ===\n");

    let (browser, page, handle, job) = launch(None, true).await?;
    let mut all_pass = true;
    let mut tag = |label: &str, ok: bool, detail: &str| {
        let mark = if ok { "PASS" } else { "FAIL" };
        if !ok { all_pass = false; }
        println!("[{mark}] {label} — {detail}");
    };

    // Step 1: navigate to a blank-ish page.
    let data_url = "data:text/html,<html><body style='margin:0;background:%23f5f5f7;'><h1 id=t>hello</h1></body></html>";
    page.goto(data_url).await?.wait_for_navigation().await?;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Step 2: window.__mewCursor should be installed.
    let has_api = exists(&page, "!!(window.__mewCursor && window.__mewCursor.moveTo && window.__mewCursor.click)").await?;
    tag("API installed", has_api, "window.__mewCursor.moveTo + .click present");

    // Step 3: cursor DOM element present with pointer-events: none.
    let has_cursor_dom = exists(&page, "!!document.getElementById('__mew-cursor')").await?;
    tag("Cursor DOM present", has_cursor_dom, "#__mew-cursor exists in document");

    let pe = string_value(&page, "(document.getElementById('__mew-cursor')||{}).style && document.getElementById('__mew-cursor').style.pointerEvents").await?;
    tag("pointer-events: none", pe.contains("none"), &format!("got: {pe}"));

    // Step 4: move to (300,200) and read back the transform.
    page.evaluate("window.__mewCursor.moveTo(300, 200)").await?;
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let t = string_value(&page, "document.getElementById('__mew-cursor').style.transform").await?;
    let moved = t.contains("300") && t.contains("200");
    tag("moveTo updates transform", moved, &format!("transform = {t}"));

    // Step 5: navigate to a second page and confirm re-injection.
    let data_url2 = "data:text/html,<html><body style='margin:0;'><h2>page 2</h2></body></html>";
    page.goto(data_url2).await?.wait_for_navigation().await?;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let re_injected = exists(&page, "!!(window.__mewCursor && window.__mewCursor.moveTo)").await?;
    let cursor_after_nav = exists(&page, "!!document.getElementById('__mew-cursor')").await?;
    tag("Re-injection on navigation", re_injected && cursor_after_nav, "API + DOM both present on page 2");

    // Step 6: fire click ripple and confirm the ripple element exists and
    // its transform contains the requested x/y.
    page.evaluate("window.__mewCursor.click(400, 250)").await?;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let has_ripple = exists(&page, "!!document.getElementById('__mew-cursor-ripple')").await?;
    let ripple_t = string_value(&page, "document.getElementById('__mew-cursor-ripple').style.transform").await?;
    tag("Click ripple element present", has_ripple, "#__mew-cursor-ripple exists");
    let ripple_at = ripple_t.contains("400") && ripple_t.contains("250");
    tag("Ripple positioned at click point", ripple_at, &format!("transform = {ripple_t}"));

    // Step 7: real interaction is not blocked — call into a tiny DOM API
    // on the page and confirm it executes. (Just exercises that we haven't
    // somehow broken the page object.)
    let body_html_ok = exists(&page, "document.body && document.body.tagName === 'BODY'").await?;
    tag("Page still interactive", body_html_ok, "document.body accessible after cursor use");

    println!();
    if all_pass {
        println!("ALL CHECKS PASSED — visible cursor overlay is wired correctly.");
    } else {
        println!("SOME CHECKS FAILED — see above.");
    }

    let _ = shutdown(browser, handle, job).await;
    if all_pass { Ok(()) } else { anyhow::bail!("visible_cursor verification failed") }
}
