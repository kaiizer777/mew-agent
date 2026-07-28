use crate::ProviderConfig;
use crate::chat::{MessageBus, UserMessage};
use crate::session::{SessionError, SessionHandle};
use chromiumoxide::Page;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;
use std::io::Write;
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
    transcript_file: Option<std::fs::File>,
    force_snapshot: bool,
    /// Phase 12.1: explicit state-machine handle. Cloned cheaply and handed to
    /// any external thread (UI, signal handler, Step 13 chat reader) that needs
    /// to pause/resume/stop the loop. The ReAct loop calls `checkpoint()` on
    /// this between iterations.
    session: SessionHandle,
    /// Phase 13.1: live chat channel. The CLI calls `take_message_sender()`
    /// to get the sender half; the ReAct loop drains the receiver at every
    /// checkpoint via `drain_user_messages()`. Wrapped in `Option` so the
    /// loop can detect "no bus attached" and skip the drain entirely.
    /// Sized to None after `take_message_sender` so we don't double-take.
    bus: Option<MessageBus>,
}

impl Agent {
    pub fn new(config: ProviderConfig, task: &str) -> Self {
        let system_prompt = "You are mew, a visible browser agent. You drive a real Chromium window.
You can perceive pages via accessibility-tree snapshots and take actions like click, type, scroll.
If you need to interact with an element that has no meaningful accessible name/role (e.g. an empty button or canvas), use the `vision_inspect` tool first to visually inspect it.
You must achieve the user's objective by observing the state, choosing a tool, and waiting for the next turn.
When you are completely done and have the final answer or outcome, call finish() with the result. CRITICAL RULE: If an action times out or fails (e.g., stale reference), NEVER report in finish() that the action 'was performed', 'succeeded', or 'was completed'. You must explicitly state that you attempted the action and it failed/timed out, and do not conflate the attempt with success.";

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

        let session_id = format!("session_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs());
        let transcript_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("transcript_{}.log", session_id))
            .ok();

        let session = SessionHandle::new(session_id.clone());

        // Record the initial "Start" transition to the transcript so the log
        // is never empty. The handle already pushed a Start record into its
        // own history; we mirror that to the file transcript here.
        if let Some(mut file) = transcript_file.as_ref() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let entry = format!(
                "[{}] [{}] STATE: -> Running (start)\n\n",
                now, session_id
            );
            let _ = file.write_all(entry.as_bytes());
        }

        Self {
            config,
            messages,
            client: reqwest::Client::new(),
            total_tokens: 0,
            iterations: 0,
            state: Arc::new(Mutex::new(PerceptionState::new())),
            session_id,
            current_url: None,
            transcript_file,
            force_snapshot: false,
            session,
            bus: Some(MessageBus::new()),
        }
    }

    /// Borrow the session handle. External code (CLI, future UI thread, the
    /// Step 13 chat reader) calls `pause()`/`resume()`/`stop()` on the
    /// returned clone.
    pub fn session_handle(&self) -> SessionHandle {
        self.session.clone()
    }

    /// Phase 13.1: hand the sender half of the message bus to a caller
    /// (typically the CLI's stdin reader thread). The agent loop keeps the
    /// receiver and drains it between iterations. Called once at startup,
    /// before `run()`. Returns the tokio mpsc sender — clones are cheap,
    /// so a future UI thread or signal handler can also call this if
    /// multiple input sources are needed.
    pub fn take_message_sender(&mut self) -> tokio::sync::mpsc::Sender<UserMessage> {
        let bus = self
            .bus
            .as_mut()
            .expect("MessageBus already taken or not initialized");
        bus.take_sender()
    }

    /// Phase 13.1: borrow the bus (mutable) so the loop can drain pending
    /// messages at the checkpoint. Returns `None` if the bus was never
    /// created (e.g. a future test that wants to run the agent with no
    /// chat input at all).
    fn bus_mut(&mut self) -> Option<&mut MessageBus> {
        self.bus.as_mut()
    }

    /// Phase 13.1: test/integration helper. Public so the example tests
    /// in `mew-agent/examples/` can drive the real `Agent`'s chat path
    /// without spinning up Chrome. The CLI never calls this — the real
    /// loop calls `drain_and_apply_user_messages` directly each
    /// iteration.
    pub fn drain_user_messages(&mut self) -> Vec<UserMessage> {
        match self.bus.as_mut() {
            Some(b) => b.drain_pending(),
            None => Vec::new(),
        }
    }

    /// Phase 13.1: test/integration helper. Append a user note exactly
    /// the way `drain_and_apply_user_messages` does, so example tests
    /// can drive the real Agent and inspect the resulting history.
    /// Not called by the real loop — it goes through the drain path.
    pub fn apply_user_message_for_test(&mut self, msg: &UserMessage) {
        self.messages.push(json!({
            "role": "user",
            "content": format!("[user note while task is running] {}", msg.text),
        }));
        if let Some(mut file) = self.transcript_file.as_ref() {
            let line = format!(
                "[{}] [{}] USER: {}\n\n",
                msg.timestamp_secs, self.session_id, msg.text
            );
            let _ = file.write_all(line.as_bytes());
        }
    }

    /// Phase 13.1: test/integration helper. Read-only view of the
    /// conversation history. Used by example tests to assert messages
    /// were appended.
    pub fn history_snapshot(&self) -> Vec<serde_json::Value> {
        self.messages.clone()
    }

    /// Read-only accessor for the session id. Used by example tests
    /// to find the transcript file path on disk.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Phase 13.1: test/integration helper. Trigger the same
    /// truncate-with-notes-preservation the real loop uses on
    /// navigation. Used by example tests to assert the preservation
    /// behavior on the real Agent, not just on a stand-in simulator.
    pub fn truncate_for_test(&mut self, keep_front: usize) {
        self.truncate_preserving_user_notes(keep_front);
    }

    /// Best-effort: write a state transition line to the transcript file,
    /// using the format defined on `SessionHandle::format_transition_line` so
    /// tests and the live agent agree on the exact shape.
    fn write_state_line(
        file: Option<&std::fs::File>,
        record: &crate::session::TransitionRecord,
        session_id: &str,
    ) {
        if let Some(mut f) = file {
            let reason_part = record
                .reason
                .as_deref()
                .map(|r| format!(" reason={}", r))
                .unwrap_or_default();
            let line = format!(
                "[{}] [{}] STATE: {} -> {} ({}){}\n\n",
                record.timestamp_secs,
                session_id,
                record.from.as_str(),
                record.to.as_str(),
                record.kind.as_str(),
                reason_part
            );
            let _ = f.write_all(line.as_bytes());
        }
    }

    /// Phase 13.1: drain any pending user-typed messages and fold them into
    /// the running conversation history as `role: user` entries, plus log
    /// them to the transcript. The LLM on the *next* iteration sees them
    /// alongside the freshest snapshot/diff, with no task restart, no
    /// state wipe, and no special-cased "injection" system prompt.
    ///
    /// Empty-drain is a true no-op: this is the per-iteration path when the
    /// user has typed nothing, and it must add zero latency and zero log
    /// noise (per the 13.2 spec).
    /// `pub` so the example tests in `mew-agent/examples/` can
    /// drive the real per-iteration drain path the live loop uses. The
    /// CLI never calls this directly — only the ReAct loop in `run_inner`.
    pub fn drain_and_apply_user_messages(&mut self) {
        // No bus attached => no chat input => nothing to do.
        let pending = match self.bus_mut() {
            Some(b) => b.drain_pending(),
            None => return,
        };
        if pending.is_empty() {
            return;
        }

        // Count so the println + transcript can show "N messages applied"
        // without listing every line on the console (which would be noisy
        // for a 4-message burst).
        let count = pending.len();
        println!(
            "[chat] {} pending user message(s) from stdin — folding into history",
            count
        );

        for msg in pending {
            // Echo to the console so the user sees their own message land
            // in the conversation, in order. This is the "you can type here
            // anytime" affordance working as intended.
            println!("[chat] user: {}", msg.text);

            // Append to the live history as a real `role: user` message.
            // The system prompt is unchanged — this is exactly how the
            // user-typed first message got in, so subsequent injections
            // look identical to the LLM.
            self.messages.push(json!({
                "role": "user",
                "content": format!("[user note while task is running] {}", msg.text),
            }));

            // Mirror to the transcript file. Same shape as a tool call
            // entry, so the transcript can be searched with the same tools.
            if let Some(mut file) = self.transcript_file.as_ref() {
                let line = format!(
                    "[{}] [{}] USER: {}\n\n",
                    msg.timestamp_secs, self.session_id, msg.text
                );
                let _ = file.write_all(line.as_bytes());
            }
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
                    "name": "vision_inspect",
                    "description": "Inspect a region visually if it lacks an accessible name/role. Returns visual description.",
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

    /// Phase 13.1: when a navigation (or full-replace) wipes most of the
    /// conversation history, preserve any user-typed steering notes so the
    /// LLM in the next iteration still sees them. Without this helper, a
    /// note typed just before a navigation would be silently dropped by
    /// `truncate(2)`, defeating the "no state wipe" guarantee.
    ///
    /// The convention is the prefix written in `drain_and_apply_user_messages`.
    /// Anything that doesn't match the system / task / steering-note shape
    /// is dropped along with the rest of the trimmed window.
    const USER_NOTE_PREFIX: &'static str = "[user note while task is running]";

    fn truncate_preserving_user_notes(&mut self, keep_front: usize) {
        if self.messages.len() <= keep_front {
            return;
        }
        let head: Vec<serde_json::Value> = self.messages.drain(..keep_front).collect();
        let kept_notes: Vec<serde_json::Value> = self
            .messages
            .iter()
            .filter(|m| {
                m.get("role").and_then(|r| r.as_str()) == Some("user")
                    && m.get("content")
                        .and_then(|c| c.as_str())
                        .map(|c| c.starts_with(Self::USER_NOTE_PREFIX))
                        .unwrap_or(false)
            })
            .cloned()
            .collect();
        let mut rebuilt = head;
        rebuilt.extend(kept_notes);
        self.messages = rebuilt;
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
        // Outer wrapper so the state machine always reflects how the loop
        // exited. Even if `loop { ... }` is broken out of unexpectedly, we
        // mark Done/Failed based on the result and log the transition.
        let run_result = self.run_inner(page).await;

        // Transition to terminal state based on how we exited, and log it.
        let terminal = match &run_result {
            Ok(_) => self.session.complete().await,
            Err(e) => {
                let reason = format!("loop terminated: {}", e);
                self.session.fail(reason).await
            }
        };

        // If the terminal transition itself succeeded, log it. If it failed
        // (e.g. loop was already stopped by a caller), fall through and just
        // log whatever the latest recorded history says.
        match terminal {
            Ok(_) => {
                let history = self.session.history().await;
                if let Some(last) = history.last() {
                    let f = self.transcript_file.as_ref();
                    Self::write_state_line(f, last, &self.session_id);
                }
            }
            Err(SessionError::TerminalState(_)) => {
                // External stop() beat us to the terminal; the caller already
                // logged their own transition. Nothing more to do.
            }
            Err(e) => {
                eprintln!("warning: could not record terminal state: {}", e);
            }
        }

        run_result
    }

    async fn run_inner(&mut self, page: &Page) -> anyhow::Result<String> {
        loop {
            // Phase 12.1: every iteration starts by checking the state
            // machine. `Running` is a fast no-op (returns immediately). A
            // `Paused` state parks here until `resume()`. A terminal state
            // (Stopped/Done/Failed) breaks the loop with a typed error so the
            // outer wrapper can record the right transition.
            //
            // Wired in at the *top* of the loop on top of the existing
            // iteration-limit / token-limit checks. The spec also requires a
            // checkpoint right after the tool call is parsed; that one lives
            // further down inside the `if let Some(calls) = tool_calls` block.
            if let Err(e) = self.session.checkpoint().await {
                // The session moved to a terminal state (Stopped/Done/Failed)
                // or some other cancellation. Surface as a regular Err so the
                // outer `run` records `Failed` with the reason.
                return Err(anyhow::anyhow!(e.to_string()));
            }

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
                // Phase 13.1: preserve any pending user-typed steering notes
                // across the navigation reset, per the "no state wipe" rule.
                self.truncate_preserving_user_notes(2); // Keep system (0) and task (1) plus any user notes
            } else {
                // Justify K=5: 5 recent actions provide enough short-term memory (e.g. opened dropdown, scrolled down, typed input)
                // to continue the task without hallucinating or losing the immediate thread of action, while dropping older stale results.
                self.trim_in_page_history(5);
            }

            // Phase 13.1: drain the live chat channel *after* navigation has
            // potentially reset history, so user-typed notes land in the
            // post-truncate history and the LLM in this very iteration
            // sees them alongside the freshest page state. This is the
            // "no task restart, no state wipe" guarantee from the spec.
            //
            // Non-blocking (try_recv under the hood of `drain_pending`) so
            // the default is "steer while running" — a blocking recv here
            // would let the user pause the agent by simply not typing.
            // Empty-drain is a true no-op per the 13.2 checklist.
            self.drain_and_apply_user_messages();

            // Step 1: Perceive state
            let observation = {
                let mut state = self.state.lock().await;

                let mut tree_res = tokio::time::timeout(tokio::time::Duration::from_secs(1), mew_perception::extract_tree(page, true)).await.unwrap_or_else(|_| Err(anyhow::anyhow!("Timeout extracting tree")));
                let mut retries = 0;
                while tree_res.is_err() && retries < 3 {
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                    tree_res = tokio::time::timeout(tokio::time::Duration::from_secs(1), mew_perception::extract_tree(page, true)).await.unwrap_or_else(|_| Err(anyhow::anyhow!("Timeout extracting tree")));
                    retries += 1;
                }

                let (tree, ref_map, _) = match tree_res {
                    Ok(res) => res,
                    Err(e) => {
                        println!("Failed to extract tree after retries: {}", e);
                        (mew_perception::TreeNode {
                            id: "0".to_string(),
                            role: "RootWebArea".to_string(),
                            name: "Error: Failed to load page state".to_string(),
                            value: "".to_string(),
                            category: mew_perception::NodeCategory::Structural,
                            ref_id: None,
                            backend_node_id: None,
                            children: vec![],
                        }, std::collections::HashMap::new(), std::time::Duration::from_secs(0))
                    }
                };

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

                let obs_text = if is_navigation || is_full_replace || self.force_snapshot || state.get_previous_tree(&self.session_id).is_none() {
                    mew_perception::diff::serialize_full_tree(&tree)
                } else {
                    computed_diff.unwrap().serialize_compact()
                };

                self.force_snapshot = false;

                state.save_tree(&self.session_id, tree);
                (obs_text, ref_map, is_full_replace)
            };

            // The perception block returned whether the diff heuristic
            // detected a full-page replace. If it did, reset history
            // *now* — outside the `state` lock, so we can mutate
            // `self.messages` without borrowing conflicts.
            //
            // Phase 13.1: use the note-preserving truncate so a user-
            // typed steering note isn't lost across the reset.
            let (obs_text, ref_map, is_full_replace) = observation;
            if is_full_replace {
                println!("Full page replace detected via diff: Resetting history and forcing full snapshot.");
                self.truncate_preserving_user_notes(2);
            }

            let obs_summary = format!("Observation: {} bytes", obs_text.len());
            println!("--- {} ---", obs_summary);
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

            let mut res_json: Option<serde_json::Value> = None;
            let mut backoff = 2;
            for attempt in 1..=5 {
                let res = self.client.post(&url)
                    .header("Authorization", format!("Bearer {}", self.config.opencode_zen.api_key))
                    .json(&body)
                    .send()
                    .await;
                
                match res {
                    Ok(response) if response.status().is_success() => {
                        res_json = Some(response.json().await?);
                        break;
                    },
                    Ok(response) => {
                        let err = response.text().await.unwrap_or_default();
                        println!("LLM API returned error (attempt {}): {}", attempt, err);
                    },
                    Err(e) => {
                        println!("LLM API request failed (attempt {}): {}", attempt, e);
                    }
                }
                if attempt < 5 {
                    println!("Retrying LLM API in {} seconds...", backoff);
                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff)).await;
                    backoff *= 2;
                }
            }

            let res_json = match res_json {
                Some(j) => j,
                None => anyhow::bail!("LLM API returned error consistently after 5 attempts"),
            };
            
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

                // Phase 12.1: checkpoint after the tool call is parsed, before
                // it is executed. This is the point where pause()/resume()/
                // stop() can safely interrupt without leaving the browser in a
                // half-mutated state. We log the current state so a pause
                // shows up in the transcript with a real timestamp; the resume
                // line is logged the next time the state changes back to
                // Running.
                let pre_exec_state = self.session.state().await;
                if let Err(e) = self.session.checkpoint().await {
                    // If the session went terminal (e.g. caller called
                    // stop() between the LLM response and the dispatch), log
                    // the transition we observed and bail.
                    let post = self.session.history().await;
                    if let Some(last) = post.last() {
                        if last.from != last.to {
                            let f = self.transcript_file.as_ref();
                            Self::write_state_line(f, last, &self.session_id);
                        }
                    }
                    let _ = pre_exec_state; // documented, retained for the next state read
                    return Err(anyhow::anyhow!(e.to_string()));
                }
                // If state changed through the checkpoint (Running -> Paused ->
                // Running or Running -> Stopped), log it. We use a snapshot of
                // the history because the post-checkpoint state is the one the
                // loop will actually use.
                let post_history = self.session.history().await;
                if let Some(last) = post_history.last() {
                    if last.from != last.to {
                        let f = self.transcript_file.as_ref();
                        Self::write_state_line(f, last, &self.session_id);
                    }
                }

                let args: serde_json::Value = serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
                let mut tool_result = String::new();

                let normalize_ref = |r: &str| -> String {
                    if !r.starts_with('@') { format!("@{}", r) } else { r.to_string() }
                };

                match name {
                    "navigate" => {
                        let url = args["url"].as_str().unwrap_or("");
                        
                        if url.contains("force-crash") {
                            anyhow::bail!("Deliberately forced crash mid-task!");
                        }
                        
                        // Domain allowlist check
                        let mut allowed = true;
                        if let Some(ref allowed_domains) = self.config.agent.allowed_domains {
                            if let Ok(parsed_url) = url::Url::parse(url) {
                                if let Some(domain) = parsed_url.domain() {
                                    if !allowed_domains.iter().any(|d| domain == d || domain.ends_with(&format!(".{}", d))) {
                                        allowed = false;
                                    }
                                }
                            }
                        }

                        if !allowed {
                            tool_result = format!("ERROR: Action failed (Navigation). Domain for URL '{}' is not in the allowlist. Do not attempt this domain.", url);
                        } else {
                            let mut success = false;
                            let mut backoff = 1;
                            for attempt in 1..=3 {
                                match tokio::time::timeout(tokio::time::Duration::from_secs(15), mew_cdp::navigate(page, url)).await {
                                    Ok(Ok(_)) => {
                                        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                                        tool_result = "Navigated successfully".to_string();
                                        success = true;
                                        break;
                                    },
                                    Ok(Err(e)) => tool_result = format!("ERROR: Action failed (Navigation). {}. Do not assume success.", e),
                                    Err(_) => tool_result = "ERROR: Action failed (Timeout while navigating). The navigation timed out and did not complete successfully. Do not assume your action succeeded.".to_string(),
                                }
                                if attempt < 3 && !success { 
                                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff)).await; 
                                    backoff *= 2; 
                                }
                            }
                        }
                    },
                    "click" => {
                        let r = args["ref"].as_str().unwrap_or("");
                        let r_norm = normalize_ref(r);
                        if let Some(backend_id) = ref_map.get(&r_norm) {
                            let mut success = false;
                            let mut backoff = 1;
                            for attempt in 1..=3 {
                                match tokio::time::timeout(tokio::time::Duration::from_secs(1), mew_cdp::click_ref(page, backend_id.clone())).await {
                                    Ok(Ok(_)) => {
                                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                                        tool_result = "Clicked successfully".to_string();
                                        success = true;
                                        break;
                                    },
                                    Ok(Err(e)) => {
                                        if let mew_cdp::StaleRefError::NotFound(_) = e {
                                            tool_result = "ERROR: Action failed (Stale Element Reference). The element was removed from the DOM before your action could execute. Your action had NO effect. Do not assume your action succeeded. Re-evaluate the new page state and decide your next step.".to_string();
                                            self.force_snapshot = true;
                                            success = true;
                                            break;
                                        } else {
                                            tool_result = format!("ERROR: Action failed (Click). {}. Do not assume success.", e);
                                        }
                                    },
                                    Err(_) => tool_result = "ERROR: Action failed (Timeout while clicking). The click action timed out and did not complete successfully. Do not assume your action succeeded.".to_string(),
                                }
                                if attempt < 3 && !success { 
                                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff)).await; 
                                    backoff *= 2; 
                                }
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
                            let mut success = false;
                            let mut backoff = 1;
                            for attempt in 1..=3 {
                                match tokio::time::timeout(tokio::time::Duration::from_secs(15), mew_cdp::type_ref(page, backend_id.clone(), text)).await {
                                    Ok(Ok(_)) => {
                                        tool_result = "Typed successfully".to_string();
                                        success = true;
                                        break;
                                    },
                                    Ok(Err(e)) => {
                                        if let mew_cdp::StaleRefError::NotFound(_) = e {
                                            tool_result = "ERROR: Action failed (Stale Element Reference). The element was removed from the DOM before your action could execute. Your action had NO effect. Do not assume your action succeeded. Re-evaluate the new page state and decide your next step.".to_string();
                                            self.force_snapshot = true;
                                            success = true;
                                            break;
                                        } else {
                                            tool_result = format!("ERROR: Action failed (Type). {}. Do not assume success.", e);
                                        }
                                    },
                                    Err(_) => tool_result = "ERROR: Action failed (Timeout while typing). The type action timed out and did not complete successfully. Do not assume your action succeeded.".to_string(),
                                }
                                if attempt < 3 && !success { 
                                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff)).await; 
                                    backoff *= 2; 
                                }
                            }
                        } else {
                            tool_result = format!("ref_id {} (normalized: {}) not found on page", r, r_norm);
                            println!("{}", tool_result);
                        }
                    },
                    "scroll" => {
                        let dir = args["direction"].as_str().unwrap_or("down");
                        let d = if dir == "up" { mew_cdp::ScrollDirection::Up } else { mew_cdp::ScrollDirection::Down };
                        match tokio::time::timeout(tokio::time::Duration::from_secs(15), mew_cdp::scroll(page, d, 800)).await {
                            Ok(Ok(_)) => {
                                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                                tool_result = "Scrolled successfully".to_string();
                            },
                            Ok(Err(e)) => tool_result = format!("ERROR: Action failed (Scroll). {}. Do not assume success.", e),
                            Err(_) => tool_result = "ERROR: Action failed (Timeout while scrolling). The scroll action timed out and did not complete successfully. Do not assume your action succeeded.".to_string(),
                        }
                    },
                    "press_key" => {
                        let key = args["key"].as_str().unwrap_or("");
                        match tokio::time::timeout(tokio::time::Duration::from_secs(15), mew_cdp::press_key(page, key)).await {
                            Ok(Ok(_)) => {
                                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                                tool_result = "Key pressed successfully".to_string();
                            },
                            Ok(Err(e)) => tool_result = format!("ERROR: Action failed (Press Key). {}. Do not assume success.", e),
                            Err(_) => tool_result = "ERROR: Action failed (Timeout while pressing key). The press_key action timed out and did not complete successfully. Do not assume your action succeeded.".to_string(),
                        }
                    },
                    "snapshot" => {
                        tool_result = "Snapshot taken. Observe the new page state in the next user message.".to_string();
                    },
                    "vision_inspect" => {
                        let r = args["ref"].as_str().unwrap_or("");
                        let r_norm = normalize_ref(r);
                        if let Some(backend_id) = ref_map.get(&r_norm) {
                            match tokio::time::timeout(tokio::time::Duration::from_secs(15), mew_cdp::screenshot_region(page, backend_id.clone())).await {
                                Ok(Ok((base64_data, x, y, w, h))) => {
                                    println!("Region screenshot captured: {}x{} at {},{}", w, h, x, y);
                                    let vision_body = serde_json::json!({
                                        "model": "mimo-v2.5-free",
                                        "messages": [
                                            {
                                                "role": "user",
                                                "content": [
                                                    {"type": "text", "text": "Describe what you see in this cropped UI region. Identify if it looks like a button, icon, or specific widget."},
                                                    {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", base64_data)}}
                                                ]
                                            }
                                        ]
                                    });
                                    let url = format!("{}/chat/completions", self.config.opencode_zen.base_url);
                                    let v_res_future = self.client.post(&url)
                                        .header("Authorization", format!("Bearer {}", self.config.opencode_zen.api_key))
                                        .json(&vision_body)
                                        .send();
                                        
                                    match tokio::time::timeout(tokio::time::Duration::from_secs(30), v_res_future).await {
                                        Ok(Ok(resp)) => {
                                            if let Ok(v_json) = resp.json::<serde_json::Value>().await {
                                                if let Some(desc) = v_json["choices"][0]["message"]["content"].as_str() {
                                                    tool_result = format!("Vision fallback result for region {}:\nDescription: {}\nBounds: {}x{} at {},{}", r, desc, w, h, x, y);
                                                } else {
                                                    tool_result = "Vision API returned no description".to_string();
                                                }
                                            } else {
                                                tool_result = "Vision API returned invalid JSON".to_string();
                                            }
                                        },
                                        Ok(Err(e)) => tool_result = format!("Vision API call failed: {}", e),
                                        Err(_) => tool_result = "Vision API call timed out".to_string(),
                                    }
                                },
                                Ok(Err(e)) => tool_result = format!("Failed to take screenshot of region: {}", e),
                                Err(_) => tool_result = "Timeout while taking screenshot of region".to_string(),
                            }
                        } else {
                            tool_result = format!("ref_id {} (normalized: {}) not found on page", r, r_norm);
                            println!("{}", tool_result);
                        }
                    },
                    "finish" => {
                        let res = args["result"].as_str().unwrap_or("").to_string();
                        println!("Task finished with result: {}", res);
                        // Log to transcript
                        if let Some(mut file) = self.transcript_file.as_ref() {
                            let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                            let log_entry = format!("[{}] [{}] TOOL CALL: {} (args: {})\nRESULT: Task finished: {}\n\n", timestamp, obs_summary, name, args_str, res);
                            let _ = file.write_all(log_entry.as_bytes());
                        }
                        return Ok(res);
                    },
                    _ => {
                        tool_result = format!("Unknown tool '{}'", name);
                    }
                }

                println!("Tool result: {}", tool_result);
                // Append tool response
                self.messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": &tool_result
                }));

                // Log to transcript
                if let Some(mut file) = self.transcript_file.as_ref() {
                    let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                    let log_entry = format!("[{}] [{}] TOOL CALL: {} (args: {})\nRESULT: {}\n\n", timestamp, obs_summary, name, args_str, tool_result);
                    let _ = file.write_all(log_entry.as_bytes());
                }
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
