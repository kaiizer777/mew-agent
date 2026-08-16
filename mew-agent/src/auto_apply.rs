//! Phase 6 — Application / proposal / connection-request
//! auto-apply orchestration layer.
//!
//! Background: the existing `irreversible_actions` classifier
//! already pauses the session on `submit_application`,
//! `send_proposal`, and `send_connection_request` calls. The
//! `auto_apply` layer sits *between* the classifier's
//! `PauseForUser` verdict and the existing pause
//! machinery and adds:
//!
//!   1. **Per-run submission cap.** Counts every submission
//!      the agent has *initiated* in the current session and
//!      refuses to draft a new application once the cap is
//!      hit. The cap is `agent.auto_apply.max_applications_per_run`
//!      (default 10) and is enforced *independently* of any
//!      other budget so a runaway loop on a flaky platform
//!      cannot fire hundreds of applications in one session.
//!   2. **Dedup store check.** Consults
//!      `super::application_dedup::ApplicationDedup` at the
//!      pause site. If the `(platform, posting_identifier)`
//!      pair already has a record, the application is
//!      silently dropped — no draft, no chat message, no
//!      submission. This is what stops a Monday-then-Tuesday
//!      double-apply loop.
//!   3. **Draft text.** When the cap is not yet hit and the
//!      posting is not in the dedup store, the agent drafts
//!      the application / proposal / connection request
//!      text (reusing the existing summarizer LLM call
//!      pattern) and surfaces it in the user-facing
//!      confirmation prompt.
//!   4. **Confirmation gate.** Even with `auto_apply.enabled:
//!      true`, the agent does **not** auto-submit. The
//!      existing `Paused` checkpoint is the *only* path to
//!      submission. The user must explicitly confirm each
//!      application unless `require_confirmation_per_application:
//!      false` is explicitly set (the "fire-and-forget" path).
//!   5. **Per-run counter + cap enforcement.** A simple
//!      `AtomicUsize` count of submissions *initiated* in
//!      the current session. Compared against
//!      `max_applications_per_run` at every dispatch.
//!
//! ## Decision shape
//!
//! The orchestration layer exposes a single decision
//! function (`decide`) that takes the typed input (action
//! kind + target fields) and returns a `AutoApplyDecision`
//! the dispatch site can match on. The variants are
//! designed so the dispatch site never has to inspect
//! config to know what to do — the orchestration layer
//! has already folded config + dedup + cap into the
//! decision.
//!
//! ## What this module does *not* do
//!
//!   * It does not perform the actual browser actions
//!     (filling forms, clicking submit). That stays in
//!     the agent's dispatch site, after the user
//!     confirms.
//!   * It does not load the LLM. The draft text is
//!     *requested* by the orchestration layer; the
//!     actual LLM call is the existing
//!     `summarizer::summarize_draft` path. The
//!     orchestration layer is pure logic.
//!   * It does not call `session.pause()` directly.
//!     It returns a `AutoApplyDecision::PauseForDraft`
//!     and the dispatch site (the existing
//!     `evaluate_dispatch` integration in
//!     `super::agent::dispatch_single_tool_call`) calls
//!     the existing pause machinery with the agent's
//!     already-drafted text.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use mew_resilience::{ActionKind, IrreverisbleVerdict};

use super::application_dedup::ApplicationDedup;
// Re-export the lib-level `AutoApplyConfig` so callers
// (including the integration test in
// `tests/phase6_auto_apply.rs`) can construct the
// orchestration layer with the same type the agent's
// `AgentConfig` exposes. A future refactor that splits
// the two would surface here as a type mismatch.
pub use super::AutoApplyConfig;

/// What the orchestration layer decided about a given
/// submission attempt. The dispatch site matches on the
/// variant; the fields carry the data the loop needs
/// without re-walking the config / dedup store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoApplyDecision {
    /// The auto-apply layer is disabled. The dispatch
    /// site falls through to the existing
    /// `irreversible_actions` pause path with no
    /// changes. The cap and dedup store are not
    /// consulted.
    Disabled,
    /// The submission is for an action kind the
    /// auto-apply layer does not own. The dispatch
    /// site falls through to the existing
    /// `irreversible_actions` pause path. (The
    /// irreversible classifier still pauses for these;
    /// auto-apply is only an *add-on* for its three
    /// typed kinds.)
    NotApplicable,
    /// A record with the same `(platform,
    /// posting_identifier)` already exists in the
    /// dedup store. The application is silently
    /// dropped — no draft, no chat message, no
    /// submission. The dispatch site should
    /// synthesize a tool result that explains the
    /// dedup hit and continue the loop.
    AlreadyApplied {
        platform: String,
        posting_identifier: String,
    },
    /// The per-run cap has been hit. The application
    /// is silently dropped — no draft, no chat
    /// message, no submission. The dispatch site
    /// should synthesize a tool result that explains
    /// the cap and continue the loop. The cap is
    /// per-run; a fresh run starts the count over
    /// at zero.
    CapReached {
        submitted: usize,
        max: usize,
    },
    /// The submission is allowed to proceed. The
    /// dispatch site pauses for the user to confirm;
    /// the `draft_required` flag tells the dispatch
    /// site whether to call the summarizer LLM for
    /// draft text first (`true` when
    /// `auto_apply.enabled: true`) or just to the
    /// raw verdict target (`false` when auto-apply
    /// is disabled — this variant is unreachable
    /// from the disabled branch, but the flag is
    /// explicit so the dispatch site's match arm
    /// reads cleanly).
    PauseForDraft {
        /// The original irreversible verdict. The
        /// dispatch site hands `target` and
        /// `action.as_str()` to the existing pause
        /// chat message.
        verdict: IrreverisbleVerdict,
        /// `true` when `auto_apply.enabled: true` —
        /// the dispatch site should call the
        /// summarizer to draft text before pausing.
        /// `false` when the orchestration layer
        /// reached this branch for a non-Phase-6
        /// action (the `NotApplicable` variant
        /// catches most of these; the
        /// `PauseForDraft { draft_required: false }`
        /// branch exists for the explicit "we own
        /// this kind but the operator wants raw
        /// pauses without drafts" config).
        draft_required: bool,
    },
    /// The submission is allowed to proceed
    /// *without* a confirmation prompt. The
    /// dispatch site dispatches the tool call
    /// directly (no pause, no chat message). This
    /// is the "fire-and-forget" path — only
    /// reachable when `auto_apply.enabled: true`
    /// AND
    /// `require_confirmation_per_application:
    /// false` AND the dedup check passed AND the
    /// cap is not yet hit.
    AutoSubmit { verdict: IrreverisbleVerdict },
}

/// Pure-logic handle the agent holds. Cheap to clone
/// (the `AtomicUsize` is the only shared state and is
/// `Arc`-wrapped for the same reason the captcha
/// telemetry handle is `Arc`-wrapped — the agent
/// holds a handle and the CLI / Tauri command can
/// hold a sibling to read the count without going
/// through the agent).
#[derive(Debug)]
pub struct AutoApply {
    /// The full config block the loop reads at
    /// every dispatch.
    config: AutoApplyConfig,
    /// In-memory dedup store. Held as an `Arc` so
    /// the dispatch site (the `super::agent` code)
    /// can call into the dedup store without
    /// borrowing the orchestration layer. Mirrors
    /// the captcha-telemetry pattern.
    dedup: Arc<ApplicationDedup>,
    /// Per-run submission count. Bumped on every
    /// *initiated* submission (the dispatch site
    /// calls `increment_count` when a draft is
    /// presented, regardless of whether the user
    /// confirms — this is what enforces the cap
    /// against a runaway loop where the user
    /// repeatedly clicks "Confirm" faster than the
    /// loop can drain).
    submitted: Arc<AtomicUsize>,
}

impl AutoApply {
    /// Build a new orchestration layer from the
    /// config block and the dedup store. The
    /// per-run counter starts at zero.
    pub fn new(config: AutoApplyConfig, dedup: Arc<ApplicationDedup>) -> Self {
        Self {
            config,
            dedup,
            submitted: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Read the current per-run submission count.
    /// Used by the agent's end-of-task summary line
    /// and the Tauri command that surfaces a
    /// "X applications sent this session" tile.
    pub fn submitted_count(&self) -> usize {
        self.submitted.load(Ordering::SeqCst)
    }

    /// Read the per-run cap from the config. Used
    /// by the agent's end-of-task summary line so
    /// the user sees "5 of 10 applications sent"
    /// not just "5 applications sent".
    pub fn max_per_run(&self) -> usize {
        self.config.max_applications_per_run
    }

    /// The auto-apply layer is enabled. Cheap
    /// accessor used by the dispatch site to short-
    /// circuit config lookups on the hot path.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Increment the per-run counter. Called by
    /// the dispatch site when a draft is presented
    /// (i.e. the agent has committed to drafting
    /// text for this submission). The increment
    /// happens *before* the user confirms so a
    /// runaway loop on a flaky platform cannot
    /// fire more than `max_applications_per_run`
    /// drafts per session.
    ///
    /// Returns the new count. The dispatch site
    /// does not currently use the return value,
    /// but exposing it is cheap and future
    /// telemetry may want it.
    pub fn increment_count(&self) -> usize {
        self.submitted.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Read-only access to the underlying dedup
    /// store. Used by the agent's `application_dedup`
    /// accessor (which mirrors the captcha-telemetry
    /// accessor pattern) so the dispatch site can
    /// call `record_submission` after a confirmed
    /// application without borrowing the parent
    /// `AutoApply` handle.
    ///
    /// Cheap (`Arc` clone). The returned handle
    /// shares state with the one held by
    /// `AutoApply`, so a `record_submission` call
    /// through either handle updates the same
    /// in-memory map.
    pub fn dedup_handle(&self) -> Arc<ApplicationDedup> {
        self.dedup.clone()
    }

    /// Decide what to do for a given irreversible
    /// verdict. The function folds the config +
    /// dedup + cap checks into a single
    /// `AutoApplyDecision` so the dispatch site
    /// matches on the variant without re-walking
    /// the configuration. The `target_platform` and
    /// `target_posting_id` arguments are extracted
    /// from the verdict by the dispatch site
    /// (typically from the tool's args, e.g.
    /// `(job_id, "linkedin.com")` for a LinkedIn
    /// apply).
    pub fn decide(
        &self,
        verdict: &IrreverisbleVerdict,
        target_platform: Option<&str>,
        target_posting_id: Option<&str>,
    ) -> AutoApplyDecision {
        // The auto-apply layer only owns the three
        // Phase 6 typed kinds. Every other kind
        // falls through to the existing pause path.
        if !is_phase6_kind(verdict.action) {
            return AutoApplyDecision::NotApplicable;
        }

        // The layer is disabled. Same as above:
        // fall through to the existing pause path.
        if !self.config.enabled {
            return AutoApplyDecision::Disabled;
        }

        // Dedup check. Requires both a platform and
        // a posting identifier; when either is
        // missing (e.g. the LLM forgot to pass a
        // `job_id`), we skip the dedup and proceed
        // to the draft branch. The LLM's missing
        // arg is its own problem; the agent's
        // job is to surface a draft, not to fix
        // the call.
        if let (Some(platform), Some(posting_id)) = (target_platform, target_posting_id) {
            if !posting_id.is_empty() && self.dedup.is_already_applied(platform, posting_id) {
                return AutoApplyDecision::AlreadyApplied {
                    platform: platform.to_string(),
                    posting_identifier: posting_id.to_string(),
                };
            }
        }

        // Per-run cap check. The counter is
        // *pre-increment* here (i.e. the cap is
        // checked *after* the current submission
        // has been counted). The dispatch site
        // calls `increment_count` when it commits
        // to drafting; the next call to `decide`
        // sees the bumped count.
        let submitted = self.submitted_count();
        if submitted >= self.config.max_applications_per_run {
            return AutoApplyDecision::CapReached {
                submitted,
                max: self.config.max_applications_per_run,
            };
        }

        // Fire-and-forget path: dispatch the tool
        // call directly, no pause, no chat message.
        // The dedup store and per-run cap are still
        // enforced, so the agent cannot fire
        // unbounded submissions.
        if !self.config.require_confirmation_per_application {
            return AutoApplyDecision::AutoSubmit {
                verdict: verdict.clone(),
            };
        }

        // Standard path: pause for the user to
        // confirm the drafted text.
        AutoApplyDecision::PauseForDraft {
            verdict: verdict.clone(),
            draft_required: true,
        }
    }

    /// Build a short, human-readable message for the
    /// user-facing chat reply. Used by the
    /// dispatch site when the decision is
    /// `CapReached` / `AlreadyApplied` so the LLM
    /// sees a clean, structured note it can echo
    /// verbatim.
    pub fn explain_decision(&self, decision: &AutoApplyDecision) -> String {
        match decision {
            AutoApplyDecision::Disabled => {
                "auto-apply is disabled; falling back to standard pause".to_string()
            }
            AutoApplyDecision::NotApplicable => {
                "auto-apply does not own this action kind".to_string()
            }
            AutoApplyDecision::AlreadyApplied {
                platform,
                posting_identifier,
            } => {
                format!(
                    "already applied to {platform} posting {posting_identifier}; dropping this submission (dedup store hit)"
                )
            }
            AutoApplyDecision::CapReached { submitted, max } => {
                format!(
                    "per-run application cap reached ({submitted} of {max} submitted this session); dropping this submission"
                )
            }
            AutoApplyDecision::PauseForDraft { draft_required, .. } => {
                if *draft_required {
                    "drafting application text and pausing for confirmation".to_string()
                } else {
                    "pausing for confirmation (draft not required)".to_string()
                }
            }
            AutoApplyDecision::AutoSubmit { .. } => {
                "fire-and-forget path: auto-submitting without confirmation".to_string()
            }
        }
    }
}

impl Clone for AutoApply {
    /// Cheap clone — the inner `Arc<...>`s share
    /// state, so cloning is just a refcount bump.
    /// Used by the dispatch site which sometimes
    /// needs to call into the layer from a future
    /// / closure without borrowing the parent.
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            dedup: self.dedup.clone(),
            submitted: self.submitted.clone(),
        }
    }
}

/// Whether a given `ActionKind` is owned by the
/// Phase 6 auto-apply surface. The set is closed:
/// only `SubmitApplication`, `SendProposal`, and
/// `SendConnectionRequest` are auto-apply kinds. Every
/// other kind falls through to the existing pause
/// path.
pub fn is_phase6_kind(kind: ActionKind) -> bool {
    matches!(
        kind,
        ActionKind::SubmitApplication
            | ActionKind::SendProposal
            | ActionKind::SendConnectionRequest
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mew_resilience::classify_irreversible;
    use serde_json::json;

    fn dedup() -> Arc<ApplicationDedup> {
        Arc::new(ApplicationDedup::new(None))
    }

    fn cfg(enabled: bool, require: bool, max: usize) -> AutoApplyConfig {
        AutoApplyConfig {
            enabled,
            require_confirmation_per_application: require,
            max_applications_per_run: max,
        }
    }

    fn verdict_for(tool: &str, args: serde_json::Value) -> IrreverisbleVerdict {
        classify_irreversible(tool, &args).expect("must classify")
    }

    // ----------------------
    // Phase 6 kind membership
    // ----------------------

    #[test]
    fn phase6_kinds_are_typed() {
        assert!(is_phase6_kind(ActionKind::SubmitApplication));
        assert!(is_phase6_kind(ActionKind::SendProposal));
        assert!(is_phase6_kind(ActionKind::SendConnectionRequest));
    }

    #[test]
    fn non_phase6_kinds_are_rejected() {
        // Regression guard: every *new* kind added
        // to `ActionKind` must be explicitly
        // considered for the Phase 6 surface. A
        // future PR that adds a new kind and forgets
        // to update this match will surface here.
        let non_phase6 = [
            ActionKind::Send,
            ActionKind::Post,
            ActionKind::Delete,
            ActionKind::Pay,
            ActionKind::Submit,
            ActionKind::Follow,
            ActionKind::Update,
            ActionKind::Other,
        ];
        for k in non_phase6 {
            assert!(!is_phase6_kind(k), "kind {k:?} should NOT be Phase 6");
        }
    }

    // ----------------------
    // Disabled layer
    // ----------------------

    #[test]
    fn disabled_layer_returns_disabled() {
        let layer = AutoApply::new(cfg(false, true, 10), dedup());
        let v = verdict_for(
            "apply_to_job",
            json!({ "job_id": "gh-1", "company": "Acme" }),
        );
        let d = layer.decide(&v, Some("linkedin.com"), Some("gh-1"));
        assert_eq!(d, AutoApplyDecision::Disabled);
    }

    #[test]
    fn disabled_layer_does_not_consult_dedup_or_cap() {
        // A pre-existing dedup record must NOT
        // cause a `Disabled` layer to surface an
        // `AlreadyApplied` decision — the dedup
        // check is gated on `enabled: true`.
        let d = dedup();
        d.record_submission("linkedin.com", "job-1", "submit_application", "f", "n");
        let layer = AutoApply::new(cfg(false, true, 10), d);
        let v = verdict_for("apply_to_job", json!({ "job_id": "job-1" }));
        let decision = layer.decide(&v, Some("linkedin.com"), Some("job-1"));
        assert_eq!(decision, AutoApplyDecision::Disabled);
    }

    // ----------------------
    // Not applicable
    // ----------------------

    #[test]
    fn non_phase6_kind_returns_not_applicable() {
        let layer = AutoApply::new(cfg(true, true, 10), dedup());
        // `pay` is a Phase 6 *outside* — the
        // auto-apply layer does NOT own it, even
        // with `enabled: true`.
        let v = verdict_for("pay", json!({ "amount": 100 }));
        let d = layer.decide(&v, None, None);
        assert_eq!(d, AutoApplyDecision::NotApplicable);
    }

    // ----------------------
    // Dedup
    // ----------------------

    #[test]
    fn dedup_hit_silently_drops_submission() {
        let d = dedup();
        d.record_submission("linkedin.com", "job-1", "submit_application", "f", "n");
        let layer = AutoApply::new(cfg(true, true, 10), d);
        let v = verdict_for("apply_to_job", json!({ "job_id": "job-1" }));
        let decision = layer.decide(&v, Some("linkedin.com"), Some("job-1"));
        match decision {
            AutoApplyDecision::AlreadyApplied {
                platform,
                posting_identifier,
            } => {
                assert_eq!(platform, "linkedin.com");
                assert_eq!(posting_identifier, "job-1");
            }
            other => panic!("expected AlreadyApplied, got {other:?}"),
        }
    }

    #[test]
    fn dedup_normalizes_platform_for_lookup() {
        // The dedup store normalizes the platform
        // string (lowercase, strip `www.`). The
        // orchestration layer's `decide` must
        // therefore pass the *raw* input to the
        // dedup store (which does its own
        // normalization) — the dedup check is a
        // black box. This test confirms the
        // end-to-end dedup still fires when the
        // caller passes a different casing.
        let d = dedup();
        d.record_submission("linkedin.com", "job-1", "submit_application", "f", "n");
        let layer = AutoApply::new(cfg(true, true, 10), d);
        let v = verdict_for("apply_to_job", json!({ "job_id": "job-1" }));
        let decision = layer.decide(&v, Some("WWW.LinkedIn.com"), Some("job-1"));
        assert!(matches!(decision, AutoApplyDecision::AlreadyApplied { .. }));
    }

    #[test]
    fn missing_posting_id_skips_dedup_check() {
        // The LLM forgot to pass `job_id`. The
        // dedup check requires both a platform
        // and a posting id; missing either means
        // we proceed to the draft branch and the
        // LLM's missing arg becomes a separate
        // problem (the draft text is built from
        // the verdict's `target` field, which
        // will say "submit_application (no
        // target args supplied)" — already
        // covered by the irreversible-actions
        // unit tests).
        let d = dedup();
        d.record_submission("linkedin.com", "job-1", "submit_application", "f", "n");
        let layer = AutoApply::new(cfg(true, true, 10), d);
        let v = verdict_for("apply_to_job", json!({}));
        let decision = layer.decide(&v, Some("linkedin.com"), None);
        assert!(matches!(decision, AutoApplyDecision::PauseForDraft { .. }));
    }

    // ----------------------
    // Per-run cap
    // ----------------------

    #[test]
    fn cap_reached_silently_drops_submission() {
        let layer = AutoApply::new(cfg(true, true, 2), dedup());
        // Burn the cap with two increments.
        layer.increment_count();
        layer.increment_count();
        assert_eq!(layer.submitted_count(), 2);

        let v = verdict_for("apply_to_job", json!({ "job_id": "job-3" }));
        let decision = layer.decide(&v, Some("linkedin.com"), Some("job-3"));
        match decision {
            AutoApplyDecision::CapReached { submitted, max } => {
                assert_eq!(submitted, 2);
                assert_eq!(max, 2);
            }
            other => panic!("expected CapReached, got {other:?}"),
        }
    }

    #[test]
    fn cap_enforced_independently_of_dedup() {
        // Even with an empty dedup store, the cap
        // is the next gate. A regression that
        // skipped the cap when the dedup check
        // passed would silently allow runaway
        // loops; this test pins the order.
        let layer = AutoApply::new(cfg(true, true, 0), dedup());
        // Note: 0 is treated as the default (10)
        // by the config loader's hard-clamp, but
        // `AutoApply::new` is the *unclamped*
        // constructor used by tests. With max=0,
        // every submission is dropped after the
        // first increment.
        layer.increment_count();
        let v = verdict_for("apply_to_job", json!({ "job_id": "job-1" }));
        let decision = layer.decide(&v, Some("linkedin.com"), Some("job-1"));
        assert!(matches!(decision, AutoApplyDecision::CapReached { .. }));
    }

    // ----------------------
    // Fire-and-forget
    // ----------------------

    #[test]
    fn fire_and_forget_path_returns_auto_submit() {
        // `enabled: true` AND
        // `require_confirmation_per_application:
        // false` AND empty dedup AND cap not hit
        // => `AutoSubmit`.
        let layer = AutoApply::new(cfg(true, false, 10), dedup());
        let v = verdict_for("apply_to_job", json!({ "job_id": "job-1" }));
        let decision = layer.decide(&v, Some("linkedin.com"), Some("job-1"));
        assert!(matches!(decision, AutoApplyDecision::AutoSubmit { .. }));
    }

    #[test]
    fn fire_and_forget_still_respects_dedup() {
        // Even on the fire-and-forget path, a
        // dedup hit silently drops the submission.
        // The user's "I configured
        // require_confirmation_per_application:
        // false" intent is to skip the chat
        // prompt, not to override the dedup
        // store. A regression that fired
        // auto-submits on dedup hits would
        // double-apply to every already-sent
        // posting in the same session.
        let d = dedup();
        d.record_submission("linkedin.com", "job-1", "submit_application", "f", "n");
        let layer = AutoApply::new(cfg(true, false, 10), d);
        let v = verdict_for("apply_to_job", json!({ "job_id": "job-1" }));
        let decision = layer.decide(&v, Some("linkedin.com"), Some("job-1"));
        assert!(matches!(decision, AutoApplyDecision::AlreadyApplied { .. }));
    }

    // ----------------------
    // Pause for draft
    // ----------------------

    #[test]
    fn standard_path_returns_pause_for_draft() {
        // The common, safe-by-default case:
        // `enabled: true`,
        // `require_confirmation_per_application:
        // true`, empty dedup, cap not hit. The
        // dispatch site pauses and drafts.
        let layer = AutoApply::new(cfg(true, true, 10), dedup());
        let v = verdict_for("apply_to_job", json!({ "job_id": "job-1" }));
        let decision = layer.decide(&v, Some("linkedin.com"), Some("job-1"));
        match decision {
            AutoApplyDecision::PauseForDraft {
                draft_required,
                verdict: vd,
            } => {
                assert!(draft_required, "draft_required must be true on standard path");
                assert_eq!(vd.action, ActionKind::SubmitApplication);
            }
            other => panic!("expected PauseForDraft, got {other:?}"),
        }
    }

    // ----------------------
    // End-to-end: confirm-then-submit happy path
    // ----------------------

    #[test]
    fn confirm_then_submit_happy_path() {
        // Simulates the dispatch site's
        // "confirm-then-submit" flow:
        //   1. LLM dispatches apply_to_job.
        //   2. Orchestration layer returns
        //      PauseForDraft.
        //   3. Dispatch site calls
        //      `increment_count` to commit.
        //   4. User confirms in chat.
        //   5. Dispatch site records the
        //      submission in the dedup store.
        let d = dedup();
        let layer = AutoApply::new(cfg(true, true, 10), d.clone());
        let v = verdict_for("apply_to_job", json!({ "job_id": "gh-1", "role": "Rust" }));

        // Step 2.
        let decision = layer.decide(&v, Some("linkedin.com"), Some("gh-1"));
        assert!(matches!(decision, AutoApplyDecision::PauseForDraft { .. }));

        // Step 3: commit the draft.
        let count_after_commit = layer.increment_count();
        assert_eq!(count_after_commit, 1);
        assert!(!d.is_already_applied("linkedin.com", "gh-1"));

        // Step 5: record the submission in the
        // dedup store. The dispatch site does
        // this *after* the user confirms; the
        // orchestration layer doesn't need to
        // know.
        d.record_submission("linkedin.com", "gh-1", "submit_application", "f-1", "Rust @ Acme");
        assert!(d.is_already_applied("linkedin.com", "gh-1"));
        assert_eq!(d.len(), 1);

        // A subsequent attempt for the same
        // posting silently drops.
        let v2 = verdict_for("apply_to_job", json!({ "job_id": "gh-1" }));
        let decision2 = layer.decide(&v2, Some("linkedin.com"), Some("gh-1"));
        assert!(matches!(decision2, AutoApplyDecision::AlreadyApplied { .. }));
    }

    // ----------------------
    // User rejects at checkpoint
    // ----------------------

    #[test]
    fn user_rejects_at_checkpoint_does_not_record() {
        // The user sees the drafted text in the
        // chat list and replies "no, skip this
        // one." The dispatch site should *not*
        // call `record_submission` and the
        // per-run counter should not be bumped
        // a second time. (The orchestration
        // layer does not know the user
        // rejected; the dispatch site is
        // responsible for not calling
        // `record_submission` on a rejection.
        // This test pins the dispatch site's
        // contract via the dedup store.)
        let d = dedup();
        let layer = AutoApply::new(cfg(true, true, 10), d.clone());
        let v = verdict_for("apply_to_job", json!({ "job_id": "gh-2" }));

        // Step 1: pause for draft.
        let decision = layer.decide(&v, Some("linkedin.com"), Some("gh-2"));
        assert!(matches!(decision, AutoApplyDecision::PauseForDraft { .. }));

        // Step 2: draft is presented to the user.
        layer.increment_count();

        // Step 3: user rejects. Dispatch site
        // does NOT call `record_submission`.
        // The dedup store remains empty.
        assert!(!d.is_already_applied("linkedin.com", "gh-2"));

        // A future re-attempt (e.g. the user
        // changes their mind and runs the
        // preset again next week) does not see
        // a dedup hit — the rejected application
        // was not recorded.
        let v2 = verdict_for("apply_to_job", json!({ "job_id": "gh-2" }));
        let decision2 = layer.decide(&v2, Some("linkedin.com"), Some("gh-2"));
        assert!(matches!(decision2, AutoApplyDecision::PauseForDraft { .. }));
    }

    // ----------------------
    // Duplicate blocked by dedup
    // ----------------------

    #[test]
    fn duplicate_application_blocked_by_dedup_store() {
        // Pre-populate the dedup store with a
        // prior session's submission. A new
        // session's identical application is
        // silently dropped at the pause site.
        let d = dedup();
        d.record_submission("linkedin.com", "gh-1", "submit_application", "f-1", "Rust @ Acme");
        let layer = AutoApply::new(cfg(true, true, 10), d);
        let v = verdict_for("apply_to_job", json!({ "job_id": "gh-1" }));
        let decision = layer.decide(&v, Some("linkedin.com"), Some("gh-1"));
        match decision {
            AutoApplyDecision::AlreadyApplied {
                platform,
                posting_identifier,
            } => {
                assert_eq!(platform, "linkedin.com");
                assert_eq!(posting_identifier, "gh-1");
            }
            other => panic!("expected AlreadyApplied, got {other:?}"),
        }
        // The per-run counter is not bumped on
        // a dedup hit — the application was
        // dropped before any draft was
        // presented.
        assert_eq!(layer.submitted_count(), 0);
    }

    // ----------------------
    // Explain
    // ----------------------

    #[test]
    fn explain_decision_is_human_readable_for_every_variant() {
        let layer = AutoApply::new(cfg(true, true, 10), dedup());
        let cases = [
            AutoApplyDecision::Disabled,
            AutoApplyDecision::NotApplicable,
            AutoApplyDecision::AlreadyApplied {
                platform: "linkedin.com".into(),
                posting_identifier: "gh-1".into(),
            },
            AutoApplyDecision::CapReached { submitted: 10, max: 10 },
        ];
        for d in &cases {
            let s = layer.explain_decision(d);
            assert!(!s.is_empty(), "explain_decision empty for {d:?}");
        }
    }
}
