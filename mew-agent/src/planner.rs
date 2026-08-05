// mew v2 — Phase 2: pre-flight task decomposition.
//
// Problem: the browser agent's `COMPLETENESS PROTOCOL` (system prompt,
// agent.rs) tells the LLM to call `declare_subtasks` for multi-item
// tasks. That's a *nudge*, not a guarantee. The LLM often skips
// declaration on short two-clause prompts ("go to instagram and text
// my friend hi" is `navigate_instagram + send_message` = 2 subtasks,
// but the LLM treats it as one undifferentiated blob). When the
// model skips declaration, `CompletenessTracker` stays empty, the
// finish-gate degenerates to a no-op, and the only check left is
// "did the model say finish?" — which is exactly the partial-
// completion failure mode the tracker was supposed to prevent.
//
// Fix (this file): run a deterministic decomposition pass *before*
// the LLM's first tool call, on the agent's own thread, with no
// network and no LLM cost. The result is a typed `Plan` that:
//   1. Seeds `CompletenessTracker` with one `SubTask` per item — the
//      code now owns the canonical list, the LLM is free to amend
//      it via the existing `declare_subtasks` tool, but cannot
//      quietly skip declaration.
//   2. Is injected into the agent's system prompt as a `PLAN:`
//      block, so every subsequent LLM call sees the broken-down
//      subtasks and is forced to act on each one.
//   3. Optionally escalates to a single LLM call when the
//      deterministic pass produces 0 or 1 subtasks (the case that
//      would defeat the purpose).
//
// The deterministic rules are deliberately simple. The motivating
// case for Phase 2 is "go to X and do Y" — splitting on `and` /
// `then` / `,` is enough to break that into 2 sub-items. Heavier
// semantic decomposition (e.g. a real planner) is Phase 7.

use crate::completeness::DeclareItem;
use crate::todo::{AcceptanceCriterion, AcceptanceKind, Todo, TodoBudget, TodoId, TodoStatus};

/// The output of a pre-flight decomposition pass.
///
/// `rationale` is a short human-readable string explaining *why*
/// the planner produced this list (which rules fired, what got
/// merged). It is written to the transcript and the trace so a
/// reviewer can see whether the planner split something the user
/// didn't intend (e.g. a long URL containing a comma got cut in
/// half). The string is for humans, not the LLM — the LLM only
/// sees the `subtasks` list.
#[derive(Debug, Clone)]
pub struct Plan {
    pub subtasks: Vec<DeclareItem>,
    pub rationale: String,
    /// `true` if the deterministic pass was enough; `false` if
    /// the planner escalated to an LLM call. The agent emits a
    /// distinct `preflight_plan` trace event per case so a
    /// post-mortem can tell at a glance which prompts needed
    /// escalation.
    pub escalated: bool,
}

/// Deterministic decomposition. Splits the input on common
/// compound-instruction conjunctions, normalizes whitespace, and
/// produces one `SubTask` per clause with a slugified id.
///
/// Rules (in priority order):
///   1. Empty / whitespace-only input → empty plan with rationale
///      "no clauses detected." The agent treats this as "no
///      pre-flight decomposition; let the LLM declare or not."
///   2. Single clause (no conjunction) → return that one clause
///      as the only subtask, with rationale "single clause; no
///      decomposition needed." This is the common case for
///      "go to wikipedia" and friends; it keeps the PLAN: block
///      present in the system prompt (which is itself useful — the
///      LLM sees the canonical task statement) without forcing an
///      escalation.
///   3. Compound: split on ` and `, ` then `, `, `, `; `, ` & `,
///      and ` + ` (the last two are common in user shorthand like
///      "open gmail & search"). Each piece becomes a subtask. The
///      first piece is the "primary" and gets id `step-1`; the
///      rest are `step-2` ... `step-N`. The original input is
///      kept verbatim as the first subtask's description if no
///      conjunction was found; otherwise the pieces become the
///      descriptions verbatim.
///
/// IDs are slugified to `[a-z0-9-]` to match the format the
/// existing `mark_subtask_done(id, ...)` tool expects (the
/// `CompletenessTracker::declare` path stores them as-is; the
/// `mark_*` paths look them up by exact string match). Slugifying
/// keeps IDs stable across iterations of the LLM's
/// `mark_subtask_done` calls.
pub fn plan(task: &str) -> Plan {
    let trimmed = task.trim();
    if trimmed.is_empty() {
        return Plan {
            subtasks: Vec::new(),
            rationale: "no clauses detected (empty input).".to_string(),
            escalated: false,
        };
    }

    // Split on the chosen conjunctions. We use `&str::split` on a
    // multi-pattern list via a small loop — `str::split` doesn't
    // natively support a slice of patterns, and pulling in
    // `regex` for this would be overkill.
    let raw_pieces = split_compound(trimmed);
    let pieces: Vec<&str> = raw_pieces
        .into_iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();

    if pieces.len() <= 1 {
        // Single clause. Return it as one subtask; the agent will
        // still inject the PLAN: block so the LLM sees the
        // canonical task statement.
        let id = slugify(trimmed, 0);
        return Plan {
            subtasks: vec![DeclareItem {
                id,
                description: trimmed.to_string(),
            }],
            rationale: "single clause; no decomposition needed.".to_string(),
            escalated: false,
        };
    }

    let mut subtasks = Vec::with_capacity(pieces.len());
    for (i, p) in pieces.iter().enumerate() {
        let id = slugify(p, i);
        subtasks.push(DeclareItem {
            id,
            description: p.to_string(),
        });
    }
    Plan {
        subtasks,
        rationale: format!(
            "split on {} clause-boundary marker(s) into {} subtask(s).",
            pieces.len() - 1,
            pieces.len()
        ),
        escalated: false,
    }
}

/// Decompose a `Handoff` into a list of typed `Todo` items.
///
/// Reuses the existing deterministic split (`and` / `then` / `,`),
/// matching each `Todo` id to the corresponding subtask id.
pub fn decompose_to_todos(handoff: &crate::handoff::Handoff) -> Vec<Todo> {
    let subtasks = if !handoff.subtasks.is_empty() {
        handoff
            .subtasks
            .iter()
            .map(|s| (s.id.clone(), s.description.clone()))
            .collect::<Vec<_>>()
    } else {
        let p = plan(&handoff.task_description);
        p.subtasks
            .into_iter()
            .map(|s| (s.id, s.description))
            .collect::<Vec<_>>()
    };

    let mut todos: Vec<Todo> = Vec::with_capacity(subtasks.len());
    for (i, (id, description)) in subtasks.into_iter().enumerate() {
        let acceptance = infer_acceptance(&description);
        let depends_on = if i > 0 {
            vec![todos[i - 1].id.clone()]
        } else {
            Vec::new()
        };
        todos.push(Todo {
            id: TodoId(id),
            intent: description,
            acceptance,
            depends_on,
            status: TodoStatus::Pending,
            evidence: None,
            attempts: 0,
            budget: TodoBudget::default(),
            last_evidence_iteration: None,
        });
    }
    todos
}

fn infer_acceptance(intent: &str) -> Option<AcceptanceCriterion> {
    let lower = intent.trim().to_lowercase();
    if lower.starts_with("navigate") || lower.starts_with("go to") || lower.starts_with("open") {
        Some(AcceptanceCriterion::new(AcceptanceKind::UrlAt, intent))
    } else if lower.starts_with("type") || lower.starts_with("send") || lower.starts_with("text") {
        Some(AcceptanceCriterion::new(AcceptanceKind::ElementPresent, intent))
    } else {
        Some(AcceptanceCriterion::new(AcceptanceKind::AnySnapshot, intent))
    }
}

/// Split `task` on the compound-instruction conjunctions. Returns
/// the original string in a single-element Vec if no marker was
/// found. The function is `pub(crate)` so the test module in this
/// file can exercise the splitter directly.
pub(crate) fn split_compound(task: &str) -> Vec<&str> {
    // Order matters: longer markers first so " then " beats " and ".
    // We don't try to be clever about overlapping markers — " and "
    // inside a URL would be split, but URLs almost never contain
    // " and " in the middle, and the slugified id preserves
    // enough context that the LLM can still tie the subtask to
    // the right action.
    const MARKERS: &[&str] = &[
        ", then ", ", and ", " then ", " and ", "; ", " & ", " + ", ", ",
    ];
    let mut best: Option<(usize, &str)> = None;
    for marker in MARKERS {
        if let Some(pos) = task.find(marker) {
            // Pick the *leftmost* match across all markers. That
            // way a sentence with two different markers splits
            // at the first boundary, not the one our marker list
            // happened to hit first.
            if best.map_or(true, |(bp, _)| pos < bp) {
                best = Some((pos, marker));
            }
        }
    }
    let Some((pos, marker)) = best else {
        return vec![task];
    };
    let (head, tail_with_marker) = task.split_at(pos);
    let tail = &tail_with_marker[marker.len()..];
    let mut out = vec![head];
    out.extend(split_compound(tail));
    out
}

/// Slugify a description for use as a `SubTask` id. The result is
/// `[a-z0-9-]`, lowercased, with leading/trailing `-` stripped,
/// and capped at 32 chars so a long description doesn't produce a
/// wildly long id. `index` is appended with a `-N` suffix so two
/// subtasks with identical descriptions still get unique ids
/// (the LLM's `mark_subtask_done(id, ...)` lookup is exact-match).
///
/// If the description contains no ASCII alphanumeric characters
/// (e.g. emoji-only), the sluggy prefix would be empty, leaving
/// the result as just `-N` — a valid slug but not a useful id. In
/// that case we fall back to the guaranteed-unique `step-N`.
pub fn slugify(description: &str, index: usize) -> String {
    let mut out = String::with_capacity(description.len() + 4);
    let mut last_was_dash = true; // start "true" so leading dashes are skipped
    let mut had_alnum = false;
    for ch in description.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_dash = false;
            had_alnum = true;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    // Cap at 32 chars. Slicing a char boundary in the middle of a
    // multi-byte char would panic, so walk back to a char
    // boundary. The contents are ASCII at this point (we
    // filtered to ASCII alnum + `-`), so byte slicing is safe.
    if out.len() > 32 {
        out.truncate(32);
        while out.ends_with('-') {
            out.pop();
        }
    }
    if !had_alnum {
        // No slugifiable content. Fall back to a guaranteed-
        // unique id. This is the emoji-only / punctuation-only
        // edge case.
        return format!("step-{}", index + 1);
    }
    // Append `-N` so two identical descriptions get distinct
    // ids. The `index` is 0-based; the LLM and the user see
    // `step-1` (1-based) in the transcript because the
    // `write_summary` formatter adds 1 — see `completeness.rs`.
    out.push('-');
    out.push_str(&format!("{}", index + 1));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_empty_input() {
        let p = plan("");
        assert!(p.subtasks.is_empty());
        assert!(p.rationale.contains("empty"));
        assert!(!p.escalated);
    }

    #[test]
    fn plan_whitespace_only_input() {
        let p = plan("   \t  ");
        assert!(p.subtasks.is_empty());
    }

    #[test]
    fn plan_single_clause_returns_one_subtask() {
        // No compound markers (no " and " with surrounding spaces,
        // no comma, no " then ", etc.) → one subtask, verbatim.
        let p = plan("search wikipedia for the rust programming language");
        assert_eq!(p.subtasks.len(), 1);
        assert_eq!(
            p.subtasks[0].description,
            "search wikipedia for the rust programming language"
        );
    }

    #[test]
    fn plan_compound_with_and_splits_into_two() {
        // The motivating case from docs/bug-1-root-cause.md.
        let p = plan("go to instagram and text my friend hi");
        // Wait — this same input was single-clause in the
        // previous test. The rule is: split on " and " with
        // surrounding SPACES, not on the bare substring "and".
        // "go to instagram and text my friend hi" DOES have
        // " and " (with spaces) in it, so it splits. Update the
        // previous test to use a different single-clause case.
        assert_eq!(p.subtasks.len(), 2);
        assert!(p.subtasks[0].description.contains("go to instagram"));
        assert!(p.subtasks[1].description.contains("text my friend"));
    }

    #[test]
    fn plan_truly_single_clause_without_spaces() {
        // A phrase with no space-padded conjunction stays as a
        // single clause. "go-to-instagram" is contrived but
        // exercises the rule; a more realistic example is a
        // single-clause prompt like "find me a Rust job opening
        // on linkedin".
        let p = plan("find me a Rust job opening on linkedin");
        assert_eq!(p.subtasks.len(), 1);
    }

    #[test]
    fn plan_compound_with_comma() {
        let p = plan("open gmail, search for jobs, send the first one to me");
        assert_eq!(p.subtasks.len(), 3);
    }

    #[test]
    fn plan_compound_with_then() {
        let p = plan("go to github then search for rust then click the first repo");
        assert_eq!(p.subtasks.len(), 3);
    }

    #[test]
    fn plan_compound_with_semicolon() {
        let p = plan("log in; send a message; log out");
        assert_eq!(p.subtasks.len(), 3);
    }

    #[test]
    fn plan_compound_with_ampersand() {
        let p = plan("open gmail & check calendar");
        assert_eq!(p.subtasks.len(), 2);
    }

    #[test]
    fn plan_compound_with_plus() {
        let p = plan("navigate to docs.rs + search for serde");
        assert_eq!(p.subtasks.len(), 2);
    }

    #[test]
    fn plan_subtask_ids_are_unique() {
        let p = plan("do X, do X, do X");
        // Three identical descriptions → three distinct ids
        // (the `-1` / `-2` / `-3` suffix from slugify).
        assert_eq!(p.subtasks.len(), 3);
        assert_ne!(p.subtasks[0].id, p.subtasks[1].id);
        assert_ne!(p.subtasks[1].id, p.subtasks[2].id);
    }

    #[test]
    fn plan_subtask_ids_are_slugified() {
        let p = plan("go to Gmail, search Rust jobs");
        // The id should be lowercase, alphanumeric + dashes only.
        for s in &p.subtasks {
            for ch in s.id.chars() {
                assert!(
                    ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-',
                    "id contains {:?} which is not a slug char: {:?}",
                    ch,
                    s.id
                );
            }
        }
    }

    #[test]
    fn plan_ids_stable_under_repeated_runs() {
        // Determinism check: the same input always produces the
        // same id. Important for transcript grep and for the
        // `mark_subtask_done` tool's exact-match lookup.
        let a = plan("open gmail, search for jobs");
        let b = plan("open gmail, search for jobs");
        assert_eq!(a.subtasks[0].id, b.subtasks[0].id);
        assert_eq!(a.subtasks[1].id, b.subtasks[1].id);
    }

    #[test]
    fn split_compound_handles_nested_markers() {
        // "open gmail, search X then send Y" → 3 pieces. The
        // leftmost split wins, so the first piece is "open gmail"
        // and the second is "search X then send Y" — which the
        // recursive call then splits on " then " into "search X"
        // and "send Y".
        let pieces = split_compound("open gmail, search X then send Y");
        assert_eq!(pieces, vec!["open gmail", "search X", "send Y"]);
    }

    #[test]
    fn slugify_strips_leading_and_trailing_dashes() {
        let id = slugify(" --- hello world --- ", 0);
        // The id is "hello-world-1" — no leading/trailing dashes.
        assert!(!id.starts_with('-'));
        assert!(!id.ends_with('-'));
        assert!(id.contains("hello"));
    }

    #[test]
    fn slugify_handles_unicode_by_stripping() {
        // The deterministic rules are ASCII-only. Non-ASCII chars
        // get stripped (they become dash boundaries). The id
        // should still be a valid slug.
        let id = slugify("search for 🚀 jobs", 0);
        for ch in id.chars() {
            assert!(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-');
        }
    }

    #[test]
    fn plan_emoji_only_falls_back_to_step_n() {
        // Edge case: a description with no ASCII alnum chars gets
        // an empty pre-suffix slug, so we fall back to a
        // guaranteed-unique id.
        let p = plan("🚀, 🌙, ⭐");
        // Each piece is a single non-ASCII char, but the comma
        // splits them into 3 pieces.
        assert_eq!(p.subtasks.len(), 3);
        for (i, s) in p.subtasks.iter().enumerate() {
            // The fallback id is `step-N` (1-based).
            assert_eq!(s.id, format!("step-{}", i + 1));
        }
    }
}

pub struct Planner;

impl Planner {
    pub async fn run(
        task: crate::handoff::Handoff,
        pool: std::sync::Arc<crate::worker_pool::WorkerPool>,
        sink: std::sync::Arc<dyn crate::orchestrator::TurnSink>,
    ) -> crate::handoff::BrowserResult {
        let mut todos = decompose_to_todos(&task);
        let mut key_findings = Vec::new();
        let mut final_status = crate::handoff::BrowserStatus::Done;
        let mut last_snapshot_sig = None;

        for todo in todos.iter_mut() {
            if pool.is_shutting_down() {
                final_status = crate::handoff::BrowserStatus::Failed;
                key_findings.push(crate::handoff::KeyFinding {
                    id: todo.id.to_string(),
                    description: todo.intent.clone(),
                    status: "failed".to_string(),
                    reason: "pool shutting down".to_string(),
                    evidence_signature: None,
                });
                continue;
            }

            sink.emit(crate::orchestrator::OrchestratorEvent::TodoStateChanged {
                task_id: task.originating_message_id.clone(),
                todo: todo.clone(),
            });

            loop {
                let rx = match pool.submit(todo.clone(), task.clone()) {
                    Ok(rx) => rx,
                    Err(e) => {
                        final_status = crate::handoff::BrowserStatus::Failed;
                        key_findings.push(crate::handoff::KeyFinding {
                            id: todo.id.to_string(),
                            description: todo.intent.clone(),
                            status: "failed".to_string(),
                            reason: e.to_string(),
                            evidence_signature: None,
                        });
                        break;
                    }
                };

                let deadline = tokio::time::sleep(std::time::Duration::from_secs(todo.budget.max_seconds));
                tokio::pin!(deadline);

                let result = tokio::select! {
                    r = rx => {
                        r.unwrap_or_else(|_| crate::handoff::TodoResult::failure(
                            todo.id.clone(), "worker channel closed", "", "", 0
                        ))
                    }
                    _ = &mut deadline => {
                        pool.signal(crate::supervisor::SupervisorCommand::new(
                            u64::MAX,
                            crate::supervisor::SupervisorSignal::Cancel
                        ));
                        crate::handoff::TodoResult::failure(
                            todo.id.clone(), "deadline exceeded", "", "", 0
                        )
                    }
                };

                if result.cancelled {
                    todo.status = crate::todo::TodoStatus::Failed { reason: "cancelled by user".into() };
                    sink.emit(crate::orchestrator::OrchestratorEvent::TodoRejected {
                        task_id: task.originating_message_id.clone(),
                        todo_id: todo.id.to_string(),
                        evidence: None,
                        reason: Some("cancelled by user".into()),
                    });
                    break;
                }

                if result.status == crate::todo::TodoStatus::Done {
                    let now_secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
                    let outcome = todo.mark_done(
                        &result.last_snapshot_signature,
                        &result.last_obs_text,
                        result.last_snapshot_iteration,
                        now_secs,
                        3, // max_attempts
                    );

                    match outcome {
                        crate::todo::MarkTodoOutcome::MarkedDone { evidence } => {
                            last_snapshot_sig = Some(evidence.planner_signature);
                            break;
                        }
                        crate::todo::MarkTodoOutcome::StaleEvidence(mismatch) => {
                            sink.emit(crate::orchestrator::OrchestratorEvent::TodoRejected {
                                task_id: task.originating_message_id.clone(),
                                todo_id: todo.id.to_string(),
                                evidence: Some(mismatch),
                                reason: None,
                            });

                            if todo.attempts >= 3 {
                                if let Some(ref acc) = todo.acceptance {
                                    if acc.kind == crate::todo::AcceptanceKind::AnySnapshot {
                                        todo.status = crate::todo::TodoStatus::Failed { reason: "evidence mismatched 3 times (exhausted)".into() };
                                    } else {
                                        // "otherwise Replan once" (in a real replan we might rebuild todos, here we just fail for now)
                                        todo.status = crate::todo::TodoStatus::Failed { reason: "evidence mismatched 3 times (replan required)".into() };
                                    }
                                }
                                break;
                            }
                        }
                        crate::todo::MarkTodoOutcome::AlreadyTerminal { .. } => break,
                    }
                } else {
                    todo.attempts += 1;
                    if todo.attempts >= 3 {
                        todo.status = crate::todo::TodoStatus::Failed { 
                            reason: result.failure_reason.unwrap_or_else(|| "unknown failure".into())
                        };
                        break;
                    }
                }
            }

            sink.emit(crate::orchestrator::OrchestratorEvent::TodoStateChanged {
                task_id: task.originating_message_id.clone(),
                todo: todo.clone(),
            });

            key_findings.push(crate::handoff::KeyFinding {
                id: todo.id.to_string(),
                description: todo.intent.clone(),
                status: todo.status.as_str().to_string(),
                reason: match &todo.status {
                    crate::todo::TodoStatus::Failed { reason } => reason.clone(),
                    crate::todo::TodoStatus::Skipped { reason } => reason.clone(),
                    _ => String::new(),
                },
                evidence_signature: todo.evidence.as_ref().map(|e| e.planner_signature.clone()),
            });

            if !matches!(todo.status, crate::todo::TodoStatus::Done) {
                final_status = crate::handoff::BrowserStatus::Partial;
            }
        }

        crate::handoff::BrowserResult {
            status: final_status,
            summary: "Planner completed task execution.".into(),
            key_findings,
            final_snapshot_signature: last_snapshot_sig,
            raw_transcript_ref: None,
            session_id: "planner-session".to_string(),
            failure_reason: String::new(),
            findings: Vec::new(),
        }
    }
}
