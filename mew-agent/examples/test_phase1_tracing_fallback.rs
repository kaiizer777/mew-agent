// mew v2 — Phase 1.5 tracing-fallback smoke test.
//
// Purpose: prove the JSONL layer actually receives events in the
// realistic CLI scenario, where the CLI has already installed a
// `tracing_subscriber::fmt()` global subscriber. This is the
// scenario the user is most concerned about — "a silently-skipped
// layer would look identical to 'working' until someone greps for
// a trace file that isn't there."
//
// The test runs the same dance `Agent::new_with_tracing` runs:
//   1. Build the JSONL layer.
//   2. Install a fmt subscriber as the global (simulating the CLI).
//   3. Try `try_install_global` — expect Err (fmt owns the slot).
//   4. Install the layer as a thread-local override. This is the
//      fix: the layer is now stacked on top of the fmt subscriber
//      for the current thread, so `tracing::info!` events reach
//      BOTH subscribers.
//   5. Emit `tracing::info!` events and confirm the JSONL file
//      contains them.
//
// This is an integration smoke test, not a unit test, so it lives
// in `mew-agent/examples/` next to the other Phase tests.
//
// Run with: `cargo run --example test_phase1_tracing_fallback -p mew-agent`

use std::sync::Arc;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // 1. Set up a temp directory we can write the JSONL file into.
    let tmp = std::env::temp_dir().join(format!("mew-trace-fallback-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let jsonl_path = tmp.join("trace_smoke.jsonl");
    eprintln!("[smoke] jsonl path: {}", jsonl_path.display());

    // 2. Build the layer.
    let layer = mew_agent::tracing_layer::SessionJsonLayer::new(
        "smoke_session",
        jsonl_path.clone(),
    )?;
    let layer = Arc::new(layer);

    // 3. Simulate the CLI having installed a fmt subscriber first.
    //    `set_global_default` only succeeds once per process; the
    //    `try_init` on a fmt subscriber is the same path the CLI
    //    takes. If another test has already grabbed the global,
    //    this prints a warning and continues.
    let fmt_init = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .try_init();
    eprintln!("[smoke] fmt subscriber installed: {}", fmt_init.is_ok());

    // 4. Try to install the JSONL layer as a second global. This is
    //    the first stage of `Agent::new_with_tracing`'s install —
    //    we expect Err because the fmt subscriber already owns
    //    the global slot.
    let install_result = mew_agent::tracing_layer::try_install_global(layer.clone());
    match install_result {
        Ok(()) => eprintln!(
            "[smoke] WARN: JSONL layer installed as global (fmt was not). \
             This is fine in this test but unexpected in CLI mode."
        ),
        Err(e) => eprintln!(
            "[smoke] JSONL layer global install returned Err as expected: {e}"
        ),
    }

    // 5. Stage 2: install the layer as a thread-local override.
    //    This is the fix. The guard must be kept alive for the
    //    test's duration — when it drops, the override reverts.
    let _guard = mew_agent::tracing_layer::try_install_thread_local(layer.clone());
    eprintln!("[smoke] JSONL layer installed as thread-local override (guard held)");

    // 6. Emit a few `tracing::info!` events. With the thread-local
    //    override in place, the JSONL layer should receive every
    //    one of them in addition to the global fmt subscriber.
    tracing::info!(event = "smoke_event_one", "first event");
    tracing::info!(event = "smoke_event_two", iter = 2, "second event with fields");
    tracing::info!(event = "smoke_event_three", "third event");

    // 7. Drop the guard and confirm subsequent events do NOT land
    //    in the file. This proves the override is *scoped* —
    //    dropping the guard reverts the thread-local default back
    //    to the global fmt subscriber.
    drop(_guard);
    tracing::info!(event = "smoke_event_after_drop", "this MUST NOT land in the file");

    // 8. Read the file back and confirm exactly the first three
    //    events are present, the fourth is not.
    let content = std::fs::read_to_string(&jsonl_path)?;
    let lines: Vec<&str> = content.lines().collect();
    eprintln!("[smoke] file has {} line(s)", lines.len());
    for (i, line) in lines.iter().enumerate() {
        eprintln!("[smoke]   line {}: {}", i + 1, line);
    }
    let events_present = ["smoke_event_one", "smoke_event_two", "smoke_event_three"];
    for ev in &events_present {
        if !content.contains(ev) {
            anyhow::bail!(
                "smoke test failed: expected event '{ev}' in file, but it is missing. \
                 The thread-local override is not routing tracing::info! to the JSONL layer."
            );
        }
    }
    if content.contains("smoke_event_after_drop") {
        anyhow::bail!(
            "smoke test failed: 'smoke_event_after_drop' landed in the file even though \
             the thread-local guard was dropped. The override is not scoped to the guard's \
             lifetime — investigate."
        );
    }
    eprintln!("[smoke] OK: 3 events present, 1 post-drop event correctly absent");

    // 9. Clean up the temp dir.
    let _ = std::fs::remove_dir_all(&tmp);
    eprintln!("[smoke] all assertions passed");
    Ok(())
}
