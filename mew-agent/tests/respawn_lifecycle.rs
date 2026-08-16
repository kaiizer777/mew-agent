// mew v2 — Phase 27 integration test suite: respawn policy & lifecycle.

use std::sync::Arc;
use std::time::Duration;

use mew_agent::agent::Agent;
use mew_agent::completeness::{SubTask, SubTaskStatus};
use mew_agent::handoff::{BrowserResult, Handoff, RespawnHandoff};
use mew_agent::orchestrator::BrowserAgentFactory;
use mew_agent::respawn_policy::{RespawnDecision, RespawnPolicy};
use mew_agent::supervisor::{RespawnReason, SupervisorCommand, SupervisorSignal};
use mew_agent::todo::{AcceptanceCriterion, AcceptanceKind, Todo, TodoId, TodoStatus};
use mew_agent::worker::BrowserAgentWorker;
use mew_agent::worker_pool::WorkerPool;
use mew_resilience::stuck_detector::{detect_stuck, StuckDetectorConfig, WorkerHealth};

struct MockFactory {
    delay_ms: u64,
}

impl MockFactory {
    fn new(delay_ms: u64) -> Self {
        Self { delay_ms }
    }
}

impl BrowserAgentFactory for MockFactory {
    fn run_browser_task<'a>(
        &'a self,
        _handoff: Handoff,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<BrowserResult>> + Send + 'a>,
    > {
        let delay = self.delay_ms;
        Box::pin(async move {
            if delay > 0 {
                tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
            }
            Ok(BrowserResult::done(
                "sess-1",
                "completed",
                vec![],
                None,
                None,
            ))
        })
    }
}

fn sample_handoff() -> Handoff {
    Handoff::bare("respawn test task", "msg-respawn-1")
}

fn sample_todo(id: &str) -> Todo {
    Todo::new(
        TodoId::from(id),
        "test task action",
        Some(AcceptanceCriterion::new(
            AcceptanceKind::AnySnapshot,
            "test",
        )),
    )
}

#[test]
fn test_respawn_policy_nudge_first_for_stuck_worker() {
    let mut policy = RespawnPolicy::new(2, Duration::from_secs(30));
    let mut health = WorkerHealth::new("w-1");
    let cfg = StuckDetectorConfig::default();

    // Artificially induce loop
    health.record_action(0xDEAD, Some(0x1111));
    health.record_action(0xDEAD, Some(0x1111));
    health.record_action(0xDEAD, Some(0x1111));
    health.on_checkpoint();

    let report = detect_stuck(&health, &cfg);
    assert!(report.suspected_stuck);

    // First evaluation must return Nudge!
    let decision = policy.evaluate("task-1", &report);
    match decision {
        RespawnDecision::Nudge { instruction } => {
            assert!(instruction.contains("System nudge"));
            assert!(policy.is_nudge_pending("task-1"));
        }
        other => panic!("expected Nudge, got {:?}", other),
    }
}

#[test]
fn test_respawn_policy_nudge_resolves_stuck_prevents_respawn() {
    let mut policy = RespawnPolicy::new(2, Duration::from_secs(30));
    let mut health = WorkerHealth::new("w-2");
    let cfg = StuckDetectorConfig::default();

    health.record_action(0xDEAD, Some(0x1111));
    health.record_action(0xDEAD, Some(0x1111));
    health.record_action(0xDEAD, Some(0x1111));
    health.on_checkpoint();

    let report = detect_stuck(&health, &cfg);
    let decision = policy.evaluate("task-2", &report);
    assert!(matches!(decision, RespawnDecision::Nudge { .. }));

    // Worker receives nudge, takes different action, records progress
    health.record_action(0xBEEF, Some(0x2222));
    health.record_progress();

    let report2 = detect_stuck(&health, &cfg);
    assert!(!report2.suspected_stuck);

    let decision2 = policy.evaluate("task-2", &report2);
    assert_eq!(decision2, RespawnDecision::NoAction);
    assert!(!policy.is_nudge_pending("task-2"));
    assert_eq!(policy.respawn_count("task-2"), 0);
}

#[test]
fn test_respawn_policy_persistent_stuck_triggers_respawn() {
    let mut policy = RespawnPolicy::new(2, Duration::from_secs(30));
    let mut health = WorkerHealth::new("w-3");
    let cfg = StuckDetectorConfig::default();

    health.record_action(0xDEAD, Some(0x1111));
    health.record_action(0xDEAD, Some(0x1111));
    health.record_action(0xDEAD, Some(0x1111));
    health.on_checkpoint();

    let report = detect_stuck(&health, &cfg);
    let _ = policy.evaluate("task-3", &report);

    // Worker remains stuck for 2 post-nudge iterations
    health.on_checkpoint();
    health.on_checkpoint();

    let report2 = detect_stuck(&health, &cfg);
    let decision2 = policy.evaluate("task-3", &report2);

    match decision2 {
        RespawnDecision::Respawn { reason, attempt } => {
            assert_eq!(attempt, 1);
            assert!(matches!(reason, RespawnReason::Stuck(_)));
        }
        other => panic!("expected Respawn, got {:?}", other),
    }
}

#[test]
fn test_respawn_policy_token_limit_approaching_proactive_respawn() {
    let mut policy = RespawnPolicy::new(2, Duration::from_secs(30));
    let mut health = WorkerHealth::new("w-4");
    let mut cfg = StuckDetectorConfig::default();
    cfg.max_token_budget = Some(10_000);

    health.record_tokens(8_600, false);
    let report = detect_stuck(&health, &cfg);
    assert!(report.token_limit_approaching);

    let decision = policy.evaluate("task-4", &report);
    match decision {
        RespawnDecision::Respawn { reason, attempt } => {
            assert_eq!(attempt, 1);
            assert!(matches!(
                reason,
                RespawnReason::TokenLimitApproaching {
                    spend: 8600,
                    max: 10000
                }
            ));
        }
        other => panic!("expected proactive Respawn, got {:?}", other),
    }
}

#[test]
fn test_respawn_policy_immediate_crash_handling() {
    let mut policy = RespawnPolicy::new(2, Duration::from_secs(30));
    let decision = policy.handle_crash("task-5", "unrecoverable CDP websocket dropped");

    match decision {
        RespawnDecision::Respawn { reason, attempt } => {
            assert_eq!(attempt, 1);
            assert!(matches!(reason, RespawnReason::Crashed(_)));
        }
        other => panic!("expected immediate Respawn, got {:?}", other),
    }
}

#[test]
fn test_respawn_policy_max_respawns_cap_exhaustion_fails_task() {
    let mut policy = RespawnPolicy::new(2, Duration::from_secs(30));

    let d1 = policy.handle_crash("task-cap", "crash 1");
    assert!(matches!(d1, RespawnDecision::Respawn { attempt: 1, .. }));

    let d2 = policy.handle_crash("task-cap", "crash 2");
    assert!(matches!(d2, RespawnDecision::Respawn { attempt: 2, .. }));

    // 3rd trigger exceeds cap of 2!
    let d3 = policy.handle_crash("task-cap", "crash 3");
    match d3 {
        RespawnDecision::Fail { reason } => {
            assert!(reason.contains("exhausted maximum respawn limit (2/2)"));
        }
        other => panic!("expected Fail, got {:?}", other),
    }
}

#[tokio::test]
async fn test_worker_respawn_signal_handling() {
    let factory = Arc::new(MockFactory::new(500));
    let worker = BrowserAgentWorker::new("w-respawn-sig", factory);

    let todo = sample_todo("todo-respawn-sig");
    let rx = worker.submit(todo, sample_handoff());

    tokio::time::sleep(Duration::from_millis(30)).await;
    worker.signal(SupervisorCommand::new(
        1,
        SupervisorSignal::Respawn(RespawnReason::Crashed("mock crash".into())),
    ));

    let res = rx.await.expect("receiver done");
    assert!(matches!(res.status, TodoStatus::Failed { .. }));
    assert!(res
        .failure_reason
        .as_deref()
        .unwrap_or("")
        .contains("respawn requested: Crashed"));
}

#[tokio::test]
async fn test_worker_pool_replace_worker() {
    let factory = Arc::new(MockFactory::new(10));
    let worker1 = BrowserAgentWorker::new("w-old", factory.clone());
    let worker2 = BrowserAgentWorker::new("w-new", factory);

    let mut pool = WorkerPool::new(vec![worker1]);
    let old_worker = pool.replace_worker(0, worker2, RespawnReason::Stuck("loop".into()));

    assert!(old_worker.is_some());
    assert_eq!(old_worker.unwrap().worker_id, "w-old");
}

#[test]
fn test_respawn_handoff_excludes_completed_subtasks_on_new_worker() {
    let completed_task = SubTask {
        id: "step-1".into(),
        description: "Open job board search page".into(),
        status: SubTaskStatus::Done,
        evidence_iteration: Some(1),
        evidence_signature: Some("sig_123".into()),
        decided_at_secs: Some(100),
    };
    let remaining_task = SubTask {
        id: "step-2".into(),
        description: "Filter by remote roles".into(),
        status: SubTaskStatus::Pending,
        evidence_iteration: None,
        evidence_signature: None,
        decided_at_secs: None,
    };

    let handoff = RespawnHandoff::new(
        "search rust jobs",
        vec![completed_task.clone()],
        vec![remaining_task.clone()],
        Some("sig_123".into()),
        RespawnReason::Stuck("stuck in loop".into()),
        "Worker got stuck repeating filter clicks",
    );

    let mut agent = Agent::new_for_test("search rust jobs");
    agent.set_respawn_handoff(handoff);

    agent
        .completeness_mut()
        .seed_from_handoff(vec![completed_task], vec![remaining_task]);

    assert_eq!(agent.completeness().subtasks.len(), 2);
    assert_eq!(agent.completeness().incomplete_count(), 1);
    assert_eq!(agent.completeness().subtasks[0].status, SubTaskStatus::Done);
    assert_eq!(
        agent.completeness().subtasks[1].status,
        SubTaskStatus::Pending
    );
}

#[test]
fn test_respawn_handoff_negative_raw_context_check() {
    let handoff = RespawnHandoff::new(
        "search rust jobs",
        vec![],
        vec![],
        None,
        RespawnReason::TokenLimitApproaching {
            spend: 8500,
            max: 10000,
        },
        "Token budget limit approaching warning",
    );

    let mut agent = Agent::new_for_test("search rust jobs");
    agent.set_respawn_handoff(handoff.clone());

    let resuming_block = handoff.render_resuming_block();
    assert!(resuming_block.contains("RESUMING (from a previous attempt):"));
    assert!(resuming_block.contains("Reason for previous restart: TokenLimitApproaching"));
    assert!(resuming_block.contains("Token budget limit approaching warning"));

    assert!(!resuming_block.contains("AXNode"));
    assert!(!resuming_block.contains("Role: button"));
    assert!(!resuming_block.contains("previous raw conversation transcript"));
}

#[test]
fn test_respawn_handoff_failure_context_writes_memory_lesson() {
    let temp_dir = std::env::temp_dir().join(format!(
        "mew_test_respawn_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let handoff = RespawnHandoff::new(
        "test task",
        vec![],
        vec![],
        Some("snapshot_sig_abc".into()),
        RespawnReason::Stuck("loop detected".into()),
        "Friction detected: repetitive click loop on cookie banner",
    );

    let store = mew_memory::LessonStore::new(&temp_dir);
    let lesson = handoff.to_lesson("session_respawn_test");
    store.write_lesson(&lesson).unwrap();

    let lessons = store.read_all().unwrap();
    assert_eq!(lessons.len(), 1);
    assert_eq!(
        lessons[0].lesson_text,
        "Friction detected: repetitive click loop on cookie banner"
    );
    assert_eq!(lessons[0].originating_session_id, "session_respawn_test");
    assert_eq!(lessons[0].outcome, mew_memory::Outcome::Failure);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_respawn_max_cap_narrates_multi_attempt_history() {
    let mut policy = RespawnPolicy::new(2, Duration::from_secs(30));

    let h1 = RespawnHandoff::new(
        "task-1",
        vec![],
        vec![],
        None,
        RespawnReason::Stuck("Action loop on button click".into()),
        "Stuck loop on element #submit",
    );
    let h2 = RespawnHandoff::new(
        "task-1",
        vec![],
        vec![],
        None,
        RespawnReason::TokenLimitApproaching {
            spend: 9000,
            max: 10000,
        },
        "Token spend 90%",
    );

    policy.record_handoff("task-multi", h1);
    let d1 = policy.handle_crash("task-multi", "crash 1");
    assert!(matches!(d1, RespawnDecision::Respawn { attempt: 1, .. }));

    policy.record_handoff("task-multi", h2);
    let d2 = policy.handle_crash("task-multi", "crash 2");
    assert!(matches!(d2, RespawnDecision::Respawn { attempt: 2, .. }));

    let d3 = policy.handle_crash("task-multi", "crash 3");
    match d3 {
        RespawnDecision::Fail { reason } => {
            assert!(reason.contains("Task failed after 2 respawn attempt(s)"));
            assert!(reason.contains("Attempt 1: Stuck (Stuck loop on element #submit)"));
            assert!(reason.contains("Attempt 2: TokenLimitApproaching (Token spend 90%)"));
            assert!(reason.contains("exhausted maximum respawn limit (2/2)"));
        }
        other => panic!(
            "expected Fail with multi-attempt narrative, got {:?}",
            other
        ),
    }
}
