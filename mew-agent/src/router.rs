use serde::{Deserialize, Serialize};

pub use mew_memory::TaskShape;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Intent {
    Chat(String),
    BrowserTask {
        task: String,
        shape: TaskShape,
        job_query: Option<crate::handoff::JobQuery>,
        lead_query: Option<crate::handoff::LeadQuery>,
        confidence: Option<String>,
        is_ambiguous: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
struct ClassifyArgs {
    intent: String,
    #[serde(default)]
    task_shape: Option<TaskShape>,
    reply: String,
    #[serde(default)]
    confidence: Option<String>,
    #[serde(default)]
    is_ambiguous: Option<bool>,
    #[serde(default)]
    job_query: Option<crate::handoff::JobQuery>,
    #[serde(default)]
    lead_query: Option<crate::handoff::LeadQuery>,
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
        "content": "You are an AI assistant that can either chat with the user or perform browser automation tasks. Classify the user's latest message based on their intent. If they want you to navigate to a website, open a URL, search for jobs, find SaaS leads/clients, or run a Google Maps local-business lead generation task (e.g. 'salons in Mumbai with no website'), classify it as 'browser_task' (or 'job_search' / 'lead_search' / 'local_lead_gen') and rephrase their request into a clear standalone task description (resolving any ambiguous pronouns like 'it' using the conversation history). Determine the specific task_shape (job_search, lead_gen, local_lead_gen, generic_research, generic_browse), classification confidence (high, medium, low), and whether the request is ambiguous or mixes multiple distinct task shapes (is_ambiguous). Otherwise, classify it as 'chat' and provide a direct reply."
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
                "description": "Classify the user intent and identify the browser task shape.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "intent": {
                            "type": "string",
                            "enum": ["chat", "browser_task", "job_search", "lead_search", "local_lead_gen"]
                        },
                        "task_shape": {
                            "type": "string",
                            "enum": ["job_search", "lead_gen", "local_lead_gen", "generic_research", "generic_browse"],
                            "description": "Specific shape of browser task: job_search (jobs/internships), lead_gen (SaaS leads/contacts), local_lead_gen (Google Maps local-business lead generation, e.g. 'salons in Mumbai with no website'), generic_research (multi-source research), generic_browse (single page navigation)."
                        },
                        "reply": {
                            "type": "string",
                            "description": "If intent is 'chat', the direct reply to the user. If intent is 'browser_task', 'job_search', or 'lead_search', the clear rephrased standalone browser task description."
                        },
                        "confidence": {
                            "type": "string",
                            "enum": ["high", "medium", "low"],
                            "description": "Confidence level in classification and shape determination."
                        },
                        "is_ambiguous": {
                            "type": "boolean",
                            "description": "Set to true if request mixes multiple distinct task shapes or is genuinely ambiguous."
                        },
                        "job_query": {
                            "type": "object",
                            "description": "If the user is looking for jobs or internships, extract their search constraints.",
                            "properties": {
                                "roles": { "type": "array", "items": { "type": "string" } },
                                "locations": { "type": "array", "items": { "type": "string" } },
                                "is_remote": { "type": "boolean" },
                                "is_internship": { "type": "boolean" },
                                "experience_level": { "type": "string" }
                            }
                        },
                        "lead_query": {
                            "type": "object",
                            "description": "If the user is looking for SaaS leads, clients, or company decision makers, extract their lead search constraints.",
                            "properties": {
                                "industry": { "type": "array", "items": { "type": "string" } },
                                "geography": { "type": "array", "items": { "type": "string" } },
                                "target_role_titles": { "type": "array", "items": { "type": "string" } },
                                "product_category_keywords": { "type": "array", "items": { "type": "string" } }
                            }
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
        .header(
            "Authorization",
            format!("Bearer {}", config.opencode_zen.api_key),
        )
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

    let finish_reason = response_json["choices"][0]["finish_reason"]
        .as_str()
        .unwrap_or("");

    if finish_reason == "stop" {
        let content = response_json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();
        if content.is_empty() {
            anyhow::bail!("finish_reason=stop with empty content. Raw response: {text}");
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

    let args: ClassifyArgs = serde_json::from_str(args_str).map_err(|e| {
        anyhow::anyhow!("Failed to parse tool call arguments: {e}. Raw args: {args_str}")
    })?;

    if args.intent == "browser_task"
        || args.intent == "job_search"
        || args.intent == "lead_search"
        || args.intent == "local_lead_gen"
    {
        let shape = args.task_shape.unwrap_or_else(|| {
            if args.intent == "job_search" || args.job_query.is_some() {
                TaskShape::JobSearch
            } else if args.intent == "lead_search" || args.lead_query.is_some() {
                TaskShape::LeadGen
            } else if args.intent == "local_lead_gen" {
                TaskShape::LocalLeadGen
            } else {
                TaskShape::GenericResearch
            }
        });
        Ok(Intent::BrowserTask {
            task: args.reply,
            shape,
            job_query: args.job_query,
            lead_query: args.lead_query,
            confidence: args.confidence,
            is_ambiguous: args.is_ambiguous.unwrap_or(false),
        })
    } else {
        Ok(Intent::Chat(args.reply))
    }
}
