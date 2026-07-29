use mew_agent::agent::Agent;
use mew_agent::load_config;
use std::env;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Suppress heavy logs unless testing
    // tracing_subscriber::fmt::init();
    
    let config = load_config()?;
    let cwd = env::current_dir().unwrap();
    let file_url = format!("file:///{}/test_vision.html", cwd.display().to_string().replace("\\", "/"));
    
    let (browser, page, handler) = mew_cdp::launch(
        config.browser.as_ref().and_then(|b| b.binary_path.clone()),
        false,
    ).await?;
    
    // Navigate
    println!("Navigating to test page...");
    mew_cdp::navigate(&page, &file_url).await?;
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    
    // Task 1: Normal button
    println!("\n=== TASK 1: Click the 'Click Me Normal' button ===");
    let mut agent1 = Agent::new(config.clone(), "Click the 'Click Me Normal' button and then finish.");
    agent1.run(&page).await?;
    
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    
    // Task 2: Image button (vision fallback)
    println!("\n=== TASK 2: There is a small square image button on the page that has no text. Click it. ===");
    let mut agent2 = Agent::new(config.clone(), "There is a small square image button on the page that has no text. Find its ref in the accessibility tree (it may just look like an empty button or image). Use vision_inspect on it to verify it is the image button, then click it and finish.");
    agent2.run(&page).await?;
    
    mew_cdp::shutdown(browser, handler).await?;
    
    Ok(())
}
