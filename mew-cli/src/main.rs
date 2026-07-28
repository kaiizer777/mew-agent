use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .try_init();

    println!("=== Step 0: API Smoke Test ===");
    let config = mew_agent::load_config()?;
    mew_agent::smoke_test(&config).await?;
    println!("API Smoke Test Passed!");

    println!("\n=== Step 1.1: Launching Visible Chrome via CDP ===");
    println!("Launching headed Chrome on remote debugging port {}...", mew_cdp::DEFAULT_PORT);

    let (browser, page, handler_task) = match mew_cdp::launch().await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Failed to launch Chrome: {e}");
            return Err(e.into());
        }
    };

    println!("Chrome process launched successfully!");
    println!("CDP listening on port: 9222");

    if let Ok(url) = page.url().await {
        println!("Page active and navigated to: {:?}", url);
    }

    println!("\nPress Ctrl+C to exit, or waiting 5 seconds before closing...");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("\nReceived Ctrl+C interrupt!");
        }
        _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {
            println!("\nTimer finished.");
        }
    }

    println!("Shutting down browser cleanly...");
    if let Err(e) = mew_cdp::shutdown(browser, handler_task).await {
        eprintln!("Error during browser shutdown: {e}");
    } else {
        println!("Browser process closed cleanly.");
    }

    Ok(())
}
