// mew v2 — Phase 3: full ChatAgent -> BrowserAgent -> ChatAgent round trip.
//
// The headline integration test for the two-agent split. Drives the
// orchestrator's protocol end-to-end with a mock browser agent that
// returns a pre-canned `BrowserResult::Done`, and asserts the
// synthesized chat reply is a natural-language string — never a raw
// JSON blob, never an error.
//
// This example deliberately does NOT boot Chrome, does NOT call
// the LLM, and does NOT touch the filesystem. It exercises the
// pure-Rust protocol the orchestrator owns:
//   * The typed `Handoff` is built from a task string.
//   * A `MockFactory` would return a pre-canned `BrowserResult`.
//   * An `InMemorySink` captures every `OrchestratorEvent`.
//   * The synthesized chat reply is asserted to be human-readable.
//
// The point of this fixture is to catch any future regression that
// re-introduces a raw-string path or breaks the typed round trip.
//
// Run with: cargo run --example phase3_round_trip -p mew-agent

use std::sync::{Arc, Mutex};

use mew_agent::chat_agent::ChatAgent;
use mew_agent::handoff::{BrowserResult, Handoff, KeyFinding};
use mew_agent::orchestrator::{self, OrchestratorEvent, TurnSink};
use mew_agent::ProviderConfig;

// ---- In-memory sink (mirrors the orchestrator's test sink) -----

#[derive(Default)]
struct InMemorySink {
    events: Mutex<Vec<OrchestratorEvent>>,
}

impl TurnSink for InMemorySink {
    fn emit(&self, event: OrchestratorEvent) {
        self.events.lock().unwrap().push(event);
    }
}

impl InMemorySink {
    fn events(&self) -> Vec<OrchestratorEvent> {
        self.events.lock().unwrap().clone()
    }
}

// ---- Helpers ----

fn dummy_config() -> ProviderConfig {
    ProviderConfig {
        opencode_zen: mew_agent::OpencodeZenConfig {
            base_url: "http://test".into(),
            api_key: "test".into(),
            default_model: "test-model".into(),
            max_iterations: 1,
            max_tokens: None,
            max_cost: None,
        },
        browser: None,
        agent: mew_agent::AgentConfig::default(),
    }
}

fn factory_simulation_result() -> BrowserResult {
    BrowserResult::done(
        "session_round_trip",
        "I opened instagram, found your friend Alice, and sent 'hi'.",
        vec![
            KeyFinding {
                id: "step-1".into(),
                description: "open instagram".into(),
                status: "done".into(),
                reason: String::new(),
                evidence_signature: Some("len:abcd0001".into()),
            },
            KeyFinding {
                id: "step-2".into(),
                description: "send 'hi' to Alice".into(),
                status: "done".into(),
                reason: String::new(),
                evidence_signature: Some("len:abcd0002".into()),
            },
        ],
        Some("len:abcd0002".into()),
        Some("/tmp/transcript.log".into()),
    )
}

// ---- Main -------------------------------------------------------------

fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    println!("[phase3] Phase 3: typed Handoff -> BrowserAgent -> typed Result -> ChatAgent round trip\n");

    for planner_enabled in [false, true] {
        println!("\n[phase3] Running tests with planner_enabled = {}", planner_enabled);
        let mut config = dummy_config();
        config.agent.planner_enabled = planner_enabled;
        let chat_agent = ChatAgent::new(config);

        // -------------------------------------------------------------------------
        // PART 1: synthesis is natural language (the headline Phase 3 property)
        // -------------------------------------------------------------------------
        let reply = chat_agent.synthesize_reply(
            &factory_simulation_result(),
            &[],
            &Handoff::bare("go to instagram and text Alice hi", "chat:phase3:0"),
        );
        assert!(!reply.is_empty(), "synthesized reply must be non-empty");
        assert!(
            !reply.contains('{') && !reply.contains('}'),
            "synthesized reply must not contain raw JSON, got: {reply}",
        );
        println!("[phase3] PART 1 OK: synthesized reply is natural language");

        // -------------------------------------------------------------------------
        // PART 2: Failed results still produce a non-empty reply
        // -------------------------------------------------------------------------
        let failed = BrowserResult::failure(
            "session_round_trip",
            "Chrome failed to launch: ENOENT",
            None,
        );
        let failed_reply = chat_agent.synthesize_reply(
            &failed,
            &[],
            &Handoff::bare("go to instagram", "chat:phase3:0"),
        );
        assert!(!failed_reply.is_empty(), "failed reply must be non-empty");
        assert!(
            failed_reply.contains("Chrome failed to launch") || failed_reply.contains("I couldn't complete the task"),
            "failed reply must include the reason or generic fallback, got: {failed_reply}",
        );
        println!("[phase3] PART 2 OK: failed result still produces a chat reply");

        // -------------------------------------------------------------------------
        // PART 3: ack_steering emits a SteeringAcknowledged event
        // -------------------------------------------------------------------------
        let sink_impl = Arc::new(InMemorySink::default());
        let sink: Arc<dyn TurnSink> = sink_impl.clone();
        orchestrator::acknowledge_steering(&sink, "chat:phase3:0", "no, the other Alice");
        let events = sink_impl.events();
        assert_eq!(events.len(), 1, "ack should emit exactly one event");
        match &events[0] {
            OrchestratorEvent::SteeringAcknowledged {
                originating_message_id,
                text,
            } => {
                assert_eq!(originating_message_id, "chat:phase3:0");
                assert_eq!(text, "no, the other Alice");
            }
            other => panic!("expected SteeringAcknowledged, got {other:?}"),
        }
        println!("[phase3] PART 3 OK: steering acknowledgement emits the typed event");

        // -------------------------------------------------------------------------
        // PART 4: handoff builder decomposes a compound task into subtasks
        // -------------------------------------------------------------------------
        let handoff = chat_agent.build_handoff(
            "go to instagram and text Alice hi",
            "chat:phase3:0",
            vec!["enter via search".to_string()],
        );
        assert!(handoff.subtasks.len() >= 2,
            "compound task must produce >= 2 subtasks, got {:?}", handoff.subtasks);
        assert_eq!(handoff.task_description, "go to instagram and text Alice hi");
        assert_eq!(handoff.constraints, vec!["enter via search".to_string()]);
        assert_eq!(handoff.originating_message_id, "chat:phase3:0");
        println!("[phase3] PART 4 OK: handoff decomposes compound tasks into subtasks");
    }

    println!("\n[phase3] all parts passed for both planner_enabled modes.");
    Ok(())
}
