//! Phase 8 — Job / freelance lessons surface.
//!
//! Background: the existing `get_recent_lessons` Tauri command
//! surfaces *all* recent high-confidence lessons to the chat
//! list, but doesn't filter by task shape. The Phase 8 spec
//! calls for a more targeted surface: a Tauri command that
//! surfaces *job and freelance* lessons only, with a typed
//! shape and platform-aware rendering, so the user can see at
//! a glance "what has the agent picked up about which job
//! boards / freelance platforms actually work well for me?"
//!
//! Why a separate surface (not just re-use `get_recent_lessons`):
//!
//!   * The Phase 29 `get_recent_lessons` returns a
//!     `Vec<String>` of plain-language one-liners, all shapes
//!     mixed together. The Phase 8 spec needs a *typed*
//!     response (`JobFreelanceLesson` with `task_shape`,
//!     `platform`, `lesson_text`, etc.) so the chat surface
//!     can render it with a stable layout and the user can
//!     filter / sort in the UI.
//!   * The Tauri command should be self-contained — no LLM
//!     call, no network. The pure-Rust surface here reads
//!     the on-disk `lessons.jsonl` file the `mew-memory`
//!     store already maintains and filters to the two
//!     relevant task shapes (JobSearch, LeadGen).
//!
//! ## Public surface
//!
//! ```ignore
//! pub struct JobFreelanceLesson { ... }
//! pub fn collect(data_dir: &Path) -> Vec<JobFreelanceLesson>
//! pub fn render_summary(lessons: &[JobFreelanceLesson]) -> String
//! ```
//!
//! See `mew-agent/tests/phase8_presets.rs` for the typed
//! fixture tests and `mew-ui/src-tauri/src/lib.rs::get_job_freelance_lessons`
//! for the Tauri command wiring.

use std::path::Path;

use mew_memory::{Lesson, Outcome, TaskShape};

/// A job or freelance lesson the user-facing surface can
/// render. The fields are stable so the chat list / Tauri
/// command payload can be diff-tested. `Serialize` is
/// derived so the Tauri `generate_handler!` macro can
/// ship the struct to the webview as a typed JSON payload.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct JobFreelanceLesson {
    /// Lesson id (the same id the `mew-memory` store uses).
    /// Surfaced so the Tauri command can implement
    /// "thumbs-up / thumbs-down" feedback in a future
    /// iteration without changing the wire format.
    pub id: String,
    /// Whether this is a `JobSearch` lesson (about job
    /// boards) or a `LeadGen` lesson (about freelance
    /// lead-gen platforms). Rendered as a leading
    /// "Jobs:" / "Freelance:" tag in the chat surface.
    pub task_shape: TaskShape,
    /// The platform the lesson is about (e.g. "LinkedIn",
    /// "Upwork"). `None` when the lesson is cross-platform
    /// (e.g. "any job board that returns CAPTCHA will
    /// require a `via_search` entry"). Rendered as a
    /// `[platform]` tag.
    pub platform: Option<String>,
    /// Plain-language lesson text. Same string the
    /// `mew-memory` store records.
    pub lesson_text: String,
    /// Confidence score, normalized to `[0.0, 2.0]`.
    /// Lessons with `confidence_score < 0.5` are filtered
    /// out by `collect` (they don't carry useful signal
    /// yet).
    pub confidence_score: f32,
    /// Number of times the lesson has been "helped"
    /// (i.e. the task that injected the lesson succeeded).
    /// Surfaced as "(helped N times)" in the chat list.
    pub times_helped: u32,
    /// Task outcome this lesson was distilled from.
    /// `Success` lessons are surfaced first; `Failure`
    /// lessons are filtered out by default (the user wants
    /// to see what works, not what didn't).
    pub outcome: Outcome,
}

/// Filter lessons for the two task shapes that matter
/// for the user's daily-use case (JobSearch + LeadGen),
/// drop low-confidence and conflicting lessons, and
/// return a stable-sorted `Vec<JobFreelanceLesson>`.
///
/// "Stable-sorted" means: by `task_shape` first
/// (JobSearch < LeadGen so jobs come first), then by
/// `confidence_score` desc (most confident first), then
/// by `times_helped` desc, then by `id` asc (deterministic
/// tiebreaker). The sort is total so the chat list is
/// byte-identical across runs.
pub fn collect(data_dir: &Path) -> Vec<JobFreelanceLesson> {
    let store = mew_memory::store::LessonStore::new(data_dir);
    let lessons = match store.read_all() {
        Ok(ls) => ls,
        Err(_) => return Vec::new(), // missing / corrupted file → empty list, not a panic
    };
    collect_from_lessons(&lessons)
}

/// Pure-Rust filter + sort, the testable core of `collect`.
/// Separated so the unit tests don't need a temp dir.
pub fn collect_from_lessons(lessons: &[Lesson]) -> Vec<JobFreelanceLesson> {
    let mut out: Vec<JobFreelanceLesson> = lessons
        .iter()
        .filter(|l| {
            l.task_shape == TaskShape::JobSearch || l.task_shape == TaskShape::LeadGen
        })
        .filter(|l| l.confidence_score >= 0.5)
        .filter(|l| l.outcome == Outcome::Success)
        .filter(|l| l.conflicting_with.is_empty())
        .map(|l| JobFreelanceLesson {
            id: l.id.clone(),
            task_shape: l.task_shape,
            platform: l.platform.clone(),
            lesson_text: l.lesson_text.clone(),
            confidence_score: l.confidence_score,
            times_helped: l.times_helped,
            outcome: l.outcome,
        })
        .collect();
    // Sort: by task_shape (JobSearch < LeadGen), then
    // confidence desc, then times_helped desc, then id asc.
    // The total ordering makes the chat list stable.
    out.sort_by(|a, b| {
        task_shape_order(&a.task_shape)
            .cmp(&task_shape_order(&b.task_shape))
            .then_with(|| {
                b.confidence_score
                    .partial_cmp(&a.confidence_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| b.times_helped.cmp(&a.times_helped))
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

/// A stable ordering for `TaskShape` used by the sort
/// above. `mew_memory::TaskShape` doesn't derive
/// `PartialOrd`/`Ord`, so we implement the order manually
/// — JobSearch first, then LeadGen, then the rest. This
/// matches the "jobs first" convention the chat list
/// uses so a future Tauri UI can show a single ordered
/// list without re-sorting client-side.
fn task_shape_order(t: &TaskShape) -> u8 {
    match t {
        TaskShape::JobSearch => 0,
        TaskShape::LeadGen => 1,
        TaskShape::GenericResearch => 2,
        TaskShape::GenericBrowse => 3,
        TaskShape::LocalLeadGen => 4,
    }
}

/// Render a `Vec<JobFreelanceLesson>` as a Markdown
/// summary the user can paste into a personal note or
/// read in the chat list. Two sections — `## Job-search
/// lessons` and `## Freelance lead lessons` — each
/// with a bulleted list. Empty when the user has no
/// lessons yet (a fresh install); the chat surface
/// surfaces "No job/freelance lessons learned yet —
/// run a preset to start collecting them."
pub fn render_summary(lessons: &[JobFreelanceLesson]) -> String {
    if lessons.is_empty() {
        return "No job/freelance lessons learned yet.".to_string();
    }
    let mut jobs: Vec<&JobFreelanceLesson> = lessons
        .iter()
        .filter(|l| l.task_shape == TaskShape::JobSearch)
        .collect();
    let mut freelance: Vec<&JobFreelanceLesson> = lessons
        .iter()
        .filter(|l| l.task_shape == TaskShape::LeadGen)
        .collect();
    // Each list is already sorted by confidence desc from
    // `collect`. The render order is the input order, so
    // no re-sort here.
    let mut out = String::new();
    if !jobs.is_empty() {
        out.push_str("## Job-search lessons\n");
        render_section(&mut out, &jobs);
    }
    if !freelance.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("## Freelance lead lessons\n");
        render_section(&mut out, &freelance);
    }
    out
}

fn render_section(out: &mut String, lessons: &[&JobFreelanceLesson]) {
    for l in lessons {
        let platform_str = l
            .platform
            .as_deref()
            .map(|p| format!(" [{}]", p))
            .unwrap_or_default();
        out.push_str(&format!(
            "- (conf {:.2}{}) {}",
            l.confidence_score, platform_str, l.lesson_text
        ));
        if l.times_helped > 0 {
            out.push_str(&format!(" (helped {} time{})", l.times_helped, if l.times_helped == 1 { "" } else { "s" }));
        }
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mew_memory::{Lesson, Outcome, TaskShape};

    fn lesson(
        id: &str,
        task_shape: TaskShape,
        platform: Option<&str>,
        confidence: f32,
        times_helped: u32,
        outcome: Outcome,
    ) -> Lesson {
        let mut l = Lesson::new(
            id,
            1000,
            task_shape,
            platform.map(|s| s.to_string()),
            "test situation",
            outcome,
            format!("lesson text for {id}"),
            "session-1",
        );
        l.confidence_score = confidence;
        l.times_helped = times_helped;
        l
    }

    #[test]
    fn filters_out_non_job_or_freelance_shapes() {
        let lessons = vec![
            lesson("1", TaskShape::JobSearch, Some("LinkedIn"), 1.0, 5, Outcome::Success),
            lesson("2", TaskShape::LeadGen, Some("Upwork"), 1.0, 3, Outcome::Success),
            lesson("3", TaskShape::GenericResearch, Some("Wiki"), 1.0, 10, Outcome::Success),
            lesson("4", TaskShape::LocalLeadGen, Some("Maps"), 1.0, 8, Outcome::Success),
            lesson("5", TaskShape::GenericBrowse, Some("Blog"), 1.0, 2, Outcome::Success),
        ];
        let out = collect_from_lessons(&lessons);
        assert_eq!(out.len(), 2, "only JobSearch + LeadGen pass");
        let ids: Vec<&str> = out.iter().map(|l| l.id.as_str()).collect();
        assert!(ids.contains(&"1"));
        assert!(ids.contains(&"2"));
    }

    #[test]
    fn filters_out_low_confidence() {
        let lessons = vec![
            lesson("1", TaskShape::JobSearch, Some("LinkedIn"), 0.4, 5, Outcome::Success),
            lesson("2", TaskShape::JobSearch, Some("Indeed"), 0.5, 3, Outcome::Success),
            lesson("3", TaskShape::JobSearch, Some("Wellfound"), 0.0, 10, Outcome::Success),
        ];
        let out = collect_from_lessons(&lessons);
        let ids: Vec<&str> = out.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(out.len(), 1);
        assert!(ids.contains(&"2"));
    }

    #[test]
    fn filters_out_failure_outcome() {
        let lessons = vec![
            lesson("1", TaskShape::JobSearch, Some("LinkedIn"), 1.0, 5, Outcome::Success),
            lesson("2", TaskShape::JobSearch, Some("Indeed"), 1.0, 5, Outcome::Failure),
        ];
        let out = collect_from_lessons(&lessons);
        let ids: Vec<&str> = out.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(out.len(), 1);
        assert!(ids.contains(&"1"));
    }

    #[test]
    fn filters_out_conflicting_lessons() {
        // The Phase 25 contradiction semantics in
        // `mew-memory::LessonStore::retrieve_relevant` only
        // fires inside the `retrieve_relevant*` methods —
        // the `read_all` path returns both lessons. The
        // Phase 8 surface filters any lesson that has
        // `conflicting_with` non-empty so a contradiction
        // pair never makes it into the chat list. The
        // partner lesson (no `conflicting_with` set)
        // passes the filter — that's the conservative
        // Phase 8 contract; the Phase 25 path handles the
        // rest of the contradiction work elsewhere.
        let mut a = lesson("a", TaskShape::JobSearch, Some("LinkedIn"), 1.0, 5, Outcome::Success);
        a.conflicting_with.push("b".into());
        let b = lesson("b", TaskShape::JobSearch, Some("LinkedIn"), 1.0, 5, Outcome::Success);
        let out = collect_from_lessons(&[a, b]);
        assert_eq!(out.len(), 1, "lesson 'a' is filtered; lesson 'b' has no conflict flag");
        assert_eq!(out[0].id, "b");
    }

    #[test]
    fn sort_is_deterministic_jobs_first_then_confidence_desc() {
        // JobSearch lessons with varying confidence and
        // times_helped — the sort must put highest confidence
        // first; ties broken by times_helped desc, then id asc.
        let lessons = vec![
            lesson("z", TaskShape::JobSearch, Some("LinkedIn"), 0.8, 0, Outcome::Success),
            lesson("a", TaskShape::JobSearch, Some("Indeed"), 1.0, 0, Outcome::Success),
            lesson("m", TaskShape::JobSearch, Some("Wellfound"), 0.9, 0, Outcome::Success),
        ];
        let out = collect_from_lessons(&lessons);
        assert_eq!(out[0].id, "a", "highest confidence first");
        assert_eq!(out[1].id, "m");
        assert_eq!(out[2].id, "z");
    }

    #[test]
    fn sort_jobs_come_before_freelance() {
        let lessons = vec![
            lesson("f", TaskShape::LeadGen, Some("Upwork"), 1.0, 10, Outcome::Success),
            lesson("j", TaskShape::JobSearch, Some("LinkedIn"), 0.5, 0, Outcome::Success),
        ];
        let out = collect_from_lessons(&lessons);
        assert_eq!(out[0].task_shape, TaskShape::JobSearch);
        assert_eq!(out[1].task_shape, TaskShape::LeadGen);
    }

    #[test]
    fn render_summary_handles_empty_input() {
        assert_eq!(render_summary(&[]), "No job/freelance lessons learned yet.");
    }

    #[test]
    fn render_summary_uses_two_sections() {
        let lessons = vec![
            JobFreelanceLesson {
                id: "j1".into(),
                task_shape: TaskShape::JobSearch,
                platform: Some("LinkedIn".into()),
                lesson_text: "use via_search".into(),
                confidence_score: 1.0,
                times_helped: 3,
                outcome: Outcome::Success,
            },
            JobFreelanceLesson {
                id: "f1".into(),
                task_shape: TaskShape::LeadGen,
                platform: Some("Upwork".into()),
                lesson_text: "draft proposals short".into(),
                confidence_score: 0.9,
                times_helped: 0,
                outcome: Outcome::Success,
            },
        ];
        let out = render_summary(&lessons);
        assert!(out.contains("## Job-search lessons"));
        assert!(out.contains("## Freelance lead lessons"));
        assert!(out.contains("[LinkedIn]"));
        assert!(out.contains("[Upwork]"));
        assert!(out.contains("use via_search"));
        assert!(out.contains("draft proposals short"));
        assert!(out.contains("(helped 3 times)"));
    }

    #[test]
    fn render_summary_handles_jobs_only() {
        let lessons = vec![JobFreelanceLesson {
            id: "j1".into(),
            task_shape: TaskShape::JobSearch,
            platform: Some("LinkedIn".into()),
            lesson_text: "use via_search".into(),
            confidence_score: 1.0,
            times_helped: 0,
            outcome: Outcome::Success,
        }];
        let out = render_summary(&lessons);
        assert!(out.contains("## Job-search lessons"));
        assert!(!out.contains("## Freelance lead lessons"));
    }

    #[test]
    fn render_summary_handles_freelance_only() {
        let lessons = vec![JobFreelanceLesson {
            id: "f1".into(),
            task_shape: TaskShape::LeadGen,
            platform: Some("Upwork".into()),
            lesson_text: "draft proposals short".into(),
            confidence_score: 1.0,
            times_helped: 0,
            outcome: Outcome::Success,
        }];
        let out = render_summary(&lessons);
        assert!(!out.contains("## Job-search lessons"));
        assert!(out.contains("## Freelance lead lessons"));
    }
}
