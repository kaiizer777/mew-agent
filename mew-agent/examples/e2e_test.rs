use mew_agent::load_config;
use mew_agent::agent::Agent;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    let config = load_config()?;
    
    let task = "Search Wikipedia for 'Rust programming language', submit the search by pressing Enter, and as soon as you see the article text, call finish(result) with the first sentence. Do NOT keep scrolling.";
    
    println!("Launching browser...");
    let (browser, page, handler) = mew_cdp::launch(
        config.browser.as_ref().and_then(|b| b.binary_path.clone()),
        false,
    ).await?;
    
    println!("\nTesting with real config...");
    let mut agent_real = Agent::new(config, task);
    let res2 = agent_real.run(&page).await;
    println!("Result of real run: {:?}", res2);
    
    mew_cdp::shutdown(browser, handler).await?;
    Ok(())
}
