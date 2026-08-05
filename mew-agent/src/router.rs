use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Intent {
    Chat(String),
    BrowserTask(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
struct ClassifyArgs {
    intent: String,
    reply: String,
}

pub async fn classify(
    message: &str,
    conversation_context: &[ConversationMessage],
    config: &crate::ProviderConfig,
) -> anyhow::Result<Intent> {
    let client = reqwest::Client::new();
    let url = format!("{}/chat/completions", config.opencode_zen.base_url);

    let mut messages = Vec::new();
    
    // Add a system prompt
    messages.push(serde_json::json!({
        "role": "system",
        "content": "You are an AI assistant that can either chat with the user or perform browser automation tasks. Classify the user's latest message based on their intent. If they want you to navigate to a website, open a URL, or perform actions on a web page, classify it as 'browser_task' and rephrase their request into a clear standalone task description (resolving any ambiguous pronouns like 'it' using the conversation history). Otherwise, classify it as 'chat' and provide a direct reply."
    }));
    
    // Add history
    for msg in conversation_context {
        messages.push(serde_json::json!({
            "role": msg.role,
            "content": msg.content
        }));
    }
    
    // Add the latest message
    messages.push(serde_json::json!({
        "role": "user",
        "content": message
    }));
    
    let tools = serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "classify_intent",
                "description": "Classify the user intent.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "intent": {
                            "type": "string",
                            "enum": ["chat", "browser_task"]
                        },
                        "reply": {
                            "type": "string",
                            "description": "If intent is 'chat', the direct reply to the user. If intent is 'browser_task', the clear rephrased standalone browser task description."
                        }
                    },
                    "required": ["intent", "reply"]
                }
            }
        }
    ]);
    
    let body = serde_json::json!({
        "model": config.opencode_zen.default_model,
        "messages": messages,
        "tools": tools,
        "tool_choice": {
            "type": "function",
            "function": {
                "name": "classify_intent"
            }
        }
    });

    let res = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", config.opencode_zen.api_key))
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Network error during classification: {e}"))?;

    let status = res.status();
    let text = res
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read classification response body: {e}"))?;

    if !status.is_success() {
        anyhow::bail!("API returned error status {status}: {text}");
    }

    let response_json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("Failed to parse classification JSON: {e}"))?;

    // Phase 2.2 fix: the model sometimes ignores `tool_choice: "required"`
    // and returns `finish_reason: "stop"` with a free-text reply in
    // `message.content` — this is the stochastic behavior Phase 1.2
    // evidence already documented (case 3). The model has explicitly
    // decided this is not a tool task; treat its `content` as a `chat`
    // reply rather than bailing with "No tool_calls in response", which
    // would crash every plain greeting the model decides to answer
    // conversationally instead of via the tool.
    let finish_reason = response_json["choices"][0]["finish_reason"]
        .as_str()
        .unwrap_or("");

    if finish_reason == "stop" {
        // Try the content field; if it's empty or missing, bail with
        // an informative error.
        let content = response_json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();
        if content.is_empty() {
            anyhow::bail!(
                "finish_reason=stop with empty content. Raw response: {text}"
            );
        }
        return Ok(Intent::Chat(content));
    }

    let tool_calls = response_json["choices"][0]["message"]["tool_calls"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("No tool_calls in response. Raw response: {text}"))?;

    let call = tool_calls
        .first()
        .ok_or_else(|| anyhow::anyhow!("Empty tool_calls array"))?;

    let args_str = call["function"]["arguments"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Tool call arguments is not a string"))?;

    let args: ClassifyArgs = serde_json::from_str(args_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse tool call arguments: {e}. Raw args: {args_str}"))?;

    if args.intent == "browser_task" {
        Ok(Intent::BrowserTask(args.reply))
    } else {
        Ok(Intent::Chat(args.reply))
    }
}
