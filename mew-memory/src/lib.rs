//! mew v2 — Phase 23: Episodic Memory Store (Reflexion-style write path).
//!
//! Pure Rust local storage for episodic memory lessons distilled from agent task runs.
//! Lessons are text-form verbal insights written to `<data_dir>/memory/lessons.jsonl`.

pub mod store;

use serde::{Deserialize, Serialize};
pub use store::LessonStore;

/// Task shape classification, mirroring `mew-agent`'s router dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum TaskShape {
    JobSearch,
    LeadGen,
    #[default]
    GenericResearch,
    GenericBrowse,
    /// Phase 34: Google Maps local-business lead generation. Like
    /// `GenericBrowse` in that the user is on a single site, but
    /// the site (Maps) has a specific panel + sidebar + escape-key
    /// SPA workflow that the agent needs explicit guidance for.
    /// Adds `MapsQuery` / `MapsFinding` to `Handoff` / `BrowserResult`
    /// and a system-prompt hint block during `run_preflight_plan`.
    LocalLeadGen,
}

/// Task execution outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Success,
    Failure,
}

fn default_confidence_score() -> f32 {
    1.0
}

/// A short, reusable, text-form lesson distilled from a task execution session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lesson {
    pub id: String,
    pub created_at: u64,
    pub task_shape: TaskShape,
    pub platform: Option<String>,
    pub situation: String,
    pub outcome: Outcome,
    pub lesson_text: String,
    pub originating_session_id: String,

    /// Phase 25: Confidence score for memory decay and feedback (default: 1.0).
    #[serde(default = "default_confidence_score")]
    pub confidence_score: f32,
    /// Number of times injected/retrieved during agent execution.
    #[serde(default)]
    pub times_seen: u32,
    /// Number of times session succeeded without warning recurrence when injected.
    #[serde(default)]
    pub times_helped: u32,
    /// List of lesson IDs that contradict this lesson.
    #[serde(default)]
    pub conflicting_with: Vec<String>,
}

impl Lesson {
    pub fn new(
        id: impl Into<String>,
        created_at: u64,
        task_shape: TaskShape,
        platform: Option<String>,
        situation: impl Into<String>,
        outcome: Outcome,
        lesson_text: impl Into<String>,
        originating_session_id: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            created_at,
            task_shape,
            platform,
            situation: situation.into(),
            outcome,
            lesson_text: lesson_text.into(),
            originating_session_id: originating_session_id.into(),
            confidence_score: 1.0,
            times_seen: 0,
            times_helped: 0,
            conflicting_with: Vec::new(),
        }
    }
}

/// Heuristic check if two lessons for the same platform/situation express opposing guidance.
pub fn check_contradiction(new_lesson: &Lesson, existing: &Lesson) -> bool {
    if new_lesson.task_shape != existing.task_shape {
        return false;
    }

    let same_platform = match (&new_lesson.platform, &existing.platform) {
        (Some(p1), Some(p2)) => {
            let p1_clean = p1.trim().to_ascii_lowercase();
            let p2_clean = p2.trim().to_ascii_lowercase();
            p1_clean == p2_clean || p1_clean.contains(&p2_clean) || p2_clean.contains(&p1_clean)
        }
        (None, None) => true,
        _ => false,
    };

    if !same_platform {
        return false;
    }

    let text1 = new_lesson.lesson_text.to_lowercase();
    let text2 = existing.lesson_text.to_lowercase();

    // Opposing action guidance pairs
    let opposing_pairs = [
        ("use", "avoid"),
        ("always", "never"),
        ("enable", "disable"),
        ("require", "skip"),
        ("click", "avoid click"),
        ("query param", "ui click"),
        ("do not", "do"),
    ];

    for (word1, word2) in opposing_pairs {
        if (text1.contains(word1) && text2.contains(word2))
            || (text1.contains(word2) && text2.contains(word1))
        {
            return true;
        }
    }

    false
}

/// Helper struct for parsing LLM lesson distillation responses.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LessonCandidate {
    pub platform: Option<String>,
    pub situation: String,
    pub lesson_text: String,
}

/// Parse distillation LLM output softly into a vector of `LessonCandidate`.
/// Returns an empty vector on malformed/unparseable JSON without ever panicking.
pub fn parse_llm_lessons_soft(raw_json: &str) -> Vec<LessonCandidate> {
    #[derive(Deserialize)]
    struct DistillationResponse {
        #[serde(default)]
        lessons: Vec<LessonCandidate>,
    }

    if raw_json.trim().is_empty() {
        return Vec::new();
    }

    serde_json::from_str::<DistillationResponse>(raw_json)
        .map(|r| r.lessons)
        .unwrap_or_default()
}

/// Top-level helper for retrieving relevant lessons from a store directory.
pub fn retrieve_relevant(
    data_dir: impl AsRef<std::path::Path>,
    task_shape: TaskShape,
    platform: Option<&str>,
    limit: usize,
    min_lesson_age_secs: u64,
) -> anyhow::Result<Vec<Lesson>> {
    let store = LessonStore::new(data_dir);
    store.retrieve_relevant(task_shape, platform, limit, min_lesson_age_secs)
}

/// Render retrieved lessons into a prompt block for system prompt injection.
/// Each lesson includes situation context to ensure traceability.
pub fn render_learned_block(lessons: &[Lesson]) -> String {
    if lessons.is_empty() {
        return "LEARNED (from past sessions):\n(none — no prior failure lessons apply to this task)".to_string();
    }

    let mut lines = Vec::new();
    lines.push("LEARNED (from past sessions):".to_string());
    for lesson in lessons {
        let platform_str = lesson
            .platform
            .as_deref()
            .map(|p| format!(" [{p}]"))
            .unwrap_or_default();
        lines.push(format!(
            "- [ID: {}]{} Situation: \"{}\" -> Lesson: {}",
            lesson.id, platform_str, lesson.situation, lesson.lesson_text
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(1);

    fn temp_test_dir() -> std::path::PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("mew_memory_test_{}_{}", ts, id));
        let _ = std::fs::create_dir_all(&path);
        path
    }

    #[test]
    fn test_write_and_read_lesson() {
        let dir = temp_test_dir();
        let store = LessonStore::new(&dir);

        let lesson = Lesson::new(
            "les_1001",
            1700000000,
            TaskShape::JobSearch,
            Some("linkedin.com".into()),
            "LinkedIn job filter chips require JS click",
            Outcome::Failure,
            "Use UI click action on filter chips when URL query param fails",
            "sess_abc",
        );

        store.write_lesson(&lesson).unwrap();
        let read = store.read_all().unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0], lesson);
    }

    #[test]
    fn test_failure_session_with_detector_produces_lesson() {
        let dir = temp_test_dir();
        let store = LessonStore::new(&dir);

        // Mock LLM response for a failure session where modal_interruption fired
        let mock_llm_response = r#"{
            "summary": "Task failed to submit due to newsletter modal.",
            "lessons": [
                {
                    "platform": "example.com",
                    "situation": "Newsletter modal blocked submission button",
                    "lesson_text": "Auto-dismiss newsletter overlay before attempting form submit"
                }
            ]
        }"#;

        let candidates = parse_llm_lessons_soft(mock_llm_response);
        assert_eq!(candidates.len(), 1);

        let lesson = Lesson::new(
            "les_mock_1",
            1700000100,
            TaskShape::GenericBrowse,
            candidates[0].platform.clone(),
            candidates[0].situation.clone(),
            Outcome::Failure,
            candidates[0].lesson_text.clone(),
            "sess_fail",
        );

        store.write_lesson(&lesson).unwrap();
        let all = store.read_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].outcome, Outcome::Failure);
        assert!(all[0].lesson_text.contains("Auto-dismiss"));
    }

    #[test]
    fn test_routine_clean_success_produces_zero_lessons() {
        let dir = temp_test_dir();
        let store = LessonStore::new(&dir);

        // Mock LLM response for clean routine success
        let mock_llm_response = r#"{
            "summary": "Navigated to home page and extracted title.",
            "lessons": []
        }"#;

        let candidates = parse_llm_lessons_soft(mock_llm_response);
        assert_eq!(candidates.len(), 0);

        for c in candidates {
            let lesson = Lesson::new(
                "les_clean",
                1700000200,
                TaskShape::GenericBrowse,
                c.platform,
                c.situation,
                Outcome::Success,
                c.lesson_text,
                "sess_clean",
            );
            store.write_lesson(&lesson).unwrap();
        }

        let all = store.read_all().unwrap();
        assert_eq!(all.len(), 0, "Clean success should write zero lessons");
    }

    #[test]
    fn test_malformed_llm_output_fails_soft() {
        let dir = temp_test_dir();
        let store = LessonStore::new(&dir);

        let malformed_outputs = vec![
            "",
            "not json at all",
            r#"{"summary": "incomplete", "lessons": "invalid_type"}"#,
            r#"{"summary": "broken json"#,
        ];

        for output in malformed_outputs {
            let candidates = parse_llm_lessons_soft(output);
            assert_eq!(
                candidates.len(),
                0,
                "Malformed output must parse soft to 0 candidates"
            );
        }

        assert_eq!(store.read_all().unwrap().len(), 0);
    }

    #[test]
    fn test_retrieve_relevant_platform_matching_and_cap() {
        let dir = temp_test_dir();
        let store = LessonStore::new(&dir);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        for i in 1..=8 {
            let platform = if i % 2 == 0 {
                Some("linkedin.com".into())
            } else {
                Some("indeed.com".into())
            };
            let lesson = Lesson::new(
                format!("les_{}", i),
                now - (i * 10),
                TaskShape::JobSearch,
                platform,
                format!("Situation {}", i),
                Outcome::Failure,
                format!("Lesson text {}", i),
                format!("sess_{}", i),
            );
            store.write_lesson(&lesson).unwrap();
        }

        // Test matching platform & cap (limit = 3)
        let linkedin_lessons = store
            .retrieve_relevant(TaskShape::JobSearch, Some("linkedin.com"), 3, 0)
            .unwrap();
        assert_eq!(linkedin_lessons.len(), 3);
        for l in &linkedin_lessons {
            assert_eq!(l.platform.as_deref(), Some("linkedin.com"));
        }
        // Most recent first: i=2 is most recent among linkedin (now - 20)
        assert_eq!(linkedin_lessons[0].id, "les_2");

        // Test filtering irrelevant platform
        let apollo_lessons = store
            .retrieve_relevant(TaskShape::JobSearch, Some("apollo.io"), 5, 0)
            .unwrap();
        assert_eq!(apollo_lessons.len(), 0);
    }

    #[test]
    fn test_retrieve_relevant_min_lesson_age() {
        let dir = temp_test_dir();
        let store = LessonStore::new(&dir);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Lesson created 10 seconds ago
        let recent_lesson = Lesson::new(
            "les_recent",
            now - 10,
            TaskShape::LeadGen,
            Some("apollo.io".into()),
            "Recent friction",
            Outcome::Failure,
            "Recent lesson text",
            "sess_rec",
        );
        // Lesson created 500 seconds ago
        let old_lesson = Lesson::new(
            "les_old",
            now - 500,
            TaskShape::LeadGen,
            Some("apollo.io".into()),
            "Old friction",
            Outcome::Failure,
            "Old lesson text",
            "sess_old",
        );

        store.write_lesson(&recent_lesson).unwrap();
        store.write_lesson(&old_lesson).unwrap();

        // Require age >= 100 seconds: recent_lesson (10s old) should be filtered out
        let mature_lessons = store
            .retrieve_relevant(TaskShape::LeadGen, Some("apollo.io"), 5, 100)
            .unwrap();
        assert_eq!(mature_lessons.len(), 1);
        assert_eq!(mature_lessons[0].id, "les_old");
    }

    #[test]
    fn test_render_learned_block_traceability() {
        let lessons = vec![Lesson::new(
            "les_1001",
            1700000000,
            TaskShape::JobSearch,
            Some("linkedin.com".into()),
            "LinkedIn job filter chips require JS click",
            Outcome::Failure,
            "Use UI click action on filter chips when URL query param fails",
            "sess_abc",
        )];

        let rendered = render_learned_block(&lessons);
        assert!(rendered.contains("LEARNED (from past sessions):"));
        assert!(rendered.contains("[ID: les_1001]"));
        assert!(rendered.contains("[linkedin.com]"));
        assert!(rendered.contains("Situation: \"LinkedIn job filter chips require JS click\""));
        assert!(rendered.contains("Lesson: Use UI click action"));

        let empty_rendered = render_learned_block(&[]);
        assert!(empty_rendered.contains("(none"));
    }

    #[test]
    fn test_confidence_feedback_increment_and_decrement() {
        let dir = temp_test_dir();
        let store = LessonStore::new(&dir);

        let lesson = Lesson::new(
            "les_fb_1",
            1700000000,
            TaskShape::JobSearch,
            Some("linkedin.com".into()),
            "Filter chip issue",
            Outcome::Failure,
            "Use UI click action",
            "sess_fb",
        );
        store.write_lesson(&lesson).unwrap();

        // Feed positive feedback (+0.1)
        let updated1 = store.record_feedback("les_fb_1", true).unwrap().unwrap();
        assert_eq!(updated1.times_seen, 1);
        assert_eq!(updated1.times_helped, 1);
        assert!((updated1.confidence_score - 1.1).abs() < 1e-5);

        // Feed negative feedback (-0.2) twice
        store.record_feedback("les_fb_1", false).unwrap();
        let updated3 = store.record_feedback("les_fb_1", false).unwrap().unwrap();
        assert_eq!(updated3.times_seen, 3);
        assert_eq!(updated3.times_helped, 1);
        assert!((updated3.confidence_score - 0.7).abs() < 1e-5);
    }

    #[test]
    fn test_decay_pass_excludes_low_confidence_without_deleting() {
        let dir = temp_test_dir();
        let store = LessonStore::new(&dir);

        let mut low_conf_lesson = Lesson::new(
            "les_decay_1",
            1700000000,
            TaskShape::JobSearch,
            Some("linkedin.com".into()),
            "Stale tip",
            Outcome::Failure,
            "Use obsolete parameter",
            "sess_decay",
        );
        low_conf_lesson.confidence_score = 0.2;
        low_conf_lesson.times_seen = 2;

        store.write_lesson(&low_conf_lesson).unwrap();

        // Excluded from retrieve_relevant because confidence (0.2) < 0.3 after times_seen >= 2
        let retrieved = store
            .retrieve_relevant(TaskShape::JobSearch, Some("linkedin.com"), 5, 0)
            .unwrap();
        assert_eq!(retrieved.len(), 0);

        // Crucial check: still present in raw read_all (no silent deletion)
        let raw = store.read_all().unwrap();
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0].id, "les_decay_1");
    }

    #[test]
    fn test_contradiction_detection_and_withholding() {
        let dir = temp_test_dir();
        let store = LessonStore::new(&dir);

        let lesson1 = Lesson::new(
            "les_c1",
            1700000000,
            TaskShape::JobSearch,
            Some("linkedin.com".into()),
            "URL vs UI filter",
            Outcome::Failure,
            "Use UI click on filter chips",
            "sess_c1",
        );

        let lesson2 = Lesson::new(
            "les_c2",
            1700000010,
            TaskShape::JobSearch,
            Some("linkedin.com".into()),
            "URL vs UI filter",
            Outcome::Failure,
            "Avoid click on filter chips, use URL query param",
            "sess_c2",
        );

        store.write_lesson(&lesson1).unwrap();
        store.write_lesson(&lesson2).unwrap();

        let all = store.read_all().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].conflicting_with.contains(&"les_c2".to_string()));
        assert!(all[1].conflicting_with.contains(&"les_c1".to_string()));

        // Both tied at confidence 1.0 -> withheld from injection
        let retrieved = store
            .retrieve_relevant(TaskShape::JobSearch, Some("linkedin.com"), 5, 0)
            .unwrap();
        assert_eq!(retrieved.len(), 0);

        // Give lesson1 higher confidence (1.5 vs 1.0)
        store
            .update_lesson("les_c1", |l| l.confidence_score = 1.5)
            .unwrap();

        // Higher confidence lesson1 wins!
        let retrieved_resolved = store
            .retrieve_relevant(TaskShape::JobSearch, Some("linkedin.com"), 5, 0)
            .unwrap();
        assert_eq!(retrieved_resolved.len(), 1);
        assert_eq!(retrieved_resolved[0].id, "les_c1");
    }

    #[test]
    fn test_audit_trail_no_silent_deletion() {
        let dir = temp_test_dir();
        let store = LessonStore::new(&dir);

        for i in 1..=5 {
            let mut l = Lesson::new(
                format!("les_audit_{}", i),
                1700000000 + i,
                TaskShape::GenericBrowse,
                Some("example.com".into()),
                "Generic situation",
                Outcome::Failure,
                format!("Tip {}", i),
                "sess_audit",
            );
            if i % 2 == 0 {
                l.confidence_score = 0.1;
                l.times_seen = 5;
            }
            store.write_lesson(&l).unwrap();
        }

        let raw = store.read_all().unwrap();
        assert_eq!(raw.len(), 5, "All 5 records must exist in raw store file");
    }
}
