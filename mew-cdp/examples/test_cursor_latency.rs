//! Phase 16.2 — latency comparison: cursor ON vs OFF.
//!
//! Measures click-path latency (compute_element_center + move_cursor +
//! sleep + click_ref + ripple) vs the bare click_ref. Run with `--on`
//! and `--off` as separate processes so the browser is clean between
//! runs. Reports the median of 5 trials for each mode.

use std::time::{Duration, Instant};

use anyhow::Result;
use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::dom::BackendNodeId;
use mew_cdp::{click_ref, compute_element_center, launch, move_cursor_and_ripple, shutdown};

fn parse_args() -> Result<bool> {
    let args: Vec<String> = std::env::args().collect();
    for a in &args[1..] {
        match a.as_str() {
            "--on" => return Ok(true),
            "--off" => return Ok(false),
            _ => {}
        }
    }
    anyhow::bail!("usage: test_cursor_latency --on|--off")
}

fn median(v: &mut Vec<u128>) -> u128 {
    v.sort();
    let n = v.len();
    if n == 0 { return 0; }
    if n % 2 == 1 { v[n/2] } else { (v[n/2 - 1] + v[n/2]) / 2 }
}

#[tokio::main]
async fn main() -> Result<()> {
    let on = parse_args()?;
    let label = if on { "ON" } else { "OFF" };
    println!("=== Phase 16.2 latency check (flag = {label}) ===");

    let (browser, page, handle, job) = launch(None, on).await?;

    let data_url = "data:text/html,<html><body><button id=b>Click</button><div id=o></div><script>document.getElementById('b').onclick=function(){document.getElementById('o').textContent='C';};</script></body></html>";
    page.goto(data_url).await?.wait_for_navigation().await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let btn = page.find_element("#b").await?;
    let desc = btn.description().await?;
    let backend_id: BackendNodeId = desc.backend_node_id;

    const TRIALS: usize = 5;
    let mut bare_clicks = Vec::with_capacity(TRIALS);
    let mut full_clicks = Vec::with_capacity(TRIALS);

    for i in 0..TRIALS {
        // Reset the page so each trial starts in the same state.
        page.goto(data_url).await?.wait_for_navigation().await?;
        tokio::time::sleep(Duration::from_millis(80)).await;
        let btn = page.find_element("#b").await?;
        let desc = btn.description().await?;
        let id: BackendNodeId = desc.backend_node_id;

        // 1) Bare click_ref latency (the off-mode click path).
        let t0 = Instant::now();
        click_ref(&page, id.clone()).await?;
        let bare = t0.elapsed().as_millis();

        // 2) Full path including cursor pre-move + ripple (only on the
        //    "on" trials; on the "off" trials we just do bare again so
        //    the runs are comparable).
        page.goto(data_url).await?.wait_for_navigation().await?;
        tokio::time::sleep(Duration::from_millis(80)).await;
        let btn = page.find_element("#b").await?;
        let desc = btn.description().await?;
        let id: BackendNodeId = desc.backend_node_id;

        let t1 = Instant::now();
        if let Ok(Some((cx, cy))) = compute_element_center(&page, id.clone()).await {
            move_cursor_and_ripple(&page, cx, cy).await;
        }
        // Spec: 100-200ms slide delay (the agent uses 200ms).
        tokio::time::sleep(Duration::from_millis(200)).await;
        click_ref(&page, id).await?;
        let full = t1.elapsed().as_millis();

        bare_clicks.push(bare);
        full_clicks.push(full);
        println!("trial {}: bare={}ms full={}ms (delta={}ms)", i+1, bare, full, full as i128 - bare as i128);
    }

    let bare_med = median(&mut bare_clicks);
    let full_med = median(&mut full_clicks);
    println!();
    println!("median bare click_ref:   {} ms", bare_med);
    println!("median full (cursor):    {} ms", full_med);
    println!("median cursor overhead:  {} ms", full_med as i128 - bare_med as i128);
    println!("(includes 200ms slide sleep + 1 box-model call + 1 ripple evaluate)");

    let _ = shutdown(browser, handle, job).await;
    Ok(())
}
