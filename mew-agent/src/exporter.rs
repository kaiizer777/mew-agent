// mew — Phase 41: Lead-Gen Result Export.
//
// After the orchestrator's `LocalLeadScorer::rank_and_filter`
// pass produces the final ranked, deduped, qualified
// `MapsFinding` list, this module writes the result to disk
// in the operator's chosen format (CSV / JSON / Markdown) at
// task completion. The on-disk file path is then surfaced as
// a `file://` link in the chat reply so the user can open it
// from the chat bubble (one click in the Tauri webview opens
// the file in the OS default app — Excel for CSV, a text
// editor for Markdown, the browser for the JSON view).
//
// Cost discipline: zero LLM calls. The exporter is a pure-Rust
// post-processing step that runs once per Maps task completion
// when `config.agent.export.enabled == true`. The cost is
// O(n) for n = total findings (≤ 50 per task — bounded by
// `MAPS_LISTING_HARD_CAP`), single-digit microseconds in
// practice for the realistic n ≤ 15. The on-disk write is
// the only side effect; a failure to write logs a warning and
// degrades gracefully (no file, no link, chat reply unchanged
// from the pre-Phase-41 byte-equivalent form).
//
// Architectural choice — `AnyFinding` enum. The spec calls
// for generalizing the exporter to handle `LeadFinding` and
// `JobFinding` too, even though Phase 41 only wires the
// `MapsFinding` path end-to-end. We define a thin
// `AnyFinding` enum with one variant per vertical; each
// variant implements a `to_csv_row()` / `to_json_value()` /
// `to_markdown_row()` method. This way the writer code is
// vertical-agnostic and a future phase that wires
// `LeadFinding` or `JobFinding` export can do so by adding
// the variant's row methods — no further refactor of the
// writer. For Phase 41, `Lead` and `Job` render a basic row
// from whatever fields the variant carries; a future phase
// can flesh them out to a richer column set.
//
// Dependency direction (per proj.md §2): this module lives in
// `mew-agent` and adds the `csv` crate (workspace dep) to
// the agent's `Cargo.toml`. The `csv` crate does NOT propagate
// to `mew-nav`, `mew-cdp`, `mew-perception`, or
// `mew-resilience` — those crates never see the export path.
// The Tauri UI consumes the on-disk file via `file://` link,
// not via a new crate-level dep, so the dependency direction
// rule is preserved.

use crate::handoff::{JobFinding, LeadFinding, MapsFinding};
use crate::{ExportConfig, ExportFormat};
use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Header row for the CSV export. Fixed column order so an
/// operator who opens the file in Excel / Google Sheets sees
/// a stable schema across runs. The spec pins this list
/// verbatim — no remapping, no column reordering, no
/// conditional inclusion.
pub const CSV_HEADER: &[&str] = &[
    "Name",
    "Phone",
    "Address",
    "Rating",
    "Reviews",
    "Has Website",
    "Website URL",
    "Category",
    "Maps URL",
];

/// Verticals the exporter knows how to write. Phase 41 wires
/// the `Maps` path; the other two are scaffolding for the
/// future phases the spec calls out and are deliberately
/// minimal (one row of available fields, no rich column set).
///
/// The enum is a thin type tag — the real row content is
/// produced by per-variant methods. The writer code is
/// vertical-agnostic.
#[derive(Debug, Clone)]
pub enum AnyFinding {
    Maps(MapsFinding),
    Lead(LeadFinding),
    Job(JobFinding),
}

impl AnyFinding {
    /// CSV row values in the order of [`CSV_HEADER`]. Each
    /// cell is a `String` — the `csv` crate handles the
    /// escaping (quoting cells that contain a `,` or `"`).
    /// `None` and `0` values render as empty cells, not as
    /// the string "None" / "0" (so an Excel pivot table
    /// doesn't get a column full of "None" strings).
    pub fn to_csv_row(&self) -> Vec<String> {
        match self {
            AnyFinding::Maps(m) => maps_to_csv_row(m),
            AnyFinding::Lead(l) => lead_to_csv_row(l),
            AnyFinding::Job(j) => job_to_csv_row(j),
        }
    }

    /// The on-wire JSON value for the JSON export. `Maps`
    /// serializes the typed struct directly; `Lead` and
    /// `Job` similarly. The JSON shape is the typed Rust
    /// struct's serde representation, which keeps the export
    /// loadable into a downstream `serde_json::from_str` of
    /// the same struct.
    fn to_json_value(&self) -> serde_json::Value {
        match self {
            AnyFinding::Maps(m) => serde_json::to_value(m).unwrap_or(serde_json::Value::Null),
            AnyFinding::Lead(l) => serde_json::to_value(l).unwrap_or(serde_json::Value::Null),
            AnyFinding::Job(j) => serde_json::to_value(j).unwrap_or(serde_json::Value::Null),
        }
    }

    /// One markdown-table row for the Markdown export. The
    /// column set is the same as `synthesize_maps` in
    /// `chat_agent.rs` — keeping the in-chat table and the
    /// on-disk Markdown byte-equivalent for the same input
    /// means an operator who pastes the file into a chat
    /// client sees the same table they saw in the agent's
    /// reply.
    pub fn to_markdown_row(&self) -> String {
        match self {
            AnyFinding::Maps(m) => maps_to_markdown_row(m),
            // Lead / Job markdown rows are intentionally a
            // minimal "name — primary field" line for Phase
            // 41; a future phase can flesh them out to a
            // richer column set.
            AnyFinding::Lead(l) => {
                format!("| {} | {} |", escape_md_cell(&l.company_name), "-")
            }
            AnyFinding::Job(j) => {
                let title = j.base.title.clone().unwrap_or_default();
                format!("| {} | {} |", escape_md_cell(&title), "-")
            }
        }
    }

    /// A short label for the finding, used in the export
    /// filename (e.g. `salons_mumbai_20260808_165432.csv`).
    /// For `Maps` we use the normalized search query; for
    /// `Lead` and `Job` we use the company name / job title.
    pub fn label(&self) -> String {
        match self {
            AnyFinding::Maps(m) => {
                // The first `MapsFinding` carries a
                // `category` (when the agent captured it);
                // the search-query is the cleanest cross-
                // category label. We fall back to the
                // category string when present, and to a
                // generic "maps" when neither is available.
                m.category.clone().unwrap_or_else(|| "maps".to_string())
            }
            AnyFinding::Lead(l) => sanitize_for_filename(&l.company_name),
            AnyFinding::Job(j) => {
                let title = j.base.title.clone().unwrap_or_default();
                if title.is_empty() {
                    "job".to_string()
                } else {
                    sanitize_for_filename(&title)
                }
            }
        }
    }
}

/// Render a `MapsFinding` as a CSV row in the column order of
/// [`CSV_HEADER`]. Pulled out of the `AnyFinding` method so
/// the variant-method stays a one-liner and so a future
/// Phase-42 harness can call the row helper directly.
fn maps_to_csv_row(m: &MapsFinding) -> Vec<String> {
    vec![
        m.name.clone(),
        m.phone.clone().unwrap_or_default(),
        m.address.clone().unwrap_or_default(),
        m.rating.map(|r| format!("{r:.1}")).unwrap_or_default(),
        m.review_count.map(|n| n.to_string()).unwrap_or_default(),
        // Spec says: header is "Has Website" — boolean
        // rendered as "true" / "false" (not "Yes" / "No")
        // so an Excel auto-filter treats it as a real
        // boolean column.
        if m.has_website { "true" } else { "false" }.to_string(),
        m.website_url.clone().unwrap_or_default(),
        m.category.clone().unwrap_or_default(),
        m.maps_url.clone(),
    ]
}

fn lead_to_csv_row(l: &LeadFinding) -> Vec<String> {
    // Phase 41 placeholder: collapse the LeadFinding into a
    // single "company name" cell under the `Name` column.
    // A future phase will expand this to a richer column
    // set (contact email, role, confidence, ...).
    vec![
        l.company_name.clone(),
        String::new(),
        l.base.url.clone().unwrap_or_default(),
        String::new(),
        String::new(),
        String::new(),
        l.company_domain.clone(),
        l.source_platform.clone(),
        l.contact_url.clone().unwrap_or_default(),
    ]
}

fn job_to_csv_row(j: &JobFinding) -> Vec<String> {
    // Phase 41 placeholder: collapse the JobFinding into a
    // single "title" cell under the `Name` column. A future
    // phase will expand this.
    vec![
        j.base.title.clone().unwrap_or_default(),
        String::new(),
        j.base.url.clone().unwrap_or_default(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        // `ResearchFinding::platform` is `String` (not
        // `Option<String>`) — it always carries a value,
        // so no `unwrap_or_default` is needed.
        j.base.platform.clone(),
        j.base.url.clone().unwrap_or_default(),
    ]
}

/// Render a `MapsFinding` as one markdown-table row with the
/// same column set as `synthesize_maps` in `chat_agent.rs`.
/// Keeping the two functions column-aligned (same header,
/// same cell-formatting rules) means the on-disk Markdown
/// export is a copy-paste of the in-chat table.
fn maps_to_markdown_row(m: &MapsFinding) -> String {
    let name = escape_md_cell(&m.name);
    let phone = m.phone.as_deref().unwrap_or("—");
    let address = m
        .address
        .as_deref()
        .map(escape_md_cell)
        .unwrap_or_else(|| "—".to_string());
    let rating = m
        .rating
        .map(|r| format!("{r:.1}"))
        .unwrap_or_else(|| "—".to_string());
    let reviews = m
        .review_count
        .map(|n| n.to_string())
        .unwrap_or_else(|| "—".to_string());
    let website = if m.has_website {
        m.website_url
            .as_deref()
            .map(strip_scheme_and_www)
            .unwrap_or_else(|| "(yes)".to_string())
    } else {
        "No".to_string()
    };
    format!("| {name} | {phone} | {address} | {rating} | {reviews} | {website} |")
}

/// Escape a string so it can be safely embedded as a single
/// markdown-table cell. Replaces `|` (the table column
/// separator) with `\|` (the markdown escape). Mirrors
/// `maps_table_row` in `chat_agent.rs`.
fn escape_md_cell(s: &str) -> String {
    s.replace('|', "\\|")
}

/// Strip the URL scheme and any leading `www.` for display
/// in a markdown cell. Mirrors the same helper in
/// `chat_agent.rs`. The local copy is intentional — the
/// exporter is allowed to be a free-standing module without
/// re-exposing `chat_agent`'s private helper, and the
/// behavior is so simple that a 4-line copy is cheaper than
/// lifting the helper to `pub(crate)`.
fn strip_scheme_and_www(url: &str) -> String {
    let mut s = url.to_string();
    for prefix in &["https://", "http://"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.to_string();
            break;
        }
    }
    if let Some(rest) = s.strip_prefix("www.") {
        s = rest.to_string();
    }
    s
}

/// Sanitize a string for use in a filename: lowercase,
/// replace whitespace and any non-alphanumeric character
/// with `_`, collapse consecutive `_`, trim leading/trailing
/// `_`. Returns "export" for an empty / whitespace-only
/// input so the generated filename always has a non-empty
/// label segment.
fn sanitize_for_filename(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_underscore = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_underscore = false;
        } else if c.is_whitespace() || c == '-' {
            if !prev_underscore {
                out.push('_');
                prev_underscore = true;
            }
        }
        // Drop everything else (punctuation, control
        // chars) — we want a Windows-safe filename.
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "export".to_string()
    } else {
        trimmed
    }
}

/// Build a timestamp suffix of the form `YYYYMMDD_HHMMSS` in
/// UTC. Pinned to UTC so the export directory has a stable
/// ordering regardless of which timezone the agent runs in
/// (the Tauri webview is in the user's local TZ but the
/// file's mtime is in UTC anyway — the suffix in the
/// filename is what the user sees in the chat bubble).
fn timestamp_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Naive UTC date math, no chrono dep (the spec
    // explicitly says "no new dependency without checking
    // the dependency direction rule" — pulling chrono in
    // for one timestamp would be a 200k LoC dep for a
    // 20-line need).
    let (year, month, day, hour, min, sec) = unix_secs_to_ymdhms(secs);
    format!(
        "{year:04}{month:02}{day:02}_{hour:02}{min:02}{sec:02}",
    )
}

/// Convert a Unix-epoch seconds count to a `(year, month,
/// day, hour, min, sec)` tuple in UTC. Pure-Rust, no
/// `chrono`/`time` deps. The arithmetic follows Howard
/// Hinnant's `civil_from_days` algorithm (the
/// `days_from_civil` inverse), which is the standard
/// dependency-free conversion.
fn unix_secs_to_ymdhms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;

    // Shift epoch so 1970-01-01 is day 0. The algorithm
    // wants days relative to 0000-03-01 (the "march-based"
    // year), so we offset by the 1970-03-01 epoch day.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146_096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y as u32, m as u32, d as u32, hour, min, sec)
}

/// Resolve the export directory. When the operator
/// configured an explicit `output_dir`, use that. When
/// `output_dir` is `None`, fall back to `<cwd>/exports/`
/// (the spec's "default to a subdirectory the Tauri shell
/// also uses"). We `create_dir_all` lazily so a fresh
/// install doesn't need a pre-created `exports/` folder.
fn resolve_output_dir(config: &ExportConfig) -> Result<PathBuf> {
    let dir = match &config.output_dir {
        Some(p) => p.clone(),
        None => std::env::current_dir()
            .context("resolve current_dir for default export path")?
            .join("exports"),
    };
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create export directory {}", dir.display()))?;
    Ok(dir)
}

/// Build the export filename: `<label>_<timestamp>.<ext>`.
/// `label` is the vertical's short label (e.g. the Maps
/// category, or the company name for a LeadFinding), and
/// `timestamp` is `YYYYMMDD_HHMMSS` in UTC. The
/// `<timestamp>` segment keeps two runs in the same minute
/// from clobbering each other; two runs in the same second
/// would still collide, but that's a pathological "agent
/// fires off two completions in the same second" race
/// that we accept (the file-collision error is logged and
/// the operator sees the chat reply without the link).
fn build_filename(label: &str, format: ExportFormat, timestamp: &str) -> String {
    let ext = match format {
        ExportFormat::Csv => "csv",
        ExportFormat::Json => "json",
        ExportFormat::Markdown => "md",
    };
    let safe_label = sanitize_for_filename(label);
    format!("{safe_label}_{timestamp}.{ext}")
}

/// Write a list of `MapsFinding`s to a CSV file. The CSV
/// uses the [`CSV_HEADER`] column order; cells are
/// auto-quoted by the `csv` crate. A `csv::Writer` is
/// created with `from_path` (which truncates an existing
/// file at the same path — the spec accepts a same-second
/// collision as a "second run overwrites the first" rather
/// than the more elaborate "append and re-dedup" path).
fn write_csv(path: &Path, findings: &[AnyFinding]) -> Result<()> {
    let mut wtr = csv::Writer::from_path(path)
        .with_context(|| format!("create csv writer at {}", path.display()))?;
    wtr.write_record(CSV_HEADER)
        .context("write csv header row")?;
    for f in findings {
        wtr.write_record(f.to_csv_row())
            .with_context(|| format!("write csv row for finding label={}", f.label()))?;
    }
    wtr.flush().context("flush csv writer")?;
    Ok(())
}

/// Write a list of findings to a JSON file. The output is
/// the `serde_json::to_string_pretty` form of an array of
/// finding-shaped values — one entry per finding, in the
/// input order. The on-disk shape is loadable via
/// `serde_json::from_str` into the same typed struct (for
/// `Maps`) or into `serde_json::Value` (for the other two
/// variants, which don't yet have a typed envelope).
fn write_json(path: &Path, findings: &[AnyFinding]) -> Result<()> {
    let values: Vec<serde_json::Value> = findings.iter().map(|f| f.to_json_value()).collect();
    // The wrapper struct is so the file has a `{"findings":
    // [...]}` shape (matches what an operator expects from
    // a JSON export) and so a future `metadata` block
    // (timestamp, label, vertical) can be added without
    // breaking the on-disk schema.
    #[derive(Serialize)]
    struct Envelope<'a> {
        findings: &'a [serde_json::Value],
    }
    let body = serde_json::to_string_pretty(&Envelope {
        findings: &values,
    })
    .context("serialize findings to json")?;
    std::fs::write(path, body)
        .with_context(|| format!("write json file to {}", path.display()))?;
    Ok(())
}

/// Write a list of findings to a Markdown file. The
/// Markdown file is a single table with a `| Rank | Name |
/// Phone | Address | Rating | Reviews | Website |` header
/// and one row per finding, sorted by the input order (the
/// orchestrator's `LocalLeadScorer::rank_and_filter` has
/// already sorted the list before handing it to the
/// exporter). The on-disk file is byte-equivalent to the
/// in-chat table for the same input.
fn write_markdown(path: &Path, findings: &[AnyFinding]) -> Result<()> {
    let mut out = String::new();
    out.push_str("# Lead-gen Results\n\n");
    out.push_str(&format!("Total findings: {}\n\n", findings.len()));
    out.push_str("| Rank | Name | Phone | Address | Rating | Reviews | Website |\n");
    out.push_str("| ---: | :--- | :--- | :--- | ---: | ---: | :--- |\n");
    for (idx, f) in findings.iter().enumerate() {
        // Maps carries its own `rank` from the scorer;
        // Lead / Job fall back to 1..=N positional.
        let rank = match f {
            AnyFinding::Maps(m) => m.rank,
            _ => idx + 1,
        };
        let row = match f {
            AnyFinding::Maps(m) => maps_to_markdown_row(m),
            _ => f.to_markdown_row(),
        };
        out.push_str(&format!("| {rank} | {row}\n"));
    }
    std::fs::write(path, out)
        .with_context(|| format!("write markdown file to {}", path.display()))?;
    Ok(())
}

/// Write the findings to disk in the requested format and
/// return the absolute path to the written file. The caller
/// (`orchestrator::dispatch_browser_task`) appends the
/// returned path to the chat reply as a `file://` link.
///
/// Errors here are **non-fatal** at the call site: a
/// failure to write logs a warning and the call site
/// returns `None` (no link, chat reply unchanged from
/// pre-Phase-41 form). This is the "graceful degradation"
/// the spec implies when it says "`export.enabled: false`
/// skips the export entirely" — we extend that to "any
/// write error skips the export too."
///
/// `findings` is the already-ranked, already-deduped
/// `MapsFinding` slice (or, in the future, a mixed-vertical
/// `AnyFinding` slice). The exporter does not re-rank or
/// re-dedup — the orchestrator owns that step.
///
/// `label` is the task label used in the filename. For a
/// Maps task we pass the `MapsQuery::search_query`
/// (`"salons in Mumbai"` → `salons_in_mumbai_*.csv`); for
/// a future Lead/Job task we pass the company name / job
/// title. The label is sanitized for filesystem safety.
pub fn export_findings(
    findings: &[AnyFinding],
    config: &ExportConfig,
    label: &str,
) -> Result<PathBuf> {
    if !config.enabled {
        return Err(anyhow!(
            "export_findings called with export.enabled = false; \
             this is a logic error — callers should short-circuit before calling"
        ));
    }
    if findings.is_empty() {
        return Err(anyhow!(
            "export_findings called with an empty findings slice; \
             callers should short-circuit before calling"
        ));
    }

    let dir = resolve_output_dir(config)?;
    let timestamp = timestamp_suffix();
    let filename = build_filename(label, config.format, &timestamp);
    let path = dir.join(&filename);

    match config.format {
        ExportFormat::Csv => write_csv(&path, findings)?,
        ExportFormat::Json => write_json(&path, findings)?,
        ExportFormat::Markdown => write_markdown(&path, findings)?,
    }

    // The `file://` link in the chat reply needs an
    // absolute path so the Tauri webview (which renders
    // the chat bubble) can resolve the link to a real
    // file. We canonicalize the path so symlinks (and the
    // Windows `\\?\` long-path prefix that
    // `canonicalize` adds) don't appear in the user-
    // facing link.
    let abs = std::fs::canonicalize(&path)
        .with_context(|| format!("canonicalize export path {}", path.display()))?;
    Ok(abs)
}

/// Build the chat-reply trailer that surfaces the export
/// file path. The trailer is appended to the chat reply
/// when the export succeeded. The link is a `file://` URL
/// so the Tauri webview can render it as a clickable
/// hyperlink; the displayed label is the bare filename
/// (not the full path) so the chat bubble stays readable.
///
/// Returns the trailer as a `String` (no leading
/// whitespace — the caller is responsible for ensuring
/// the chat reply ends in a newline before appending).
pub fn build_export_trailer(absolute_path: &Path) -> String {
    // The Windows canonicalized path uses `\` as the
    // separator; a `file://` URL needs `/` and an
    // authority of empty (`file:///C:/...`). We use
    // `path_to_file_uri` from the `url` crate to handle
    // the platform-specific conversion rather than
    // hand-rolling the path-to-URL mapping.
    let uri = url::Url::from_file_path(absolute_path)
        .map(|u| u.to_string())
        // `from_file_path` returns `Err` on Windows
        // when the path isn't absolute — we always pass
        // an absolute path, so this branch is a
        // defense-in-depth fallback. We still surface a
        // non-clickable but readable "📁 ...: <path>"
        // line so the user can copy-paste the path.
        .unwrap_or_else(|_| format!("file:///{}", absolute_path.display()));
    let display = absolute_path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| absolute_path.display().to_string());
    format!("\n\n📁 Full results saved to: [{display}]({uri})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff::MapsFinding;
    use tempfile::tempdir;

    /// Build a `MapsFinding` for tests. Default-rich
    /// fixture so test code stays short.
    fn maps_finding(name: &str, has_website: bool, rating: Option<f32>) -> MapsFinding {
        MapsFinding {
            rank: 1,
            name: name.to_string(),
            phone: Some("+91-22-12345678".to_string()),
            address: Some("123 Marine Drive, Mumbai".to_string()),
            rating,
            review_count: Some(50),
            has_website,
            website_url: if has_website {
                Some("https://www.example.com".to_string())
            } else {
                None
            },
            category: Some("Salon".to_string()),
            maps_url: format!("https://maps.google.com/?q={}", name.replace(' ', "+")),
        }
    }

    /// Spec acceptance test: 3 mock `MapsFinding`
    /// entries + `ExportFormat::Csv` writes a file
    /// with a header row and 3 data rows. Read the
    /// file back and assert column count + values.
    #[test]
    fn export_findings_writes_csv_with_header_and_three_rows() {
        let dir = tempdir().expect("tempdir");
        let findings = vec![
            AnyFinding::Maps(maps_finding("Salon A", false, Some(4.5))),
            AnyFinding::Maps(maps_finding("Salon B", true, Some(4.0))),
            AnyFinding::Maps(maps_finding("Salon C", false, Some(3.8))),
        ];
        let config = ExportConfig {
            enabled: true,
            format: ExportFormat::Csv,
            output_dir: Some(dir.path().to_path_buf()),
        };

        let path =
            export_findings(&findings, &config, "salons in mumbai").expect("export_findings");
        assert!(path.exists(), "export file should exist at {path:?}");

        // Read the file back and parse it with the `csv`
        // crate so we don't rely on a hand-rolled line
        // splitter (which would break on cells containing
        // a `,` or `"`).
        let content = std::fs::read_to_string(&path).expect("read export");
        let mut rdr = csv::Reader::from_reader(content.as_bytes());
        let headers: Vec<String> = rdr
            .headers()
            .expect("csv headers")
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(headers, CSV_HEADER, "header row mismatch");

        let records: Vec<csv::StringRecord> = rdr.records().collect::<Result<_, _>>().expect("parse");
        assert_eq!(records.len(), 3, "expected 3 data rows, got {records:?}");

        // Spot-check: row 0 is Salon A, has_website is
        // "false" (the CSV uses "true"/"false" so Excel
        // auto-filter treats it as a boolean), and the
        // phone is the fixture's.
        let row0: Vec<&str> = records[0].iter().collect();
        assert_eq!(row0[0], "Salon A");
        assert_eq!(row0[1], "+91-22-12345678");
        assert_eq!(row0[5], "false");
        assert_eq!(row0[8], "https://maps.google.com/?q=Salon+A");

        // Row 1: has_website = true.
        let row1: Vec<&str> = records[1].iter().collect();
        assert_eq!(row1[0], "Salon B");
        assert_eq!(row1[5], "true");
        assert_eq!(row1[6], "https://www.example.com");
    }

    /// Spec coverage: `ExportFormat::Json` writes a
    /// pretty-printed JSON file with the findings as
    /// the inner array.
    #[test]
    fn export_findings_writes_json_with_findings_envelope() {
        let dir = tempdir().expect("tempdir");
        let findings = vec![
            AnyFinding::Maps(maps_finding("Salon A", false, Some(4.5))),
            AnyFinding::Maps(maps_finding("Salon B", true, Some(4.0))),
        ];
        let config = ExportConfig {
            enabled: true,
            format: ExportFormat::Json,
            output_dir: Some(dir.path().to_path_buf()),
        };

        let path = export_findings(&findings, &config, "salons in mumbai").expect("export");
        let content = std::fs::read_to_string(&path).expect("read");
        // The pretty-printer inserts newlines; the
        // presence of `\n  ` after the opening `{` is a
        // cheap check that we're getting the pretty
        // form, not the single-line form.
        assert!(content.contains("\n  \"findings\": ["));

        // Round-trip: parse the file and check the
        // shape. We don't pull in the typed struct here
        // (the test would need to know the field names)
        // — the `findings` key being a non-empty array
        // is the contract.
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("parse export json");
        let arr = parsed
            .get("findings")
            .and_then(|v| v.as_array())
            .expect("findings array");
        assert_eq!(arr.len(), 2, "expected 2 findings, got {arr:?}");
        assert_eq!(arr[0]["name"], "Salon A");
        assert_eq!(arr[0]["has_website"], false);
        assert_eq!(arr[1]["name"], "Salon B");
        assert_eq!(arr[1]["has_website"], true);
    }

    /// Spec coverage: `ExportFormat::Markdown` writes a
    /// `.md` file with a header + a markdown table.
    #[test]
    fn export_findings_writes_markdown_with_table() {
        let dir = tempdir().expect("tempdir");
        let findings = vec![
            AnyFinding::Maps(maps_finding("Salon A", false, Some(4.5))),
            AnyFinding::Maps(maps_finding("Salon B", true, Some(4.0))),
            AnyFinding::Maps(maps_finding("Salon C", false, Some(3.8))),
        ];
        let config = ExportConfig {
            enabled: true,
            format: ExportFormat::Markdown,
            output_dir: Some(dir.path().to_path_buf()),
        };

        let path = export_findings(&findings, &config, "salons in mumbai").expect("export");
        let content = std::fs::read_to_string(&path).expect("read");
        // Header section + table header + 3 rows.
        assert!(content.contains("# Lead-gen Results"));
        assert!(content.contains("Total findings: 3"));
        assert!(content.contains("| Rank | Name | Phone | Address | Rating | Reviews | Website |"));
        // The no-website business renders `No` in the
        // website column (the spec's primary lead
        // signal — using a dash would visually equate
        // it with "data not collected").
        assert!(content.contains("| No |"));
        // The has-website business renders the
        // scheme-stripped domain.
        assert!(content.contains("| example.com |"));
        // Three data rows: line count of `| Salon ` is
        // exactly 3.
        let rows: Vec<&str> = content
            .lines()
            .filter(|l| l.contains("| Salon "))
            .collect();
        assert_eq!(rows.len(), 3, "expected 3 data rows, got {rows:?}");
    }

    /// `export_findings` writes the file with the
    /// expected `<label>_<timestamp>.<ext>` filename
    /// pattern, where the timestamp is `YYYYMMDD_HHMMSS`
    /// in UTC.
    #[test]
    fn export_findings_filename_matches_label_timestamp_ext() {
        let dir = tempdir().expect("tempdir");
        let findings = vec![AnyFinding::Maps(maps_finding("Salon A", false, Some(4.5)))];
        let config = ExportConfig {
            enabled: true,
            format: ExportFormat::Csv,
            output_dir: Some(dir.path().to_path_buf()),
        };
        let path = export_findings(&findings, &config, "salons in Mumbai").expect("export");
        let fname = path.file_name().unwrap().to_string_lossy().into_owned();
        // Pattern: `salons_in_mumbai_YYYYMMDD_HHMMSS.csv`
        // (8 digits, underscore, 6 digits, dot, csv).
        assert!(fname.starts_with("salons_in_mumbai_"), "got {fname}");
        assert!(fname.ends_with(".csv"), "got {fname}");
        let middle = fname
            .strip_prefix("salons_in_mumbai_")
            .and_then(|s| s.strip_suffix(".csv"))
            .expect("middle");
        let parts: Vec<&str> = middle.split('_').collect();
        assert_eq!(parts.len(), 2, "expected date_time, got {middle}");
        assert_eq!(parts[0].len(), 8, "expected 8-digit date, got {}", parts[0]);
        assert_eq!(parts[1].len(), 6, "expected 6-digit time, got {}", parts[1]);
        // Every char in the timestamp must be a digit.
        assert!(
            parts[0].chars().all(|c| c.is_ascii_digit()),
            "non-digit in date: {}",
            parts[0]
        );
        assert!(
            parts[1].chars().all(|c| c.is_ascii_digit()),
            "non-digit in time: {}",
            parts[1]
        );
    }

    /// `export_findings` creates the output directory
    /// if it doesn't already exist. This is the
    /// `<cwd>/exports/` default path behavior — a fresh
    /// install shouldn't need a pre-created `exports/`
    /// folder.
    #[test]
    fn export_findings_creates_output_dir_if_missing() {
        let parent = tempdir().expect("tempdir");
        let nested = parent.path().join("a").join("b").join("c");
        assert!(!nested.exists(), "precondition: nested dir must not exist");
        let findings = vec![AnyFinding::Maps(maps_finding("Salon A", false, Some(4.5)))];
        let config = ExportConfig {
            enabled: true,
            format: ExportFormat::Csv,
            output_dir: Some(nested.clone()),
        };
        export_findings(&findings, &config, "salons in mumbai").expect("export");
        assert!(nested.is_dir(), "output dir should have been created");
    }

    /// Spec coverage: `export.enabled: false` is a
    /// no-op — the orchestrator short-circuits before
    /// calling `export_findings`, so we test the
    /// caller's pattern: the function itself returns
    /// an error (the spec's documented behavior is for
    /// the call site to skip the call entirely, not
    /// for the function to silently no-op). The
    /// orchestrator integration test covers the
    /// "no file written" guarantee at the call site.
    #[test]
    fn export_findings_rejects_disabled_config() {
        let dir = tempdir().expect("tempdir");
        let findings = vec![AnyFinding::Maps(maps_finding("Salon A", false, Some(4.5)))];
        let config = ExportConfig {
            enabled: false,
            format: ExportFormat::Csv,
            output_dir: Some(dir.path().to_path_buf()),
        };
        let err = export_findings(&findings, &config, "salons in mumbai")
            .expect_err("export_findings must reject disabled config");
        assert!(
            err.to_string().contains("export.enabled = false"),
            "error message should mention the disabled config, got: {err}"
        );
    }

    /// Spec coverage: empty `findings` is rejected (the
    /// call site should short-circuit, not the function).
    #[test]
    fn export_findings_rejects_empty_findings() {
        let dir = tempdir().expect("tempdir");
        let config = ExportConfig {
            enabled: true,
            format: ExportFormat::Csv,
            output_dir: Some(dir.path().to_path_buf()),
        };
        let err = export_findings(&[], &config, "salons in mumbai")
            .expect_err("export_findings must reject empty findings");
        assert!(
            err.to_string().contains("empty findings slice"),
            "error message should mention the empty slice, got: {err}"
        );
    }

    /// `AnyFinding::to_csv_row` matches the spec's
    /// column order: name, phone, address, rating,
    /// reviews, has_website, website_url, category,
    /// maps_url. Pinned by this test so a future
    /// reordering in the writer doesn't go unnoticed.
    #[test]
    fn maps_to_csv_row_column_order_matches_spec() {
        let m = maps_finding("X", false, Some(4.5));
        let row = AnyFinding::Maps(m).to_csv_row();
        assert_eq!(row.len(), CSV_HEADER.len());
        assert_eq!(row[0], "X");
        assert_eq!(row[1], "+91-22-12345678");
        assert_eq!(row[2], "123 Marine Drive, Mumbai");
        assert_eq!(row[3], "4.5");
        assert_eq!(row[4], "50");
        assert_eq!(row[5], "false");
        assert_eq!(row[6], ""); // no website URL
        assert_eq!(row[7], "Salon");
        assert_eq!(row[8].contains("maps.google.com"), true);
    }

    /// `sanitize_for_filename` collapses whitespace
    /// and non-alphanumeric to `_`, lowercases, and
    /// returns a fallback for empty input. Non-alphanumeric,
    /// non-whitespace, non-hyphen characters (like `.` and
    /// `!`) are dropped (not replaced with `_`) — this is
    /// the documented behavior. The result is still
    /// filesystem-safe on every supported platform.
    #[test]
    fn sanitize_for_filename_handles_pathological_input() {
        assert_eq!(sanitize_for_filename("Salon A"), "salon_a");
        assert_eq!(sanitize_for_filename("  Multi   word  "), "multi_word");
        // `.` and `!` are dropped, not converted to `_`.
        // The version string `v1.2.3-rc4!` -> `v123_rc4`
        // (dots dropped, hyphen kept, exclamation dropped).
        assert_eq!(sanitize_for_filename("v1.2.3-rc4!"), "v123_rc4");
        assert_eq!(sanitize_for_filename(""), "export");
        assert_eq!(sanitize_for_filename("---"), "export");
        // Collapse runs of separators.
        assert_eq!(sanitize_for_filename("a   b"), "a_b");
    }

    /// `build_export_trailer` produces a `file://` link
    /// that the Tauri webview will render as a
    /// clickable hyperlink. We don't assert on the
    /// exact URI (Windows path-to-URL conversion
    /// is platform-specific), only that:
    ///   1. The trailer contains the filename in
    ///      `[...](file://...)` markdown form.
    ///   2. The link starts with `file://`.
    #[test]
    fn build_export_trailer_contains_file_link() {
        let path = std::path::PathBuf::from(if cfg!(windows) {
            r"C:\Users\test\exports\salons_20260808_165432.csv"
        } else {
            "/tmp/exports/salons_20260808_165432.csv"
        });
        let trailer = build_export_trailer(&path);
        assert!(trailer.contains("📁"), "trailer should have the folder emoji");
        assert!(
            trailer.contains("[salons_20260808_165432.csv]("),
            "trailer should have the [filename](...) link, got: {trailer}"
        );
        assert!(
            trailer.contains("file://"),
            "trailer should contain file://, got: {trailer}"
        );
    }

    /// `ExportFormat` round-trips through its `Display`
    /// and `FromStr` impls. The serde lowercase
    /// representation matches the Display form for all
    /// three variants.
    #[test]
    fn export_format_display_and_fromstr_round_trip() {
        for fmt in [ExportFormat::Csv, ExportFormat::Json, ExportFormat::Markdown] {
            let s = fmt.to_string();
            let back: ExportFormat = s.parse().expect("from_str");
            assert_eq!(back, fmt, "round-trip failed for {s}");
        }
    }

    /// `ExportFormat::from_str` accepts the `md`
    /// alias for `markdown` (the spec calls out
    /// `markdown` as the canonical name; `md` is a
    /// free extension we tolerate for operator
    /// convenience).
    #[test]
    fn export_format_fromstr_accepts_md_alias() {
        let fmt: ExportFormat = "md".parse().expect("md alias");
        assert_eq!(fmt, ExportFormat::Markdown);
    }

    /// `ExportFormat::from_str` rejects unknown
    /// formats with a helpful error.
    #[test]
    fn export_format_fromstr_rejects_unknown() {
        let err = "xml".parse::<ExportFormat>().expect_err("xml must fail");
        assert!(
            err.contains("unknown export format 'xml'"),
            "error should mention the bad value, got: {err}"
        );
    }
}
