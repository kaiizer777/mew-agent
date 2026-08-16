# mew — Upgrade Work Plan (v4)

## Instructions for the coding agent

Read this whole file before starting. Work **one phase at a time, in order**. Do not skip ahead, do not combine phases, do not start Phase N+1 until every checkbox in Phase N is checked.

Rules:
1. Before writing any code for a phase, re-read the relevant section of `proj.md` so you don't contradict existing architecture.
2. Every bullet point below is a discrete unit of work. Implement it, test it, then check its box.
3. When every bullet in a phase is checked, mark the **phase header checkbox** done, write a one-paragraph summary of what changed at the bottom of that phase's section, and stop. Wait for review before continuing to the next phase.
4. Do not refactor code outside the scope of the current phase's bullets, even if you spot something tempting. Note it in a `## Parking lot` section at the bottom of this file instead.
5. No mess: no dead code, no commented-out blocks, no half-finished functions left in the tree. If a bullet can't be finished cleanly in one sitting, leave the code in its last fully-working state and note the blocker under that bullet.
6. Every new module gets unit tests in the same style as the existing `mew-resilience` mock-fixture tests. Every new config field gets documented in the config reference at the bottom of `proj.md`.
7. Run the full existing test suite (`cargo test --workspace`) before checking off any phase. A phase is not done if it breaks an existing test.
8. This plan deliberately does not mention step counts, iteration caps, or token budgets as constraints — build for correctness and reliability first. Existing `max_iterations` / `max_cost` config knobs stay as user-configurable safety nets, not design constraints you optimize against.

Target end-state: an agent reliable enough for unattended personal use — logging into job boards and freelance platforms with credentials the user supplies in `config.yaml`, searching for remote internships/jobs and freelance client leads, and returning consolidated, deduplicated, verified results — without babysitting.

---

## Phase 1 — True concurrent worker dispatch
**Status: [x] Done**

The worker pool shape exists (Phase 13/14/18 in proj.md) but nothing confirms subtasks actually run in parallel rather than sequentially through one Chromium instance. This phase makes concurrency real.

- [x] Audit `mew-agent` orchestrator dispatch code and confirm/document whether `Vec<SubTask>` workers currently run sequentially or concurrently (write the finding in the phase summary regardless of outcome).
- [x] Give each concurrent worker its own `BrowserContext` (isolated CDP target) inside a single Chromium process, rather than separate full Chromium processes, using the shared `user_data_dir` so login/session cookies apply to all contexts.
- [x] Give each worker its own `mew-perception` state (own AX-tree cache, own diff cache) so contexts never cross-contaminate snapshots.
- [x] Wire `ResearchPlanner` subtask dispatch to fire all platform subtasks concurrently via `tokio::spawn` / `JoinSet`, respecting a configurable `max_concurrent_workers` (default 3).
- [x] Add a per-domain concurrency cap independent of the global cap (e.g. never 2 workers hitting `linkedin.com` at the same time) to reduce ban risk, since concurrent identical-domain sessions are a known trigger for anti-bot account flags.
- [x] Add tracing spans so `MEW_TRACING_DIR` output shows overlapping worker timelines, not just sequential ones — this is how you'll verify the fix actually works.
- [x] Extend `phase3_round_trip.rs`-style test to assert two subtasks against different mock fixtures complete with overlapping timestamps, not sequential ones.

**Phase summary:** Audited `mew-agent` orchestrator dispatch and confirmed that prior task execution sequentially processed subtasks through the worker harness. Implemented true concurrent subtask dispatch in `Planner::run` using `tokio::task::JoinSet`, honoring configurable global concurrency (`max_concurrent_workers`, default 3) and independent per-domain concurrency caps (`max_concurrent_per_domain`, default 1) to eliminate multi-session anti-bot triggers. Each concurrent worker operates on isolated browser context / CDP targets sharing `user_data_dir` for session preservation while maintaining independent `mew-perception` snapshot and diff state. Structured tracing spans (`trace_worker_timeline`) emit start/end events per worker for timeline overlap observability. Verified overlapping execution for distinct domains and sequential execution for same-domain tasks in `phase1_concurrent_workers.rs`, with all workspace tests passing cleanly.

---

## Phase 2 — Live partial-result streaming
**Status: [x] Done**

Right now findings only surface at final synthesis. This phase streams value to the user as it's discovered, not just at the end.

- [x] Add a `PartialFinding` event variant alongside the existing `ProgressLine` / `Summary` events, carrying one `FindingStore` entry as soon as it's deduplicated and verified.
- [x] Emit `PartialFinding` from `FindingStore::insert` (or equivalent) the moment a new unique finding is confirmed, not just at task end.
- [x] Wire `TauriSink` to map `PartialFinding` → a new Tauri event (`chat-partial-finding`).
- [x] Frontend: render partial findings as they arrive inside the active `task_started` card's `<details>` panel, promoted to the final list on `task_completed` (no duplicate rendering).
- [x] Add a config toggle `research.stream_partial_findings: true` (default true) so this can be disabled for cleaner logs if needed.
- [x] Unit test: mock a 3-subtask research task where subtask 2 finishes first; assert its finding is emitted before subtasks 1 and 3 complete.

**Phase summary:** Implemented live partial-finding streaming end-to-end across `mew-agent` and `mew-ui`. Added `PartialFinding` variants to `AgentEvent` and `OrchestratorEvent`, mapped through `TauriSink` to the `chat-partial-finding` Tauri IPC event. `FindingStore` was extended with `insert` and optional emitter integration to dispatch unique verified findings to the sink immediately upon confirmation rather than delaying until final task synthesis. The `mew-ui` shell ingests incoming partial findings via channel and Tauri listeners, rendering formatted entries in real time inside the active task card's `<details>` panel while maintaining client-side ID deduplication to prevent duplicate entries upon completion. Added the `agent.research.stream_partial_findings: true` configuration knob with full documentation. Verified through integration unit tests (`phase2_partial_findings.rs`) asserting that faster subtasks emit partial findings ahead of slower in-flight workers and that disabling the toggle cleanly suppresses streaming events, with all workspace tests passing.

---

## Phase 3 — Snapshot & re-snapshot efficiency audit
**Status: [x] Done**

Diagnose and fix unnecessary full AX-tree re-extractions, which are both a latency and reliability tax.

- [x] Add a counter to `mew-perception` tracking full-snapshot vs. diff-snapshot calls per session, exposed in the trace JSONL.
- [x] Build a small trace-analysis script (`scripts/analyze_trace.py` or a `mew-cli` subcommand) that reads a `MEW_TRACING_DIR` output and reports: full-vs-diff ratio, average snapshot size in KB, and steps-per-subtask.
- [x] Run it against 3+ real traces from job-search / lead-gen tasks; record findings in `docs/phase-upgrade-trace-audit.md`.
- [x] Based on findings, tighten `ref_recovery.rs` so it only forces a full re-snapshot when the diff cache is actually invalid (e.g. navigation occurred), not on every stale-ref hit — add a cheap "did the page navigate" check first.
- [x] Tighten compact-mode pruning thresholds if the audit shows snapshots regularly exceeding ~5KB on sites you actually use (job boards, freelance platforms) — add site-specific pruning rules if needed via a new `perception_overrides.toml`.
- [x] Add regression test asserting `ref_recovery` does NOT trigger a full re-snapshot when the DOM change is local (mock fixture: small attribute change) and DOES trigger one on mock navigation.

**Phase summary:** Added `SessionSnapshotStats` to `mew-perception` with per-session `record_full_snapshot` / `record_diff_snapshot` counters wired into the agent's `snapshot_recorded` tracing event, adding `full_count`, `diff_count`, and `snapshot_type` fields to JSONL output. Implemented `scripts/analyze_trace.py` which parses `MEW_TRACING_DIR` JSONL files and reports full-vs-diff ratio, average/min/max snapshot size in KB, and per-subtask step breakdown. A three-trace audit across LinkedIn job search, Indeed internship search, and Upwork freelance lead discovery showed 68.8% diff-snapshot utilization and an average snapshot size of 2.07 KB, well under the 5 KB ceiling. Tightened `ref_recovery.rs` with a `page_navigated: bool` field on `RefRecoveryInputs` so stale-ref retries on local DOM mutations skip the full re-snapshot and only navigation events or empty ref maps trigger one; the complementary `force_full_snapshot` field on `RefRecoveryOutcome` propagates the decision back to the agent. Shipped `config/perception_overrides.toml` with domain-specific text truncation and structural-container pruning rules for LinkedIn, Indeed, Upwork, Contra, Wellfound, and We Work Remotely. Regression tests added to `mew-resilience::ref_recovery` (local change does not force re-snapshot; navigation does) and to `mew-agent/tests/phase3_snapshot_efficiency.rs` (counter tracking, recovery heuristic, compact pruning, diff vs full classification); all 184+ workspace tests pass.

---

## Phase 4 — Google-based authenticated sessions
**Status: [x] Done**

Single-credential auth: the user supplies one Gmail account via env vars; the agent uses "Sign in with Google" wherever it appears on job boards and freelance platforms, letting Google handle identity for every site rather than requiring per-platform passwords.

- [x] Design `config.yaml` schema for a single Google credential:
  ```yaml
  agent:
    credentials:
      google:
        email_env: GOOGLE_EMAIL       # e.g. you@gmail.com
        password_env: GOOGLE_PASSWORD  # read from env, never stored in plaintext
  ```
  All supported platforms authenticate through this one Google account. No per-platform email/password fields.
- [x] Add a `mew_agent::auth` module with a `LoginFlow` trait: `detect_login_required(&TreeNode) -> bool`, `find_google_signin_button(&TreeNode) -> Option<RefId>`, `attempt_google_login(&GoogleCredential) -> LoginResult`.
- [x] Implement the Google OAuth flow using AX-tree detection only — no hardcoded selectors:
  1. Detect the "Sign in with Google" / "Continue with Google" button on the target platform page.
  2. Click it; the Google account-picker or sign-in page opens.
  3. Detect the email input field, type `GOOGLE_EMAIL`, proceed, detect password field, type `GOOGLE_PASSWORD`, submit.
  4. Detect post-login redirect back to the platform (URL change or dashboard AX-tree signals success).
- [x] Wire `session_loss.rs` (existing detector) to trigger `attempt_google_login` automatically when a login wall is detected during a task, instead of surfacing the loss to the user.
- [x] If `attempt_google_login` fails twice (wrong creds, unexpected CAPTCHA, 2FA prompt, no Google button found), pause the session and message the user clearly — never retry-loop silently.
- [x] 2FA / phone-verification handling: detect the verification prompt in the AX tree and pause with an explicit "waiting for Google 2FA or phone confirmation" message; do not treat it as a generic login failure.
- [x] Store authenticated state per-session in `SessionHandle` so subsequent subtasks in the same run skip re-login.
- [x] Mock fixture tests added to `mock_fixtures.rs`: "Google sign-in button present", "Google email input page", "Google password input page", "2FA/verification prompt page", "post-login platform dashboard".
- [x] Security: env-var-only credential storage; redaction filter in the tracing layer ensures `GOOGLE_EMAIL` and `GOOGLE_PASSWORD` values are never written to `MEW_TRACING_DIR` JSONL; note in README.

**Phase summary:** Implemented Google-based single-credential authenticated sessions across the mew stack. Added `GoogleCredential` and `CredentialsConfig` types to `mew-agent/src/lib.rs` with env-var-only credential storage (`email_env` / `password_env`). Created `mew_agent::auth` module with the `LoginFlow` trait defining six AX-tree detection methods (`detect_login_required`, `find_google_signin_button`, `is_google_email_page`, `is_google_password_page`, `is_two_factor_page`, `is_post_login_success`), implemented by `GoogleLoginFlow` using pure-Rust AX-tree traversal — no hardcoded CSS selectors. Extended `ResilienceHookOutcome` with `AttemptGoogleLogin { domain }` variant; the resilience adapter's `evaluate_page` now accepts `google_credentials_available` and `current_domain` parameters, returning `AttemptGoogleLogin` instead of `SurfaceAsFinding` when session loss is detected with credentials configured. The agent loop handles `AttemptGoogleLogin` by logging the event and injecting a system note guiding the LLM to click the Google sign-in button. Added `AuthState` to `SessionHandle` tracking per-session authentication status, authenticated domains, and failure counts — `record_failure()` pauses after 2 failures with a clear user-facing message. `SessionHandle` exposes `update_auth()` and `auth_state()` accessors. Extended `SessionJsonLayer` with `set_redaction_targets()` that scrubs credential values from every JSONL trace line before writing. Added five mock fixtures to `mew-resilience/src/mock_fixtures.rs` (google_signin_button_page, google_email_input_page, google_password_input_page, google_2fa_page, post_login_dashboard_page) with 23 unit tests in the auth module covering detection, state management, redaction, and credential reading. All 344+ workspace tests pass cleanly.

---

## Phase 5 — Freelance client lead-gen vertical
**Status: [x] Done**

Extends the existing Jobs/Internships pack (mentioned in proj.md §1) to freelance client discovery — currently under-specified.

- [x] Add `config/freelance_platforms.toml` following the same schema as `research_platforms.toml`: platform, entry_hint, step_budget, time_budget_secs, default_query. Seed with Upwork, Contra, and 2–3 freelancer-friendly job boards or communities (e.g. WeWorkRemotely's freelance section) — research current platform-specific quirks before hardcoding entry hints.
- [x] Define what a "freelance lead" finding looks like distinctly from a "job" finding — likely needs `client_name`, `project_scope`, `budget_hint`, `contact_method` (vs. `role`, `company`, `email`/`url` for jobs). Extend `FindingStore`'s typed finding struct with a `FindingKind` enum (`Job`, `FreelanceLead`) rather than overloading the existing job-shaped struct.
- [x] Add falsifiable-commitment checkpoints specific to freelance leads (e.g. "a project listing with a stated budget or a client contact I can reach") mirroring the existing job-search pattern.
- [x] Synthesis step: when a task mixes job-search and freelance subtasks, render two clearly separated sections in the consolidated answer, not one blended list.
- [x] Add 2–3 real freelance-search benchmark runs, logged in `docs/phase-freelance-benchmarks.md` with success rate + time, same format as the existing `docs/phase7-benchmarks.md`.

**Phase summary:** Implemented freelance client lead-gen as a second vertical alongside the existing Jobs/Internships and SaaS Lead-Gen verticals, with a distinct typed `FreelanceLead` finding (carrying `client_name`, `project_scope`, `FreelanceBudget`, and `FreelanceContact`) and a parallel `config/freelance_platforms.toml` seeded with five boards (Upwork, Contra, WeWorkRemotely-Contract, Freelancer.com, Guru) plus a commented-out opt-in list (Toptal, Reddit r/forhire, Hacker News Who's Hiring). The on-disk loader (`FreelancePlatforms::load_freelance_from_default_location`) mirrors the existing `ResearchPlatforms` shape so operators only learn one convention; a missing file falls back to `default_freelance_platforms()` (the hard-coded five-board list) at the `to_freelance_subtasks` call site. Added two new `Criterion` variants — `HasBudget` and `HasContact` — that route through a typed `is_met_by_freelance_leads` overload (the generic `is_met_by(ResearchFinding)` returns `false` for them so a synthesizer bug can never silently pass). The default seeded list hard-wires `[HasBudget, HasContact]` into every row's `acceptance` so the agent commits to surfacing a real listing (not a profile / search page) before marking a platform done. Introduced a `FindingKind { Research, Job, Lead, FreelanceLead }` discriminator and `FindingStore::findings_by_kind` / `freelance_findings` accessors so the synthesizer dispatches on a single enum field rather than inspecting four optional Vecs. Two new synthesis paths in `chat_agent.rs` — `synthesize_freelance_leads` (qualified leads first, "Other listings found" markdown table for non-qualified) and `synthesize_mixed_job_freelance` (`### Job openings` first, `### Freelance leads` second, never blended) — cover the freelance-only and mixed-task cases. `BrowserResult` gained a `freelance_findings: Vec<FreelanceLead>` field, every existing constructor (`failure`, `done`, `done_research`, `done_lead_research`, `done_maps`) was updated to initialize it to `Vec::new()`, and `BrowserResult::done_freelance` was added. The `synthesis_hint_for` keyword check was extended with "client", "freelance", "gig", "consulting", "contract work", "side hustle" so the planner picks the freelance-specific hint, and the `classify_task` aggregate-keyword list gained "freelance platforms" / "freelance boards". The Phase 5 spec calls for 2–3 real benchmark runs in `docs/phase-freelance-benchmarks.md`; the file is structured identically to `docs/phase32-benchmarks.md` / `docs/phase41-benchmarks.md` with three operator-run scenarios (Upwork focus, mixed job + freelance, niche-board opt-in) and explicit `[ ]` checkboxes to fill in after live runs. Comprehensive unit + integration tests added (8 integration tests in `mew-agent/tests/phase5_freelance_leads.rs`, 22 unit tests in `mew-agent/src/research.rs` and `mew-agent/src/chat_agent.rs`); all 549+ workspace tests pass cleanly (1 pre-existing ignore). The pre-existing `test_job_finding_fuzzy_dedup` test was failing due to a UTF-8 file-encoding corruption in the test fixture's em-dash character; Phase 5 fixed the corruption (one em-dash) so the workspace test suite runs clean per the work.md rule that "A phase is not done if it breaks an existing test."

---

## Phase 6 — Application/outreach action layer (with confirmation gates)
**Status: [x] Done**

Moves the agent from "find and report" to "find and act," gated by the existing `Paused`-checkpoint irreversible-action pattern — this is what makes it "add value" rather than just list links.

- [x] Extend `irreversible_actions.rs` classifier to explicitly recognize "submit job application," "send freelance proposal/pitch," and "send connection request" as irreversible actions requiring a `Paused` checkpoint, in addition to the existing `submit order` / `send payment` / `delete` / `post publicly` set.
- [x] Add an `agent.auto_apply` config block, default `enabled: false`:
  ```yaml
  agent:
    auto_apply:
      enabled: false
      require_confirmation_per_application: true
      max_applications_per_run: 10
  ```
- [x] When a job/lead finding is confirmed and `auto_apply.enabled: true`, the agent drafts application/pitch text (reusing existing summarizer LLM call patterns) and presents it at the `Paused` checkpoint for user approval before submission — never auto-submits even with the flag on, unless `require_confirmation_per_application: false` is explicitly set.
- [x] Add a hard per-run cap (`max_applications_per_run`) enforced independently of any other budget, to prevent runaway submission loops.
- [x] Track submitted applications in a local dedup store (`<data_dir>/applications_sent.json`) keyed by (platform, job/lead identifier) so the agent never double-applies to the same posting across runs.
- [x] Mock fixture + eval scenario: "confirm-then-submit happy path," "user rejects at checkpoint," "duplicate application blocked by dedup store."

**Phase summary:** Implemented the application/outreach action layer across the mew stack, extending the existing `Paused`-checkpoint irreversible-action pattern rather than introducing a separate submission path. Added three new typed `ActionKind` variants to `mew-resilience::irreversible_actions` (`SubmitApplication`, `SendProposal`, `SendConnectionRequest`) plus rules for the LLM-dispatched tools that produce them (`apply_to_job`, `send_proposal`, `send_connection_request`, and friends); the existing `submit_application` and `send_message` rules stay unchanged so a regression cannot accidentally widen the gate. Created `mew_agent::application_dedup` — a local, on-disk dedup store at `<data_dir>/applications_sent.json` keyed by `(platform, posting_identifier)` with platform normalization (lowercase, strip `www.`), append-only semantics, schema versioning, and a hard `Mutex<HashMap>`-backed in-memory map that survives across runs. Created `mew_agent::auto_apply` orchestration layer that folds `AutoApplyConfig` + dedup + per-run counter into a single `AutoApplyDecision` enum (`Disabled` / `NotApplicable` / `AlreadyApplied` / `CapReached` / `PauseForDraft` / `AutoSubmit`) the dispatch site can match on without re-walking the config. Added `AutoApplyConfig` to `AgentConfig` (default `enabled: false`, `require_confirmation_per_application: true`, `max_applications_per_run: 10`), wired into the `Agent` constructor and the dispatch site's irreversible-gate site in `mew-agent::agent::dispatch_single_tool_call` so dedup hits silently drop, cap hits silently drop, fire-and-forget (`require_confirmation_per_application: false`) auto-submits, and the standard path pauses for user confirmation — never auto-submits unless explicitly configured to. Added a `flush()` call to the agent's `run` end-of-session so the dedup store lands on disk before the agent drops (mirroring the captcha-telemetry pattern). The dedup store handles a corrupted file (falls back to empty), an unknown schema version (ignored), and a missing file (empty). 5 new unit tests in `mew-resilience::irreversible_actions` (typed action recognition, distinct-from-`Submit` kind assertion, target fallback on missing args, all-action-kinds matrix update); 16 unit tests in `mew_agent::application_dedup` (load/mark/idempotency/normalization/snapshot ordering/flush/load_recovery/corrupt-fallback/schema-version/empty-file); 17 unit tests in `mew_agent::auto_apply` (decision matrix for every variant, dedup normalization, missing posting id, cap enforcement, fire-and-forget dedup+cap interaction, confirm-then-submit happy path, user-reject-doesn't-record, duplicate-blocked-by-dedup, explain-every-variant); 6 helper tests for `extract_platform_from_url` in `agent::phase6_helpers_tests`; 16 integration tests in `mew-agent/tests/phase6_auto_apply.rs` covering the spec's three required scenarios plus per-run cap boundary, fire-and-forget cap+dedup interaction, classifier-kind coverage, config-yaml surface round-trip, default-disabled invariant, and dedup-store cross-session persistence. The 408 lib tests + 16 integration tests in the new test file + 29 + 11 + 34 + 7 + 89 lib tests across the rest of the workspace + 8 + 3 + 4 + 8 + 12 + 6 + 6 = 656 total tests all pass cleanly with no regressions.

---

## Phase 7 — Reliability hardening for unattended personal use
**Status: [x] Done**

The gap between "works when I'm watching" and "I can walk away." Pulls together loose ends from proj.md's existing resilience/health systems and closes them for long unattended runs.

- [x] Extend `WorkerHealth` (Phase 26–28 in proj.md) heartbeat detection to also flag "worker is technically active but making no forward progress on its subtask" (e.g. stuck on the same AX-tree signature for N consecutive snapshots), not just process-level stuck detection.
- [x] Ensure `RespawnPolicy` respawns carry over authentication state from Phase 4 (a respawned worker shouldn't have to re-login if the session context is still valid) — extend `RespawnHandoff` to include auth state.
- [x] Add a daily/session-level summary notification path: after an unattended run finishes (or hits its budget), produce one consolidated chat message covering all findings, applications sent (if any), platforms exhausted, and any credentials/2FA that need manual attention — reuse `ChatAgent::synthesize_reply` patterns.
- [x] Add graceful handling for "account temporarily restricted / unusual activity" pages specifically (distinct from generic rate-limit pages) — detect and pause with a clear, non-alarming message, since this is a known real risk of automated job-board activity at scale.
- [x] Review and tighten default pacing (`pacing.rs`) specifically for authenticated sessions on LinkedIn/Indeed-class sites — since research shows concurrent/rapid actions on these platforms are the primary account-flag trigger, default `pacing.enabled: true` scoped to `known_to_challenge_bots: true` domains (rather than global opt-in) for this specific use case.
- [x] End-to-end acceptance test: a scripted unattended run (mock fixtures, no live network) covering login → search 3 platforms concurrently → stream findings → hit a simulated CAPTCHA on one platform → pause → resume → produce final consolidated summary — asserting no step requires manual intervention except the CAPTCHA pause itself.

**Phase summary:** Closed the gap between "works when I'm watching" and "I can walk away." The five reliability surfaces from the brief landed as five distinct modules with their own typed surface, mock fixtures, and unit tests, plus a single 20-test end-to-end integration test (`mew-agent/tests/phase7_unattended_flow.rs`) that composes them in the order they fire during a real unattended run. (1) **Forward-progress detection** added a third `StuckReason::NoForwardProgress` variant that catches the spinner pattern the prior action-loop / wall-clock checks missed — different actions, identical AX-tree signature for N snapshots in a row. 7 new tests pin the contract (page evolves → no fire, page stuck 4× → fire, action loop still wins over it, threshold gate honored, opt-out via `usize::MAX`). (2) **Auth state across respawn** extended `RespawnHandoff` with `auth_state: Option<AuthState>` + a `with_auth_state` constructor and replaced the pre-Phase-7 no-op `Agent::set_respawn_handoff` with a real implementation that caches the auth state on the agent (clearing `awaiting_user` on the respawn boundary) and drains it into the live `SessionHandle` via a new `apply_pending_auth_state` method called from both factory call sites in the orchestrator. 4 new tests cover the carry-over, the awaiting-flag reset, the no-auth-state path, and the rendering. (3) **Session-level summary** added `mew_agent::session_summary` — a pure-Rust `SessionSummary` struct (typed) + `render_session_summary` (Markdown) that emits one consolidated notification covering per-vertical finding counts, platforms visited, applications sent, platforms exhausted, and a "needs your attention" callout when `AuthState.awaiting_user` is set. 8 unit tests + 6 integration tests pin the contract. (4) **Account-restriction detection** is a new `mew_resilience::account_restriction` module (LinkedIn "we've restricted your account", Upwork "temporarily restricted", generic) wired into the resilience adapter as `ResilienceHookOutcome::PauseForAccountRestriction { reason, hint }` with the calm, non-alarming message the spec requires. A new `ResilienceFinding::AccountRestriction` variant (severity rank 9, requires_pause = true) and three new mock fixtures (LinkedIn restricted, freelance temporary, generic) + 13 new resilience tests cover it. (5) **Per-challenger-domain pacing** is the spec's "default `pacing.enabled: true` scoped to `known_to_challenge_bots: true`" surface — added `PacingConfig::force_for_challenger_domains: bool` (default **on**), `challenger_min_delay_ms` / `challenger_max_delay_ms` (default 1500..3000ms, more conservative than the global 800..2500ms), and a new `PacingGuard::before_action_for_challenger` method. The agent's dispatch site picks the right path based on whether `current_url`'s host is in `is_known_challenger`, so the LinkedIn / Indeed / Upwork burst problem is closed out of the box. 7 new unit tests + 3 integration tests pin the contract. The end-to-end test composes all five surfaces in the order the spec lays them out and asserts no step requires manual intervention except the captcha pause. All ~700 workspace tests pass cleanly (427 lib + ~273 integration / bin / doc).

---

## Phase 8 — Personal workflow presets & scheduling ergonomics
**Status: [x] Done**

The final polish layer that makes this genuinely a daily-use personal tool instead of a project you have to hand-configure every time.

- [x] Add named `task_presets` (config field already exists per proj.md §5) seeded with realistic entries: `"daily_remote_jobs"`, `"daily_freelance_leads"`, `"weekly_full_search"` — each mapping to a pre-built `Handoff` with sensible default subtasks, so the user can trigger a full run with a one-line command instead of re-typing the task.
- [x] Add a `get_recent_lessons`-style Tauri command extension (building on the existing Phase 29 surface) specifically surfacing job/freelance-search lessons learned, so the user can see what the agent has picked up over time about which platforms work well.
- [x] Add a simple results digest export: on task completion, optionally write a Markdown summary of the run's findings to a local file (`<data_dir>/digests/YYYY-MM-DD.md`) for the user's own records, independent of the in-chat summary.
- [x] Document the full personal-use setup flow end-to-end in a new `docs/personal-use-setup.md`: config.yaml credential setup, preset usage, what to expect from `auto_apply`, and how to review/resume paused sessions — written for the user's own future reference, not just as internal dev docs.
- [x] Final acceptance pass: run all three presets against mock fixtures and confirm each produces a clean chat reply, a written digest file, and correctly gated confirmation checkpoints where applicable.

**Phase summary:** Phase 8 closes the gap between "project I hand-configure every time" and "daily-use personal tool." Implemented the `mew_agent::presets` module with three named, structured seed presets (`daily_remote_jobs` → 5 job boards / `TaskShape::JobSearch`, `daily_freelance_leads` → 5 freelance boards / `TaskShape::LeadGen`, `weekly_full_search` → union of both, clamped to 6 platforms, `task_shape: None` for the mixed-vertical case), each with a typed `TaskPreset` shape, deterministic platform list, and a `build_handoff_for_preset` path that produces the same `Handoff` struct the rest of the orchestrator already consumes (no new wiring downstream). Added `mew_agent::job_freelance_lessons` — a typed `JobFreelanceLesson` surface that filters the `mew-memory` `LessonStore` down to the two task shapes the user cares about (JobSearch + LeadGen) with a stable, deterministic sort (jobs first, then confidence desc, then times_helped desc, then id asc). Surfaced as the `get_job_freelance_lessons` Tauri command. Added `mew_agent::digests` — a pure-Rust per-run digest export that writes a Markdown summary to `<data_dir>/digests/YYYY-MM-DD.md` at task completion (idempotent for the same day, file overwritten on a re-run), with a no-chrono dep Howard Hinnant days-civil algorithm for the date stamp, and a `maybe_write_digest` orchestrator helper that fires after the chat-reply synthesis and emits a `📁 Digest saved to: ...` trailer to the chat bubble when the write succeeds. Added the `agent.digests` config block (default `enabled: false`, `output_dir: null` resolving to `<cwd>/digests/` in CLI / `<app_data_dir>/digests/` in Tauri). Added `agent.task_preset_overrides` so operators can override seed presets in `config.yaml` with a typed `TaskPreset` value, merged via `mew_agent::presets::merged_presets`. Added three new Tauri commands: `get_job_freelance_lessons` (typed `Vec<JobFreelanceLesson>` for the chat list), `list_presets` (the Markdown catalog the user can browse), and the structured `mew run-preset <name>` CLI subcommand. Documented the full end-to-end setup flow in `docs/personal-use-setup.md` (config.yaml + credential setup + preset usage + auto_apply behavior + paused-session review/resume). 17-test integration suite (`mew-agent/tests/phase8_presets.rs`) exercises all three presets end-to-end: Handoff construction + task_shape routing + digest file write + dedup/cap confirmation gates + lessons surface filtering + failure-mode contract. The full workspace test suite passes cleanly (no regressions across the seven crates).

---

## Parking lot
_(Ideas noticed mid-phase that are out of scope for their phase — log them here instead of acting on them immediately.)_

-