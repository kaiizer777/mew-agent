//! Phase 8 — Per-run results digest export.
//!
//! Background: the in-chat `chat-reply` summary is great for
//! "I just ran a preset, what did I get?" but it scrolls off
//! the chat list as the user runs more presets. The Phase 8
//! spec calls for an *on-disk* Markdown digest of every run,
//! appended to `<data_dir>/digests/YYYY-MM-DD.md`, so the user
//! can:
//!   * review "what did the agent find last Monday?" without
//!     scrolling the chat list,
//!   * keep a personal record independent of the chat surface
//!     (the chat list is a UI; the digest is a file the user
//!     can grep, copy, archive, etc.),
//!   * debug "why does this platform keep coming back
//!     exhausted?" by reading the platform-exhausted list
//!     across many runs.
//!
//! Format: a single file per calendar day, with one run as a
//! section. The file is overwritten when a fresh run lands on
//! the same day (idempotent — re-running `daily_remote_jobs`
//! on the same day is the expected use case). The first line
//! of every run section is `## <HH:MM:SS> <task_description>`,
//! followed by the same content the
//! `session_summary::render_session_summary` produces. The
//! file is plain Markdown; no frontmatter, no
//! machine-parseable header.
//!
//! ## Public surface
//!
//! ```ignore
//! pub fn render_digest(when: SystemTime, summary: &SessionSummary) -> String
//! pub fn write_digest(out_dir: &Path, when: SystemTime, body: &str) -> Result<PathBuf, ...>
//! ```
//!
//! See `mew-agent/tests/phase8_presets.rs` for the typed
//! fixture tests.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::session_summary::SessionSummaryInputs;

/// Render a single run's digest section. The section starts
/// with `## <HH:MM:SS> <task_description>` and is followed
/// by the same Markdown body the
/// `session_summary::render_session_summary` already
/// produces. We re-use the session-summary renderer so the
/// digest is byte-identical to the in-chat summary the user
/// just read.
pub fn render_run_section(
    when: SystemTime,
    inputs: &SessionSummaryInputs<'_>,
    task_description: &str,
) -> String {
    use std::time::UNIX_EPOCH;
    let secs = when
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let time_str = format_unix_secs_as_hms(secs);
    let body = crate::session_summary::render_session_summary(inputs);
    let mut out = String::new();
    out.push_str(&format!("## {} {}\n\n", time_str, task_description));
    out.push_str(&body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Render a complete daily-digest file (header + all
/// run-sections). The header is a one-line
/// "Daily digest — YYYY-MM-DD" banner so a user opening the
/// file in a text editor immediately knows what it is.
pub fn render_daily_digest(date_str: &str, sections: &[(SystemTime, String)]) -> String {
    let mut out = format!("# Daily digest — {}\n\n", date_str);
    for (when, section) in sections {
        let _ = when; // date is in the header
        out.push_str(section);
        if !out.ends_with("\n\n") {
            out.push('\n');
        }
    }
    out
}

/// Write a digest section to the per-day file at
/// `<out_dir>/digests/YYYY-MM-DD.md`. The file is overwritten
/// when it already exists — the digest is "what the agent
/// found today" and re-running a preset on the same day is
/// the expected pattern. Returns the path that was written.
pub fn write_run_digest(
    out_dir: &Path,
    when: SystemTime,
    section: &str,
) -> std::io::Result<PathBuf> {
    use std::time::UNIX_EPOCH;
    let secs = when
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let date_str = format_unix_secs_as_date(secs);
    let dir = out_dir.join("digests");
    fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{date_str}.md"));
    let mut f = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&file)?;
    f.write_all(section.as_bytes())?;
    f.sync_all()?;
    Ok(file)
}

/// Format a `SystemTime` as a `YYYY-MM-DD` date string
/// (UTC). Stable, machine-parseable, file-name-safe.
pub fn format_unix_secs_as_date(secs: u64) -> String {
    // Days-since-epoch arithmetic; no chrono dep.
    let days = (secs / 86_400) as i64;
    let (y, m, d) = days_to_ymd(days);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// Format a `SystemTime` as a `HH:MM:SS` clock string
/// (UTC). Stable, log-line-friendly.
pub fn format_unix_secs_as_hms(secs: u64) -> String {
    let s = (secs % 86_400) as u32;
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let s = s % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

/// Inverse of `days_to_ymd` — used by tests to pin the
/// known-good dates. Days-since-epoch arithmetic; no
/// chrono dep.
pub fn ymd_to_days(y: i64, m: i64, d: i64) -> i64 {
    // Howard Hinnant's days_from_civil algorithm (public
    // domain, see http://howardhinnant.github.io/date_algorithms.html).
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y / 400 } else { (y - 399) / 400 };
    let yoe = (y - era * 400) as u64;
    let m = m as u64;
    let d = d as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

fn days_to_ymd(days: i64) -> (i64, i64, i64) {
    // Howard Hinnant's days_from_civil inverse, also
    // public domain. We use a non-recursive branchless
    // variant.
    let z = days + 719468;
    let era = if z >= 0 { z / 146097 } else { (z - 146096) / 146097 };
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as i64, d as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn date_format_stable_for_known_timestamps() {
        // 2026-08-16 00:00:00 UTC = 1_786_838_400.
        assert_eq!(format_unix_secs_as_date(1_786_838_400), "2026-08-16");
        // 2024-01-01 00:00:00 UTC = 1_704_067_200.
        assert_eq!(format_unix_secs_as_date(1_704_067_200), "2024-01-01");
        // 1970-01-01 00:00:00 UTC = 0.
        assert_eq!(format_unix_secs_as_date(0), "1970-01-01");
    }

    #[test]
    fn hms_format_stable_for_known_timestamps() {
        assert_eq!(format_unix_secs_as_hms(0), "00:00:00");
        assert_eq!(format_unix_secs_as_hms(3661), "01:01:01");
        assert_eq!(format_unix_secs_as_hms(86399), "23:59:59");
    }

    #[test]
    fn ymd_to_days_round_trip() {
        for (y, m, d) in [
            (1970, 1, 1),
            (2024, 1, 1),
            (2026, 8, 16),
            (2000, 2, 29),
        ] {
            let days = ymd_to_days(y, m, d);
            let (yy, mm, dd) = days_to_ymd(days);
            assert_eq!((yy, mm, dd), (y, m, d));
        }
    }

    #[test]
    fn write_run_digest_creates_file_with_section_content() {
        let tmp = std::env::temp_dir().join(format!(
            "mew-digest-create-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let when = UNIX_EPOCH + Duration::from_secs(1_786_838_400);
        let section = "## 00:00:00 test\n\nbody\n";
        let path =
            write_run_digest(&tmp, when, section).expect("write_run_digest should succeed");
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, section);
        assert!(path.ends_with("digests/2026-08-16.md"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn write_run_digest_overwrites_existing_file_for_same_day() {
        // Re-running a preset on the same day is the
        // expected pattern — the digest must overwrite
        // (not append), so a stale "Monday run" record
        // doesn't survive a fresh "Monday morning" run.
        let tmp = std::env::temp_dir().join(format!(
            "mew-digest-overwrite-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let when = UNIX_EPOCH + Duration::from_secs(1_786_838_400);
        let _ = write_run_digest(&tmp, when, "first\n").unwrap();
        let _ = write_run_digest(&tmp, when, "second\n").unwrap();
        // Both writes land on the same day file; the
        // second wins.
        let content =
            std::fs::read_to_string(tmp.join("digests").join("2026-08-16.md")).unwrap();
        assert_eq!(content, "second\n");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn render_run_section_starts_with_hms_and_task() {
        use crate::handoff::BrowserResult;
        let br = BrowserResult::done("test-session", "ok".to_string(), Vec::new(), None, None);
        let inputs = SessionSummaryInputs {
            result: &br,
            auth_state: None,
            applications_sent: 0,
            platforms_not_reached: Vec::new(),
        };
        let when = UNIX_EPOCH + Duration::from_secs(1_786_838_400);
        let out = render_run_section(when, &inputs, "preset=daily_remote_jobs");
        assert!(out.starts_with("## 00:00:00 preset=daily_remote_jobs"));
    }

    #[test]
    fn render_daily_digest_includes_header_and_sections() {
        let when1 = UNIX_EPOCH + Duration::from_secs(1_786_838_400);
        let when2 = UNIX_EPOCH + Duration::from_secs(1_786_842_400);
        let s1 = "## 00:00:00 run1\nbody1\n".to_string();
        let s2 = "## 01:00:00 run2\nbody2\n".to_string();
        let out = render_daily_digest("2026-08-16", &[(when1, s1.clone()), (when2, s2.clone())]);
        assert!(out.contains("# Daily digest — 2026-08-16"));
        assert!(out.contains(&s1));
        assert!(out.contains(&s2));
    }
}
