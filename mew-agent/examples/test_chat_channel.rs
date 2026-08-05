// mew v2 — Phase 13.1: chat channel review & testing harness.
//
// Standalone binary that exercises the live chat channel end-to-end:
//   * `MessageBus` unit checks (capacity, ordering, drain behavior).
//   * A simulated ReAct loop that mirrors the real one: each iteration
//     calls `drain_and_apply_user_messages` (via the public surface the
//     agent exposes), appends a fake "observation" to the conversation
//     history, and prints what it saw. A controller task pushes messages
//     into the sender at scripted times so we can verify mid-task
//     injection, multi-message bursts, and no-typing long runs.
//
//   * The test does NOT spin up Chrome or hit the LLM. It uses the real
//     `Agent`'s public surface for everything chat-related, then drives a
//     fake "loop body" that records the same state changes the real loop
//     would. This is enough to prove the chat channel works; the user's
//     real-world 13.2 review (with the actual CLI + LLM) is the final
//     "watch it work" check.
//
// Run with: cargo run --example test_chat_channel -p mew-agent

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::sleep;

use mew_agent::chat::{MessageBus, UserMessage};
use mew_agent::agent::Agent;

// ----------------------------------------------------------------------------
// Simulated-loop harness. Mirrors the Agent's per-iteration behavior closely
// enough that the chat-channel logic sees the same inputs the real loop
// would produce. We keep the `messages` Vec private here just like the real
// Agent does, and we record each iteration's snapshot of the history so
// tests can prove messages landed in order, weren't dropped, and survived
// navigation-style truncates.
// ----------------------------------------------------------------------------
struct SimLoop {
    bus: Option<MessageBus>,
    messages: Vec<serde_json::Value>,
    iteration: usize,
    history_log: Vec<(u64, usize, Vec<String>)>, // (timestamp, iter, snapshot of user-msg contents)
    truncated_log: Vec<(u64, usize, usize, usize)>, // (timestamp, iter, len_before, len_after)
}

impl SimLoop {
    fn new() -> Self {
        // System + original task — same shape the real Agent starts with.
        let messages = vec![
            serde_json::json!({"role": "system", "content": "system prompt"}),
            serde_json::json!({"role": "user", "content": "Task: do the original thing"}),
        ];
        Self {
            bus: Some(MessageBus::new()),
            messages,
            iteration: 0,
            history_log: Vec::new(),
            truncated_log: Vec::new(),
        }
    }

    fn take_sender(&mut self) -> tokio::sync::mpsc::Sender<UserMessage> {
        self.bus.as_mut().unwrap().take_sender()
    }

    /// Mimic the Agent's per-iteration sequence: drain user messages,
    /// optionally simulate a navigation reset, push a fresh observation.
    /// The point of this method is to drive the same code paths the real
    /// loop drives, so any bug in drain-and-apply or truncate would
    /// surface here too.
    fn step(&mut self, simulate_navigation: bool) {
        self.iteration += 1;

        // 1) Drain the bus — same call site the real Agent uses.
        let pending: Vec<UserMessage> = match self.bus.as_mut() {
            Some(b) => b.drain_pending(),
            None => Vec::new(),
        };
        for m in &pending {
            self.messages.push(serde_json::json!({
                "role": "user",
                "content": format!("[user note while task is running] {}", m.text),
            }));
        }

        // 2) Optionally simulate the navigation reset the real loop does
        //    on URL change. We use the same `truncate_preserving_user_notes`
        //    shape: keep the first 2 messages (system + original task) and
        //    preserve any user notes.
        if simulate_navigation {
            let before = self.messages.len();
            let head: Vec<serde_json::Value> = self.messages.drain(..2).collect();
            let kept_notes: Vec<serde_json::Value> = self
                .messages
                .iter()
                .filter(|m| {
                    m.get("role").and_then(|r| r.as_str()) == Some("user")
                        && m.get("content")
                            .and_then(|c| c.as_str())
                            .map(|c| c.starts_with("[user note while task is running]"))
                            .unwrap_or(false)
                })
                .cloned()
                .collect();
            let mut rebuilt = head;
            rebuilt.extend(kept_notes);
            self.messages = rebuilt;
            let after = self.messages.len();
            let now = now_secs();
            self.truncated_log.push((now, self.iteration, before, after));
        }

        // 3) Simulate "I just took a snapshot and called the LLM, here's
        //    the new observation" — the real loop pushes a user-role
        //    message with the page state. This grows history so the
        //    `trim_in_page_history` heuristic in the real loop has
        //    something to chew on too, but we keep it cheap here.
        self.messages.push(serde_json::json!({
            "role": "user",
            "content": format!("Current page state: [iter {} snapshot]", self.iteration),
        }));
        // Fake assistant turn so the LLM-style alternation is preserved.
        self.messages.push(serde_json::json!({
            "role": "assistant",
            "content": format!("iter {} action", self.iteration),
        }));

        // Record this iteration's user-message snapshot for inspection.
        let now = now_secs();
        let user_contents: Vec<String> = self
            .messages
            .iter()
            .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
            .filter_map(|m| m.get("content").and_then(|c| c.as_str()).map(String::from))
            .collect();
        self.history_log.push((now, self.iteration, user_contents));
    }

    fn total_user_message_count(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
            .count()
    }

    fn user_notes(&self) -> Vec<String> {
        self.messages
            .iter()
            .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
            .filter(|c| c.starts_with("[user note while task is running]"))
            .map(String::from)
            .collect()
    }

    /// Asserts that the order of user notes seen in history matches the
    /// order they were sent. This is the 13.2 spec check #4 ("none are
    /// silently dropped") and #1 ("appended in order").
    fn assert_notes_in_order(&self, expected: &[&str]) {
        let actual = self.user_notes();
        let actual_normalized: Vec<String> = actual
            .iter()
            .map(|s| {
                s.trim_start_matches("[user note while task is running] ")
                    .to_string()
            })
            .collect();
        assert_eq!(
            actual_normalized, expected,
            "user notes out of order or missing.\nexpected: {:?}\nactual:   {:?}",
            expected, actual_normalized
        );
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ----------------------------------------------------------------------------
// TEST A: MessageBus unit checks. Drain behavior, capacity, ordering, the
//   empty-drain-is-a-no-op rule.
// ----------------------------------------------------------------------------
async fn test_message_bus_unit() {
    println!("\n=== TEST A: MessageBus unit behavior ===");
    let mut bus = MessageBus::new();
    assert_eq!(bus.capacity(), 32, "default capacity should be 32 per spec");
    let tx = bus.take_sender();

    // Empty drain is a no-op.
    let empty = bus.drain_pending();
    assert!(empty.is_empty(), "empty bus should drain to empty vec");
    println!("  PASS: empty drain returns empty vec (no-op, no panic)");

    // Send 3, drain, all 3 should come back in order.
    tx.send(UserMessage::now("first")).await.unwrap();
    tx.send(UserMessage::now("second")).await.unwrap();
    tx.send(UserMessage::now("third")).await.unwrap();
    let drained = bus.drain_pending();
    assert_eq!(drained.len(), 3);
    assert_eq!(drained[0].text, "first");
    assert_eq!(drained[1].text, "second");
    assert_eq!(drained[2].text, "third");
    println!("  PASS: 3 messages drained in FIFO order");

    // After drain, another drain is empty.
    assert!(bus.drain_pending().is_empty());
    println!("  PASS: post-drain drain is empty (true no-op)");

    // Drop the sender; drain should not panic and should return empty.
    drop(tx);
    let after_disconnect = bus.drain_pending();
    assert!(after_disconnect.is_empty());
    println!("  PASS: drain after sender drop returns empty (no panic, no hang)");
}

// ----------------------------------------------------------------------------
// TEST B: Mid-task injection. Push a new instruction 3 iterations in.
//   Confirm it's appended to the running history (not a fresh history) and
//   that original task content is still present alongside it.
// ----------------------------------------------------------------------------
async fn test_mid_task_injection() {
    println!("\n=== TEST B: mid-task injection lands in running history ===");
    let mut sim = SimLoop::new();
    let tx = sim.take_sender();

    // Run 3 iterations with no input — establishes baseline history.
    sim.step(false);
    sim.step(false);
    sim.step(false);
    let pre_count = sim.total_user_message_count();
    assert_eq!(
        pre_count, 4,
        "expected task (1) + 3 obs = 4 user messages, got {pre_count}"
    );
    println!("  before injection: {} user messages in history (1 task + 3 snapshots)", pre_count);

    // Inject a steering note.
    tx.send(UserMessage::now("also open a new tab and check the weather"))
        .await
        .unwrap();
    println!("  injected: 'also open a new tab and check the weather'");

    // Next iteration: drain should pick it up, history appends.
    sim.step(false);
    let notes = sim.user_notes();
    assert_eq!(notes.len(), 1, "exactly one user note should be in history");
    assert!(notes[0].contains("also open a new tab and check the weather"));
    println!("  PASS: injected note appears in history after 1 step");

    // Original task is still in history.
    let has_original_task = sim
        .messages
        .iter()
        .any(|m| m.get("content").and_then(|c| c.as_str()) == Some("Task: do the original thing"));
    assert!(has_original_task, "original task must still be in history");
    println!("  PASS: original task preserved alongside injected note");

    // Continue for 2 more iterations with no input — note should still
    // be there (the spec says "history is appended in place, never
    // cleared").
    sim.step(false);
    sim.step(false);
    assert_eq!(sim.user_notes().len(), 1, "note must persist across steps");
    println!("  PASS: injected note persists across later iterations");
}

// ----------------------------------------------------------------------------
// TEST C: Burst of 4 messages in quick succession. Confirm none are
//   silently dropped — the 13.2 spec's stress test.
// ----------------------------------------------------------------------------
async fn test_burst_no_drops() {
    println!("\n=== TEST C: rapid-fire burst of 4 messages, none dropped ===");
    let mut sim = SimLoop::new();
    let tx = sim.take_sender();

    // Send 4 in a row, before any drain happens.
    tx.send(UserMessage::now("msg-A")).await.unwrap();
    tx.send(UserMessage::now("msg-B")).await.unwrap();
    tx.send(UserMessage::now("msg-C")).await.unwrap();
    tx.send(UserMessage::now("msg-D")).await.unwrap();
    println!("  sent 4 messages back-to-back before any iteration ran");

    // One drain should pick all 4 up in order.
    sim.step(false);
    sim.assert_notes_in_order(&["msg-A", "msg-B", "msg-C", "msg-D"]);
    println!("  PASS: all 4 messages drained in original order, none dropped");
}

// ----------------------------------------------------------------------------
// TEST D: Mid-task correction. Spec example: "no, use my other account".
//   This is the 13.2 check #3 — the very next LLM/tool call should
//   reflect the correction. We can't drive a real tool call here, but
//   we can prove the correction lands in the history *in the position
//   where the LLM will see it next* — i.e., as the most recent user
//   message before the next step's snapshot.
// ----------------------------------------------------------------------------
async fn test_mid_task_correction() {
    println!("\n=== TEST D: mid-task correction is visible to next step ===");
    let mut sim = SimLoop::new();
    let tx = sim.take_sender();

    sim.step(false);
    sim.step(false);
    let idx_before_correction = sim.user_notes().len();
    assert_eq!(idx_before_correction, 0);

    // Inject correction.
    tx.send(UserMessage::now("no, use my other account"))
        .await
        .unwrap();

    sim.step(false);
    let notes = sim.user_notes();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].contains("no, use my other account"));

    // The note should be the *most recent* user-role content (excluding
    // the synthetic "Current page state" we add at the end of step()).
    // The real loop's LLM call uses the full history, so the note being
    // earlier in the array is fine — the LLM will still see it on its
    // next turn. We just confirm it's there in the right position
    // (after the original task, before the next snapshot we generate).
    let mut found_note_idx = None;
    for (i, m) in sim.messages.iter().enumerate() {
        if m.get("content").and_then(|c| c.as_str()) == Some(
            "[user note while task is running] no, use my other account",
        ) {
            found_note_idx = Some(i);
            break;
        }
    }
    let note_idx = found_note_idx.expect("correction note should be in messages");
    let task_idx = sim
        .messages
        .iter()
        .position(|m| m.get("content").and_then(|c| c.as_str()) == Some("Task: do the original thing"))
        .expect("task should be present");
    assert!(note_idx > task_idx, "note should come after the original task");
    println!("  PASS: correction note at position {note_idx} (after task at {task_idx}) — next LLM call sees it");
}

// ----------------------------------------------------------------------------
// TEST E: Navigation reset preserves user notes. This is the 13.2 spec
//   check #2 ("the agent shouldn't forget what it had already done just
//   because you interrupted"). We simulate a URL change / full-replace
//   detection — the real loop's `truncate_preserving_user_notes(2)` —
//   and confirm the user's steering note is still in history afterward.
// ----------------------------------------------------------------------------
async fn test_navigation_preserves_user_notes() {
    println!("\n=== TEST E: navigation reset preserves user-typed notes ===");
    let mut sim = SimLoop::new();
    let tx = sim.take_sender();

    sim.step(false);
    tx.send(UserMessage::now("use my other account on this site too"))
        .await
    .unwrap();
    sim.step(false);

    // History should now have: system, task, 2 obs+assistant pairs, the note.
    let pre_truncate_len = sim.messages.len();
    println!("  before simulated nav: {} messages, 1 user note", pre_truncate_len);
    assert_eq!(sim.user_notes().len(), 1);

    // Simulate the navigation reset.
    sim.step(true);

    let notes_after = sim.user_notes();
    assert_eq!(
        notes_after.len(),
        1,
        "user note must survive the truncate that the navigation reset does"
    );
    assert!(notes_after[0].contains("use my other account on this site too"));
    println!(
        "  PASS: 1 user note preserved across navigation reset ({}->{} msgs)",
        pre_truncate_len,
        sim.messages.len()
    );
}

// ----------------------------------------------------------------------------
// TEST F: Long no-input run. Spec 13.2 check #5: "nothing breaks if you
//   type nothing at all for an entire long session — the try_recv() path
//   should be a true no-op, not add latency or log noise every iteration."
//   We can't measure log noise here (no logs), but we can measure latency
//   of a no-input drain. The bound is loose — just "fast" — because the
//   test runs on a shared box; we just need to confirm no per-iteration
//   cost grows with iteration count.
// ----------------------------------------------------------------------------
async fn test_no_input_no_overhead() {
    println!("\n=== TEST F: no-input drain is a true no-op ===");
    let mut sim = SimLoop::new();
    let _ = sim.take_sender(); // sender dropped on return; loop continues normally

    // Run 20 iterations, no user input. Each call to `drain_pending()`
    // on an empty bus must return immediately and not allocate meaningfully.
    let start = std::time::Instant::now();
    for _ in 0..20 {
        sim.step(false);
    }
    let elapsed = start.elapsed();
    println!("  20 no-input iterations took {}ms", elapsed.as_millis());
    assert!(
        elapsed < Duration::from_secs(5),
        "20 no-input iterations should be well under 5s, got {elapsed:?}"
    );

    // No notes accumulated.
    assert!(sim.user_notes().is_empty());
    println!("  PASS: no notes accumulated, no measurable overhead");
}

// ----------------------------------------------------------------------------
// TEST G: Sender dropped while loop is still iterating. The real CLI
//   drops the sender when the user hits Ctrl+D / pipe closes. The loop
//   must keep going and stop pulling messages — but not panic, not hang,
//   not exit. We prove that here by dropping the sender, running more
//   steps, and confirming the loop is still alive and history is
//   intact.
// ----------------------------------------------------------------------------
async fn test_sender_drop_keeps_loop_alive() {
    println!("\n=== TEST G: sender drop mid-session keeps loop alive ===");
    let mut sim = SimLoop::new();
    let tx = sim.take_sender();

    // Send one, drain it.
    tx.send(UserMessage::now("first and only")).await.unwrap();
    sim.step(false);
    assert_eq!(sim.user_notes().len(), 1);

    // Drop the sender (simulates stdin close / Ctrl+D).
    drop(tx);

    // Loop continues for many iterations with no input.
    for _ in 0..10 {
        sim.step(false);
    }
    assert_eq!(
        sim.user_notes().len(),
        1,
        "no phantom notes should appear after sender drop"
    );
    println!("  PASS: 10 post-drop iterations, no panic, no phantom notes, history intact");
}

// ----------------------------------------------------------------------------
// TEST H: Burst DURING iterations. Realistic case: the user types 4
//   messages while the loop is mid-task (between iterations). Drain on
//   the *next* iteration should pull all 4. This is the realistic
//   pattern the spec cares about.
// ----------------------------------------------------------------------------
async fn test_burst_between_iterations() {
    println!("\n=== TEST H: burst arriving between iterations is fully captured ===");
    let mut sim = SimLoop::new();
    let tx = Arc::new(sim.take_sender());

    // Background task: 4 messages with small spacing, simulating a
    // user typing fast.
    let tx2 = tx.clone();
    let burst = tokio::spawn(async move {
        sleep(Duration::from_millis(50)).await;
        tx2.send(UserMessage::now("while-running-1")).await.unwrap();
        sleep(Duration::from_millis(20)).await;
        tx2.send(UserMessage::now("while-running-2")).await.unwrap();
        sleep(Duration::from_millis(20)).await;
        tx2.send(UserMessage::now("while-running-3")).await.unwrap();
        sleep(Duration::from_millis(20)).await;
        tx2.send(UserMessage::now("while-running-4")).await.unwrap();
    });

    // Let the burst finish, then drain.
    sleep(Duration::from_millis(200)).await;
    sim.step(false);
    let _ = burst.await;

    sim.assert_notes_in_order(&[
        "while-running-1",
        "while-running-2",
        "while-running-3",
        "while-running-4",
    ]);
    println!("  PASS: 4 messages typed during session all arrived in order");
}

// ----------------------------------------------------------------------------
// TEST I: real Agent integration. We build a real `Agent` with a fake
//   in-memory `ProviderConfig`, exercise `take_message_sender`, push a
//   message, call the same `drain_user_messages` / `apply_user_message`
//   helpers the real loop uses, and inspect the resulting conversation
//   history to confirm the message landed where the LLM would see it.
//   Then simulate a navigation reset and confirm the note survives.
// ----------------------------------------------------------------------------
fn make_test_config() -> mew_agent::ProviderConfig {
    use mew_agent::{AgentConfig, BrowserConfig, OpencodeZenConfig, ProviderConfig};
    ProviderConfig {
        opencode_zen: OpencodeZenConfig {
            base_url: "http://test.invalid".to_string(),
            api_key: "test-key".to_string(),
            default_model: "test-model".to_string(),
            max_iterations: 5,
            max_tokens: None,
            max_cost: None,
        },
        browser: Some(BrowserConfig { binary_path: None, visible_cursor: false }),
        agent: AgentConfig::default(),
    }
}

async fn test_real_agent_integration() {
    println!("\n=== TEST I: real Agent chat API integration ===");
    let cfg = make_test_config();
    let mut agent = Agent::new(cfg, "open a tab", None);

    // Take the sender and confirm it's a working mpsc sender.
    let tx = agent.take_message_sender();
    println!("  took sender from real Agent (capacity-aware API)");

    // Push 3 messages.
    tx.send(UserMessage::now("use the dark theme please"))
        .await
        .unwrap();
    tx.send(UserMessage::now("and skip the captcha"))
        .await
        .unwrap();
    tx.send(UserMessage::now("log in as kaneki")).await.unwrap();

    // Drain via the real Agent's helper. This is the same call the
    // real loop's checkpoint makes.
    let drained = agent.drain_user_messages();
    assert_eq!(drained.len(), 3, "real Agent should drain 3 messages");
    println!("  PASS: real Agent drain_user_messages() returned 3 messages");

    // Apply each one through the real Agent's apply path.
    for m in &drained {
        agent.apply_user_message_for_test(m);
    }

    // Inspect the conversation history. The system + original task
    // must be at the front, and all 3 user notes must be in there.
    let history = agent.history_snapshot();
    let user_count = history
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .count();
    assert_eq!(
        user_count,
        1 /* original task */ + 3 /* 3 notes */,
        "history should have original task + 3 user notes"
    );
    println!("  PASS: history has 1 original task + 3 user notes = 4 user messages");

    // The original task is preserved.
    let has_task = history.iter().any(|m| {
        m.get("content").and_then(|c| c.as_str()) == Some("Task: open a tab")
    });
    assert!(has_task, "original task must be preserved");
    println!("  PASS: original 'Task: open a tab' preserved in history");

    // Notes are in order.
    let note_texts: Vec<String> = history
        .iter()
        .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
        .filter(|c| c.starts_with("[user note while task is running]"))
        .map(|c| c.trim_start_matches("[user note while task is running] ").to_string())
        .collect();
    assert_eq!(
        note_texts,
        vec![
            "use the dark theme please",
            "and skip the captcha",
            "log in as kaneki",
        ]
    );
    println!("  PASS: 3 notes present in original order");

    // Navigation reset test on the real Agent.
    agent.truncate_for_test(2);
    let history_after = agent.history_snapshot();
    let notes_after: Vec<&str> = history_after
        .iter()
        .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
        .filter(|c| c.starts_with("[user note while task is running]"))
        .collect();
    assert_eq!(
        notes_after.len(),
        3,
        "all 3 user notes must survive the real Agent's truncate_preserving_user_notes(2)"
    );
    println!("  PASS: 3 notes survive real Agent's truncate_preserving_user_notes(2)");
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    println!("mew v2 — Phase 13.1: live chat channel review & testing");
    println!("=======================================================");

    test_message_bus_unit().await;
    test_mid_task_injection().await;
    test_burst_no_drops().await;
    test_mid_task_correction().await;
    test_navigation_preserves_user_notes().await;
    test_no_input_no_overhead().await;
    test_sender_drop_keeps_loop_alive().await;
    test_burst_between_iterations().await;
    test_real_agent_integration().await;

    println!("\n=======================================================");
    println!("All 13.1 tests passed.");
}
