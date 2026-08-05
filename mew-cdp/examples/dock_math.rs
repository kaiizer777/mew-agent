//! Phase 5.1 dock-math sanity check. Compiles-only verification that
//! `mew_cdp::compute_dock_rect` produces the expected rect for the
//! default Tauri window config (600x1000 at (100, 100)) and that
//! the math adapts to non-default positions and sizes.
//!
//! Run with:  cargo run -p mew-cdp --example dock_math

fn main() {
    use mew_cdp::compute_dock_rect;

    // Case 1: default tauri.conf.json — 600x1000 at (100,100), chromium 1280 wide.
    let r = compute_dock_rect(100, 100, 600, 1000, 1280);
    assert_eq!(r, mew_cdp::WindowRect { left: 700, top: 100, width: 1280, height: 1000 });
    println!("default config: {:?}", r);

    // Case 2: window moved to (0, 0), resized to 800x900.
    let r = compute_dock_rect(0, 0, 800, 900, 1280);
    assert_eq!(r, mew_cdp::WindowRect { left: 800, top: 0, width: 1280, height: 900 });
    println!("moved/resized:  {:?}", r);

    // Case 3: negative top-left (multi-monitor, second display to the left of primary).
    let r = compute_dock_rect(-1920, 200, 600, 1000, 1280);
    assert_eq!(r, mew_cdp::WindowRect { left: -1320, top: 200, width: 1280, height: 1000 });
    println!("negative left:  {:?}", r);

    // Case 4: different chromium width.
    let r = compute_dock_rect(100, 100, 600, 1000, 1024);
    assert_eq!(r, mew_cdp::WindowRect { left: 700, top: 100, width: 1024, height: 1000 });
    println!("1024-wide chromium: {:?}", r);

    println!("all dock-math cases passed");
}
