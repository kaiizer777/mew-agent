use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpencodeZenConfig {
    pub base_url: String,
    pub api_key: String,
    pub default_model: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    pub opencode_zen: OpencodeZenConfig,
}

pub fn load_config() -> anyhow::Result<ProviderConfig> {
    let config_path = "config.yaml";
    let content = std::fs::read_to_string(config_path)
        .map_err(|e| anyhow::anyhow!("Failed to read config file '{config_path}': {e}"))?;
    let config: ProviderConfig = serde_yaml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse config file '{config_path}': {e}"))?;
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
