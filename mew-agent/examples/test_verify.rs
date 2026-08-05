use mew_agent::load_config;
use mew_agent::agent::Agent;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    let config = load_config()?;
    
    let args: Vec<String> = std::env::args().collect();
    let (url, task) = if args.len() >= 3 {
        (args[1].clone(), args[2].clone())
    } else {
        ("https://www.theverge.com/".to_string(), "Wait for the consent popup, click 'Accept All' or 'Agree', then click on the first article headline you see.".to_string())
    };
    
    println!("Launching browser...");
    let (browser, page, handler, job) = mew_cdp::launch(
        config.browser.as_ref().and_then(|b| b.binary_path.clone()),
        false,
    ).await?;
    
    println!("Initial navigation to: {}", url);
    let _ = page.goto(&url).await?.wait_for_navigation().await;

    println!("\nRunning agent on {}...", url);
    let mut agent_real = Agent::new(config, &task, None);
    let res = agent_real.run(&page).await;
    println!("Result of run: {:?}", res);
    
    mew_cdp::shutdown(browser, handler, job).await?;
    Ok(())
}
