use std::env;
use tracing_subscriber::EnvFilter;

mod stdin_chat;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .try_init();

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage:");
        eprintln!("  mew run \"task description\"");
        eprintln!("  mew run --preset \"preset_name\"");
        eprintln!("  mew run-preset \"preset_name\"          # Phase 8: structured preset path");
        eprintln!("  mew list-presets                        # Phase 8: catalog of available presets");
        eprintln!("  mew memory dump");
        return Ok(());
    }

    let config = mew_agent::load_config()?;

    let mut task_desc = String::new();
    // Phase 8: structured-preset path. The CLI dispatches on
    // `args[1] == "run-preset"`; the legacy `mew run --preset
    // <name>` (free-form text) stays as the back-compat surface.
    let mut structured_handoff: Option<mew_agent::handoff::Handoff> = None;

    if args[1] == "memory" {
        let sub = args.get(2).map(|s| s.as_str()).unwrap_or("dump");
        if sub == "dump" {
            let data_dir = std::path::PathBuf::from("transcripts");
            let store = mew_memory::LessonStore::new(&data_dir);
            let lessons = store.read_all()?;
            println!(
                "=== Episodic Memory Store Dump ({} lessons) ===",
                lessons.len()
            );
            if lessons.is_empty() {
                println!("(memory store is empty)");
            } else {
                for l in &lessons {
                    let platform = l.platform.as_deref().unwrap_or("any");
                    let conflicts = if l.conflicting_with.is_empty() {
                        "none".to_string()
                    } else {
                        l.conflicting_with.join(", ")
                    };
                    println!("--------------------------------------------------");
                    println!("ID:           {}", l.id);
                    println!("Created At:   {}", l.created_at);
                    println!("Task Shape:   {:?}", l.task_shape);
                    println!("Platform:     {}", platform);
                    println!("Situation:    {}", l.situation);
                    println!("Lesson:       {}", l.lesson_text);
                    println!("Confidence:   {:.2}", l.confidence_score);
                    println!("Times Seen:   {}", l.times_seen);
                    println!("Times Helped: {}", l.times_helped);
                    println!("Outcome:      {:?}", l.outcome);
                    println!("Conflicts:    {}", conflicts);
                }
                println!("--------------------------------------------------");
            }
            return Ok(());
        }
    }

    // Phase 8: `mew list-presets` — print the available
    // preset catalog to stdout. Useful for "what presets
    // can I trigger?" and for shell-completion.
    if args[1] == "list-presets" {
        let merged = mew_agent::presets::merged_presets(&config.agent.task_preset_overrides);
        print!("{}", mew_agent::presets::render_preset_catalog(&merged));
        return Ok(());
    }

    if args[1] == "run-preset" {
        if args.len() != 3 {
            eprintln!("Usage: mew run-preset \"preset_name\"");
            eprintln!(
                "Available presets: {}",
                mew_agent::presets::render_preset_catalog(&mew_agent::presets::merged_presets(
                    &config.agent.task_preset_overrides
                ))
            );
            return Ok(());
        }
        let preset_name = &args[2];
        let merged = mew_agent::presets::merged_presets(&config.agent.task_preset_overrides);
        match mew_agent::presets::resolve_preset(preset_name, &merged) {
            Some(preset) => {
                let handoff = mew_agent::presets::build_handoff_for_preset(preset, "cli-preset");
                task_desc = handoff.task_description.clone();
                structured_handoff = Some(handoff);
            }
            None => {
                eprintln!(
                    "Preset '{}' not found. Available presets:",
                    preset_name
                );
                eprint!(
                    "{}",
                    mew_agent::presets::render_preset_catalog(&merged)
                );
                return Ok(());
            }
        }
    } else if args[1] == "run" {
        if args.len() == 3 {
            task_desc = args[2].clone();
        } else if args.len() == 4 && args[2] == "--preset" {
            let preset_name = &args[3];
            if let Some(desc) = config.agent.task_presets.get(preset_name) {
                task_desc = desc.clone();
            } else {
                eprintln!("Preset '{}' not found in config.yaml", preset_name);
                return Ok(());
            }
        } else {
            eprintln!("Usage: mew run \"task description\" or mew memory dump");
            return Ok(());
        }
    } else {
        eprintln!("Usage: mew run \"task description\" or mew memory dump");
        return Ok(());
    }

    println!("Task: {}", task_desc);
    if let Some(h) = &structured_handoff {
        println!(
            "Preset subtasks ({}): {}",
            h.subtasks.len(),
            h.subtasks
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<&str>>()
                .join(", ")
        );
    }

    println!("Launching headed Chrome...");
    let (browser, page, handler_task, job) = match mew_cdp::launch(
        config.browser.as_ref().and_then(|b| b.binary_path.clone()),
        config
            .browser
            .as_ref()
            .map(|b| b.visible_cursor)
            .unwrap_or(false),
    )
    .await
    {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Failed to launch Chrome: {e}");
            return Err(e);
        }
    };

    println!("Starting agent loop...");
    // Phase 4 (Bug 3 fix): explicit `None` for the transcript dir
    // keeps the historical `./transcripts/` behavior. The CLI runs
    // from the workspace root, so the path lands at
    // `<workspace>/transcripts/` — gitignored by `.gitignore`'s
    // `/transcripts/` rule, and outside the Tauri dev watcher's
    // scope (the CLI doesn't run under `cargo tauri dev`).
    let mut agent = mew_agent::agent::Agent::new(config, &task_desc, None);

    // Phase 13.1: wire the live chat channel. Take the sender half from
    // the agent's MessageBus and hand it to the stdin reader thread. The
    // reader pushes whatever the user types into the channel; the agent
    // loop drains it at every checkpoint via `drain_and_apply_user_messages`.
    //
    // Per the spec: "Make sure typing a message doesn't require the agent
    // to be paused" — the agent starts in Running, and `drain_pending`
    // uses a non-blocking try_recv, so messages typed right now will be
    // picked up on the very next loop iteration. No pause() needed.
    let chat_tx = agent.take_message_sender();
    stdin_chat::spawn_stdin_reader(chat_tx);
    println!("Live chat channel ready: type while the agent is running to steer it.");

    let result = agent.run(&page).await;
    match result {
        Ok(res) => println!("\nTask completed successfully.\nResult: {}", res),
        Err(e) => eprintln!("\nAgent loop terminated with error: {}", e),
    }

    println!("Shutting down browser cleanly...");
    if let Err(e) = mew_cdp::shutdown(browser, handler_task, job).await {
        eprintln!("Error during browser shutdown: {e}");
    } else {
        println!("Browser process closed cleanly.");
    }

    Ok(())
}
