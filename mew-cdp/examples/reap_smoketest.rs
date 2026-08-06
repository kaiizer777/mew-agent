use mew_cdp::orphan_reap::reap_orphaned_chromes;
fn main() {
    // Initialize a tracing subscriber so the reaper's
    // debug/trace events are visible from this tiny
    // example harness. Real callers (the Tauri shell,
    // the tests) install their own.
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_target(false)
        .without_time()
        .init();
    let profile = std::path::PathBuf::from("profile");
    std::fs::create_dir_all(&profile).ok();
    let killed = reap_orphaned_chromes(&profile, 9222);
    println!("orphans killed: {}", killed);
}
