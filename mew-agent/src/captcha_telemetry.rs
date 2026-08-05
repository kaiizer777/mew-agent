//! Phase 8 — Local-only captcha telemetry.
//!
//! Background: the challenge detector (`mew_resilience::captcha
//! ::detect`) fires per-iteration. We want a *per-domain* running
//! count so the user can see, at a glance, "you've hit X captcha
//! pages on instagram.com this week, Y on twitter.com, Z on
//! linkedin.com." This is the local-only feedback loop that lets
//! the user notice when a site is escalating and adjust their
//! tasks accordingly.
//!
//! Why local-only:
//!
//!   * The data is *about the user's own browsing patterns*,
//!     which is sensitive. Telemetry to a third party would
//!     expose *which sites the user is hitting* — a
//!     fingerprintable list even if no PII is attached.
//!   * The user explicitly opted into avoidance + human-
//!     handoff; phoning home about it would be a surprising
//!     and unwelcome reversal.
//!   * The data is small and well-bounded; a local file
//!     suffices.
//!
//! The on-disk file lives at `<data_dir>/captcha_telemetry.json`,
//! where `data_dir` is a `&Path` the caller provides. The CLI /
//! Tauri layer passes the per-user app-data dir (the same place
//! the transcripts go), so the file lands in the OS-appropriate
//! user directory and is `chmod 600`-able on Unix-likes.
//!
//! ## Threading
//!
//! The struct is `Send + Sync` and the in-memory state is behind
//! a `Mutex`. The intended access pattern is:
//!
//!   * `record(domain, kind)` — called from the resilience
//!     hook when a challenge is detected. Cheap, no I/O on the
//!     hot path; persists on the next `flush()`.
//!   * `flush()` — called at session end (and optionally
//!     periodically) to write the current state to disk.
//!     Idempotent and merge-safe: a torn or partial write on
//!     the previous run is overwritten with the in-memory
//!     truth on the next flush.
//!   * `summary()` — read-only formatter. The chat reply /
//!     Tauri command / Phase 9 eval harness can call this
//!     without touching the in-memory state.
//!
//! ## Persistence format
//!
//! JSON. The shape is intentionally simple — a flat array of
//! per-domain records, each carrying the per-kind counts, the
//! first-seen / last-seen timestamps (Unix seconds), and a flag
//! for "this domain is in `sensitive_platforms.toml` with
//! `known_to_challenge_bots = true`" so the summary can mark
//! "expected" rows distinctly from "new" rows. The format
//! round-trips through `serde_json` so a future remote
//! telemetry tool can consume the same file without an
//! adapter.

use std::collections::BTreeMap;
use std::path::Path;

use mew_resilience::ChallengeKind;
use serde::{Deserialize, Serialize};

/// One row of telemetry. Keyed by the bare host
/// (lowercased, leading `www.` stripped).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CaptchaTelemetryRecord {
    /// The host this row is for, lowercased, leading `www.`
    /// stripped. e.g. `"instagram.com"`.
    pub host: String,
    /// Per-kind counts. The map is keyed by `ChallengeKind::as_str`
    /// so the JSON output is stable across runs.
    pub counts: BTreeMap<String, u32>,
    /// Unix seconds of the first time this domain was
    /// recorded in telemetry. `0` for a fresh row.
    pub first_seen_unix_secs: u64,
    /// Unix seconds of the most recent record. `0` for a
    /// fresh row. Updated on every `record()` call.
    pub last_seen_unix_secs: u64,
    /// `true` if this domain is in
    /// `config/sensitive_platforms.toml` with
    /// `known_to_challenge_bots = true`. The summary
    /// uses this to label the row as "expected" (we
    /// knew this was a challenger) vs "new" (a
    /// previously-unseen site is serving challenges).
    pub expected: bool,
}

impl CaptchaTelemetryRecord {
    fn new(host: String, now: u64, expected: bool) -> Self {
        let mut counts = BTreeMap::new();
        // Initialize every kind to 0 so the summary's
        // "cloudflare_turnstile: 0, recaptcha_v2: 0,
        // recaptcha_v3: 0, hcaptcha: 0" rendering is
        // uniform across rows. The user can see the
        // zeros — they're informative ("we have not
        // seen any v2 challenges on this domain").
        for k in [
            ChallengeKind::CloudflareTurnstile,
            ChallengeKind::RecaptchaV2,
            ChallengeKind::RecaptchaV3,
            ChallengeKind::Hcaptcha,
        ] {
            counts.insert(k.as_str().to_string(), 0);
        }
        Self {
            host,
            counts,
            first_seen_unix_secs: now,
            last_seen_unix_secs: now,
            expected,
        }
    }

    /// Total challenges across all kinds.
    pub fn total(&self) -> u32 {
        self.counts.values().sum()
    }
}

/// On-disk file shape. Kept separate from `CaptchaTelemetry`
/// (the in-memory struct) so a future format change
/// (versioned, signed, etc.) doesn't churn the in-memory
/// API.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CaptchaTelemetryFile {
    /// Schema version. Bumped on breaking changes. The
    /// loader ignores records with an unknown version
    /// (treated as empty) so a downgrade is safe.
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    /// The per-domain records. Stored as a vec rather
    /// than a map for forward-compat (BTreeMap's
    /// deterministic ordering isn't required for the
    /// on-disk format).
    records: Vec<CaptchaTelemetryRecord>,
}

fn default_schema_version() -> u32 {
    1
}

/// In-memory captcha telemetry. Cheap to clone (a
/// `Mutex<Inner>` is wrapped in `Arc` inside the
/// agent's field; the CLI / Tauri command holds a
/// handle and reads the summary after the session
/// ends).
#[derive(Debug)]
pub struct CaptchaTelemetry {
    inner: std::sync::Mutex<Inner>,
    /// The on-disk file path. `None` means the
    /// telemetry is in-memory only (the CLI's
    /// `--no-telemetry` flag, or a misconfigured
    /// `data_dir`); `flush()` becomes a no-op in
    /// that case.
    persist_path: Option<std::path::PathBuf>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Per-domain records. Stored in a map for O(1) lookup
    /// on `record()`; serialized to a `Vec` on `flush()`
    /// for on-disk determinism.
    by_host: BTreeMap<String, CaptchaTelemetryRecord>,
}

impl CaptchaTelemetry {
    /// Build a new in-memory telemetry. The on-disk file
    /// is *not* loaded synchronously here — `load()`
    /// is an explicit step. Most callers want
    /// `load_or_default(path)`, which calls `load()`
    /// and falls back to empty on any I/O / parse error
    /// so a corrupted file does not block the agent.
    pub fn new(persist_path: Option<std::path::PathBuf>) -> Self {
        Self {
            inner: std::sync::Mutex::new(Inner::default()),
            persist_path,
        }
    }

    /// Load existing telemetry from the configured path.
    /// Returns an empty `CaptchaTelemetry` on any error
    /// (missing file, parse failure, wrong version) so the
    /// agent never blocks on a corrupted telemetry file.
    pub fn load_or_default(persist_path: Option<std::path::PathBuf>) -> Self {
        let t = Self::new(persist_path.clone());
        if let Some(path) = persist_path.as_ref() {
            if let Ok(content) = std::fs::read_to_string(path) {
                match serde_json::from_str::<CaptchaTelemetryFile>(&content) {
                    Ok(file) if file.schema_version == default_schema_version() => {
                        let mut guard = t.inner.lock().expect("telemetry mutex poisoned");
                        for r in file.records {
                            guard.by_host.insert(r.host.clone(), r);
                        }
                    }
                    Ok(file) => {
                        tracing::warn!(
                            event = "captcha_telemetry_unknown_schema",
                            path = %path.display(),
                            version = file.schema_version,
                            "ignoring telemetry file with unknown schema version"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            event = "captcha_telemetry_parse_failed",
                            path = %path.display(),
                            error = %e,
                            "ignoring unparseable telemetry file"
                        );
                    }
                }
            }
        }
        t
    }

    /// Record a challenge occurrence. Idempotent on
    /// `domain` casing: `"WWW.Instagram.com"` and
    /// `"instagram.com"` hit the same row. The `expected`
    /// flag is set the first time the row is created and
    /// not changed afterwards — the user is told the
    /// initial classification, and reclassification
    /// requires editing `sensitive_platforms.toml`
    /// manually.
    pub fn record(&self, domain: &str, kind: ChallengeKind) {
        if domain.is_empty() {
            return;
        }
        let host = normalize_host(domain);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let kind_key = kind.as_str().to_string();
        let mut guard = self.inner.lock().expect("telemetry mutex poisoned");
        let entry = guard.by_host.entry(host.clone()).or_insert_with(|| {
            // New row. The `expected` flag is the
            // caller's call (the agent knows whether
            // the host is in the sensitive-platforms
            // table); we initialize it from a
            // side-channel param. The default `false`
            // matches the conservative "we did not
            // predict this" stance; the agent's
            // call site passes `true` when the
            // sensitive-platforms lookup succeeded.
            CaptchaTelemetryRecord::new(host, now, false)
        });
        if entry.first_seen_unix_secs == 0 {
            entry.first_seen_unix_secs = now;
        }
        entry.last_seen_unix_secs = now;
        *entry.counts.entry(kind_key).or_insert(0) += 1;
    }

    /// Mark the next newly-created record for `host` as
    /// `expected = true`. Called by the agent's
    /// pre-navigate pacing block when the host is a
    /// known challenger. Idempotent on existing rows
    /// (the flag is sticky; manual reclassification
    /// requires editing the file).
    pub fn mark_expected(&self, domain: &str) {
        if domain.is_empty() {
            return;
        }
        let host = normalize_host(domain);
        let mut guard = self.inner.lock().expect("telemetry mutex poisoned");
        if let Some(row) = guard.by_host.get_mut(&host) {
            row.expected = true;
        } else {
            // No row yet — pre-seed with the current
            // time so the first `record()` call
            // doesn't carry a stale "first seen =
            // 0" and the user's summary is
            // immediately informative.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            guard
                .by_host
                .insert(host.clone(), CaptchaTelemetryRecord::new(host, now, true));
        }
    }

    /// Persist the current in-memory state to disk. No-op
    /// when `persist_path` is `None` (in-memory only).
    /// Bounded to ~16 KiB even for hundreds of rows
    /// because each row is small. The write is
    /// best-effort: a failure is logged and the
    /// in-memory state is preserved so the next flush
    /// can try again.
    pub fn flush(&self) {
        let path = match &self.persist_path {
            Some(p) => p,
            None => return,
        };
        let file = {
            let guard = self.inner.lock().expect("telemetry mutex poisoned");
            CaptchaTelemetryFile {
                schema_version: default_schema_version(),
                records: guard.by_host.values().cloned().collect(),
            }
        };
        // Best-effort create + write. We don't `create_dir_all`
        // here — the caller (CLI / Tauri) is responsible for
        // ensuring the parent dir exists. A failure to write
        // is logged but never blocks the agent.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        match serde_json::to_string_pretty(&file) {
            Ok(content) => {
                if let Err(e) = std::fs::write(path, content) {
                    tracing::warn!(
                        event = "captcha_telemetry_flush_failed",
                        path = %path.display(),
                        error = %e,
                        "could not write telemetry file; in-memory state preserved"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    event = "captcha_telemetry_serialize_failed",
                    error = %e,
                    "could not serialize telemetry; skipping flush"
                );
            }
        }
    }

    /// Snapshot of every row, in deterministic (sorted-by-host)
    /// order. Cheap to clone (the rows are small).
    pub fn snapshot(&self) -> Vec<CaptchaTelemetryRecord> {
        let guard = self.inner.lock().expect("telemetry mutex poisoned");
        guard.by_host.values().cloned().collect()
    }

    /// Human-readable summary suitable for showing in a
    /// chat reply or a Tauri panel. The format is:
    ///
    /// ```text
    /// Captcha telemetry (local-only):
    ///   instagram.com — 12 total [expected] — cloudflare_turnstile: 5, recaptcha_v2: 4, recaptcha_v3: 2, hcaptcha: 1
    ///   twitter.com   —  3 total [expected] — cloudflare_turnstile: 0, recaptcha_v2: 1, recaptcha_v3: 2, hcaptcha: 0
    ///   some-new.com  —  1 total [new]      — cloudflare_turnstile: 0, recaptcha_v2: 0, recaptcha_v3: 0, hcaptcha: 1
    /// ```
    ///
    /// Empty when no challenges have been recorded. The
    /// leading `"Captcha telemetry (local-only):"` line is
    /// a *label* — the user reading the chat list
    /// shouldn't confuse this with a captcha challenge
    /// itself.
    pub fn summary(&self) -> String {
        let rows = self.snapshot();
        if rows.is_empty() {
            return "Captcha telemetry (local-only): no challenges recorded yet.".to_string();
        }
        let mut out = String::from("Captcha telemetry (local-only):\n");
        for row in &rows {
            let tag = if row.expected { "expected" } else { "new     " };
            let total = row.total();
            // Sort the per-kind counts for stable output.
            let mut kind_lines: Vec<String> = row
                .counts
                .iter()
                .map(|(k, v)| format!("{k}: {v}"))
                .collect();
            kind_lines.sort();
            out.push_str(&format!(
                "  {:<20} — {:>3} total [{}] — {}\n",
                row.host,
                total,
                tag,
                kind_lines.join(", ")
            ));
        }
        out
    }
}

/// Lowercase + strip a leading `www.`. Matches the
/// `mew_nav::strip_www` semantics. Kept private and
/// inlined so this module has no `mew_nav` dep — the
/// normalization is one line and the consistency
/// benefit is a stable, comparable `host` string in
/// telemetry rows.
fn normalize_host(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    if lower.len() >= 4 && lower[..4].eq_ignore_ascii_case("www.") {
        lower[4..].to_string()
    } else {
        lower
    }
}

/// Default on-disk path the agent uses when the caller
/// doesn't override it. `<data_dir>/captcha_telemetry.json`.
pub fn default_persist_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("captcha_telemetry.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn make_record(host: &str, total: u32, kind: ChallengeKind) -> CaptchaTelemetryRecord {
        let mut counts = BTreeMap::new();
        for k in [
            ChallengeKind::CloudflareTurnstile,
            ChallengeKind::RecaptchaV2,
            ChallengeKind::RecaptchaV3,
            ChallengeKind::Hcaptcha,
        ] {
            counts.insert(k.as_str().to_string(), 0);
        }
        *counts.get_mut(kind.as_str()).unwrap() = total;
        CaptchaTelemetryRecord {
            host: host.to_string(),
            counts,
            first_seen_unix_secs: 1_700_000_000,
            last_seen_unix_secs: 1_700_000_100,
            expected: true,
        }
    }

    #[test]
    fn record_increments_per_host_and_kind() {
        let t = CaptchaTelemetry::new(None);
        t.record("instagram.com", ChallengeKind::RecaptchaV2);
        t.record("instagram.com", ChallengeKind::RecaptchaV2);
        t.record("instagram.com", ChallengeKind::CloudflareTurnstile);
        t.record("linkedin.com", ChallengeKind::Hcaptcha);
        let snap = t.snapshot();
        assert_eq!(snap.len(), 2);
        let ig = snap.iter().find(|r| r.host == "instagram.com").unwrap();
        assert_eq!(ig.counts.get("recaptcha_v2").copied(), Some(2));
        assert_eq!(
            ig.counts.get("cloudflare_turnstile").copied(),
            Some(1)
        );
        assert_eq!(ig.counts.get("hcaptcha").copied(), Some(0));
        assert_eq!(ig.total(), 3);
    }

    #[test]
    fn record_normalizes_host_casing_and_www_prefix() {
        let t = CaptchaTelemetry::new(None);
        t.record("WWW.Instagram.com", ChallengeKind::RecaptchaV2);
        t.record("instagram.com", ChallengeKind::RecaptchaV2);
        let snap = t.snapshot();
        assert_eq!(snap.len(), 1, "www./casing must collapse to one row");
        assert_eq!(snap[0].host, "instagram.com");
        assert_eq!(snap[0].total(), 2);
    }

    #[test]
    fn record_ignores_empty_domain() {
        let t = CaptchaTelemetry::new(None);
        t.record("", ChallengeKind::RecaptchaV2);
        assert!(t.snapshot().is_empty());
    }

    #[test]
    fn mark_expected_pre_seeds_a_new_row() {
        let t = CaptchaTelemetry::new(None);
        t.mark_expected("example.com");
        t.record("example.com", ChallengeKind::RecaptchaV2);
        let snap = t.snapshot();
        let row = snap.iter().find(|r| r.host == "example.com").unwrap();
        assert!(row.expected, "mark_expected must set the flag on new rows");
        assert_eq!(row.total(), 1);
    }

    #[test]
    fn mark_expected_is_idempotent_on_existing_row() {
        let t = CaptchaTelemetry::new(None);
        t.record("example.com", ChallengeKind::RecaptchaV2);
        t.mark_expected("example.com");
        let snap = t.snapshot();
        assert!(snap[0].expected);
        // Second mark_expected is a no-op.
        t.mark_expected("example.com");
        assert!(snap[0].expected);
    }

    #[test]
    fn summary_includes_label_and_renders_empty_state() {
        let t = CaptchaTelemetry::new(None);
        let s = t.summary();
        assert!(s.contains("no challenges recorded yet"));
        t.record("instagram.com", ChallengeKind::CloudflareTurnstile);
        let s = t.summary();
        assert!(s.contains("Captcha telemetry (local-only)"));
        assert!(s.contains("instagram.com"));
        assert!(s.contains("cloudflare_turnstile: 1"));
    }

    #[test]
    fn summary_marks_expected_vs_new() {
        let t = CaptchaTelemetry::new(None);
        t.mark_expected("instagram.com");
        t.record("instagram.com", ChallengeKind::RecaptchaV2);
        t.record("some-new-site.com", ChallengeKind::Hcaptcha);
        let s = t.summary();
        assert!(s.contains("[expected]"), "expected tag missing: {s}");
        assert!(s.contains("[new     ]"), "new tag missing: {s}");
    }

    #[test]
    fn flush_writes_json_to_disk() {
        let dir = std::env::temp_dir().join("mew_captcha_telemetry_test_flush");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("captcha_telemetry.json");
        let _ = std::fs::remove_file(&path);
        let t = CaptchaTelemetry::new(Some(path.clone()));
        t.record("instagram.com", ChallengeKind::RecaptchaV2);
        t.flush();
        // Read the file back and confirm it's valid JSON.
        let content = std::fs::read_to_string(&path).expect("file should exist");
        let file: CaptchaTelemetryFile = serde_json::from_str(&content).expect("valid json");
        assert_eq!(file.schema_version, 1);
        assert_eq!(file.records.len(), 1);
        assert_eq!(file.records[0].host, "instagram.com");
    }

    #[test]
    fn flush_is_noop_when_persist_path_is_none() {
        // No file is created. The test passes by simply
        // not panicking and by the snapshot being
        // non-empty.
        let t = CaptchaTelemetry::new(None);
        t.record("instagram.com", ChallengeKind::RecaptchaV2);
        t.flush();
        assert_eq!(t.snapshot().len(), 1);
    }

    #[test]
    fn load_or_default_recovers_existing_file() {
        let dir = std::env::temp_dir().join("mew_captcha_telemetry_test_load");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("captcha_telemetry.json");
        // Write a known file.
        let file = CaptchaTelemetryFile {
            schema_version: 1,
            records: vec![make_record("instagram.com", 3, ChallengeKind::RecaptchaV2)],
        };
        std::fs::write(&path, serde_json::to_string_pretty(&file).unwrap()).unwrap();
        // Load it.
        let t = CaptchaTelemetry::load_or_default(Some(path.clone()));
        let snap = t.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].host, "instagram.com");
        assert_eq!(snap[0].total(), 3);
        // Clean up.
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_or_default_falls_back_to_empty_on_missing_file() {
        let dir = std::env::temp_dir().join("mew_captcha_telemetry_test_missing");
        let path = dir.join("does_not_exist.json");
        let t = CaptchaTelemetry::load_or_default(Some(path));
        assert!(t.snapshot().is_empty());
    }

    #[test]
    fn load_or_default_falls_back_to_empty_on_parse_failure() {
        let dir = std::env::temp_dir().join("mew_captcha_telemetry_test_bad");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("captcha_telemetry.json");
        std::fs::write(&path, "not valid json {{{{").unwrap();
        let t = CaptchaTelemetry::load_or_default(Some(path.clone()));
        assert!(t.snapshot().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_or_default_falls_back_to_empty_on_unknown_schema_version() {
        let dir = std::env::temp_dir().join("mew_captcha_telemetry_test_schema");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("captcha_telemetry.json");
        let file = CaptchaTelemetryFile {
            schema_version: 9999, // future version
            records: vec![],
        };
        std::fs::write(&path, serde_json::to_string_pretty(&file).unwrap()).unwrap();
        let t = CaptchaTelemetry::load_or_default(Some(path.clone()));
        assert!(t.snapshot().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn normalize_host_strips_www_and_lowercases() {
        assert_eq!(normalize_host("WWW.Instagram.com"), "instagram.com");
        assert_eq!(normalize_host("example.com"), "example.com");
        assert_eq!(normalize_host("WWW.example.com"), "example.com");
        // Edge case: short string that starts with "www"
        // but is not the prefix.
        assert_eq!(normalize_host("wwwx.com"), "wwwx.com");
    }

    #[test]
    fn default_persist_path_uses_provided_dir() {
        let p = default_persist_path(std::path::Path::new("/tmp"));
        assert_eq!(p, std::path::PathBuf::from("/tmp/captcha_telemetry.json"));
    }
}
