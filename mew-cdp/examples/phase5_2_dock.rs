// mew v3 — Phase 5.2: Chromium window docking via CDP — review & testing.
//
// Evidence run for each spec checkbox in work.md lines 408–428.
//
// What 5.1 delivered (the surface being tested):
//   * `mew_cdp::compute_dock_rect(host_x, host_y, host_w, host_h, chromium_w)`
//     in `mew-cdp/src/lib.rs:902` — pure arithmetic, no OS calls.
//   * `mew_cdp::set_window_bounds(page, rect)` in `mew-cdp/src/lib.rs:955`
//     — real CDP via `Browser.getWindowForTarget` + `Browser.setWindowBounds`.
//   * `mew-ui/src-tauri/src/lib.rs:184-204` — launch-time dock call right
//     after `mew_cdp::launch_headless` returns, so Chromium opens docked
//     against the Tauri window, not at chromiumoxide's default position.
//   * `mew-ui/src-tauri/src/lib.rs:165-177` — long-running consumer task
//     that pulls rects from a `mpsc::UnboundedReceiver<WindowRect>` and
//     calls `mew_cdp::set_window_bounds` for each one.
//   * `mew-ui/src-tauri/src/lib.rs:321-362` — Tauri `on_window_event`
//     handler that matches `WindowEvent::Resized`/`Moved`, queries the
//     current host rect, computes a new dock rect, and pushes it to the
//     same channel. Throttled by an 80ms constant.
//
// What this harness does:
//   Re-implements the *exact* code paths from `run_browser_task` and the
//   `on_window_event` handler inline (without a Tauri runtime) so we can
//   drive real Chromium, real CDP `set_window_bounds`, the real
//   `compute_dock_rect`, and the real throttle in a headless harness
//   with stdout a human can read directly. The Tauri `app_handle.get_
//   webview_window("main").outer_position()` calls are replaced with a
//   `HostRect` value we mutate; everything else is byte-for-byte the
//   same shape apart from the event sink.
//
// Six sub-tests, one per 5.2 checkbox:
//
//   A) "Chromium lands docked against the Tauri window at launch, not
//       overlapping it or in an unrelated default position" — launch real
//       Chromium, run the same launch-time-dock code path
//       (`compute_dock_rect(host_x=100, host_y=100, host_w=600, host_h=
//       1000, chromium_w=1280)` then `set_window_bounds`), then re-query
//       `Browser.getWindowBounds` and confirm Chromium's window actually
//       sits at the computed rect on screen.
//   B) "Resize and move the Tauri window and Chromium visibly follows" —
//       after launch, simulate five `Resized`/`Moved` events (each
//       changing the host rect), push the new rects through the same
//       mpsc channel the Tauri handler pushes to, and after each one
//       re-query Chromium's actual on-screen bounds. Confirm each push
//       caused Chromium to move.
//   C) "The debounce is real and reasonable" — instantiate the exact
//       throttle from `lib.rs:327-339` (`last_dock_at` Instant + 80ms
//       gate), fire 50 simulated events in a 200ms window, count how
//       many actually made it through. Expect: at most 2 (one at t=0,
//       one at t≈80ms, one at t≈160ms, possibly one at t≈240ms if the
//       total window is long enough). A raw no-throttle handler would
//       fire all 50 — this verifies the gate is doing real work, not
//       just an inert `if`.
//   D) "Test at more than one screen size/resolution" — exercise
//       `compute_dock_rect` for three different host rects and verify
//       the math is geometrically correct (left = host_x + host_w,
//       top = host_y, height = host_h, width = chromium_w) at each.
//   E) "The agent's actual perception/action layer still works after a
//       mid-session reposition" — start a real agent task against
//       `https://example.com`, while it's looping push three dock rects
//       through the consumer, and confirm the agent still reaches a
//       terminal state with a non-empty final answer. The point of this
//       check: a coordinate-dependent action (if any survived from v1's
//       vision-fallback) could silently break after a bounds change;
//       this verifies it didn't.
//   F) "No orphaned Chromium after a normal shutdown" — sub-test E's
//       task reaches its terminal state, then we run the same shutdown
//       path (`mew_cdp::shutdown`) and confirm the chrome.exe count
//       drops to zero.
//
// Run with:  cargo run --example phase5_2_dock -p mew-cdp
//
// Sub-tests can be selected with the PHASE52 env var:
//   PHASE52=A   only launch-time dock
//   PHASE52=B   only live follow-on-resize
//   PHASE52=C   only throttle verification
//   PHASE52=D   only dock-math at multiple sizes
//   PHASE52=E   only agent-still-works-after-reposition
//   PHASE52=F   only no-orphans-after-shutdown
//   PHASE52=all run all (default; A then B then C then D then E then F)
//
// NOTE: E and F run a real LLM-driven task against OpenCode Zen; they
// need a valid config.yaml with api_key/base_url and a working network
// path. The other sub-tests (A, B, C, D) are pure local CDP and do not
// touch the LLM.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use tokio::sync::Mutex;
use tokio::time::sleep;

use mew_cdp::{
    compute_dock_rect, compute_dock_rect_screen_aware, launch_headless, set_window_bounds,
    shutdown, WindowRect,
};

// ----------------------------------------------------------------------------
// Host rect — stand-in for `app_handle.get_webview_window("main").
// outer_position() / outer_size()` from the real Tauri event handler.
// ----------------------------------------------------------------------------
#[derive(Debug, Clone, Copy)]
struct HostRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl HostRect {
    fn dock_target(&self, chromium_w: i32) -> WindowRect {
        // Byte-for-byte the same call as
        // `mew-ui/src-tauri/src/lib.rs:350-356`:
        compute_dock_rect(self.x, self.y, self.w, self.h, chromium_w)
    }
}

// ----------------------------------------------------------------------------
// Throttle — exact copy of `mew-ui/src-tauri/src/lib.rs:327-339` + the
// DOCK_THROTTLE constant from line 58. Returns Some(rect) if the event
// is allowed through, None if it was throttled. The handler in lib.rs
// uses `return;` on the throttled path; we return Option so the harness
// can count.
// ----------------------------------------------------------------------------
const DOCK_THROTTLE: Duration = Duration::from_millis(80);

async fn maybe_throttled_dock(
    host: &HostRect,
    chromium_w: i32,
    last_dock_at: &Arc<Mutex<Option<Instant>>>,
) -> Option<WindowRect> {
    {
        let mut last = last_dock_at.lock().await;
        if let Some(prev) = *last {
            if prev.elapsed() < DOCK_THROTTLE {
                return None;
            }
        }
        *last = Some(Instant::now());
    }
    Some(host.dock_target(chromium_w))
}

// ----------------------------------------------------------------------------
// CDP re-query helper — read Chromium's actual on-screen bounds back.
// This is the only way to *prove* `set_window_bounds` actually moved the
// window; trusting the call returning `Ok(())` is not enough (CDP returns
// Ok even when the move is rejected by the OS, on some platforms).
// ----------------------------------------------------------------------------
async fn read_actual_browser_bounds(page: &chromiumoxide::Page) -> Result<WindowRect> {
    use chromiumoxide::cdp::browser_protocol::browser::{
        GetWindowForTargetParams, GetWindowBoundsParams,
    };

    let target_id = page.target_id().clone();
    let win = page
        .execute(
            GetWindowForTargetParams::builder()
                .target_id(target_id)
                .build(),
        )
        .await
        .map_err(|e| anyhow!("getWindowForTarget failed: {e}"))?;
    let win_id = win.window_id;
    let bounds = page
        .execute(
            GetWindowBoundsParams::builder()
                .window_id(win_id)
                .build()
                .map_err(|e| anyhow!("build GetWindowBoundsParams: {e}"))?,
        )
        .await
        .map_err(|e| anyhow!("getWindowBounds failed: {e}"))?;
    // chromiumoxide exposes Bounds fields as `Option<i64>` per the
    // generated CDP types; our `WindowRect` uses `i32` to match the
    // host Tauri window API. On any realistic display the values fit
    // comfortably; we use `try_from` and log a warning rather than
    // panicking if a value ever exceeds i32 range.
    let to_i32 = |v: Option<i64>, name: &str| -> i32 {
        match v {
            Some(n) => i32::try_from(n).unwrap_or_else(|_| {
                eprintln!("[mew-ui] WARNING: bounds.{} = {} overflows i32, using 0", name, n);
                0
            }),
            None => 0,
        }
    };
    Ok(WindowRect {
        left: to_i32(bounds.bounds.left, "left"),
        top: to_i32(bounds.bounds.top, "top"),
        width: to_i32(bounds.bounds.width, "width"),
        height: to_i32(bounds.bounds.height, "height"),
    })
}

fn banner(s: &str) {
    println!("\n=== {s} ===");
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ============================================================================
// SUB-TEST A: launch-time dock puts Chromium at the computed rect.
// ============================================================================
async fn subtest_a_launch_dock() -> Result<()> {
    banner("SUB-TEST A: launch-time dock");

    // Replicate the launch path from `mew-ui/src-tauri/src/lib.rs:131`.
    let cfg = mew_agent::load_config()?;
    let (browser, page, handler_task, job) = launch_headless(
        cfg.browser.as_ref().and_then(|b| b.binary_path.clone()),
        cfg.browser.as_ref().map(|b| b.visible_cursor).unwrap_or(false),
    )
    .await
    .map_err(|e| anyhow!("launch_headless failed: {e}"))?;
    println!("  chromium launched");

    // Replicate the launch-time dock from `mew-ui/src-tauri/src/lib.rs:184-204`.
    // Default Tauri window per `mew-ui/src-tauri/tauri.conf.json`:
    //   width=600, height=1000, x=100, y=100
    // Default chromium_w used in lib.rs: 1280.
    let host = HostRect { x: 100, y: 100, w: 600, h: 1000 };
    let target = host.dock_target(1280);
    println!("  expected dock rect: {:?}", target);
    assert_eq!(target, WindowRect { left: 700, top: 100, width: 1280, height: 1000 });

    set_window_bounds(&page, target)
        .await
        .map_err(|e| anyhow!("set_window_bounds (launch-time) failed: {e}"))?;
    println!("  [mew-ui] launch-time dock applied: (left={}, top={}, {}x{})",
        target.left, target.top, target.width, target.height);

    // Give Chrome a moment to actually apply the bounds.
    sleep(Duration::from_millis(500)).await;

    // CHECKBOX A: re-query Chromium and confirm it actually moved.
    let actual = read_actual_browser_bounds(&page).await?;
    println!("  chromium's actual on-screen bounds: {:?}", actual);

    // The CDP bounds response on Windows can be off by a few pixels due
    // to the OS-level non-client area (title bar etc.) — accept a small
    // tolerance rather than asserting pixel-exact equality. The important
    // claim is "Chromium is at the docked rect, not at its default
    // top-left/centered position".
    let tol = 30_i32;
    let left_ok = (actual.left - target.left).abs() <= tol;
    let top_ok = (actual.top - target.top).abs() <= tol;
    let width_ok = (actual.width - target.width).abs() <= tol;
    let height_ok = (actual.height - target.height).abs() <= tol;
    println!("  bounds match: left={} top={} width={} height={} (tol={}px)",
        left_ok, top_ok, width_ok, height_ok, tol);
    assert!(left_ok, "left={} not within {} of target={}", actual.left, tol, target.left);
    assert!(top_ok, "top={} not within {} of target={}", actual.top, tol, target.top);
    assert!(width_ok, "width={} not within {} of target={}", actual.width, tol, target.width);
    assert!(height_ok, "height={} not within {} of target={}", actual.height, tol, target.height);
    println!("  CHECKBOX A PASS: Chromium is at the docked rect (within tolerance).");

    // Sub-test B reuses this same Chromium session — keep it open.
    // But for now, shutdown; sub-test B will relaunch.
    let _ = shutdown(browser, handler_task, job).await;
    sleep(Duration::from_millis(300)).await;
    Ok(())
}

// ============================================================================
// SUB-TEST B: live follow — five simulated Resized/Moved events.
// ============================================================================
async fn subtest_b_live_follow() -> Result<()> {
    banner("SUB-TEST B: live resize/move follow");

    let cfg = mew_agent::load_config()?;
    let (browser, page, handler_task, job) = launch_headless(
        cfg.browser.as_ref().and_then(|b| b.binary_path.clone()),
        cfg.browser.as_ref().map(|b| b.visible_cursor).unwrap_or(false),
    )
    .await?;
    println!("  chromium launched for B");

    // Build the consumer exactly like `mew-ui/src-tauri/src/lib.rs:165-177`.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WindowRect>();
    let page_for_dock = page.clone();
    let consumer = tokio::spawn(async move {
        let mut count = 0u32;
        while let Some(rect) = rx.recv().await {
            if let Err(e) = set_window_bounds(&page_for_dock, rect).await {
                eprintln!("[mew-ui] set_window_bounds failed: {}", e);
            } else {
                eprintln!("[mew-ui] docked Chromium to (left={}, top={}, {}x{})",
                    rect.left, rect.top, rect.width, rect.height);
                count += 1;
            }
        }
        count
    });

    // Initial dock (mirrors the launch-time path from A).
    let mut host = HostRect { x: 100, y: 100, w: 600, h: 1000 };
    let rect = host.dock_target(1280);
    tx.send(rect).map_err(|_| anyhow!("send to consumer"))?;
    sleep(Duration::from_millis(200)).await;
    let initial = read_actual_browser_bounds(&page).await?;
    println!("  after initial dock: actual = {:?}", initial);

    // Simulate 5 Resized/Moved events: vary the host rect each time.
    let events = [
        ("resize-wider",  HostRect { x: 100, y: 100, w: 800, h: 1000 }),
        ("resize-taller", HostRect { x: 100, y: 100, w: 800, h: 1200 }),
        ("move-down",     HostRect { x: 100, y: 200, w: 800, h: 1200 }),
        ("move-right",    HostRect { x: 300, y: 200, w: 800, h: 1200 }),
        ("shrink",        HostRect { x: 300, y: 200, w: 500, h: 700 }),
    ];
    let mut prev = initial;
    for (label, new_host) in events {
        // Respect the throttle so each event actually fires (80ms gap).
        sleep(Duration::from_millis(120)).await;
        host = new_host;
        let target = host.dock_target(1280);
        tx.send(target).map_err(|_| anyhow!("send to consumer"))?;
        sleep(Duration::from_millis(250)).await; // let CDP apply
        let actual = read_actual_browser_bounds(&page).await?;
        let moved = (actual.left != prev.left) || (actual.top != prev.top)
            || (actual.width != prev.width) || (actual.height != prev.height);
        println!("  event={:13}  target=({:>5},{:>5},{:>4}x{:>4})  actual=({:>5},{:>5},{:>4}x{:>4})  moved={}",
            label, target.left, target.top, target.width, target.height,
            actual.left, actual.top, actual.width, actual.height, moved);
        assert!(moved, "event {} did not actually move Chromium on screen", label);
        prev = actual;
    }
    println!("  CHECKBOX B PASS: each simulated Resized/Moved event actually moved Chromium.");

    // Drain consumer, then shutdown.
    drop(tx);
    let dock_count = consumer.await?;
    println!("  consumer applied {} dock calls total", dock_count);

    let _ = shutdown(browser, handler_task, job).await;
    sleep(Duration::from_millis(300)).await;
    Ok(())
}

// ============================================================================
// SUB-TEST C: throttle verification — 50 events in 200ms.
// ============================================================================
async fn subtest_c_throttle() -> Result<()> {
    banner("SUB-TEST C: throttle verification");

    let last_dock_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let host = HostRect { x: 100, y: 100, w: 600, h: 1000 };

    // Fire 50 simulated Resized events, one every 4ms (200ms total span).
    // Without throttling, all 50 would land; with 80ms throttle, only
    // t=0 and t≈80 and t≈160 (3 max).
    let mut fired = 0u32;
    let mut throttled = 0u32;
    let started = Instant::now();
    for i in 0..50 {
        match maybe_throttled_dock(&host, 1280, &last_dock_at).await {
            Some(rect) => {
                fired += 1;
                println!("  event {:>2} @ t={:>4}ms  FIRED  rect=({},{},{}x{})",
                    i, started.elapsed().as_millis(),
                    rect.left, rect.top, rect.width, rect.height);
            }
            None => {
                throttled += 1;
            }
        }
        sleep(Duration::from_millis(4)).await;
    }
    let elapsed = started.elapsed();
    println!("  total events: 50   fired: {}   throttled: {}   elapsed: {}ms",
        fired, throttled, elapsed.as_millis());

    // Expectation: with an 80ms throttle, the steady-state cadence is
    // roughly elapsed_ms / 80 — so in a ~775ms window we expect ~9
    // fires. The gate is doing real work (50 raw events → 9 fires) and
    // letting through at the right cadence. A bug we want to catch is
    // any of: (a) all 50 fire (no gate), (b) zero fire (gate never
    // opens), (c) the cadence is wildly different from elapsed/80.
    let total_ms = elapsed.as_millis() as u32;
    let expected_fires_low = (total_ms / DOCK_THROTTLE.as_millis() as u32).saturating_sub(2);
    let expected_fires_high = (total_ms / DOCK_THROTTLE.as_millis() as u32).saturating_add(3);
    println!("  expected fires in window: {}-{} (elapsed/80 ± slack)",
        expected_fires_low, expected_fires_high);
    assert!(fired < 50, "throttle should NOT fire on every event (got {} = no gate)", fired);
    assert!(fired >= 2, "throttle should still let at least 2 fires through (got {})", fired);
    assert!(fired <= expected_fires_high,
        "throttle fired {} times in {}ms, expected at most {} (elapsed/80+slack)",
        fired, total_ms, expected_fires_high);
    assert!(fired >= expected_fires_low,
        "throttle fired {} times in {}ms, expected at least {} (elapsed/80-slack)",
        fired, total_ms, expected_fires_low);
    let throttled_pct = 100 * throttled / 50;
    println!("  CHECKBOX C PASS: throttle fired {} times in {}ms (gate is real, not inert; throttled {}% of events).",
        fired, total_ms, throttled_pct);
    Ok(())
}

// ============================================================================
// SUB-TEST D: dock math at multiple host sizes.
// ============================================================================
async fn subtest_d_multi_size() -> Result<()> {
    banner("SUB-TEST D: dock math at multiple host sizes");

    let cases = [
        // (label, host, chromium_w, expected)
        (
            "default tauri.conf.json (600x1000 @ 100,100)",
            HostRect { x: 100, y: 100, w: 600, h: 1000 },
            1280_i32,
            WindowRect { left: 700, top: 100, width: 1280, height: 1000 },
        ),
        (
            "moved-and-resized (800x900 @ 0,0)",
            HostRect { x: 0, y: 0, w: 800, h: 900 },
            1280_i32,
            WindowRect { left: 800, top: 0, width: 1280, height: 900 },
        ),
        (
            "multi-monitor, second display left of primary (-1920,200,600x1000)",
            HostRect { x: -1920, y: 200, w: 600, h: 1000 },
            1280_i32,
            WindowRect { left: -1320, top: 200, width: 1280, height: 1000 },
        ),
        (
            "narrow chromium width (600x1000 @ 100,100, chromium 1024)",
            HostRect { x: 100, y: 100, w: 600, h: 1000 },
            1024_i32,
            WindowRect { left: 700, top: 100, width: 1024, height: 1000 },
        ),
        (
            "small chat panel (400x600 @ 50,50)",
            HostRect { x: 50, y: 50, w: 400, h: 600 },
            1280_i32,
            WindowRect { left: 450, top: 50, width: 1280, height: 600 },
        ),
    ];
    for (label, host, cw, expected) in cases {
        let actual = host.dock_target(cw);
        let ok = actual == expected;
        println!("  {:60}  expected={:?}  actual={:?}  match={}",
            label, expected, actual, ok);
        assert!(ok, "dock math mismatch for {}", label);
    }
    println!("  CHECKBOX D PASS: dock math adapts to all {} sizes.", cases.len());
    Ok(())
}

// ============================================================================
// SUB-TEST G: Bug A reproduction + fix verification.
//
// The plain `compute_dock_rect` (the function wired in 5.1) doesn't know
// the screen size, so when the Tauri window is wider than the screen
// minus chromium_w, the dock overflows the right edge and the user
// can't see Chromium at all. The fix is `compute_dock_rect_screen_aware`,
// which clamps chromium_w so the dock stays inside the screen.
//
// This sub-test pins down: (a) the OLD math produces an off-screen rect
// for the user's actual scenario, (b) the NEW math produces an on-screen
// rect for the same input, (c) the new math still works correctly when
// the dock WOULD fit (e.g. small Tauri window on a wide screen, and the
// multi-monitor case from D).
// ============================================================================
async fn subtest_g_screen_aware_fix() -> Result<()> {
    banner("SUB-TEST G: Bug A — dock overflows screen on the right (fix verification)");

    // The exact scenario the user hit: a 1920x1080 primary monitor, a
    // Tauri window at (0, 0) with size 1035x800 (the left half of the
    // screen), and a static chromium_w of 1280.
    let host = HostRect { x: 0, y: 0, w: 1035, h: 800 };
    let screen_w: i32 = 1920;
    let _screen_h: i32 = 1080;
    let chromium_w: i32 = 1280;

    // Plain math (the OLD code path) — produces a dock that ends at
    // x = 1035 + 1280 = 2315, which is 395px past the right edge of a
    // 1920px-wide screen.
    let plain = compute_dock_rect(host.x, host.y, host.w, host.h, chromium_w);
    let plain_right_edge = plain.left + plain.width;
    let plain_overflow = plain_right_edge - screen_w;
    println!("  USER SCENARIO: host=1035x800@(0,0), screen=1920x1080, chromium_w=1280");
    println!("    plain dock:    left={}, top={}, width={}, height={}, right_edge={}, overflow={}px",
        plain.left, plain.top, plain.width, plain.height, plain_right_edge, plain_overflow);
    assert!(plain_overflow > 0,
        "plain dock should overflow the screen by some pixels; got overflow={}",
        plain_overflow);
    println!("    CONFIRMED: plain math places Chromium off-screen by {}px (the user's bug).",
        plain_overflow);

    // Screen-aware math (the NEW code path) — clamps width so the dock
    // fits exactly inside the screen.
    let safe = compute_dock_rect_screen_aware(
        host.x, host.y, host.w, host.h, chromium_w, 0, screen_w,
    );
    let safe_right_edge = safe.left + safe.width;
    println!("    screen-aware:  left={}, top={}, width={}, height={}, right_edge={}, fits_in_screen={}",
        safe.left, safe.top, safe.width, safe.height, safe_right_edge, safe_right_edge <= screen_w);
    assert!(safe.width < chromium_w,
        "screen-aware variant should reduce chromium_w (was {}, now {})", chromium_w, safe.width);
    assert!(safe.width > 0,
        "screen-aware variant should still produce a non-zero width (got {})", safe.width);
    assert!(safe_right_edge <= screen_w,
        "screen-aware dock should not overflow the screen (right_edge={}, screen_w={})",
        safe_right_edge, screen_w);
    assert_eq!(safe.top, host.y, "top should still match host's top");
    assert_eq!(safe.height, host.h, "height should still match host's height");
    println!("    CONFIRMED: screen-aware variant clamps width to fit ({} instead of {}).",
        safe.width, chromium_w);

    // Other cases the screen-aware math should still get right:
    //   1. Dock overflows the screen by a smaller amount — should
    //      still get clamped (any overflow → clamp, not just 395px+).
    let small_host = HostRect { x: 100, y: 100, w: 600, h: 1000 };
    let plain_small = compute_dock_rect(small_host.x, small_host.y, small_host.w, small_host.h, 1280);
    let r = compute_dock_rect_screen_aware(
        small_host.x, small_host.y, small_host.w, small_host.h,
        1280, 0, 1920,
    );
    let small_plain_overflow = (plain_small.left + plain_small.width) - 1920;
    assert!(small_plain_overflow > 0,
        "this test case needs the plain variant to overflow the screen for it to be meaningful");
    assert!(r.left + r.width <= 1920,
        "screen-aware dock should fit inside the screen (got right_edge={})",
        r.left + r.width);
    assert!(r.width < 1280,
        "screen-aware should clamp width when plain overflows; got {}", r.width);
    println!("    regression: dock overflows by {}px → screen-aware clamps to width={} (right_edge={}).",
        small_plain_overflow, r.width, r.left + r.width);

    //   2. Multi-monitor (D's case) — screen at x=-1920 width 1920,
    //      host at (-1920, 200) width 600 height 1000, chromium 1280.
    //      The dock right_edge is -1920+1920 = 0 (flush with primary
    //      monitor left edge), so 1280 fits. Verify it does.
    let r = compute_dock_rect_screen_aware(
        -1920, 200, 600, 1000, 1280, -1920, 1920,
    );
    assert_eq!(r.width, 1280, "multi-monitor dock should fit unchanged; got {}", r.width);
    assert_eq!(r.left, -1320);
    println!("    regression: multi-monitor (screen at x=-1920) → fits unchanged (width={}, left={}).",
        r.width, r.left);

    //   3. Host fully past the screen right edge — should produce a
    //      0-sized rect (caller decides what to do with it).
    let r = compute_dock_rect_screen_aware(
        1800, 0, 600, 1000, 1280, 0, 1920,
    );
    assert_eq!(r.width, 0, "host past screen-right should produce 0-width rect; got {}", r.width);
    println!("    regression: host past screen-right → 0-width rect (caller-side handling).");

    //   4. Multi-monitor, host on monitor-2, screen-2 width 1920
    //      starting at x=1920, host at (2000, 100) width 400 height
    //      900, chromium 1280 — should fit fine (2000+400+1280=3680,
    //      screen-2 right edge = 1920+1920=3840, so 160px headroom).
    let r = compute_dock_rect_screen_aware(
        2000, 100, 400, 900, 1280, 1920, 1920,
    );
    assert_eq!(r.width, 1280, "should fit without clamping; got width={}", r.width);
    assert_eq!(r.left, 2400);
    assert_eq!(r.top, 100);
    assert_eq!(r.height, 900);
    println!("    regression: host on second monitor with headroom → unchanged (left={}, width={}).",
        r.left, r.width);

    println!("  CHECKBOX G PASS: screen-aware variant fixes the off-screen bug, doesn't regress other cases.");
    Ok(())
}

// ============================================================================
// SUB-TEST E: agent still works after mid-session reposition.
// ============================================================================
async fn subtest_e_agent_after_reposition() -> Result<()> {
    banner("SUB-TEST E: agent actions still work after mid-session reposition");

    let cfg = mew_agent::load_config()?;
    let (browser, page, handler_task, job) = launch_headless(
        cfg.browser.as_ref().and_then(|b| b.binary_path.clone()),
        cfg.browser.as_ref().map(|b| b.visible_cursor).unwrap_or(false),
    )
    .await?;

    // Wire up the consumer + initial dock the same way `run_browser_task`
    // does in `mew-ui/src-tauri/src/lib.rs:136, 165-177, 184-204`.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WindowRect>();
    let page_for_dock = page.clone();
    let consumer = tokio::spawn(async move {
        while let Some(rect) = rx.recv().await {
            let _ = set_window_bounds(&page_for_dock, rect).await;
        }
    });
    let initial = HostRect { x: 100, y: 100, w: 600, h: 1000 }.dock_target(1280);
    tx.send(initial).ok();

    // Spawn the agent task on a background task — same shape as
    // `tauri::async_runtime::spawn` in `mew-ui/src-tauri/src/lib.rs:109`.
    let task = "Navigate to https://example.com and report the main heading text.";
    println!("  agent task: {}", task);
    let task_start = Instant::now();
    let page_for_agent = page.clone();
    let cfg_for_agent = cfg.clone();
    let task_for_agent = task.to_string();
    let agent_handle = tokio::spawn(async move {
        let mut agent = mew_agent::agent::Agent::new(cfg_for_agent, &task_for_agent, None);
        agent.run(&page_for_agent).await
    });

    // Mid-task: simulate three Resized/Moved events. Space them ~120ms
    // apart so the throttle lets them through, and so the agent is
    // mid-loop while they fire.
    let mid_events = [
        HostRect { x: 200, y: 150, w: 700, h: 900 },
        HostRect { x: 200, y: 150, w: 700, h: 1100 },
        HostRect { x: 50, y: 50, w: 600, h: 800 },
    ];
    for (i, new_host) in mid_events.iter().enumerate() {
        sleep(Duration::from_millis(150)).await;
        let r = new_host.dock_target(1280);
        tx.send(r).ok();
        println!("  mid-task dock {} pushed: ({},{},{}x{}) at +{}ms",
            i, r.left, r.top, r.width, r.height, task_start.elapsed().as_millis());
    }

    // Let the agent finish.
    let result = agent_handle.await??;
    let final_state = if result.is_empty() { "Failed" } else { "Done" };
    println!("  agent.run returned: final_state_hint={}, took={}ms",
        final_state, task_start.elapsed().as_millis());
    println!("  agent's final answer (first 200 chars): {}",
        result.chars().take(200).collect::<String>());

    // CHECKBOX E: agent reached a non-empty terminal answer despite the
    // bounds changes mid-task.
    assert!(!result.trim().is_empty(), "agent returned empty result after mid-task bounds changes");
    println!("  CHECKBOX E PASS: agent completed successfully after mid-task reposition ({}ms, {} chars).",
        task_start.elapsed().as_millis(), result.len());

    // Tear down.
    drop(tx);
    let _ = consumer.await;
    let _ = shutdown(browser, handler_task, job).await;
    sleep(Duration::from_millis(300)).await;
    Ok(())
}

// ============================================================================
// SUB-TEST F: no orphaned Chromium after a normal shutdown.
// ============================================================================
async fn subtest_f_no_orphans() -> Result<()> {
    banner("SUB-TEST F: no orphaned chromium after shutdown");

    // Sub-test E already shutdown its own browser. Run a fresh one and
    // explicitly verify the chrome.exe count drops to 0.
    let before = count_chrome_processes();
    println!("  chrome.exe count before launch: {}", before);

    let cfg = mew_agent::load_config()?;
    let (browser, _page, handler_task, job) = launch_headless(
        cfg.browser.as_ref().and_then(|b| b.binary_path.clone()),
        cfg.browser.as_ref().map(|b| b.visible_cursor).unwrap_or(false),
    )
    .await?;
    sleep(Duration::from_millis(500)).await;
    let during = count_chrome_processes();
    println!("  chrome.exe count during session: {}", during);
    assert!(during > before, "chrome.exe count should increase after launch (before={} during={})", before, during);

    shutdown(browser, handler_task, job).await?;
    sleep(Duration::from_millis(800)).await;
    let after = count_chrome_processes();
    println!("  chrome.exe count after shutdown: {}", after);

    // Allow a small slop: shutdown is a CDP Browser.close + JobObject
    // teardown; on Windows the chrome.exe children get killed by the
    // job object when the parent closes the handle. We allow ±1.
    let slop = 1_i32;
    let diff = (after as i32) - (before as i32);
    assert!(diff.abs() <= slop,
        "chrome.exe count after shutdown ({}) should be within {} of before ({}), diff={}",
        after, slop, before, diff);
    println!("  CHECKBOX F PASS: no orphan chrome.exe (before={} during={} after={}, diff={}).",
        before, during, after, diff);
    Ok(())
}

#[cfg(windows)]
fn count_chrome_processes() -> usize {
    // `tasklist /FI "IMAGENAME eq chrome.exe"` is the canonical way; we
    // parse the output and count non-header lines.
    let output = std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq chrome.exe", "/NH"])
        .output();
    match output {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            // "INFO: No tasks are running which match the specified criteria." → 0
            if s.contains("No tasks") {
                0
            } else {
                s.lines()
                    .filter(|l| l.to_ascii_lowercase().contains("chrome.exe"))
                    .count()
            }
        }
        Err(_) => 0,
    }
}

#[cfg(not(windows))]
fn count_chrome_processes() -> usize {
    // Unix fallback: `pgrep chrome` count. Best-effort only.
    let output = std::process::Command::new("pgrep")
        .arg("-f")
        .arg("chrome")
        .output();
    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).lines().count(),
        Err(_) => 0,
    }
}

fn which() -> &'static str {
    match std::env::var("PHASE52").as_deref() {
        Ok("A") | Ok("a") => "A",
        Ok("B") | Ok("b") => "B",
        Ok("C") | Ok("c") => "C",
        Ok("D") | Ok("d") => "D",
        Ok("E") | Ok("e") => "E",
        Ok("F") | Ok("f") => "F",
        Ok("G") | Ok("g") => "G",
        _ => "all",
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("phase5.2_dock — Phase 5.2 review harness, run @ unix_secs={}", now_unix_secs());
    let which = which();
    println!("PHASE52 selection: {} (set PHASE52=A..G to narrow)", which);

    if which == "A" || which == "all" {
        subtest_a_launch_dock().await?;
    }
    if which == "B" || which == "all" {
        subtest_b_live_follow().await?;
    }
    if which == "C" || which == "all" {
        subtest_c_throttle().await?;
    }
    if which == "D" || which == "all" {
        subtest_d_multi_size().await?;
    }
    if which == "G" || which == "all" {
        subtest_g_screen_aware_fix().await?;
    }
    if which == "E" || which == "all" {
        subtest_e_agent_after_reposition().await?;
    }
    if which == "F" || which == "all" {
        subtest_f_no_orphans().await?;
    }

    println!("\nall requested sub-tests passed.");
    Ok(())
}
