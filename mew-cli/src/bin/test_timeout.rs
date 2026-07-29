use mew_agent::load_config;
use mew_agent::agent::Agent;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt().try_init();
    let config = load_config()?;
    let html_path = std::env::current_dir()?.join("test_timeout.html");
    let file_url = format!("file:///{}", html_path.to_string_lossy().replace('\\', "/"));
    
    let task = "There is a button labeled 'Button 1' on the page. Your ONLY task is to call the `click` tool on Button 1. DO NOT navigate. DO NOT snapshot. ONLY call click on Button 1.";
    
    let (browser, page, handler) = mew_cdp::launch(
        config.browser.as_ref().and_then(|b| b.binary_path.clone()),
        false,
    ).await?;
    let _ = page.goto(&file_url).await?.wait_for_navigation().await;

    let mut agent_real = Agent::new(config, task);
    let _ = agent_real.run(&page).await;
    
    mew_cdp::shutdown(browser, handler).await?;
    Ok(())
}
