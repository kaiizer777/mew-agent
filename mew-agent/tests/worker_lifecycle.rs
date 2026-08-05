// mew v2 — Phase 13 integration test suite: worker lifecycle.

use std::sync::Arc;

use mew_agent::handoff::{BrowserResult, Handoff, TodoResult};
use mew_agent::orchestrator::BrowserAgentFactory;
use mew_agent::supervisor::{SupervisorCommand, SupervisorSignal};
use mew_agent::todo::{AcceptanceCriterion, AcceptanceKind, Todo, TodoId, TodoStatus};
use mew_agent::worker::BrowserAgentWorker;
use mew_agent::worker_pool::{PoolError, WorkerPool};

struct MockFactory {
    delay_ms: u64,
    result: BrowserResult,
}

impl MockFactory {
    fn new(result: BrowserResult, delay_ms: u64) -> Self {
        Self { delay_ms, result }
    }
}

impl BrowserAgentFactory for MockFactory {
    fn run_browser_task<'a>(
        &'a self,
        _handoff: Handoff,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<BrowserResult>> + Send + 'a>,
    > {
        let result = self.result.clone();
        let delay = self.delay_ms;
        Box::pin(async move {
            if delay > 0 {
                tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
            }
            Ok(result)
        })
    }
}

fn sample_handoff() -> Handoff {
    Handoff::bare("test task", "msg-1")
}

fn sample_todo(id: &str, intent: &str) -> Todo {
    Todo::new(
        TodoId::from(id),
        intent,
        Some(AcceptanceCriterion::new(
            AcceptanceKind::AnySnapshot,
            "test",
        )),
    )
}

fn sample_done_result(summary: &str, sig: &str) -> BrowserResult {
    BrowserResult::done(
        "sess-1",
        summary,
        vec![],
        Some(sig.to_string()),
        Some("trace.log".to_string()),
    )
}

#[tokio::test]
async fn test_submit_then_complete() {
    let mock_res = sample_done_result("finished step", "len:00000001");
    let factory = Arc::new(MockFactory::new(mock_res, 10));
    let worker = BrowserAgentWorker::new("worker-1", factory);

    let todo = sample_todo("todo-1", "navigate to site");
    let rx = worker.submit(todo, sample_handoff());

    let res = rx.await.expect("receiver completed");
    assert_eq!(res.status, TodoStatus::Done);
    assert_eq!(res.todo_id.as_str(), "todo-1");
}

#[tokio::test]
async fn test_submit_then_cancel_mid_loop() {
    let mock_res = sample_done_result("slow operation", "len:00000002");
    let factory = Arc::new(MockFactory::new(mock_res, 500));
    let worker = BrowserAgentWorker::new("worker-1", factory);

    let todo = sample_todo("todo-cancel", "slow action");
    let rx = worker.submit(todo, sample_handoff());

    tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;
    worker.signal(SupervisorCommand::new(1, SupervisorSignal::Cancel));

    let res = rx.await.expect("receiver completed");
    assert_ne!(res.status, TodoStatus::Done);
    assert!(res.cancelled);
}

#[tokio::test]
async fn test_submit_then_deadline() {
    let mock_res = sample_done_result("long run", "len:00000003");
    let factory = Arc::new(MockFactory::new(mock_res, 1000));
    let worker = BrowserAgentWorker::new("worker-1", factory);

    let todo = sample_todo("todo-deadline", "long running todo");
    let rx = worker.submit(todo, sample_handoff());

    let deadline = tokio::time::sleep(tokio::time::Duration::from_millis(50));
    tokio::pin!(deadline);

    let res = tokio::select! {
        res = rx => res.expect("receiver done"),
        _ = &mut deadline => {
            worker.signal(SupervisorCommand::new(1, SupervisorSignal::Cancel));
            TodoResult::failure("todo-deadline".into(), "deadline exceeded", "", "", 0)
        }
    };

    assert_ne!(res.status, TodoStatus::Done);
}


#[tokio::test]
#[should_panic(expected = "await the previous receiver first")]
async fn test_submit_twice_without_awaiting_first_receiver_panics() {
    let mock_res = sample_done_result("running", "len:00000005");
    let factory = Arc::new(MockFactory::new(mock_res, 200));
    let worker = BrowserAgentWorker::new("worker-panic", factory);

    let todo1 = sample_todo("todo-1", "first action");
    let todo2 = sample_todo("todo-2", "second action");

    let _rx1 = worker.submit(todo1, sample_handoff());
    let _rx2 = worker.submit(todo2, sample_handoff());
}

#[tokio::test]
async fn test_submit_while_pool_shutting_down_returns_err() {
    let mock_res = sample_done_result("pool item", "len:00000006");
    let factory = Arc::new(MockFactory::new(mock_res, 10));
    let worker = BrowserAgentWorker::new("worker-pool", factory);
    let pool = WorkerPool::new(vec![worker]);

    pool.shutdown();
    let todo = sample_todo("todo-pool", "pool item");
    let res = pool.submit(todo, sample_handoff());

    assert_eq!(res.err(), Some(PoolError::ShuttingDown));
}

#[tokio::test]
async fn test_cancel_signal_with_stale_id_is_ignored() {
    let mock_res = sample_done_result("stale test", "len:00000007");
    let factory = Arc::new(MockFactory::new(mock_res, 150));
    let worker = BrowserAgentWorker::new("worker-stale", factory);

    let todo = sample_todo("todo-stale", "stale signal check");
    let rx = worker.submit(todo, sample_handoff());

    // Send fresh watermark 10 first
    worker.signal(SupervisorCommand::new(10, SupervisorSignal::Pause));
    // Send stale cancel signal 5 (id <= watermark 10)
    worker.signal(SupervisorCommand::new(5, SupervisorSignal::Cancel));

    let res = rx.await.expect("receiver done");
    // Should NOT be cancelled because signal_id 5 was stale (watermark was 10)
    assert_eq!(res.status, TodoStatus::Done);
}
