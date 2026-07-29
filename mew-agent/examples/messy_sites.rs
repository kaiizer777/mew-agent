use mew_agent::load_config;
use mew_agent::agent::Agent;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    let config = load_config()?;
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <url> <task> [--throttle]", args[0]);
        return Ok(());
    }

    let url = &args[1];
    let task = &args[2];
    
    println!("Launching browser...");
    let (browser, page, handler) = mew_cdp::launch(
        config.browser.as_ref().and_then(|b| b.binary_path.clone()),
        false,
    ).await?;
    
    // Optional: enable network throttling if '--throttle' is in args
    if args.iter().any(|a| a == "--throttle") {
        println!("Enabling network throttling (2s latency, 50kbps)...");
        use chromiumoxide::cdp::browser_protocol::network::EmulateNetworkConditionsParams;
        use chromiumoxide::cdp::browser_protocol::network::ConnectionType;
        
        page.execute(chromiumoxide::cdp::browser_protocol::network::EnableParams::default()).await?;
        
        let params = EmulateNetworkConditionsParams::builder()
            .offline(false)
            .latency(2000.0) 
            .download_throughput(50000.0) 
            .upload_throughput(50000.0)
            .connection_type(ConnectionType::Cellular3g)
            .build()
            .unwrap();
        page.execute(params).await?;
    }

    // Initial navigation
    println!("Initial navigation to: {}", url);
    let _ = page.goto(url).await?.wait_for_navigation().await;

    println!("\nRunning agent on {}...", url);
    let mut agent_real = Agent::new(config, task);
    let res = agent_real.run(&page).await;
    println!("Result of run: {:?}", res);
    
    mew_cdp::shutdown(browser, handler).await?;
    Ok(())
}
