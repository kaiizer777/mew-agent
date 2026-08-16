// mew v2 — Phase 2: Live Partial-Result Streaming Unit Tests.
//
// Asserts that:
// 1. In a 3-subtask concurrent research task where subtask 2 finishes first,
//    subtask 2's finding is emitted as `PartialFinding` before subtasks 1 and 3 complete.
// 2. Setting `stream_partial_findings: false` disables live event emissions while
//    still retaining findings in the final `BrowserResult`.
// 3. Duplicate findings are absorbed and not double-emitted.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mew_agent::handoff::{BrowserResult, Handoff, KeyFinding};
use mew_agent::orchestrator::{BrowserAgentFactory, OrchestratorEvent, TurnSink};
use mew_agent::planner::Planner;
use mew_agent::research::{Criterion, FindingStore, ResearchFinding, ResearchPlan, ResearchSubTask};
use mew_agent::worker::BrowserAgentWorker;
use mew_agent::worker_pool::WorkerPool;

#[derive(Default)]
struct TimedSink {
    events: Mutex<Vec<(Instant, OrchestratorEvent)>>,
}

impl TurnSink for TimedSink {
    fn emit(&self, event: OrchestratorEvent) {
        self.events.lock().unwrap().push((Instant::now(), event));
    }
}

#[derive(Clone)]
struct StaggeredResearchFactory {
    // subtask_id -> (delay, finding)
    subtask_configs: Arc<Mutex<Vec<(String, Duration, ResearchFinding)>>>,
}

impl BrowserAgentFactory for StaggeredResearchFactory {
    fn run_browser_task<'a>(
        &'a self,
        handoff: Handoff,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<BrowserResult>> + Send + 'a>,
    > {
        let configs = self.subtask_configs.clone();
        Box::pin(async move {
            let subtask_id = handoff
                .subtasks
                .first()
                .map(|s| s.id.clone())
                .unwrap_or_else(|| "unknown".into());

            let (delay, finding) = {
                let lock = configs.lock().unwrap();
                lock.iter()
                    .find(|(id, _, _)| *id == subtask_id || subtask_id.starts_with(id))
                    .map(|(_, d, f)| (*d, f.clone()))
                    .unwrap_or((
                        Duration::from_millis(50),
                        ResearchFinding {
                            id: format!("default-finding-{}", subtask_id),
                            platform: "test".into(),
                            title: Some(format!("Role in {}", subtask_id)),
                            company: Some("TestCorp".into()),
                            email: Some("test@example.com".into()),
                            url: Some("https://example.com/job".into()),
                            note: "test note".into(),
                            added_at_secs: 0,
                        },
                    ))
            };

            tokio::time::sleep(delay).await;

            let summary = format!("Completed research for {}", subtask_id);
            let sig = mew_agent::todo::planner_signature(&summary);
            let mut result = BrowserResult::done_research(
                format!("session-{}", subtask_id),
                summary,
                vec![KeyFinding {
                    id: subtask_id.clone(),
                    description: handoff.task_description.clone(),
                    status: "done".into(),
                    reason: String::new(),
                    evidence_signature: Some(sig.clone()),
                }],
                Some(sig),
                None,
                vec![finding],
            );
            result.status = mew_agent::handoff::BrowserStatus::Done;
            Ok(result)
        })
    }
}

#[tokio::test]
async fn test_subtask_2_finishes_first_and_emits_before_1_and_3() {
    let f1 = ResearchFinding {
        id: "finding-platform-1".into(),
        platform: "LinkedIn".into(),
        title: Some("Senior Rust Engineer".into()),
        company: Some("Acme Corp".into()),
        email: Some("jobs@acme.com".into()),
        url: Some("https://linkedin.com/jobs/1".into()),
        note: "Remote US".into(),
        added_at_secs: 0,
    };

    let f2 = ResearchFinding {
        id: "finding-platform-2".into(),
        platform: "Indeed".into(),
        title: Some("Staff Systems Engineer".into()),
        company: Some("Beta LLC".into()),
        email: Some("recruiting@beta.com".into()),
        url: Some("https://indeed.com/jobs/2".into()),
        note: "Worldwide".into(),
        added_at_secs: 0,
    };

    let f3 = ResearchFinding {
        id: "finding-platform-3".into(),
        platform: "Wellfound".into(),
        title: Some("Founding Engineer".into()),
        company: Some("Gamma Inc".into()),
        email: Some("founder@gamma.com".into()),
        url: Some("https://wellfound.com/jobs/3".into()),
        note: "High equity".into(),
        added_at_secs: 0,
    };

    // Subtask 1 takes 120ms
    // Subtask 2 takes 30ms (finishes FIRST)
    // Subtask 3 takes 200ms (finishes LAST)
    let factory_configs = vec![
        ("step-1".to_string(), Duration::from_millis(120), f1.clone()),
        ("step-2".to_string(), Duration::from_millis(30), f2.clone()),
        ("step-3".to_string(), Duration::from_millis(200), f3.clone()),
    ];

    let factory: Arc<dyn BrowserAgentFactory> = Arc::new(StaggeredResearchFactory {
        subtask_configs: Arc::new(Mutex::new(factory_configs)),
    });

    let w1 = BrowserAgentWorker::new("worker-1", factory.clone());
    let w2 = BrowserAgentWorker::new("worker-2", factory.clone());
    let w3 = BrowserAgentWorker::new("worker-3", factory.clone());
    let pool = Arc::new(WorkerPool::new(vec![w1, w2, w3]));
    let sink = Arc::new(TimedSink::default());

    let plan = ResearchPlan {
        goal: "find remote rust jobs across multiple platforms".into(),
        platforms: vec![
            ResearchSubTask {
                id: "step-1".into(),
                platform: "LinkedIn".into(),
                domain: "linkedin.com".into(),
                entry_hint: "Search linkedin".into(),
                acceptance: vec![Criterion::HasEmail],
                step_budget: 10,
                time_budget_secs: 60,
                query: "Rust".into(),
                platform_kind: "job_board".into(),
                query_template: String::new(),
                lead_subtask_kind: None,
            },
            ResearchSubTask {
                id: "step-2".into(),
                platform: "Indeed".into(),
                domain: "indeed.com".into(),
                entry_hint: "Search indeed".into(),
                acceptance: vec![Criterion::HasEmail],
                step_budget: 10,
                time_budget_secs: 60,
                query: "Rust".into(),
                platform_kind: "job_board".into(),
                query_template: String::new(),
                lead_subtask_kind: None,
            },
            ResearchSubTask {
                id: "step-3".into(),
                platform: "Wellfound".into(),
                domain: "wellfound.com".into(),
                entry_hint: "Search wellfound".into(),
                acceptance: vec![Criterion::HasEmail],
                step_budget: 10,
                time_budget_secs: 60,
                query: "Rust".into(),
                platform_kind: "job_board".into(),
                query_template: String::new(),
                lead_subtask_kind: None,
            },
        ],
        synthesis_hint: "Consolidate results".into(),
        overall_deadline_secs: None,
        is_research: true,
        matched_pattern: "aggregate_keyword".into(),
        max_concurrent_workers: Some(3),
        max_concurrent_per_domain: Some(1),
        stream_partial_findings: Some(true),
    };

    let handoff = Handoff::with_research_plan(
        "find remote rust jobs across multiple platforms",
        "msg-phase2-test-1",
        plan,
    );

    let start_time = Instant::now();
    let result = Planner::run(handoff, pool, sink.clone()).await;

    assert_eq!(result.status, mew_agent::handoff::BrowserStatus::Done);
    assert_eq!(result.findings.len(), 3);

    // Inspect emitted PartialFinding events and their timestamps
    let events = sink.events.lock().unwrap().clone();
    let partial_findings: Vec<(Instant, ResearchFinding)> = events
        .into_iter()
        .filter_map(|(ts, ev)| match ev {
            OrchestratorEvent::PartialFinding { finding, .. } => Some((ts, finding)),
            _ => None,
        })
        .collect();

    assert_eq!(
        partial_findings.len(),
        3,
        "Expected exactly 3 PartialFinding events emitted"
    );

    // The FIRST emitted partial finding MUST be f2 (from Indeed / step-2) because it took only 30ms!
    let (t_first, ref first_finding) = partial_findings[0];
    let (t_second, ref second_finding) = partial_findings[1];
    let (t_third, ref third_finding) = partial_findings[2];

    assert_eq!(
        first_finding.platform, "Indeed",
        "Subtask 2 (Indeed) should have completed first and emitted first"
    );
    assert_eq!(
        second_finding.platform, "LinkedIn",
        "Subtask 1 (LinkedIn) should have completed second and emitted second"
    );
    assert_eq!(
        third_finding.platform, "Wellfound",
        "Subtask 3 (Wellfound) should have completed third and emitted third"
    );

    // Verify timeline ordering
    assert!(
        t_first < t_second,
        "Subtask 2 finding must be emitted before subtask 1 completes"
    );
    assert!(
        t_second < t_third,
        "Subtask 1 finding must be emitted before subtask 3 completes"
    );

    println!(
        "[test] Verified: Finding 2 ({}) emitted in {:?}, Finding 1 ({}) in {:?}, Finding 3 ({}) in {:?}",
        first_finding.platform,
        t_first - start_time,
        second_finding.platform,
        t_second - start_time,
        third_finding.platform,
        t_third - start_time,
    );
}

#[tokio::test]
async fn test_stream_partial_findings_toggle_disables_events() {
    let f1 = ResearchFinding {
        id: "finding-1".into(),
        platform: "LinkedIn".into(),
        title: Some("Rust Dev".into()),
        company: Some("Alpha".into()),
        email: None,
        url: Some("https://linkedin.com/1".into()),
        note: "".into(),
        added_at_secs: 0,
    };

    let factory_configs = vec![("step-1".to_string(), Duration::from_millis(20), f1)];

    let factory: Arc<dyn BrowserAgentFactory> = Arc::new(StaggeredResearchFactory {
        subtask_configs: Arc::new(Mutex::new(factory_configs)),
    });

    let w1 = BrowserAgentWorker::new("worker-1", factory.clone());
    let pool = Arc::new(WorkerPool::new(vec![w1]));
    let sink = Arc::new(TimedSink::default());

    let plan = ResearchPlan {
        goal: "find rust jobs".into(),
        platforms: vec![ResearchSubTask {
            id: "step-1".into(),
            platform: "LinkedIn".into(),
            domain: "linkedin.com".into(),
            entry_hint: "Search".into(),
            acceptance: vec![Criterion::HasUrl],
            step_budget: 5,
            time_budget_secs: 30,
            query: "Rust".into(),
            platform_kind: "job_board".into(),
            query_template: String::new(),
            lead_subtask_kind: None,
        }],
        synthesis_hint: "".into(),
        overall_deadline_secs: None,
        is_research: true,
        matched_pattern: "find_x".into(),
        max_concurrent_workers: Some(1),
        max_concurrent_per_domain: Some(1),
        stream_partial_findings: Some(false), // DISABLED
    };

    let handoff = Handoff::with_research_plan("find rust jobs", "msg-stream-disabled", plan);
    let result = Planner::run(handoff, pool, sink.clone()).await;

    assert_eq!(result.status, mew_agent::handoff::BrowserStatus::Done);
    assert_eq!(result.findings.len(), 1);

    let events = sink.events.lock().unwrap().clone();
    let partial_count = events
        .into_iter()
        .filter(|(_, ev)| matches!(ev, OrchestratorEvent::PartialFinding { .. }))
        .count();

    assert_eq!(
        partial_count, 0,
        "No PartialFinding events should be emitted when stream_partial_findings is false"
    );
}

#[test]
fn test_finding_store_dedup_and_emission() {
    let sink = Arc::new(TimedSink::default());
    let mut store = FindingStore::new();
    store.set_emitter(sink.clone(), "msg-dedup-test", true);

    let f = ResearchFinding {
        id: "finding-unique-1".into(),
        platform: "LinkedIn".into(),
        title: Some("Engineer".into()),
        company: Some("Acme".into()),
        email: None,
        url: Some("https://example.com".into()),
        note: "".into(),
        added_at_secs: 0,
    };

    assert!(store.insert(f.clone()), "First insert must succeed");
    assert!(
        !store.insert(f.clone()),
        "Duplicate insert must be rejected by dedup"
    );

    let events = sink.events.lock().unwrap().clone();
    let partial_findings: Vec<ResearchFinding> = events
        .into_iter()
        .filter_map(|(_, ev)| match ev {
            OrchestratorEvent::PartialFinding { finding, .. } => Some(finding),
            _ => None,
        })
        .collect();

    assert_eq!(
        partial_findings.len(),
        1,
        "Only the first unique insert should emit PartialFinding"
    );
}
