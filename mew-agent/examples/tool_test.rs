use mew_agent::load_config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = load_config()?;
    let client = reqwest::Client::new();
    let url = format!("{}/chat/completions", config.opencode_zen.base_url);

    let body = serde_json::json!({
        "model": config.opencode_zen.default_model,
        "messages": [
            {
                "role": "user",
                "content": "Please tell me the weather in San Francisco. Use the get_weather tool."
            }
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get the current weather for a given location",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "location": {
                                "type": "string",
                                "description": "The city and state, e.g. San Francisco, CA"
                            }
                        },
                        "required": ["location"]
                    }
                }
            }
        ],
        "tool_choice": "auto"
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
        anyhow::bail!("API returned error status {status}: {text}");
    }

    println!("{}", text);
    Ok(())
}
