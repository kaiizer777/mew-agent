use crate::ProviderConfig;
use chromiumoxide::Page;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use mew_perception::state::PerceptionState;

pub struct Agent {
    config: ProviderConfig,
    messages: Vec<serde_json::Value>,
    client: reqwest::Client,
    total_tokens: usize,
    iterations: usize,
    state: Arc<Mutex<PerceptionState>>,
    session_id: String,
    current_url: Option<String>,
}

impl Agent {
    pub fn new(config: ProviderConfig, task: &str) -> Self {
        let system_prompt = "You are mew, a visible browser agent. You drive a real Chromium window. 
You can perceive pages via accessibility-tree snapshots and take actions like click, type, scroll. 
You must achieve the user's objective by observing the state, choosing a tool, and waiting for the next turn. 
When you are completely done and have the final answer or outcome, call finish() with the result.";

        let messages = vec![
            json!({
                "role": "system",
                "content": system_prompt
            }),
            json!({
                "role": "user",
                "content": format!("Task: {}", task)
            })
        ];

        Self {
            config,
            messages,
            client: reqwest::Client::new(),
            total_tokens: 0,
            iterations: 0,
            state: Arc::new(Mutex::new(PerceptionState::new())),
            session_id: "default_session".to_string(),
            current_url: None,
        }
    }

    fn get_tools_schema(&self) -> serde_json::Value {
        json!([
            {
                "type": "function",
                "function": {
                    "name": "navigate",
                    "description": "Navigate to a URL",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "url": { "type": "string" }
                        },
                        "required": ["url"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "click",
                    "description": "Click an element by its ref_id",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "ref": { "type": "string" }
                        },
                        "required": ["ref"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "type",
                    "description": "Type text into an element by its ref_id",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "ref": { "type": "string" },
                            "text": { "type": "string" }
                        },
                        "required": ["ref", "text"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "scroll",
                    "description": "Scroll the page",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "direction": { "type": "string", "enum": ["up", "down"] }
                        },
                        "required": ["direction"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "press_key",
                    "description": "Press a key",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "key": { "type": "string" }
                        },
                        "required": ["key"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "snapshot",
                    "description": "Take a snapshot to observe page changes"
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "finish",
                    "description": "Complete the task with a final result",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "result": { "type": "string" }
                        },
                        "required": ["result"]
                    }
                }
            }
        ])
    }

    fn trim_in_page_history(&mut self, k: usize) {
        // Keep all `system` and `user` messages (diffs).
        // Keep only the last `k` pairs of `assistant` and `tool` messages.
        let mut assistant_tool_indices = Vec::new();
        for (i, msg) in self.messages.iter().enumerate() {
            if let Some(role) = msg.get("role").and_then(|r| r.as_str()) {
                if role == "assistant" || role == "tool" {
                    assistant_tool_indices.push(i);
                }
            }
        }

        let keep_count = k * 2;
        if assistant_tool_indices.len() > keep_count {
            let drop_count = assistant_tool_indices.len() - keep_count;
            let indices_to_drop: std::collections::HashSet<_> = assistant_tool_indices.into_iter().take(drop_count).collect();
            
            let mut new_messages = Vec::new();
            for (i, msg) in self.messages.drain(..).enumerate() {
                if !indices_to_drop.contains(&i) {
                    new_messages.push(msg);
                }
            }
            self.messages = new_messages;
        }
    }

    pub async fn run(&mut self, page: &Page) -> anyhow::Result<String> {
        loop {
            if self.iterations >= self.config.opencode_zen.max_iterations {
                println!("Iteration limit reached ({}). Halting.", self.iterations);
                return Err(anyhow::anyhow!("Iteration limit reached"));
            }
            if let Some(max_t) = self.config.opencode_zen.max_tokens {
                if self.total_tokens >= max_t {
                    println!("Token limit reached ({} / {}). Halting.", self.total_tokens, max_t);
                    return Err(anyhow::anyhow!("Token limit reached"));
                }
            }

            self.iterations += 1;
            println!("--- Iteration {} ---", self.iterations);

            // Check for navigation
            let current_page_url = page.url().await.ok().flatten().unwrap_or_default();
            let is_navigation = if let Some(ref old_url) = self.current_url {
                old_url != &current_page_url
            } else {
                true
            };
            self.current_url = Some(current_page_url.clone());

            if is_navigation {
                println!("Navigation detected: Resetting history and forcing full snapshot.");
                self.messages.truncate(2); // Keep only system (0) and task (1)
            } else {
                // Justify K=5: 5 recent actions provide enough short-term memory (e.g. opened dropdown, scrolled down, typed input) 
                // to continue the task without hallucinating or losing the immediate thread of action, while dropping older stale results.
                self.trim_in_page_history(5);
            }

            // Step 1: Perceive state
            let observation = {
                let mut state = self.state.lock().await;
                
                let mut tree_res = mew_perception::extract_tree(page, true).await;
                let mut retries = 0;
                while tree_res.is_err() && retries < 10 {
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    tree_res = mew_perception::extract_tree(page, true).await;
                    retries += 1;
                }
                
                let (tree, ref_map, _) = tree_res?;
                
                let mut is_full_replace = false;
                let mut computed_diff = None;

                if !is_navigation {
                    if let Some(prev) = state.get_previous_tree(&self.session_id) {
                        let diff = mew_perception::diff::compute_diff(prev, &tree);
                        // Heuristic for full page replacement
                        if diff.removed.len() > 50 && diff.added.len() > 50 {
                            is_full_replace = true;
                        } else {
                            computed_diff = Some(diff);
                        }
                    }
                }

                if is_full_replace {
                    println!("Full page replace detected via diff: Resetting history and forcing full snapshot.");
                    self.messages.truncate(2);
                }

                let obs_text = if is_navigation || is_full_replace || state.get_previous_tree(&self.session_id).is_none() {
                    mew_perception::diff::serialize_full_tree(&tree)
                } else {
                    computed_diff.unwrap().serialize_compact()
                };
                
                state.save_tree(&self.session_id, tree);
                (obs_text, ref_map)
            };

            let (obs_text, ref_map) = observation;
            println!("--- Observation Text Length: {} bytes ---", obs_text.len());
            println!("{}\n----------------------------------", obs_text);

            self.messages.push(serde_json::json!({
                "role": "user",
                "content": format!("Current page state:\n{}", obs_text)
            }));

            // Step 2: Call LLM
            let url = format!("{}/chat/completions", self.config.opencode_zen.base_url);
            let body = json!({
                "model": self.config.opencode_zen.default_model,
                "messages": self.messages,
                "tools": self.get_tools_schema(),
                "tool_choice": "auto"
            });

            let res = self.client.post(&url)
                .header("Authorization", format!("Bearer {}", self.config.opencode_zen.api_key))
                .json(&body)
                .send()
                .await?;

            if !res.status().is_success() {
                let err = res.text().await?;
                anyhow::bail!("LLM API returned error: {}", err);
            }

            let res_json: serde_json::Value = res.json().await?;
            
            if let Some(usage) = res_json.get("usage") {
                let step_total = usage.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let step_prompt = usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let step_completion = usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                
                let mut cached = 0;
                if let Some(c) = usage.get("prompt_tokens_details").and_then(|v| v.get("cached_tokens")).and_then(|v| v.as_u64()) {
                    cached = c;
                } else if let Some(c) = usage.get("cached_tokens").and_then(|v| v.as_u64()) {
                    cached = c;
                }
                
                self.total_tokens += step_total as usize;
                
                println!("--- Token Usage for Step {} ---", self.iterations);
                println!("Raw usage response: {}", serde_json::to_string_pretty(usage).unwrap_or_default());
                println!("Prompt: {} (Cached: {}), Completion: {}, Total this step: {}", step_prompt, cached, step_completion, step_total);
                let billed_input = step_prompt.saturating_sub(cached);
                println!("Effective (Billed) Input Tokens: {}", billed_input);
                println!("Session Cumulative Total: {}", self.total_tokens);
                println!("-------------------------------");
            }

            let message = &res_json["choices"][0]["message"];
            let mut assistant_msg = message.clone();
            // Remove reasoning arrays/objects if any since they can break strict schemas occasionally on next turns
            if let Some(obj) = assistant_msg.as_object_mut() {
                obj.remove("reasoning_details");
            }
            self.messages.push(assistant_msg.clone());

            let tool_calls = message.get("tool_calls").and_then(|v| v.as_array());
            if let Some(calls) = tool_calls {
                if calls.is_empty() {
                    // No tools? fallback
                    self.messages.push(json!({
                        "role": "user",
                        "content": "You didn't call any tools. Please use a tool to proceed."
                    }));
                    continue;
                }

                // Process first tool call (single action per turn for now)
                let call = &calls[0];
                let call_id = call["id"].as_str().unwrap_or("unknown_id");
                let func = &call["function"];
                let name = func["name"].as_str().unwrap_or("");
                let args_str = func["arguments"].as_str().unwrap_or("{}");
                
                println!("LLM Called Tool: {} with args: {}", name, args_str);

                let args: serde_json::Value = serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
                let mut tool_result = String::new();

                let normalize_ref = |r: &str| -> String {
                    if !r.starts_with('@') { format!("@{}", r) } else { r.to_string() }
                };

                match name {
                    "navigate" => {
                        let url = args["url"].as_str().unwrap_or("");
                        match mew_cdp::navigate(page, url).await {
                            Ok(_) => {
                                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                                tool_result = "Navigated successfully".to_string();
                            },
                            Err(e) => tool_result = format!("Failed to navigate: {}", e),
                        }
                    },
                    "click" => {
                        let r = args["ref"].as_str().unwrap_or("");
                        let r_norm = normalize_ref(r);
                        if let Some(backend_id) = ref_map.get(&r_norm) {
                            match mew_cdp::click_ref(page, backend_id.clone()).await {
                                Ok(_) => {
                                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                                    tool_result = "Clicked successfully".to_string();
                                },
                                Err(e) => tool_result = format!("Failed to click: {}", e),
                            }
                        } else {
                            tool_result = format!("ref_id {} (normalized: {}) not found on page", r, r_norm);
                            println!("{}", tool_result);
                        }
                    },
                    "type" => {
                        let r = args["ref"].as_str().unwrap_or("");
                        let r_norm = normalize_ref(r);
                        let text = args["text"].as_str().unwrap_or("");
                        if let Some(backend_id) = ref_map.get(&r_norm) {
                            match mew_cdp::type_ref(page, backend_id.clone(), text).await {
                                Ok(_) => tool_result = "Typed successfully".to_string(),
                                Err(e) => tool_result = format!("Failed to type: {}", e),
                            }
                        } else {
                            tool_result = format!("ref_id {} (normalized: {}) not found on page", r, r_norm);
                            println!("{}", tool_result);
                        }
                    },
                    "scroll" => {
                        let dir = args["direction"].as_str().unwrap_or("down");
                        let d = if dir == "up" { mew_cdp::ScrollDirection::Up } else { mew_cdp::ScrollDirection::Down };
                        match mew_cdp::scroll(page, d, 800).await {
                            Ok(_) => {
                                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                                tool_result = "Scrolled successfully".to_string();
                            },
                            Err(e) => tool_result = format!("Failed to scroll: {}", e),
                        }
                    },
                    "press_key" => {
                        let key = args["key"].as_str().unwrap_or("");
                        match mew_cdp::press_key(page, key).await {
                            Ok(_) => {
                                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                                tool_result = "Key pressed successfully".to_string();
                            },
                            Err(e) => tool_result = format!("Failed to press key: {}", e),
                        }
                    },
                    "snapshot" => {
                        tool_result = "Snapshot taken. Observe the new page state in the next user message.".to_string();
                    },
                    "finish" => {
                        let res = args["result"].as_str().unwrap_or("").to_string();
                        println!("Task finished with result: {}", res);
                        return Ok(res);
                    },
                    _ => {
                        tool_result = format!("Unknown tool '{}'", name);
                    }
                }

                // Append tool response
                self.messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": tool_result
                }));
            } else {
                let content = message["content"].as_str().unwrap_or("");
                println!("LLM generated text without tool call: {}", content);
                self.messages.push(json!({
                    "role": "user",
                    "content": "Please output a valid tool call."
                }));
            }
        }
    }
}
