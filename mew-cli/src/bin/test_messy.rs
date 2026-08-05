use mew_agent::load_config;
use mew_agent::agent::Agent;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = load_config()?;
    // println!("Running Wikipedia Run 2...");
    // let task_wiki = "Navigate to https://en.wikipedia.org. Type 'Oxidation' in the search bar and press Enter. Then click on 'Electron transfer'.";
    // let (browser, page, handler, job) = mew_cdp::launch(config.browser.as_ref().and_then(|b| b.binary_path.clone())).await?;
    // let mut agent_wiki = Agent::new(config.clone(), task_wiki);
    // if let Err(e) = agent_wiki.run(&page).await {
    //     eprintln!("Agent wiki error: {}", e);
    // }
    // mew_cdp::shutdown(browser, handler, job).await?;
    
    println!("Running The Verge Run 2...");
    let task_verge = "Navigate to https://www.theverge.com/. Click the consent banner if it appears. Then click on the first article link.";
    let (browser2, page2, handler2, job2) = mew_cdp::launch(
        config.browser.as_ref().and_then(|b| b.binary_path.clone()),
        false,
    ).await?;
    let mut agent_verge = Agent::new(config, task_verge, None);
    if let Err(e) = agent_verge.run(&page2).await {
        eprintln!("Agent verge error: {}", e);
    }
    mew_cdp::shutdown(browser2, handler2, job2).await?;
    
    Ok(())
}
