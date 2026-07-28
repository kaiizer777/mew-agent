pub mod agent;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpencodeZenConfig {
    pub base_url: String,
    pub api_key: String,
    pub default_model: String,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
    #[serde(default)]
    pub max_tokens: Option<usize>,
    #[serde(default)]
    pub max_cost: Option<f64>,
}

fn default_max_iterations() -> usize { 15 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    pub opencode_zen: OpencodeZenConfig,
}

pub fn load_config() -> anyhow::Result<ProviderConfig> {
    let mut current_dir = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("Failed to get current directory: {e}"))?;
        
    let mut config_path = None;
    
    loop {
        let potential_path = current_dir.join("config.yaml");
        if potential_path.exists() {
            config_path = Some(potential_path);
            break;
        }
        
        if !current_dir.pop() {
            break;
        }
    }
    
    let config_path = config_path.ok_or_else(|| anyhow::anyhow!("Could not find config.yaml in current or any parent directory"))?;
    let path_str = config_path.display().to_string();
    
    let content = std::fs::read_to_string(&config_path)
        .map_err(|e| anyhow::anyhow!("Failed to read config file '{path_str}': {e}"))?;
    let config: ProviderConfig = serde_yaml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse config file '{path_str}': {e}"))?;
    Ok(config)
}

pub async fn smoke_test(config: &ProviderConfig) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let url = format!("{}/chat/completions", config.opencode_zen.base_url);

    let body = serde_json::json!({
        "model": config.opencode_zen.default_model,
        "messages": [
            {
                "role": "user",
                "content": "Reply with exactly one word: hello"
            }
        ]
    });

    let res = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", config.opencode_zen.api_key))
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("HTTP request failed to {url}: {e}"))?;

    let status = res.status();
    let text = res
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read response body: {e}"))?;

    if !status.is_success() {
        println!("{text}");
        anyhow::bail!("API returned error status {status}: {text}");
    }

    println!("{text}");
    Ok(())
}
