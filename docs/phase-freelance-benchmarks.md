# Phase 5 — Freelance client lead-gen benchmarks

**Status:** Implemented. All `cargo test --workspace` tests pass
(549+ passed, 1 ignored).

## What changed

Phase 5 extends the existing research-loop infrastructure
(`ResearchPlanner`, `FindingStore`, `ChatAgent::synthesize_reply`,
`config/research_platforms.toml`) to a second vertical: freelance
client lead-gen. The motivating case is "find me 5 paying clients
for service X" — a project-listing shape, distinct from the
job-search shape ("find me a SWE role at company Y").

The implementation is five parts:

- **`config/freelance_platforms.toml`** — a new platform list
  with the same schema as `research_platforms.toml`. Pre-seeded
  with five boards: Upwork, Contra, WeWorkRemotely-Contract,
  Freelancer.com, Guru. The `required_domains` list seeds the
  session-local allowlist for the `FreelanceLeadGen` shape. A
  Toptal / Reddit / Hacker News "Who's Hiring" opt-in list is
  commented out so the default config stays small.

- **`FreelanceLead` typed finding** — a new struct in
  `mew-agent/src/handoff.rs` carrying `client_name`,
  `project_scope`, optional `FreelanceBudget`
  (`{raw, kind}`), and optional `FreelanceContact`
  (`{email, url, platform_message_hint}`). Distinct from
  `JobFinding` (full-time roles) and `LeadFinding` (SaaS
  product companies). The `BrowserResult` struct gained a
  `freelance_findings: Vec<FreelanceLead>` field alongside
  the existing `findings` / `job_findings` / `lead_findings`
  / `maps_findings` lists.

- **`Criterion::HasBudget` / `Criterion::HasContact`** —
  new acceptance criteria for the Phase 5 falsifiable
  commitment "a project listing with a stated budget or a
  client contact I can reach." The default seeded platform
  list hard-wires both into every row's `acceptance` field
  so the agent commits to surfacing a real listing (not a
  profile / search page) before marking a platform done.

- **`FindingKind` enum + `FindingStore` accessors** — a
  new `FindingKind { Research, Job, Lead, FreelanceLead }`
  discriminator plus `FindingStore::findings_by_kind` and
  `FindingStore::freelance_findings` accessors. The
  `add_freelance_lead` method dedups by project URL (or
  `(client_name, project_scope)` fallback when the URL is
  missing) and merges on a "first non-empty wins" /
  "longer wins" / "any contact channel is better than
  none" strategy.

- **Two synthesis paths in `chat_agent.rs`** —
  `synthesize_freelance_leads` for freelance-only results
  (qualified leads first, "Other listings found" table for
  non-qualified), and `synthesize_mixed_job_freelance` for
  mixed results (`### Job openings` then `### Freelance
  leads`, two clearly-separated sections, never blended
  into one list).

The `synthesis_hint_for` keyword check was extended to
recognize "client" / "clients" / "freelance" / "gig" /
"consulting" / "contract work" / "side hustle" phrases so
the planner picks the freelance-specific synthesis hint.
The `classify_task` aggregate-keyword list gained
"freelance platforms" / "freelance boards" so a
"search across all freelance platforms" task is
recognized as research-shaped.

## Live benchmark checklist (operator-run)

Before declaring Phase 5 done in production, run the
following three scenarios and record the numbers. The
format mirrors `docs/phase32-benchmarks.md` and
`docs/phase41-benchmarks.md`.

### Scenario 1 — Upwork focus

1. Configure the Google credential in
   `config.yaml`:
   ```yaml
   agent:
     credentials:
       google:
         email_env: GOOGLE_EMAIL
         password_env: GOOGLE_PASSWORD
   ```
   Set `GOOGLE_EMAIL` and `GOOGLE_PASSWORD` in the
   environment. The Phase 4 Google SSO flow handles the
   Upwork sign-in wall.
2. Trigger: "find me 5 paying clients for my Rust
   consulting services".
3. Expected: the agent visits Upwork, Contra,
   WeWorkRemotely-Contract, Freelancer.com, and Guru
   concurrently (max 3 workers at a time). Upwork is
   gated behind the Google sign-in wall — the first
   worker signs in, the other 4 reuse the session via
   the shared `user_data_dir` cookie store.
4. Record:
   - [ ] wall-clock time (target: under 3 minutes)
   - [ ] number of qualified leads (budget OR contact)
   - [ ] number of "Other listings found" (no budget
         AND no contact)
   - [ ] total LLM round-trips
   - [ ] total tokens billed
   - [ ] whether any platform hit a login wall the
         agent couldn't bypass (a hard fail, not a
         partial result)

### Scenario 2 — Mixed job + freelance

1. Trigger: "find me a remote Rust job and find me 3
   paying clients for my Rust consulting services."
2. Expected: the agent splits the task into two
   `Handoff`s — one for `JobSearch` (LinkedIn /
   Indeed / Wellfound) and one for `FreelanceLeadGen`
   (Upwork / Contra / WeWorkRemotely-Contract) — and
   the synthesizer renders two clearly-separated
   sections in the consolidated chat reply:
   `### Job openings` first, `### Freelance leads`
   second.
3. Record:
   - [ ] wall-clock time
   - [ ] whether the consolidated reply shows two
         `### Job openings` and `### Freelance leads`
         headers (not one blended list)
   - [ ] whether the user can read both halves without
         cross-talk between role titles and project
         scopes

### Scenario 3 — Niche-board opt-in

1. Uncomment the `toptal_freelance` and
   `reddit_freelance` rows in
   `config/freelance_platforms.toml`.
2. Trigger: "find me clients for my design system
   consulting."
3. Record:
   - [ ] whether the agent handles Toptal's
         "no profile, no listings" state gracefully
         (the `step_budget = 0` opt-in should produce
         an `Exhausted` status, not a hang)
   - [ ] whether Reddit's JS-heavy search bar is
         navigable via the `query_template` URL
         (`https://www.reddit.com/r/forhire/search/?q=...`)

### Pre-existing gaps uncovered during Phase 5

The build was unbroken at HEAD (Phase 4 committed) but
the uncommitted Phase 19 work introduced a single test
failure (`test_job_finding_fuzzy_dedup`) caused by a
file-encoding corruption in the test fixture's em-dash
character. Phase 5 fixed the corruption (one
UTF-8 em-dash) so the test now passes and the rest of
the workspace test suite runs clean.

## Backward compatibility

- Pre-Phase-5 setups without
  `config/freelance_platforms.toml` on disk continue
  to work: `FreelancePlatforms::load_freelance_from_default_location`
  returns an empty table, and the agent falls back to
  `default_freelance_platforms()` (the hard-coded
  five-board list) at the `to_freelance_subtasks` call
  site.
- `BrowserResult` is now a 12-field struct
  (added `freelance_findings`). Every existing
  constructor (`failure`, `done`, `done_research`,
  `done_lead_research`, `done_maps`) was updated to
  initialize the new field to `Vec::new()`, so a
  pre-Phase-5 `BrowserResult::done` call produces a
  result with `freelance_findings: []` — byte-equivalent
  to the pre-Phase-5 behavior at the chat-reply
  surface (the synthesizer ignores the empty list and
  routes to the same renderer as before).
- The new `Criterion::HasBudget` / `Criterion::HasContact`
  variants return `false` against the generic
  `ResearchFinding` slice (`is_met_by`), so a
  pre-Phase-5 caller that ignores the
  `is_met_by_freelance_leads` overload will not
  accidentally pass a freelance commitment against
  generic findings.
- `FindingKind` is a new enum with no callers in
  pre-Phase-5 code; adding it is non-breaking.
- `mew_agent::handoff::{FreelanceLead, FreelanceBudget,
  FreelanceContact}` are new public types; their
  `serde` round-trip preserves the on-disk / on-wire
  shape, so a Phase 5 `BrowserResult` serialized to
  JSON in a Tauri IPC payload deserializes cleanly in
  a pre-Phase-5 frontend that ignores the new field
  (`#[serde(default, skip_serializing_if = "Vec::is_empty")]`).

## Eval scenarios (mock-fixture, CI-safe)

The integration test
`mew-agent/tests/phase5_freelance_leads.rs` covers:

1. `freelance_platforms_toml_loads_and_seeds_five_rows`
   — verifies the on-disk loader produces the five
   pre-seeded rows.
2. `every_seeded_freelance_row_carries_budget_and_contact_commitment`
   — verifies every row's `acceptance` includes both
   `HasBudget` and `HasContact` (the Phase 5
   falsifiable-commitment contract).
3. `every_seeded_freelance_row_has_a_required_domain`
   — verifies the `required_domains` list covers every
   platform's bare and `www.`-prefixed forms.
4. `synthesize_freelance_only_renders_single_section_with_other_listings`
   — pins the input shape that triggers the
   `synthesize_freelance_leads` path.
5. `synthesize_mixed_job_freelance_renders_two_separated_sections`
   — pins the input shape that triggers the mixed
   renderer and asserts the two sections are disjoint
   on `id`.
6. `finding_store_freelance_dedup_key_is_stable_and_url_first`
   — pins the dedup key contract.
7. `finding_store_freelance_add_dedups_and_merges` —
   pins the dedup + merge behavior.
8. `finding_store_freelance_does_not_collision_with_job_or_lead`
   — pins the typed-list independence.

Plus the two `chat_agent` unit tests
`synthesize_freelance_only_renders_qualified_first_and_other_table`
and
`synthesize_mixed_job_and_freelance_renders_two_separated_sections`
that exercise the actual synthesis output.
