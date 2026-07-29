//! Phase 16.2 — single-run cursor visibility test.
//!
//! Run once with `--on` to exercise the cursor path on a real page,
//! run once with `--off` to confirm the no-op behavior. Designed to be
//! invoked twice in two separate processes so the browser state is
//! guaranteed clean between runs.
//!
//! Usage:
//!   cargo run --example test_visible_cursor_runtime -p mew-cdp -- --on
//!   cargo run --example test_visible_cursor_runtime -p mew-cdp -- --off
//!
//! Exits 0 on success, non-zero on any check failure.

use std::time::Duration;

use anyhow::Result;
use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::dom::BackendNodeId;
use mew_cdp::{click_ref, launch, shutdown};

fn parse_args() -> Result<bool> {
    let args: Vec<String> = std::env::args().collect();
    for a in &args[1..] {
        match a.as_str() {
            "--on" => return Ok(true),
            "--off" => return Ok(false),
            _ => {}
        }
    }
    anyhow::bail!("usage: test_visible_cursor_runtime --on|--off")
}

fn read_browser_binary_path() -> Option<String> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let p = dir.join("config.yaml");
        if p.exists() {
            let s = std::fs::read_to_string(&p).ok()?;
            let mut in_browser = false;
            for line in s.lines() {
                let trimmed = line.trim_start();
                if trimmed.starts_with('#') || trimmed.is_empty() { continue; }
                if line.starts_with("browser:") { in_browser = true; continue; }
                if in_browser {
                    if !line.starts_with(' ') && !line.starts_with('\t') { break; }
                    if let Some(rest) = trimmed.strip_prefix("binary_path:") {
                        return Some(rest.trim().trim_matches('"').to_string());
                    }
                }
            }
            return None;
        }
        if !dir.pop() { break; }
    }
    None
}

async fn exists(page: &Page, expr: &str) -> bool {
    // Note: `value()` returns Some for both `true` and `false` boolean
    // results. We need to check the *actual* boolean, not just whether
    // a value came back.
    page.evaluate(expr).await
        .ok()
        .and_then(|r| r.value().and_then(|v| v.as_bool()))
        .unwrap_or(false)
}

#[tokio::main]
async fn main() -> Result<()> {
    let on = parse_args()?;
    let binary_path = read_browser_binary_path();
    let label = if on { "ON" } else { "OFF" };
    println!("=== Phase 16.2 runtime check (flag = {label}) ===");
    println!("[INFO] binary_path = {:?}", binary_path);

    let (browser, page, handle) = launch(binary_path.clone(), on).await?;

    // Use a data: URL with a unique-per-run fragment so we never hit a
    // cached page from a prior run sharing the same profile.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let body = format!(
        r#"<html><body style='margin:0;background:%23f5f5f7;'><span id=nonce>{}</span><button id=b style='margin:50px;padding:20px;'>Click me</button><div id=out></div><script>document.getElementById('b').addEventListener('click',function(){{document.getElementById('out').textContent='CLICKED';}});</script></body></html>"#,
        nonce
    );
    // Encode manually for data: URL (only safe chars used in body).
    let data_url = format!("data:text/html,{}", body
        .replace(' ', "%20")
        .replace('"', "%22")
        .replace('#', "%23")
        .replace('{', "%7B")
        .replace('}', "%7D")
        .replace('<', "%3C")
        .replace('>', "%3E")
    );
    page.goto(&data_url).await?.wait_for_navigation().await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    println!("[INFO] page nonce = {nonce}");

    let api_present = exists(&page, "!!(window.__mewCursor && window.__mewCursor.moveTo)").await;
    let dom_present = exists(&page, "!!document.getElementById('__mew-cursor')").await;
    let ripple_present = exists(&page, "!!document.getElementById('__mew-cursor-ripple')").await;

    // The CLICK is the real test. Use the same path the agent uses.
    let btn = page.find_element("#b").await?;
    let desc = btn.description().await?;
    let backend_id: BackendNodeId = desc.backend_node_id;
    click_ref(&page, backend_id).await?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let clicked = exists(&page, "document.getElementById('out').textContent === 'CLICKED'").await;

    // Stealth patch should ALWAYS be in place regardless of cursor flag.
    let webdriver_hidden = page.evaluate("navigator.webdriver === false").await.ok().and_then(|r| r.value().and_then(|v| v.as_bool())).unwrap_or(false);

    println!("[{}] api present          = {}", label, api_present);
    println!("[{}] cursor dom present   = {}", label, dom_present);
    println!("[{}] ripple dom present   = {}", label, ripple_present);
    println!("[{}] real click landed    = {}", label, clicked);
    println!("[{}] stealth webdriver    = {}", label, webdriver_hidden);

    let mut pass = true;
    if on {
        if !api_present { eprintln!("FAIL: cursor should be present when flag on"); pass = false; }
        if !dom_present { eprintln!("FAIL: cursor DOM should be present when flag on"); pass = false; }
        if !clicked { eprintln!("FAIL: real click did not register"); pass = false; }
        if !webdriver_hidden { eprintln!("FAIL: stealth patch missing"); pass = false; }
    } else {
        if api_present { eprintln!("FAIL: cursor API should NOT be present when flag off"); pass = false; }
        if dom_present { eprintln!("FAIL: cursor DOM should NOT be present when flag off"); pass = false; }
        if ripple_present { eprintln!("FAIL: ripple DOM should NOT be present when flag off"); pass = false; }
        if !clicked { eprintln!("FAIL: real click did not register (off mode)"); pass = false; }
        if !webdriver_hidden { eprintln!("FAIL: stealth patch missing (off mode)"); pass = false; }
    }

    let _ = shutdown(browser, handle).await;

    // ----------------------------------------------------------------------
    // Multi-page session (only when flag is on). This is the "watch the
    // actual window" bullet: navigate to two more pages and confirm the
    // cursor is re-injected on each one, plus that a real click still
    // lands on the second page. This proves `addScriptToEvaluateOnNewDocument`
    // is re-injecting per navigation, not just showing up on the first
    // page by luck.
    // ----------------------------------------------------------------------
    if on {
        println!("\n--- Multi-page session (bullet 1+2) ---");
        let (browser2, page2, handle2) = launch(binary_path.clone(), on).await?;

        // Page 1: data URL.
        let nonce1 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p1 = format!("data:text/html,<html><body><span id=n>{}</span><button id=b1>btn1</button><div id=o1></div><script>document.getElementById('b1').onclick=function(){{document.getElementById('o1').textContent='A';}};</script></body></html>", nonce1);
        page2.goto(&p1).await?.wait_for_navigation().await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let p1_api = exists(&page2, "!!(window.__mewCursor && window.__mewCursor.moveTo)").await;
        let p1_dom = exists(&page2, "!!document.getElementById('__mew-cursor')").await;
        let btn1 = page2.find_element("#b1").await?;
        let d1 = btn1.description().await?;
        click_ref(&page2, d1.backend_node_id).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let p1_clicked = exists(&page2, "document.getElementById('o1').textContent === 'A'").await;
        println!("[PAGE1] api={} dom={} click landed={}", p1_api, p1_dom, p1_clicked);

        // Page 2: navigate again. The script should be re-injected.
        let nonce2 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p2 = format!("data:text/html,<html><body><span id=n>{}</span><button id=b2>btn2</button><div id=o2></div><script>document.getElementById('b2').onclick=function(){{document.getElementById('o2').textContent='B';}};</script></body></html>", nonce2);
        page2.goto(&p2).await?.wait_for_navigation().await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let p2_api = exists(&page2, "!!(window.__mewCursor && window.__mewCursor.moveTo)").await;
        let p2_dom = exists(&page2, "!!document.getElementById('__mew-cursor')").await;
        let btn2 = page2.find_element("#b2").await?;
        let d2 = btn2.description().await?;
        click_ref(&page2, d2.backend_node_id).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let p2_clicked = exists(&page2, "document.getElementById('o2').textContent === 'B'").await;
        println!("[PAGE2] api={} dom={} click landed={}", p2_api, p2_dom, p2_clicked);

        let multi_pass = p1_api && p1_dom && p1_clicked && p2_api && p2_dom && p2_clicked;
        if multi_pass {
            println!("[OK] multi-page session — cursor re-injected on every navigation, real clicks land on each page");
        } else {
            eprintln!("[FAIL] multi-page session had a regression");
            pass = false;
        }

        let _ = shutdown(browser2, handle2).await;
    }

    if pass {
        println!("\n[OK] all checks passed for flag = {label}");
        Ok(())
    } else {
        Err(anyhow::anyhow!("16.2 check failed for flag = {label}"))
    }
}
