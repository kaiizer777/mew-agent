// mew v3 — Phase 3.3 (Bug 2): hard-kill verification harness.
//
// Purpose:
//   Verify that the Job Object wrapper around the chromiumoxide-
//   launched Chrome process actually kills the chrome children
//   when the parent process is hard-killed (`Stop-Process -Force`,
//   `std::process::exit(1)`, Task Manager "End task", etc.).
//
// What this harness does:
//   1. `mew_cdp::launch(...)` — spawns a real chrome.exe under
//      a real Job Object. Prints our PID, the chrome PID, and the
//      chrome-children-of-our-pid count, then sleeps so the human
//      (or the PowerShell wrapper) can observe a "during" state.
//   2. After the sleep, calls `std::process::exit(1)` — the
//      *kernel* kills this process. The Job Object's last handle
//      (held by the OS in the form of our process's job-table
//      entry) is closed by the kernel as part of process teardown,
//      which triggers `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and
//      the kernel kills every process assigned to the job.
//
// The PowerShell wrapper (in the verification script) checks
// chrome processes whose parent PID is our PID, *before* and
// *after* this process exits. The after-count should be zero.
//
// Note: this harness does NOT call `mew_cdp::shutdown`. The whole
// point is to test the *ungraceful* exit path — if it called
// shutdown, the user-space close-via-CDP would run, and the Job
// Object test would be meaningless.

use std::time::Duration;

fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .try_init();

    let config = mew_agent::load_config()?;

    println!("PHASE33=hardkill — Phase 3.3 hard-kill verification");
    println!("parent pid: {}", std::process::id());

    // Build a tokio runtime so we can call the async launch.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let (_browser, _page, _handler, _job) = rt.block_on(async {
        let launch_result = mew_cdp::launch(
            config.browser.as_ref().and_then(|b| b.binary_path.clone()),
            config.browser.as_ref().map(|b| b.visible_cursor).unwrap_or(false),
        )
        .await;
        match launch_result {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[harness] mew_cdp::launch failed: {e}");
                std::process::exit(2);
            }
        }
    });

    // Give chrome a few seconds to fully spin up its child
    // processes (renderer, GPU, utility, etc.). The human (or
    // PowerShell wrapper) can `Get-Process chrome*` during this
    // window to see the "during" state.
    println!("[harness] chrome launched. sleeping 8s so children spin up...");
    std::thread::sleep(Duration::from_secs(8));

    // Print the harness PID so the PowerShell wrapper can
    // independently query chrome children of this process.
    // We don't introspect the process tree from inside the
    // harness because wmic isn't on PATH in PowerShell 7+
    // and cross-platform process-tree walking is heavy.
    println!("[harness] ready for hard-kill (parent_pid={})", std::process::id());

    // Hard-kill. We do NOT call `mew_cdp::shutdown` here.
    // `std::process::exit(1)` triggers normal process teardown
    // (no Drop runs for any Rust value, just like
    // `Stop-Process -Force` or Task Manager "End task").
    println!("[harness] hard-killing self via std::process::exit(1)...");
    std::thread::sleep(Duration::from_millis(200));
    std::process::exit(1);
}
