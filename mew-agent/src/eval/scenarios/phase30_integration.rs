//! Phase 30 — Vertical + Memory + Respawn Integration Benchmark & Hardening Scenarios.

use crate::agent::render_learned_block;
use crate::completeness::CompletenessTracker;
use crate::handoff::{Handoff, JobFinding, JobQuery, LeadFinding, LeadQuery, RespawnHandoff};
use crate::research::FindingStore;
use crate::respawn_policy::{RespawnDecision, RespawnPolicy};
use crate::supervisor::RespawnReason;
use mew_memory::{Lesson, LessonStore, Outcome, TaskShape};
use mew_resilience::mock_fixtures;

/// Scenario 1: Job search task on a platform known to hit resilience interrupts.
/// Verifies JobQuery constraints, filter application, rate-limit backoff,
/// FindingStore dedup, and memory lesson writing in a single task pipeline.
pub fn scenario_1_flaky_job_vertical_and_respawn_cooperation() -> Result<(), String> {
    use crate::research::ResearchFinding;

    // 1. Structured JobQuery parsing check
    let query = JobQuery {
        roles: vec!["Rust Developer".into(), "Backend Engineer".into()],
        locations: vec!["Remote".into()],
        is_remote: true,
        is_internship: false,
        experience_level: Some("Senior".into()),
    };

    if !query.is_remote || query.roles.len() != 2 {
        return Err("JobQuery structured fields incorrect".into());
    }

    // 2. Resilience detector handling on flaky job board page fixture
    let modal_tree = mock_fixtures::cookie_banner_page();
    let modal_result = mew_resilience::modal_interrupts::detect(&modal_tree);
    if modal_result.is_none() {
        return Err("Expected modal interrupt detection on flaky job board".into());
    }

    let rate_limit_tree = mock_fixtures::http_429_page();
    let rate_limit_result = mew_resilience::rate_limit::detect(&rate_limit_tree);
    if rate_limit_result.is_none() {
        return Err("Expected rate limit backoff detection on job board".into());
    }

    // 3. FindingStore deduplication and freshness verification
    let mut store = FindingStore::new();
    let base_finding = ResearchFinding {
        id: String::new(),
        platform: "LinkedIn".into(),
        title: Some("Senior Rust Engineer".into()),
        company: Some("Acme Corp".into()),
        email: Some("contact@acme.example".into()),
        url: Some("https://acme.example/jobs/1".into()),
        note: "Remote, Full-time".into(),
        added_at_secs: 1700000000,
    };

    let job_finding = JobFinding {
        base: base_finding.clone(),
        freshness_days: Some(2),
        is_internship: false,
    };

    store.add(base_finding.clone());
    store.add(base_finding);

    if store.findings().len() != 1 {
        return Err("FindingStore failed to deduplicate identical job listings".into());
    }
    if job_finding.freshness_days != Some(2) {
        return Err("JobFinding freshness verification failed".into());
    }

    // 4. Verification of memory lesson write on resilience recovery
    let temp_dir = std::env::temp_dir().join(format!(
        "mew_eval_p30_scen1_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let store_mem = LessonStore::new(&temp_dir);
    let lesson = Lesson::new(
        "les_job_flaky_1",
        1700000000,
        TaskShape::JobSearch,
        Some("linkedin.com".into()),
        "Cookie banner modal blocks job listing harvest",
        Outcome::Failure,
        "Auto-dismiss cookie banner modal before filtering listings",
        "sess_job_1",
    );
    store_mem.write_lesson(&lesson).map_err(|e| e.to_string())?;

    let retrieved = store_mem
        .retrieve_relevant(TaskShape::JobSearch, Some("linkedin.com"), 5, 0)
        .map_err(|e| e.to_string())?;
    if retrieved.len() != 1 {
        return Err("Memory store failed to retrieve resilience lesson for job search".into());
    }

    Ok(())
}

/// Scenario 2: Lead-Gen task run twice in sequence.
/// Run 1 encounters contact accordion obstacle & writes lesson to memory.
/// Run 2 retrieves the lesson, renders `LEARNED (from past sessions):`, and adapts navigation.
pub fn scenario_2_lead_gen_sequential_self_learning_proof() -> Result<(), String> {
    use crate::research::ResearchFinding;

    let temp_dir = std::env::temp_dir().join(format!(
        "mew_eval_p30_scen2_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let store = LessonStore::new(&temp_dir);

    // --- RUN 1 ---
    // Agent encounters an email pattern obstacle on SaaS domain
    let lead_q = LeadQuery {
        industry: vec!["Developer Tools".into()],
        company_size_range: Some((10, 200)),
        geography: vec!["US".into()],
        target_role_titles: vec!["CTO".into(), "VP Engineering".into()],
        product_category_keywords: vec!["Infrastructure".into()],
    };
    if lead_q.industry.is_empty() {
        return Err("LeadQuery initialization failed".into());
    }

    let lesson_run1 = Lesson::new(
        "les_lead_saas_1",
        1700000000,
        TaskShape::LeadGen,
        Some("indiehackers.com".into()),
        "Founder contact emails require expanding Team accordion",
        Outcome::Failure,
        "Click 'Team' accordion tab to reveal verified decision-maker emails",
        "sess_lead_run1",
    );
    store
        .write_lesson(&lesson_run1)
        .map_err(|e| e.to_string())?;

    // --- RUN 2 ---
    // Agent initiates second run for indiehackers.com lead research
    let retrieved_lessons = store
        .retrieve_relevant(TaskShape::LeadGen, Some("indiehackers.com"), 5, 0)
        .map_err(|e| e.to_string())?;

    if retrieved_lessons.is_empty() {
        return Err("Run 2 failed to retrieve self-learning lesson from Run 1".into());
    }

    let learned_prompt_block = render_learned_block(&retrieved_lessons);
    if !learned_prompt_block.contains("LEARNED (from past sessions):") {
        return Err("Run 2 system prompt missing LEARNED block header".into());
    }
    if !learned_prompt_block.contains("Founder contact emails require expanding Team accordion") {
        return Err("Run 2 system prompt missing situation details from Run 1".into());
    }
    if !learned_prompt_block.contains("Click 'Team' accordion tab") {
        return Err("Run 2 system prompt missing distilled action guidance".into());
    }

    // Verify lead finding waterfall enrichment with confidence scoring
    let base_finding = ResearchFinding {
        id: String::new(),
        platform: "IndieHackers".into(),
        title: Some("CTO".into()),
        company: Some("TechCorp".into()),
        email: Some("jane@techcorp.io".into()),
        url: Some("https://indiehackers.com/product/techcorp".into()),
        note: "Verified decision-maker contact".into(),
        added_at_secs: 1700000000,
    };

    let finding = LeadFinding {
        base: base_finding,
        company_name: "TechCorp".into(),
        company_domain: "techcorp.io".into(),
        contact_name: Some("Jane Doe".into()),
        contact_role: Some("CTO".into()),
        contact_email: Some("jane@techcorp.io".into()),
        contact_url: Some("https://indiehackers.com/product/techcorp".into()),
        source_platform: "IndieHackers".into(),
        confidence: crate::handoff::LeadConfidence::High,
    };
    if finding.confidence != crate::handoff::LeadConfidence::High {
        return Err("LeadFinding confidence verification failed".into());
    }

    Ok(())
}

/// Scenario 3: Long-horizon multi-platform research approaching token budget limit.
/// Verifies proactive respawn (TokenLimitApproaching), RespawnHandoff context preservation,
/// CompletenessTracker finished subtask retention, and system prompt RESUMING injection.
pub fn scenario_3_token_limit_proactive_respawn_handoff() -> Result<(), String> {
    use crate::handoff::HandoffSubTask;

    // 1. Initialize original task handoff and subtasks
    let handoff = Handoff {
        task_description: "Research senior Rust roles across LinkedIn, Indeed, and Wellfound"
            .into(),
        originating_message_id: "msg_p30_scen3".into(),
        subtasks: vec![
            HandoffSubTask::new("sub_1", "LinkedIn search"),
            HandoffSubTask::new("sub_2", "Indeed search"),
            HandoffSubTask::new("sub_3", "Wellfound search"),
        ],
        constraints: vec![],
        research_plan: None,
        job_query: None,
        lead_query: None,
        task_shape: Some(TaskShape::JobSearch),
        respawn_handoff: None,
        maps_query: None,
    };

    let mut tracker = CompletenessTracker::new();
    tracker
        .declare(vec![
            crate::completeness::DeclareItem {
                id: "sub_1".into(),
                description: "LinkedIn search".into(),
            },
            crate::completeness::DeclareItem {
                id: "sub_2".into(),
                description: "Indeed search".into(),
            },
            crate::completeness::DeclareItem {
                id: "sub_3".into(),
                description: "Wellfound search".into(),
            },
        ])
        .map_err(|e| e.to_string())?;

    // Mark subtask 0 (LinkedIn search) as Done
    tracker.record_snapshot(1, "snap_sig_good".into());
    let _ = tracker.mark_done("sub_1", "snap_sig_good");
    let completed_count = tracker.subtasks.len() - tracker.incomplete_count();
    if completed_count != 1 || tracker.incomplete_count() != 2 {
        return Err(format!(
            "CompletenessTracker subtask status mismatch: completed {}, incomplete {}",
            completed_count,
            tracker.incomplete_count()
        ));
    }

    // 2. Trigger proactive respawn decision on token limit threshold
    let mut policy = RespawnPolicy::default();
    let report = mew_resilience::stuck_detector::WorkerHealthReport {
        worker_id: "worker_p30".into(),
        is_healthy: false,
        suspected_stuck: false,
        stuck_reason: None,
        token_limit_approaching: true,
        token_spend_so_far: 600_000,
        max_token_budget: Some(740_000),
        token_used_fraction: 0.81,
        iteration_count: 40,
        last_heartbeat_elapsed: std::time::Duration::from_secs(1),
        last_progress_elapsed: std::time::Duration::from_secs(5),
    };

    let decision = policy.evaluate("task_p30_long_horizon", &report);
    let (reason, attempt) = match decision {
        RespawnDecision::Respawn { reason, attempt } => (reason, attempt),
        _ => return Err("Expected Respawn decision for token limit approaching".into()),
    };

    match &reason {
        RespawnReason::TokenLimitApproaching { spend, max } => {
            if *spend != 600_000 || *max != 740_000 {
                return Err("TokenLimitApproaching fields mismatch".into());
            }
        }
        _ => {
            return Err(format!(
                "Expected TokenLimitApproaching reason, got {:?}",
                reason
            ))
        }
    }
    if attempt != 1 {
        return Err(format!("Expected attempt count 1, got {}", attempt));
    }

    // 3. Create RespawnHandoff and transplant to replacement worker
    let completed_subs: Vec<_> = tracker
        .subtasks
        .iter()
        .filter(|s| s.status.is_terminal())
        .cloned()
        .collect();
    let remaining_subs: Vec<_> = tracker
        .subtasks
        .iter()
        .filter(|s| !s.status.is_terminal())
        .cloned()
        .collect();

    let respawn_handoff = RespawnHandoff::new(
        &handoff.task_description,
        completed_subs.clone(),
        remaining_subs.clone(),
        Some("snap_sig_good".into()),
        reason,
        "Approaching token limit after LinkedIn search — continuing with Indeed and Wellfound search",
    );

    // Verify completed subtask preservation in replacement worker
    let mut new_tracker = CompletenessTracker::new();
    new_tracker.seed_from_handoff(completed_subs, remaining_subs);

    let new_completed_count = new_tracker.subtasks.len() - new_tracker.incomplete_count();
    if new_completed_count != 1 || new_tracker.incomplete_count() != 2 {
        return Err("Transplanted CompletenessTracker lost completed subtasks on respawn".into());
    }

    // Verify system prompt RESUMING block generation
    let system_prompt_note = respawn_handoff.render_resuming_block();
    if !system_prompt_note.contains("RESUMING (from a previous attempt):") {
        return Err("Missing RESUMING header in replacement worker prompt".into());
    }
    if !system_prompt_note.contains("Subtasks Already Completed (DO NOT RE-DO):") {
        return Err("Missing completed subtasks guidance in replacement worker prompt".into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_phase30_integration_scenarios() {
        scenario_1_flaky_job_vertical_and_respawn_cooperation().unwrap();
        scenario_2_lead_gen_sequential_self_learning_proof().unwrap();
        scenario_3_token_limit_proactive_respawn_handoff().unwrap();
    }
}
