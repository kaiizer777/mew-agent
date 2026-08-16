//! Phase 24 — Evaluation harness scenarios for memory retrieval & prompt injection.

use crate::agent::render_learned_block;
use mew_memory::{Lesson, LessonStore, Outcome, TaskShape};

/// Test scenario 1: prior lesson for a specific platform is correctly retrieved & injected into system prompt.
pub fn test_same_platform_memory_injection() -> Result<(), String> {
    let temp_dir = std::env::temp_dir().join(format!(
        "mew_eval_mem_same_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let _ = std::fs::create_dir_all(&temp_dir);

    let store = LessonStore::new(&temp_dir);
    let lesson = Lesson::new(
        "les_linkedin_1",
        1700000000,
        TaskShape::JobSearch,
        Some("linkedin.com".into()),
        "LinkedIn job filter chips require JS click",
        Outcome::Failure,
        "Use explicit UI click action on filter chips when URL params fail",
        "sess_prior",
    );
    store.write_lesson(&lesson).map_err(|e| e.to_string())?;

    let lessons = store
        .retrieve_relevant(TaskShape::JobSearch, Some("linkedin.com"), 5, 0)
        .map_err(|e| e.to_string())?;

    if lessons.len() != 1 {
        return Err(format!(
            "expected 1 retrieved lesson, got {}",
            lessons.len()
        ));
    }

    let block = render_learned_block(&lessons);
    if !block.contains("LEARNED (from past sessions):") {
        return Err("missing LEARNED header".into());
    }
    if !block.contains("LinkedIn job filter chips require JS click") {
        return Err("missing situation text in prompt block".into());
    }
    if !block.contains("Use explicit UI click action") {
        return Err("missing lesson_text in prompt block".into());
    }

    Ok(())
}

/// Test scenario 2: lesson for an irrelevant platform is excluded from retrieval.
pub fn test_irrelevant_platform_memory_exclusion() -> Result<(), String> {
    let temp_dir = std::env::temp_dir().join(format!(
        "mew_eval_mem_excl_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let _ = std::fs::create_dir_all(&temp_dir);

    let store = LessonStore::new(&temp_dir);
    let lesson = Lesson::new(
        "les_indeed_1",
        1700000000,
        TaskShape::JobSearch,
        Some("indeed.com".into()),
        "Indeed popup overlay",
        Outcome::Failure,
        "Dismiss indeed popup first",
        "sess_prior_indeed",
    );
    store.write_lesson(&lesson).map_err(|e| e.to_string())?;

    // Query for linkedin.com task
    let lessons = store
        .retrieve_relevant(TaskShape::JobSearch, Some("linkedin.com"), 5, 0)
        .map_err(|e| e.to_string())?;

    if !lessons.is_empty() {
        return Err(format!(
            "expected 0 retrieved lessons for irrelevant platform, got {}",
            lessons.len()
        ));
    }

    let block = render_learned_block(&lessons);
    if !block.contains("(none matching prior tasks)") {
        return Err("expected '(none matching prior tasks)' for empty retrieval".into());
    }

    Ok(())
}

/// Test scenario 3: `max_lessons_per_task` cap is strictly respected.
pub fn test_max_lessons_per_task_cap() -> Result<(), String> {
    let temp_dir = std::env::temp_dir().join(format!(
        "mew_eval_mem_cap_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let _ = std::fs::create_dir_all(&temp_dir);

    let store = LessonStore::new(&temp_dir);
    for i in 1..=10 {
        let lesson = Lesson::new(
            format!("les_cap_{}", i),
            1700000000 + i,
            TaskShape::LeadGen,
            Some("apollo.io".into()),
            format!("Apollo friction {}", i),
            Outcome::Failure,
            format!("Apollo lesson {}", i),
            "sess_cap",
        );
        store.write_lesson(&lesson).map_err(|e| e.to_string())?;
    }

    // Capped at 5
    let max_5 = store
        .retrieve_relevant(TaskShape::LeadGen, Some("apollo.io"), 5, 0)
        .map_err(|e| e.to_string())?;
    if max_5.len() != 5 {
        return Err(format!("expected 5 lessons, got {}", max_5.len()));
    }

    // Capped at 3
    let max_3 = store
        .retrieve_relevant(TaskShape::LeadGen, Some("apollo.io"), 3, 0)
        .map_err(|e| e.to_string())?;
    if max_3.len() != 3 {
        return Err(format!("expected 3 lessons, got {}", max_3.len()));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eval_memory_retrieval_scenarios() {
        test_same_platform_memory_injection().unwrap();
        test_irrelevant_platform_memory_exclusion().unwrap();
        test_max_lessons_per_task_cap().unwrap();
    }
}
