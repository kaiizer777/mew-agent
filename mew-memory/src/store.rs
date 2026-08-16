use crate::{check_contradiction, Lesson, TaskShape};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Local persistent JSONL store for Reflexion-style episodic memory lessons.
#[derive(Debug, Clone)]
pub struct LessonStore {
    file_path: PathBuf,
}

impl LessonStore {
    /// Create a store instance pointing to `<data_dir>/memory/lessons.jsonl`.
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        let file_path = data_dir.as_ref().join("memory").join("lessons.jsonl");
        Self { file_path }
    }

    /// Create a store instance with an explicit file path (useful for tests/fixtures).
    pub fn with_path(file_path: impl Into<PathBuf>) -> Self {
        Self {
            file_path: file_path.into(),
        }
    }

    /// Get the target path of the lessons JSONL file.
    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    /// Write all lessons to disk, atomically replacing the existing file.
    pub fn write_all(&self, lessons: &[Lesson]) -> anyhow::Result<()> {
        if let Some(parent) = self.file_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let temp_path = self.file_path.with_extension("tmp");
        {
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&temp_path)?;

            for lesson in lessons {
                let line = serde_json::to_string(lesson)?;
                writeln!(file, "{}", line)?;
            }
        }
        fs::rename(temp_path, &self.file_path)?;
        Ok(())
    }

    /// Write a new lesson to local JSONL store with contradiction detection.
    pub fn write_lesson(&self, lesson: &Lesson) -> anyhow::Result<()> {
        let mut new_lesson = lesson.clone();
        let mut existing_lessons = self.read_all()?;

        for existing in &mut existing_lessons {
            if check_contradiction(&new_lesson, existing) {
                if !new_lesson.conflicting_with.contains(&existing.id) {
                    new_lesson.conflicting_with.push(existing.id.clone());
                }
                if !existing.conflicting_with.contains(&new_lesson.id) {
                    existing.conflicting_with.push(new_lesson.id.clone());
                }
            }
        }

        existing_lessons.push(new_lesson);
        self.write_all(&existing_lessons)
    }

    /// Update a specific lesson by ID via a mutating closure.
    pub fn update_lesson<F>(&self, id: &str, mutate: F) -> anyhow::Result<bool>
    where
        F: FnOnce(&mut Lesson),
    {
        let mut lessons = self.read_all()?;
        let mut found = false;
        for lesson in &mut lessons {
            if lesson.id == id {
                mutate(lesson);
                found = true;
                break;
            }
        }
        if found {
            self.write_all(&lessons)?;
        }
        Ok(found)
    }

    /// Record task outcome feedback for an injected lesson.
    /// Increments `times_seen`.
    /// If `helped`, increments `times_helped` and adds `0.1` to `confidence_score` (max 2.0).
    /// If `!helped`, subtracts `0.2` from `confidence_score` (min 0.0).
    pub fn record_feedback(&self, id: &str, helped: bool) -> anyhow::Result<Option<Lesson>> {
        let mut lessons = self.read_all()?;
        let mut updated = None;
        for lesson in &mut lessons {
            if lesson.id == id {
                lesson.times_seen += 1;
                if helped {
                    lesson.times_helped += 1;
                    lesson.confidence_score = (lesson.confidence_score + 0.1).min(2.0);
                } else {
                    lesson.confidence_score = (lesson.confidence_score - 0.2).max(0.0);
                }
                updated = Some(lesson.clone());
                break;
            }
        }
        if updated.is_some() {
            self.write_all(&lessons)?;
        }
        Ok(updated)
    }

    /// Read all lessons from the JSONL file without filtering.
    /// Returns empty vector if file does not exist yet.
    pub fn read_all(&self) -> anyhow::Result<Vec<Lesson>> {
        if !self.file_path.exists() {
            return Ok(Vec::new());
        }

        let content = fs::read_to_string(&self.file_path)?;
        let mut lessons = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(lesson) = serde_json::from_str::<Lesson>(line) {
                lessons.push(lesson);
            }
        }
        Ok(lessons)
    }

    /// Retrieve relevant lessons with custom decay score threshold & contradiction resolution.
    pub fn retrieve_relevant_with_decay(
        &self,
        task_shape: TaskShape,
        platform: Option<&str>,
        limit: usize,
        min_lesson_age_secs: u64,
        min_confidence_score: f32,
        min_times_seen_decay: u32,
    ) -> anyhow::Result<Vec<Lesson>> {
        let all = self.read_all()?;
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut matches: Vec<Lesson> = all
            .iter()
            .filter(|l| {
                if l.task_shape != task_shape {
                    return false;
                }
                if min_lesson_age_secs > 0
                    && now_secs.saturating_sub(l.created_at) < min_lesson_age_secs
                {
                    return false;
                }
                if let (Some(target_p), Some(lesson_p)) = (platform, &l.platform) {
                    let target_p_clean = target_p.trim().to_ascii_lowercase();
                    let lesson_p_clean = lesson_p.trim().to_ascii_lowercase();
                    if !target_p_clean.is_empty()
                        && !lesson_p_clean.is_empty()
                        && !lesson_p_clean.contains(&target_p_clean)
                        && !target_p_clean.contains(&lesson_p_clean)
                    {
                        return false;
                    }
                }

                // Phase 25: Decay pass check
                if l.times_seen >= min_times_seen_decay && l.confidence_score < min_confidence_score
                {
                    return false;
                }

                // Phase 25: Contradiction resolution check
                if !l.conflicting_with.is_empty() {
                    for conflict_id in &l.conflicting_with {
                        if let Some(conflict) = all.iter().find(|other| other.id == *conflict_id) {
                            // If the conflicting lesson has significantly higher confidence, exclude `l`.
                            // If confidence scores are equal or tied, withhold BOTH until resolved.
                            if conflict.confidence_score >= l.confidence_score + 0.3
                                || (conflict.confidence_score - l.confidence_score).abs() < 0.3
                            {
                                return false;
                            }
                        }
                    }
                }

                true
            })
            .cloned()
            .collect();

        // Most recent first
        matches.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        matches.truncate(limit);
        Ok(matches)
    }

    /// Retrieve lessons filtered by task shape, optional platform, minimum age, decay threshold, and capped by limit.
    pub fn retrieve_relevant(
        &self,
        task_shape: TaskShape,
        platform: Option<&str>,
        limit: usize,
        min_lesson_age_secs: u64,
    ) -> anyhow::Result<Vec<Lesson>> {
        self.retrieve_relevant_with_decay(task_shape, platform, limit, min_lesson_age_secs, 0.3, 2)
    }

    /// Retrieve lessons filtered by task shape and optional platform (legacy wrapper).
    pub fn retrieve_matching(
        &self,
        task_shape: TaskShape,
        platform: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<Lesson>> {
        self.retrieve_relevant(task_shape, platform, limit, 0)
    }
}
