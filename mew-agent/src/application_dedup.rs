//! Phase 6 — Application / proposal / connection-request dedup store.
//!
//! Background: the auto-apply action layer (see `super::auto_apply`)
//! is gated on the existing `Paused` checkpoint for every
//! submission — the user must confirm each application unless
//! they explicitly opt into fire-and-forget mode. That gate is
//! enough to prevent *accidental* duplicate submissions within
//! a single run, but it is not enough to prevent *deliberate*
//! duplicate submissions across runs.
//!
//! Consider: a user runs the `daily_remote_jobs` preset on
//! Monday, applies to 8 LinkedIn Easy Apply postings, comes
//! back on Tuesday, runs the same preset, and the agent
//! re-surfaces the same 8 postings. Without dedup, the
//! Tuesday run would pause 8 more times for 8 already-sent
//! applications — a frustrating, ban-risk-elevating loop.
//!
//! The fix: a local on-disk record of every submission the
//! agent has made, keyed by `(platform, posting_identifier)`.
//! The dedup store is consulted at the `Paused` checkpoint
//! and the agent silently drops the application when an
//! identical `(platform, identifier)` pair is already
//! present. The store is **append-only** — entries are
//! added when an application is confirmed-sent, never
//! removed (the user can edit the file directly if they
//! want to "reset" the store).
//!
//! ## On-disk shape
//!
//! ```text
//! <data_dir>/applications_sent.json
//! ```
//!
//! where `data_dir` is the same per-user app-data dir the
//! agent's transcripts land in. The file is a JSON array of
//! `ApplicationRecord` rows, each carrying the
//! `(platform, identifier)` key, the action kind, the
//! `finding_id` (the chat-side link back to the finding
//! that triggered the submission), the Unix-second
//! timestamp of the submission, and a free-form `note` for
//! human review. The schema is versioned so a future
//! breaking change can ignore older files cleanly.
//!
//! ## Threading
//!
//! The store is `Send + Sync` (the in-memory state is
//! `Mutex<HashMap<...>>`-wrapped). The intended access
//! pattern is:
//!
//!   * `load_or_default(path)` — read the on-disk file at
//!     agent construction. Idempotent; falls back to empty
//!     on any I/O / parse error so a corrupted file does
//!     not block the agent.
//!   * `is_already_applied(platform, identifier)` —
//!     consulted at the `Paused` checkpoint before
//!     surfacing the draft. Returns `true` if a record
//!     with the same key exists.
//!   * `record_submission(platform, identifier, kind,
//!     finding_id, note)` — called when the user confirms
//!     a submission (or the fire-and-forget path fires).
//!     Cheap, no I/O on the hot path.
//!   * `flush()` — explicit persistence step, called at
//!     session end so the in-memory state lands on disk
//!     before the agent drops. Mirrors the captcha-
//!     telemetry file pattern.
//!
//! The dedup store does **not** enforce the per-run cap
//! (`max_applications_per_run`) — that lives in
//! `super::auto_apply`, which counts submissions per
//! session and is the right place to enforce a run-scoped
//! limit.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One row in the dedup store. Keyed by the
/// `(platform, posting_identifier)` pair.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApplicationRecord {
    /// The bare host (lowercased, leading `www.`
    /// stripped). e.g. `"linkedin.com"`. Operators
    /// reading the file will recognize the domain at a
    /// glance — the full URL is in `posting_identifier`.
    pub platform: String,
    /// The platform-specific posting identifier. For
    /// LinkedIn Easy Apply this is the numeric
    /// `jobId` from the URL (`/jobs/view/12345`); for
    /// Indeed it's the `jk` query parameter; for
    /// Upwork it's the project URL slug. The agent
    /// normalizes these to a stable string at the
    /// call site; the dedup store does not validate
    /// the shape.
    pub posting_identifier: String,
    /// The action kind the agent used. Mirrors
    /// `mew_resilience::ActionKind::as_str()` so the
    /// user can grep for "submit_application" vs
    /// "send_proposal" in the file.
    pub action_kind: String,
    /// The `finding_id` (from `ResearchFinding::id`)
    /// that triggered the submission. Empty for a
    /// connection request that was not directly
    /// tied to a finding. Operators can use this
    /// to walk back from a submission to the
    /// original chat-side discovery.
    pub finding_id: String,
    /// Unix seconds of the submission. `0` if the
    /// runtime could not read the wall clock (a
    /// rare edge case on some sandboxed CI envs).
    pub submitted_at_unix_secs: u64,
    /// Free-form note the agent attaches. The
    /// current implementation writes the target
    /// string from the irreversible verdict (e.g.
    /// "Senior Rust Engineer @ Acme (Remote)"),
    /// so a human reviewer of the file can see
    /// what the application was *for* without
    /// cross-referencing the transcript.
    pub note: String,
}

/// The action kinds the dedup store tracks. Mirrors
/// `mew_resilience::ActionKind` for the Phase 6 typed
/// surface (`SubmitApplication`, `SendProposal`,
/// `SendConnectionRequest`). The string values are
/// stable across runs and are part of the on-disk file
/// format, so a regression that changes them would
/// silently invalidate the dedup of existing rows.
pub const TRACKED_ACTION_KINDS: &[&str] = &[
    "submit_application",
    "send_proposal",
    "send_connection_request",
];

/// Composite key for the in-memory map. `(String, String)`
/// is `Hash + Eq` so the map is a plain `HashMap`.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct DedupKey {
    platform: String,
    posting_identifier: String,
}

/// On-disk file shape. Kept separate from the
/// in-memory struct so a future schema bump
/// (versioned, signed, etc.) doesn't churn the
/// in-memory API. Mirrors the captcha-telemetry file
/// pattern.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ApplicationDedupFile {
    /// Schema version. Bumped on breaking changes.
    /// The loader treats records with an unknown
    /// version as empty so a downgrade is safe.
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    /// Every record we've ever recorded. Stored as a
    /// vec rather than a map for forward-compat
    /// (BTreeMap's deterministic ordering isn't
    /// required for the on-disk format).
    records: Vec<ApplicationRecord>,
}

fn default_schema_version() -> u32 {
    1
}

/// In-memory dedup store. Cheap to share (the inner
/// `Mutex<HashMap<...>>` is wrapped in this struct; the
/// agent holds a reference and the CLI / Tauri command
/// can hold a sibling to call `summary()` without
/// going through the agent).
#[derive(Debug)]
pub struct ApplicationDedup {
    /// `(platform, posting_identifier)` -> `ApplicationRecord`.
    /// We store the *full* record (not just a bool)
    /// so a future Tauri command can show a
    /// "here's every application you've ever sent"
    /// history panel without re-parsing the on-disk
    /// file.
    by_key: std::sync::Mutex<HashMap<DedupKey, ApplicationRecord>>,
    /// The on-disk file path. `None` means the
    /// store is in-memory only (the CLI's
    /// `--no-dedup` flag, or a misconfigured
    /// `data_dir`); `flush()` becomes a no-op in
    /// that case.
    persist_path: Option<PathBuf>,
}

impl ApplicationDedup {
    /// Build a new in-memory store. The on-disk file
    /// is *not* loaded synchronously here — `load()`
    /// is an explicit step. Most callers want
    /// `load_or_default(path)`, which calls `load()`
    /// and falls back to empty on any I/O / parse
    /// error so a corrupted file does not block the
    /// agent.
    pub fn new(persist_path: Option<PathBuf>) -> Self {
        Self {
            by_key: std::sync::Mutex::new(HashMap::new()),
            persist_path,
        }
    }

    /// Load existing records from the configured path.
    /// Returns an empty `ApplicationDedup` on any error
    /// (missing file, parse failure, wrong version) so
    /// the agent never blocks on a corrupted dedup file.
    pub fn load_or_default(persist_path: Option<PathBuf>) -> Self {
        let store = Self::new(persist_path.clone());
        if let Some(path) = persist_path.as_ref() {
            if let Ok(content) = std::fs::read_to_string(path) {
                match serde_json::from_str::<ApplicationDedupFile>(&content) {
                    Ok(file) if file.schema_version == default_schema_version() => {
                        let mut guard = store
                            .by_key
                            .lock()
                            .expect("application_dedup mutex poisoned");
                        for r in file.records {
                            let key = DedupKey {
                                platform: r.platform.clone(),
                                posting_identifier: r.posting_identifier.clone(),
                            };
                            guard.insert(key, r);
                        }
                    }
                    Ok(file) => {
                        tracing::warn!(
                            event = "application_dedup_unknown_schema",
                            path = %path.display(),
                            version = file.schema_version,
                            "ignoring dedup file with unknown schema version"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            event = "application_dedup_parse_failed",
                            path = %path.display(),
                            error = %e,
                            "ignoring unparseable dedup file"
                        );
                    }
                }
            }
        }
        store
    }

    /// Cheap existence check. Called at the `Paused`
    /// checkpoint before the agent surfaces a draft;
    /// when this returns `true` the application is
    /// silently dropped (no draft, no chat message,
    /// no submission).
    pub fn is_already_applied(&self, platform: &str, posting_identifier: &str) -> bool {
        let key = DedupKey {
            platform: normalize_platform(platform),
            posting_identifier: posting_identifier.to_string(),
        };
        let guard = self
            .by_key
            .lock()
            .expect("application_dedup mutex poisoned");
        guard.contains_key(&key)
    }

    /// Look up the existing record for a given key. Returns
    /// `None` if the posting has not been applied to
    /// before. The Tauri command layer can use this to
    /// surface a "you already applied on <date>" hint
    /// instead of a flat dedup.
    pub fn lookup(
        &self,
        platform: &str,
        posting_identifier: &str,
    ) -> Option<ApplicationRecord> {
        let key = DedupKey {
            platform: normalize_platform(platform),
            posting_identifier: posting_identifier.to_string(),
        };
        let guard = self
            .by_key
            .lock()
            .expect("application_dedup mutex poisoned");
        guard.get(&key).cloned()
    }

    /// Record a confirmed submission. Idempotent on
    /// `(platform, identifier)` — calling `record_submission`
    /// twice with the same key updates the existing
    /// record's `submitted_at_unix_secs` and `note`
    /// (the user re-applied, possibly with a different
    /// resume / cover letter) but does not insert a
    /// duplicate. Returns `true` when a new row was
    /// inserted, `false` when an existing row was
    /// updated.
    pub fn record_submission(
        &self,
        platform: &str,
        posting_identifier: &str,
        action_kind: &str,
        finding_id: &str,
        note: &str,
    ) -> bool {
        let normalized_platform = normalize_platform(platform);
        let key = DedupKey {
            platform: normalized_platform.clone(),
            posting_identifier: posting_identifier.to_string(),
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let record = ApplicationRecord {
            platform: normalized_platform,
            posting_identifier: posting_identifier.to_string(),
            action_kind: action_kind.to_string(),
            finding_id: finding_id.to_string(),
            submitted_at_unix_secs: now,
            note: note.to_string(),
        };
        let mut guard = self
            .by_key
            .lock()
            .expect("application_dedup mutex poisoned");
        if guard.contains_key(&key) {
            guard.insert(key, record);
            false
        } else {
            guard.insert(key, record);
            true
        }
    }

    /// Persist the current in-memory state to disk.
    /// No-op when `persist_path` is `None` (in-memory
    /// only). Mirrors the captcha-telemetry flush
    /// pattern: a failure to write is logged and the
    /// in-memory state is preserved so the next
    /// flush can try again.
    pub fn flush(&self) {
        let path = match &self.persist_path {
            Some(p) => p,
            None => return,
        };
        let file = {
            let guard = self
                .by_key
                .lock()
                .expect("application_dedup mutex poisoned");
            ApplicationDedupFile {
                schema_version: default_schema_version(),
                records: guard.values().cloned().collect(),
            }
        };
        // Best-effort create + write. The caller (CLI / Tauri)
        // is responsible for ensuring the parent dir exists.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        match serde_json::to_string_pretty(&file) {
            Ok(content) => {
                if let Err(e) = std::fs::write(path, content) {
                    tracing::warn!(
                        event = "application_dedup_flush_failed",
                        path = %path.display(),
                        error = %e,
                        "could not write dedup file; in-memory state preserved"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    event = "application_dedup_serialize_failed",
                    error = %e,
                    "could not serialize dedup file; skipping flush"
                );
            }
        }
    }

    /// Total number of records in the in-memory map.
    /// Cheap O(1) read; used by the agent's end-of-task
    /// summary line and by the Tauri command that
    /// surfaces a "X applications sent this session" tile.
    pub fn len(&self) -> usize {
        let guard = self
            .by_key
            .lock()
            .expect("application_dedup mutex poisoned");
        guard.len()
    }

    /// Whether the store is empty. Used by the same
    /// call sites that check `len()` so they can avoid
    /// a `> 0` comparison.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshot of every record, in deterministic
    /// (sorted-by-platform-then-identifier) order. Cheap
    /// to clone (records are small) and used by the
    /// end-of-task summary.
    pub fn snapshot(&self) -> Vec<ApplicationRecord> {
        let guard = self
            .by_key
            .lock()
            .expect("application_dedup mutex poisoned");
        let mut rows: Vec<ApplicationRecord> = guard.values().cloned().collect();
        // Sort by (platform, posting_identifier) for
        // a stable on-disk + summary output. The
        // `HashMap`'s iteration order is unspecified;
        // without this sort, the JSON file would
        // differ across runs even when the same
        // submissions were recorded.
        rows.sort_by(|a, b| {
            a.platform
                .cmp(&b.platform)
                .then(a.posting_identifier.cmp(&b.posting_identifier))
        });
        rows
    }
}

/// Default on-disk path the agent uses when the caller
/// doesn't override it. `<data_dir>/applications_sent.json`.
///
/// The `data_dir` argument is the same per-user app-data
/// dir the agent's transcripts land in (resolved by
/// `app_handle.path().app_data_dir()` in the Tauri shell,
/// `<cwd>/` in the CLI).
pub fn default_persist_path(data_dir: &Path) -> PathBuf {
    data_dir.join("applications_sent.json")
}

/// Lowercase + strip a leading `www.`. Matches the
/// `mew_cdp::normalize_host` semantics. Kept private
/// and inlined so this module has no `mew_cdp` dep —
/// the normalization is one line and the consistency
/// benefit is a stable, comparable `platform` string
/// across dedup rows and the on-disk file.
fn normalize_platform(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    if lower.len() >= 4 && lower[..4].eq_ignore_ascii_case("www.") {
        lower[4..].to_string()
    } else {
        lower
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------
    // Construction & basic API
    // ----------------------

    #[test]
    fn new_is_empty_and_len_is_zero() {
        let s = ApplicationDedup::new(None);
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert!(s.snapshot().is_empty());
    }

    #[test]
    fn record_submission_inserts_and_len_grows() {
        let s = ApplicationDedup::new(None);
        let inserted = s.record_submission(
            "linkedin.com",
            "job-123",
            "submit_application",
            "finding-x",
            "Senior Rust Engineer @ Acme",
        );
        assert!(inserted, "first record must report insert");
        assert_eq!(s.len(), 1);
        assert!(s.is_already_applied("linkedin.com", "job-123"));
    }

    #[test]
    fn record_submission_is_idempotent_on_key() {
        // Calling record_submission twice with the same
        // (platform, identifier) is the user's "I
        // re-applied with a different resume" path.
        // We update the existing row's timestamp / note
        // and do NOT insert a duplicate.
        let s = ApplicationDedup::new(None);
        s.record_submission(
            "upwork.com",
            "project-9876",
            "send_proposal",
            "f-1",
            "first draft",
        );
        let inserted_again = s.record_submission(
            "upwork.com",
            "project-9876",
            "send_proposal",
            "f-1",
            "updated draft",
        );
        assert!(!inserted_again, "second record must report update, not insert");
        assert_eq!(s.len(), 1, "duplicate insert would inflate len");
        let row = s.lookup("upwork.com", "project-9876").unwrap();
        assert_eq!(row.note, "updated draft");
    }

    // ----------------------
    // Platform normalization
    // ----------------------

    #[test]
    fn platform_normalization_strips_www_and_lowercases() {
        let s = ApplicationDedup::new(None);
        s.record_submission(
            "WWW.LinkedIn.com",
            "job-1",
            "submit_application",
            "f-1",
            "n",
        );
        // Different casing / leading www must still match
        // the same dedup row.
        assert!(s.is_already_applied("linkedin.com", "job-1"));
        assert!(s.is_already_applied("www.linkedin.com", "job-1"));
        assert!(s.is_already_applied("WWW.LINKEDIN.COM", "job-1"));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn empty_platform_is_accepted() {
        // Edge case: a misbehaving LLM passes an
        // empty platform. The dedup store does not
        // crash; it stores the row under the empty
        // key. A future PR can decide to reject the
        // empty platform at the call site, but the
        // dedup store's job is to be the source of
        // truth, not to validate the call site.
        let s = ApplicationDedup::new(None);
        s.record_submission("", "job-1", "submit_application", "f-1", "n");
        assert!(s.is_already_applied("", "job-1"));
        assert_eq!(s.len(), 1);
    }

    // ----------------------
    // Lookup
    // ----------------------

    #[test]
    fn lookup_returns_none_for_missing_key() {
        let s = ApplicationDedup::new(None);
        assert!(s.lookup("linkedin.com", "job-1").is_none());
    }

    #[test]
    fn lookup_returns_record_for_existing_key() {
        let s = ApplicationDedup::new(None);
        s.record_submission("indeed.com", "jk-1", "submit_application", "f-1", "n");
        let row = s.lookup("indeed.com", "jk-1").expect("row should exist");
        assert_eq!(row.action_kind, "submit_application");
        assert_eq!(row.finding_id, "f-1");
    }

    // ----------------------
    // Snapshot ordering
    // ----------------------

    #[test]
    fn snapshot_is_sorted_by_platform_then_identifier() {
        let s = ApplicationDedup::new(None);
        // Insert in a non-sorted order; the snapshot
        // must still come back sorted.
        s.record_submission("upwork.com", "z", "send_proposal", "f", "n");
        s.record_submission("linkedin.com", "a", "submit_application", "f", "n");
        s.record_submission("linkedin.com", "b", "submit_application", "f", "n");
        s.record_submission("contra.com", "m", "send_proposal", "f", "n");
        let snap = s.snapshot();
        let keys: Vec<(String, String)> = snap
            .iter()
            .map(|r| (r.platform.clone(), r.posting_identifier.clone()))
            .collect();
        assert_eq!(
            keys,
            vec![
                ("contra.com".to_string(), "m".to_string()),
                ("linkedin.com".to_string(), "a".to_string()),
                ("linkedin.com".to_string(), "b".to_string()),
                ("upwork.com".to_string(), "z".to_string()),
            ]
        );
    }

    // ----------------------
    // On-disk round trip
    // ----------------------

    #[test]
    fn flush_writes_json_to_disk() {
        let dir = std::env::temp_dir().join("mew_application_dedup_test_flush");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("applications_sent.json");
        let _ = std::fs::remove_file(&path);
        let s = ApplicationDedup::new(Some(path.clone()));
        s.record_submission("linkedin.com", "job-1", "submit_application", "f-1", "n");
        s.flush();
        let content = std::fs::read_to_string(&path).expect("file should exist");
        let file: ApplicationDedupFile = serde_json::from_str(&content).expect("valid json");
        assert_eq!(file.schema_version, 1);
        assert_eq!(file.records.len(), 1);
        assert_eq!(file.records[0].platform, "linkedin.com");
    }

    #[test]
    fn flush_is_noop_when_persist_path_is_none() {
        // No file is created. The test passes by simply
        // not panicking and by the snapshot being
        // non-empty.
        let s = ApplicationDedup::new(None);
        s.record_submission("linkedin.com", "job-1", "submit_application", "f-1", "n");
        s.flush();
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn load_or_default_recovers_records_from_disk() {
        // Write a file with one record, then re-load
        // it via `load_or_default` and verify the
        // record is present in the in-memory map.
        let dir = std::env::temp_dir().join("mew_application_dedup_test_load");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("applications_sent.json");
        let _ = std::fs::remove_file(&path);
        let writer = ApplicationDedup::new(Some(path.clone()));
        writer.record_submission("linkedin.com", "job-1", "submit_application", "f-1", "n");
        writer.flush();
        assert!(path.exists());

        let loaded = ApplicationDedup::load_or_default(Some(path));
        assert_eq!(loaded.len(), 1);
        assert!(loaded.is_already_applied("linkedin.com", "job-1"));
    }

    #[test]
    fn load_or_default_returns_empty_on_missing_file() {
        // A fresh install has no dedup file; the
        // agent must not crash on first boot.
        let dir = std::env::temp_dir().join("mew_application_dedup_test_missing");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("does-not-exist.json");
        let _ = std::fs::remove_file(&path);
        let s = ApplicationDedup::load_or_default(Some(path));
        assert!(s.is_empty());
    }

    #[test]
    fn load_or_default_recovers_from_corrupt_file() {
        // A user edited the file by hand and broke
        // the JSON. The agent must fall back to
        // empty rather than crash.
        let dir = std::env::temp_dir().join("mew_application_dedup_test_corrupt");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("applications_sent.json");
        std::fs::write(&path, "not-json{").unwrap();
        let s = ApplicationDedup::load_or_default(Some(path));
        assert!(s.is_empty(), "corrupt file must be treated as empty");
    }

    #[test]
    fn load_or_default_ignores_unknown_schema_version() {
        // A file written by a future agent (schema v2)
        // is ignored by the current loader so the
        // runtime doesn't crash on a downgrade.
        let dir = std::env::temp_dir().join("mew_application_dedup_test_schema");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("applications_sent.json");
        std::fs::write(
            &path,
            r#"{"schema_version": 999, "records": []}"#,
        )
        .unwrap();
        let s = ApplicationDedup::load_or_default(Some(path));
        assert!(s.is_empty());
    }

    // ----------------------
    // Default path
    // ----------------------

    #[test]
    fn default_persist_path_lives_under_data_dir() {
        let p = default_persist_path(Path::new("/some/data/dir"));
        assert_eq!(p, PathBuf::from("/some/data/dir/applications_sent.json"));
    }

    // ----------------------
    // Tracked action kinds
    // ----------------------

    #[test]
    fn tracked_action_kinds_lists_phase_6_surface() {
        // The store's job is to dedup the Phase 6
        // auto-apply surface (jobs, proposals,
        // connection requests). A regression that
        // drops a kind from the list would silently
        // disable dedup for that surface.
        for k in ["submit_application", "send_proposal", "send_connection_request"] {
            assert!(
                TRACKED_ACTION_KINDS.contains(&k),
                "TRACKED_ACTION_KINDS missing {k}"
            );
        }
    }
}
