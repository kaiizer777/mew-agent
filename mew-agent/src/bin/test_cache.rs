use std::env;
use std::fs;
use serde_json::json;

#[tokio::main]
async fn main() {
    let config_str = fs::read_to_string("config.yaml").unwrap();
    let config: serde_yaml::Value = serde_yaml::from_str(&config_str).unwrap();
    let api_key = config["opencode_zen"]["api_key"].as_str().unwrap();
    let base_url = config["opencode_zen"]["base_url"].as_str().unwrap();
    let model = config["opencode_zen"]["default_model"].as_str().unwrap();

    let client = reqwest::Client::new();
    let url = format!("{}/chat/completions", base_url);

    let mut messages = vec![
        json!({
            "role": "system",
            "content": "You are a helpful assistant. Here is a very long system prompt to test caching: ".to_string() + &"apple banana cherry date elderberry fig grape ".repeat(100)
        }),
        json!({
            "role": "user",
            "content": "Hello!"
        })
    ];

    println!("Sending request 1...");
    let body1 = json!({
        "model": model,
        "messages": messages
    });

    let res1 = client.post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&body1)
        .send()
        .await
        .unwrap();

    let json1: serde_json::Value = res1.json().await.unwrap();
    println!("Response 1 usage: {}", serde_json::to_string_pretty(&json1["usage"]).unwrap());

    messages.push(json1["choices"][0]["message"].clone());
    messages.push(json!({
        "role": "user",
        "content": "How are you?"
    }));

    println!("Sending request 2...");
    let body2 = json!({
        "model": model,
        "messages": messages
    });

    let res2 = client.post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&body2)
        .send()
        .await
        .unwrap();

    let json2: serde_json::Value = res2.json().await.unwrap();
    println!("Response 2 usage: {}", serde_json::to_string_pretty(&json2["usage"]).unwrap());
}
