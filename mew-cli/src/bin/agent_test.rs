use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .try_init();

    let config = mew_agent::load_config()?;
    
    let (browser, page, handler_task) = mew_cdp::launch(
        config.browser.as_ref().and_then(|b| b.binary_path.clone()),
        false,
    ).await?;
    
    let task = "Navigate to en.wikipedia.org. Search for 'Rust programming language'. Then, click at least 10 different internal links to explore related topics like Mozilla, C++, Memory Safety, etc. Scroll around if needed. Do not call finish() until you have explored for at least 15 steps.";
    
    let mut agent = mew_agent::agent::Agent::new(config, task);
    
    match agent.run(&page).await {
        Ok(res) => println!("Agent finished successfully: {}", res),
        Err(e) => eprintln!("Agent failed: {}", e),
    }

    mew_cdp::shutdown(browser, handler_task).await?;

    Ok(())
}
