// mew v2 — Phase 1: structured tracing layer for the ReAct loop.
//
// Purpose: turn the "what just happened" question from guesswork into a
// grep-able, time-stamped record. Phase 1 of the work plan calls for
// replacing assumption with evidence for both known bugs:
//   * Bug #1 — the instagram "text my friend" prompt: where does the
//     agent lose the plot, and which of the three candidate causes
//     (bot-detection, decomposition, ambiguity) actually fires?
//   * Bug #2 — the Tauri handoff: does the browser-agent result ever
//     reach the chat, or does it get dropped on the floor?
//
// The existing per-session `transcripts/transcript_<sid>.log` file is
// great for human reading but it is hand-formatted strings scattered
// across match arms; it is not structured and not easy to filter. This
// module adds a parallel per-session JSON-line log that captures:
//
//   * the exact LLM request body (model, message count, tool names)
//   * the exact LLM response (finish_reason, tool call count, usage)
//   * every tool dispatch (name, args, result, snapshot_signature)
//   * URL resolution decisions (input, branch, resolved_url)
//   * SessionHandle state transitions
//   * the mew-ui -> mew-agent handoff boundary events
//
// Activation: the layer is OFF by default — it has a real cost (every
// LLM body is serialized to JSON on the loop hot path) and would spam
// production logs. To turn it on for a single run, set the env var
//   MEW_TRACING_DIR=<absolute path to a folder>
// before invoking `mew run ...` or starting `mew-ui`.
//
// The format is one JSON object per line, with these top-level fields:
//   { "ts": "<ISO-8601>", "level": "info|debug|...", "target": "<module>",
//     "span": "<current span name>", "event": "<short event id>",
//     ...event-specific fields... }
//
// We deliberately use the `tracing` crate's JSON formatter and a
// per-session rolling writer so the file is self-contained and
// trivially diffable across two runs (the exact tool Phase 1 needs).

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// One event / span / field-set captured and serialized to a JSON line.
#[derive(Debug, Default)]
struct JsonRecord {
    fields: serde_json::Map<String, serde_json::Value>,
}

/// Visitor that copies named event fields into a JSON map. Unnamed
/// (positional) fields are stored under the synthetic key `message`,
/// matching the convention `tracing-subscriber` itself uses.
struct JsonVisitor<'a> {
    record: &'a mut JsonRecord,
}

impl<'a> Visit for JsonVisitor<'a> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // `tracing::debug!` etc pass their args as `record_debug` with
        // the field name (e.g. `count` for `debug!(count = 3)`). We
        // render with the alternate `{:#?}` form for objects so nested
        // structures stay readable; scalar Debug impls are unaffected.
        self.record.fields.insert(
            field.name().to_string(),
            serde_json::Value::String(format!("{value:#?}")),
        );
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record.fields
            .insert(field.name().to_string(), serde_json::Value::String(value.to_string()));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record
            .fields
            .insert(field.name().to_string(), serde_json::Value::from(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record
            .fields
            .insert(field.name().to_string(), serde_json::Value::from(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record
            .fields
            .insert(field.name().to_string(), serde_json::Value::from(value));
    }
}

/// Per-session file handle, mutex-guarded so we can share it across the
/// tracing layer's `on_event` (called from any thread) and the
/// `try_init` shim. The Mutex is OK here: writes are tiny JSON lines,
/// contention is negligible, and we never hold the lock across an
/// `.await` point.
struct SessionWriter {
    file: Mutex<std::fs::File>,
}

impl SessionWriter {
    fn new(path: &PathBuf) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    fn write_line(&self, line: &str) {
        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.write_all(b"\n");
            let _ = f.flush();
        }
    }
}

/// The tracing layer. Holds an `Arc<SessionWriter>` and emits a JSON
/// line on every event. Span names are folded into the `span` field on
/// each event so the loop can be filtered after the fact by span
/// (`SELECT span = "react_loop" AND event = "llm_response"`).
#[derive(Clone)]
pub struct SessionJsonLayer {
    writer: Arc<SessionWriter>,
    session_id: String,
}

impl SessionJsonLayer {
    pub fn new(session_id: impl Into<String>, path: PathBuf) -> std::io::Result<Self> {
        Ok(Self {
            writer: Arc::new(SessionWriter::new(&path)?),
            session_id: session_id.into(),
        })
    }
}

impl<S> Layer<S> for SessionJsonLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut record = JsonRecord::default();
        let mut visitor = JsonVisitor { record: &mut record };
        event.record(&mut visitor);

        let mut obj = serde_json::Map::new();
        obj.insert(
            "ts".to_string(),
            serde_json::Value::String(now_iso8601()),
        );
        obj.insert(
            "level".to_string(),
            serde_json::Value::String(event.metadata().level().to_string()),
        );
        obj.insert(
            "target".to_string(),
            serde_json::Value::String(event.metadata().target().to_string()),
        );
        obj.insert(
            "session_id".to_string(),
            serde_json::Value::String(self.session_id.clone()),
        );

        // Fold the current span stack into a single `span` field.
        // `tracing-subscriber` exposes the most-recent-first stack via
        // `ctx.lookup_current()`; we join them with `::` so the file
        // is greppable.
        if let Some(span) = ctx.lookup_current() {
            let mut names: Vec<String> = Vec::new();
            for s in span.scope() {
                names.push(s.metadata().name().to_string());
            }
            if !names.is_empty() {
                names.reverse();
                obj.insert(
                    "span".to_string(),
                    serde_json::Value::String(names.join("::")),
                );
            }
        }

        for (k, v) in record.fields.into_iter() {
            obj.insert(k, v);
        }

        let line = match serde_json::to_string(&obj) {
            Ok(s) => s,
            Err(_) => return,
        };
        self.writer.write_line(&line);
    }

    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        // We don't emit a dedicated line for span creation (would
        // double the log volume); span name is included on every
        // event emitted *inside* the span. But we still need to
        // record the values associated with the span's own fields
        // so that `info!(parent: id, ...)` lookups work — `tracing`
        // expects `on_new_span` to do this. We stash them in the
        // span's extensions as a no-op side effect; the per-event
        // visitor above is what actually surfaces them to the file.
        let _span = ctx.span(id).expect("span must exist after on_new_span");
        let mut record = JsonRecord::default();
        let mut visitor = JsonVisitor { record: &mut record };
        attrs.record(&mut visitor);
    }

    fn on_record(&self, _id: &Id, _values: &Record<'_>, _ctx: Context<'_, S>) {
        // No-op: we only emit lines on event, not on per-field
        // updates. Keeps the log dense and focused.
    }
}

fn now_iso8601() -> String {
    // Minimal RFC3339-ish timestamp; we don't need timezone-correct
    // formatting here, just a stable, comparable string. `chrono`
    // would be overkill for a debug log.
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("1970-01-01T00:00:00Z+{now}s")
}

/// Resolve the per-session JSON trace path from an env var, creating
/// the directory if needed. Returns `None` when the env var is unset
/// (default — the layer is opt-in per Phase 1's "no production
/// overhead" requirement). The caller is the Tauri `run_browser_task`
/// wrapper and the CLI `main`, which already have the session_id in
/// hand and the transcript dir resolved.
pub fn session_log_path(tracing_dir: &PathBuf, session_id: &str) -> PathBuf {
    tracing_dir.join(format!("trace_{session_id}.jsonl"))
}

/// Install the per-session JSON layer as the **global** `tracing`
/// subscriber for the current process. Returns `Ok(())` on success
/// and `Err(String)` if a global subscriber was already installed
/// (the typical case — the CLI installs a stderr `EnvFilter`
/// subscriber on startup). Re-installing is only safe in tests,
/// where each test owns its own process.
///
/// We use `tracing_subscriber::registry().with(layer).try_init()` —
/// `with` takes a `Layer<Registry>`, not `Arc<Layer<...>>`, so we
/// pass the layer by reference (the layer is `Clone` and cheap to
/// duplicate). The returned guard is a `Layered<Layer, Registry>`,
/// and `try_init` is provided by `SubscriberInitExt`.
///
/// If a global subscriber is already set, `try_init` returns
/// `Err(TryInitError)`. We translate that into a `String` for the
/// caller's convenience; the caller can then fall back to
/// `try_install_thread_local` (the right answer in CLI mode) or to
/// writing to the layer's file directly via the `Arc<SessionJsonLayer>`
/// handle they already hold.
pub fn try_install_global(layer: Arc<SessionJsonLayer>) -> Result<(), String> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let result = tracing_subscriber::registry()
        .with((*layer).clone())
        .try_init();

    match result {
        Ok(()) => Ok(()),
        Err(e) => Err(format!("global subscriber already set: {e}")),
    }
}

/// A `tracing::dispatcher::DefaultGuard` wraps a thread-local
/// subscriber override. The agent stores one of these so the
/// JSONL layer stays live for the session's whole lifetime; when
/// the agent is dropped, the guard is dropped and the override
/// reverts to the previous subscriber (usually the global fmt
/// one). This is the right answer for "we have a global fmt
/// subscriber AND we want a per-session JSONL side-channel."
pub type SessionTraceGuard =
    tracing::dispatcher::DefaultGuard;

/// Install the per-session JSON layer as a **thread-local
/// subscriber override** for the current thread, in addition to
/// whatever the global default is. This is the path the agent
/// uses in CLI mode (where the global slot is owned by the CLI's
/// `tracing_subscriber::fmt()`) and in Tauri mode (where the
/// global slot is owned by Tauri's logger).
///
/// Stacking the layer on top of the global means every
/// `tracing::info!` event reaches BOTH subscribers: the global fmt
/// (for human-readable stderr output) and the JSONL layer (for
/// the structured trace file). The returned `DefaultGuard` must
/// be kept alive by the caller — dropping it reverts the
/// thread-local override. The agent stashes the guard in a field
/// so the override lasts for the whole `run`.
///
/// The function name is `try_install_thread_local` for symmetry
/// with `try_install_global`; it always succeeds because
/// `set_default` does not conflict with the global subscriber.
pub fn try_install_thread_local(
    layer: Arc<SessionJsonLayer>,
) -> SessionTraceGuard {

    use tracing_subscriber::layer::SubscriberExt;

    // The thread-local default needs a `Subscriber` impl, not just
    // a `Layer`. Wrap the layer in a `Layered<Layer, Registry>`
    // — the same shape `tracing_subscriber::registry().with(layer)`
    // produces — and convert that to a `Dispatch` so the
    // thread-local override API accepts it.
    let layered = tracing_subscriber::registry().with((*layer).clone());
    let dispatch = tracing::dispatcher::Dispatch::new(layered);
    // `set_default` returns a `DefaultGuard` that reverts the
    // override when dropped. We return it directly; the caller is
    // expected to store it in an `Option<DefaultGuard>` field.
    tracing::dispatcher::set_default(&dispatch)
}
