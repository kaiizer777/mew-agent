// mew v2 — Phase 1: True Concurrent Worker Dispatch.
//
// Integration test asserting that multi-platform research subtasks
// execute concurrently (overlapping timelines) across worker pool workers,
// while respecting per-domain concurrency caps.
//
// Run with: cargo run --example phase1_concurrent_workers -p mew-agent

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mew_agent::handoff::{BrowserResult, Handoff, KeyFinding};
use mew_agent::orchestrator::{BrowserAgentFactory, OrchestratorEvent, TurnSink};
use mew_agent::planner::Planner;
use mew_agent::research::{Criterion, ResearchPlan, ResearchSubTask};
use mew_agent::worker::BrowserAgentWorker;
use mew_agent::worker_pool::WorkerPool;

#[derive(Default)]
struct TestSink {
    events: Mutex<Vec<OrchestratorEvent>>,
}

impl TurnSink for TestSink {
    fn emit(&self, event: OrchestratorEvent) {
        self.events.lock().unwrap().push(event);
    }
}

#[derive(Clone)]
struct TimingMockFactory {
    delay: Duration,
    timings: Arc<Mutex<Vec<(String, Instant, Instant)>>>,
}

impl BrowserAgentFactory for TimingMockFactory {
    fn run_browser_task<'a>(
        &'a self,
        handoff: Handoff,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<BrowserResult>> + Send + 'a>,
    > {
        let delay = self.delay;
        let timings = self.timings.clone();
        Box::pin(async move {
            let start = Instant::now();
            tokio::time::sleep(delay).await;
            let end = Instant::now();

            let subtask_id = handoff
                .subtasks
                .first()
                .map(|s| s.id.clone())
                .unwrap_or_else(|| "unknown".into());

            timings
                .lock()
                .unwrap()
                .push((subtask_id.clone(), start, end));

            let summary = format!("Completed research for {}", subtask_id);
            let sig = mew_agent::todo::planner_signature(&summary);
            Ok(BrowserResult::done(
                format!("session-{}", subtask_id),
                summary,
                vec![KeyFinding {
                    id: subtask_id,
                    description: handoff.task_description.clone(),
                    status: "done".into(),
                    reason: String::new(),
                    evidence_signature: Some(sig.clone()),
                }],
                Some(sig),
                None,
            ))
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("[phase1] Phase 1: True Concurrent Worker Dispatch verification\n");

    // =========================================================================
    // PART 1: Multi-platform research subtasks run CONCURRENTLY (overlapping)
    // =========================================================================
    println!("[phase1] PART 1: Verifying parallel execution across different domains...");

    let timings = Arc::new(Mutex::new(Vec::new()));
    let factory: Arc<dyn BrowserAgentFactory> = Arc::new(TimingMockFactory {
        delay: Duration::from_millis(100),
        timings: timings.clone(),
    });

    let w1 = BrowserAgentWorker::new("worker-1", factory.clone());
    let w2 = BrowserAgentWorker::new("worker-2", factory.clone());
    let pool = Arc::new(WorkerPool::new(vec![w1, w2]));
    let sink = Arc::new(TestSink::default());

    let mut plan = ResearchPlan {
        goal: "find rust jobs on linkedin and indeed".into(),
        platforms: vec![
            ResearchSubTask {
                id: "step-linkedin".into(),
                platform: "LinkedIn".into(),
                domain: "linkedin.com".into(),
                entry_hint: "search rust".into(),
                acceptance: vec![Criterion::HasUrl],
                step_budget: 5,
                time_budget_secs: 30,
                query: "rust".into(),
                platform_kind: "job_board".into(),
                query_template: String::new(),
                lead_subtask_kind: None,
            },
            ResearchSubTask {
                id: "step-indeed".into(),
                platform: "Indeed".into(),
                domain: "indeed.com".into(),
                entry_hint: "search rust".into(),
                acceptance: vec![Criterion::HasUrl],
                step_budget: 5,
                time_budget_secs: 30,
                query: "rust".into(),
                platform_kind: "job_board".into(),
                query_template: String::new(),
                lead_subtask_kind: None,
            },
        ],
        synthesis_hint: "list jobs".into(),
        overall_deadline_secs: Some(60),
        is_research: true,
        matched_pattern: "research_keyword".into(),
        max_concurrent_workers: Some(3),
        max_concurrent_per_domain: Some(1),
        stream_partial_findings: None,
    };

    let handoff = Handoff::with_research_plan(
        "find rust jobs on linkedin and indeed",
        "chat:phase1:0",
        plan.clone(),
    );

    let start_all = Instant::now();
    let result = Planner::run(handoff, pool.clone(), sink.clone()).await;
    let total_elapsed = start_all.elapsed();

    assert_eq!(
        result.status,
        mew_agent::handoff::BrowserStatus::Done,
        "all subtasks should complete as Done"
    );
    assert_eq!(result.key_findings.len(), 2);

    let recorded = timings.lock().unwrap().clone();
    assert_eq!(recorded.len(), 2, "both subtasks should have executed");

    let (_id1, start1, end1) = &recorded[0];
    let (_id2, start2, end2) = &recorded[1];

    // Overlap assertion: subtask 2 started BEFORE subtask 1 finished (and vice versa)
    let overlaps = start1 < end2 && start2 < end1;
    println!(
        "  Subtask 1: [0ms .. {:?}ms]",
        end1.duration_since(*start1).as_millis()
    );
    println!(
        "  Subtask 2: [0ms .. {:?}ms]",
        end2.duration_since(*start2).as_millis()
    );
    println!("  Total wall-clock elapsed: {:?}ms", total_elapsed.as_millis());

    assert!(
        overlaps,
        "Subtasks MUST execute with overlapping timelines! Got: task1=[{:?}..{:?}], task2=[{:?}..{:?}]",
        start1, end1, start2, end2
    );
    assert!(
        total_elapsed < Duration::from_millis(180),
        "Parallel execution should complete in ~100ms, not sequential 200ms (took {:?})",
        total_elapsed
    );
    println!("[phase1] PART 1 PASSED: subtasks executed concurrently with overlapping timelines.\n");

    // =========================================================================
    // PART 2: Same-domain concurrency cap (max_concurrent_per_domain = 1)
    // =========================================================================
    println!("[phase1] PART 2: Verifying per-domain concurrency cap (same domain runs sequentially)...");

    let timings2 = Arc::new(Mutex::new(Vec::new()));
    let factory2: Arc<dyn BrowserAgentFactory> = Arc::new(TimingMockFactory {
        delay: Duration::from_millis(100),
        timings: timings2.clone(),
    });

    let w1_b = BrowserAgentWorker::new("worker-1", factory2.clone());
    let w2_b = BrowserAgentWorker::new("worker-2", factory2.clone());
    let pool2 = Arc::new(WorkerPool::new(vec![w1_b, w2_b]));
    let sink2 = Arc::new(TestSink::default());

    // Two subtasks targeting the SAME domain ("linkedin.com") with max_concurrent_per_domain = 1
    plan.platforms = vec![
        ResearchSubTask {
            id: "step-linkedin-1".into(),
            platform: "LinkedIn Search 1".into(),
            domain: "linkedin.com".into(),
            entry_hint: "search rust".into(),
            acceptance: vec![Criterion::HasUrl],
            step_budget: 5,
            time_budget_secs: 30,
            query: "rust".into(),
            platform_kind: "job_board".into(),
            query_template: String::new(),
            lead_subtask_kind: None,
        },
        ResearchSubTask {
            id: "step-linkedin-2".into(),
            platform: "LinkedIn Search 2".into(),
            domain: "linkedin.com".into(),
            entry_hint: "search senior rust".into(),
            acceptance: vec![Criterion::HasUrl],
            step_budget: 5,
            time_budget_secs: 30,
            query: "senior rust".into(),
            platform_kind: "job_board".into(),
            query_template: String::new(),
            lead_subtask_kind: None,
        },
    ];
    plan.max_concurrent_workers = Some(3);
    plan.max_concurrent_per_domain = Some(1);

    let handoff2 = Handoff::with_research_plan(
        "search linkedin twice",
        "chat:phase1:1",
        plan,
    );

    let start_all2 = Instant::now();
    let result2 = Planner::run(handoff2, pool2.clone(), sink2.clone()).await;
    let total_elapsed2 = start_all2.elapsed();

    assert_eq!(
        result2.status,
        mew_agent::handoff::BrowserStatus::Done,
        "both same-domain subtasks should complete as Done"
    );

    let recorded2 = timings2.lock().unwrap().clone();
    assert_eq!(recorded2.len(), 2);

    let (_id1_b, start1_b, end1_b) = &recorded2[0];
    let (_id2_b, start2_b, _end2_b) = &recorded2[1];

    // Same domain cap: second task must NOT start before first task finished
    assert!(
        start2_b >= end1_b || start1_b >= _end2_b,
        "Same-domain tasks must NOT overlap when max_concurrent_per_domain = 1"
    );
    assert!(
        total_elapsed2 >= Duration::from_millis(190),
        "Same-domain tasks must run sequentially (expected >=200ms, took {:?})",
        total_elapsed2
    );
    println!("[phase1] PART 2 PASSED: same-domain concurrency cap enforced sequential execution.\n");

    println!("[phase1] ALL PHASE 1 CONCURRENCY ASSERTIONS PASSED.");
    Ok(())
}
