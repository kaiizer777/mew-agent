# Phase 34 — Google Maps Vertical Pack

**Status:** Implemented. `cargo test --features eval -p mew-agent`
and `cargo test -p mew-memory` both green. Live benchmarks are
operator-run (see checklist at the bottom).

## What changed

The Maps local-business lead-generation workflow is now a
first-class vertical alongside `JobSearch` and `LeadGen`. A
Maps task is no longer mis-classified as `GenericBrowse` —
the agent gets explicit Maps-specific guidance in the
system prompt, a typed `MapsQuery` is parsed deterministically
from the user's free-text task, and per-listing findings
flow through a typed `MapsFinding` so the synthesizer can
render a ranked table.

### Type changes

- `mew-memory/src/lib.rs` — added `TaskShape::LocalLeadGen`
  variant. The `#[serde(rename_all = "snake_case")]`
  annotation makes the JSON form `"local_lead_gen"`.
- `mew-agent/src/handoff.rs` — added:
  - `MapsQuery` (deterministic parser, no LLM call) with
    fields `search_query`, `city`, `listing_count` (capped
    at 50, default 15), `min_rating`, `require_no_website`,
    `require_phone`.
  - `MapsFinding` with the per-listing data (name, phone,
    address, rating, review_count, has_website, website_url,
    category, maps_url, rank) and a `is_qualified_lead(&MapsQuery)`
    method that applies every active filter.
  - `Handoff.maps_query: Option<MapsQuery>` (Phase 34).
  - `BrowserResult.maps_findings: Vec<MapsFinding>` (Phase 34).
  - `BrowserResult::done_maps(...)` convenience constructor.
  - `Eq` removed from `Handoff` and `BrowserResult` (they now
    contain `f32` via the Maps types; `f32` doesn't impl
    `Eq`).
- `mew-agent/src/router.rs` — updated the LLM classifier
  schema's intent + task_shape enums to include
  `local_lead_gen`, and updated the system prompt to mention
  Maps tasks explicitly.

### Chat agent changes

- `mew-agent/src/chat_agent.rs` — added a public
  `is_local_lead_gen_task(&str) -> bool` keyword check
  covering the spec's exact phrase set (with one disambiguating
  carve-out for "leads in <city>" to avoid clashing with
  the SaaS lead-gen shape). Threaded `maps_query` through
  `build_handoff_inner` and `build_handoff_for_shape_inner`.
  The `LocalLeadGen` shape is the keyword-detection fallback
  after `JobSearch` and `LeadGen` (so "find SaaS leads in
  devtools" still routes to `LeadGen`, not `LocalLeadGen`).

### Agent changes

- `mew-agent/src/agent.rs` — added
  `infer_task_shape_from_task(&str) -> TaskShape` (a cheap
  safety net the agent's preflight uses). Added
  `render_maps_hint_block(&str) -> String` which produces
  the Phase-34-spec system-prompt block (verbatim CRITICAL
  RULES 1-6). Modified `run_preflight_plan` to inject the
  hint block right after the `PLAN:` block when the task
  shape is `LocalLeadGen`. The block also includes a
  pre-parsed summary of `listing_count`, `min_rating`,
  `require_no_website`, `require_phone` so the LLM sees the
  filters inline.

### TOML config

- `config/maps_vertical.toml` — new operator-editable file
  with the 5 Maps-specific knobs (`entry_url_template`,
  `panel_close_action`, `sidebar_scroll_hint`,
  `field_extraction_mode`, `required_domains`). Loader in
  `mew_agent::research::MapsVerticalConfig` mirrors the
  `ResearchPlatforms` shape so a future operator doesn't
  have to learn two conventions. When the file is missing
  (the common case on a fresh checkout), the loader falls
  back to the built-in defaults — so the file is a
  *purely additive* change. The defaults are the contract
  the system-prompt hint block relies on; the loader docs
  call this out explicitly.

### Eval scenario

- `mew-agent/src/eval/scenarios/maps_lead_gen.rs` — new
  pure-Rust scenario, registered in `eval/scenarios/mod.rs`
  and added to `eval::harness::default_scenarios()` (count
  bumped from 10 to 11 with a corresponding test assertion
  update). 7 sub-scenarios cover:
  1. `MapsQuery::parse` on the canonical Phase 34 task.
  2. `MapsQuery::parse` on alternate phrasings (first/check,
     rating >=, with phone, hard cap, default).
  3. `MapsFinding::is_qualified_lead` with the canonical
     query (1 qualified lead: Salon A).
  4. `MapsFinding::is_qualified_lead` with rating-only
     filter (4 qualified: A, B, D, E).
  5. The system-prompt hint block contains every CRITICAL
     RULE from the spec (verbatim).
  6. The intent-classifier keyword check correctly
     classifies true positives (spec phrases) and true
     negatives (Instagram, SaaS leads, jobs, wikipedia).
  7. `Handoff` and `BrowserResult` round-trip through serde
     with the Maps fields intact.

## Cost discipline

Per the Phase 34 spec's Budget Discipline rule (work.md
rule 10):

- The Maps workflow adds **zero** LLM calls to the
  classification path. The MapsQuery is parsed by a
  deterministic Rust function; the LLM classifier's
  per-call cost is unchanged.
- The system-prompt hint block is appended to the system
  prompt *only* on Maps tasks. Non-Maps tasks see no
  increase in prompt size.
- The `extract_fields` tool (Phase 33) is referenced from
  the hint block. The LLM calls it once per listing, which
  is the same pattern the existing Maps panel flow uses;
  no new per-iteration cost.
- `MAPS_LISTING_HARD_CAP = 50` and `MAPS_LISTING_DEFAULT = 15`
  keep the worst-case listing count bounded. The phase-34
  acceptance test is for 15 listings.

## Live benchmark checklist (operator-run)

The spec calls for a live run on "Find salons in Mumbai
with no website and rating above 4.0 — check 15 listings".
Per work.md rule 6, this is a separate operator-run task
that requires a live Chrome + live LLM. To fill in:

- [ ] Hardware: (e.g. M1 Pro / 32GB / 1 Gbps)
- [ ] Model endpoint: (e.g. `opencode-zen` / `gpt-5.1`)
- [ ] Listings visited: __ / 15
- [ ] Extraction success rate: __ / __ (fields found /
      fields expected)
- [ ] Wall-clock time: __ (target ≤ 8 minutes)
- [ ] Total LLM calls: __

## Dependencies on other phases

- Phase 31 (SPA navigation detection & context preservation) —
  the Maps hint block references `SpaTransition` behavior.
  The implementation is a textual reference in the system
  prompt; no code-level coupling.
- Phase 32 (multi-action batch dispatch per LLM turn) —
  the Maps execution strategy assumes multi-tool-per-turn
  (snapshot + extract_fields + escape in one turn).
- Phase 33 (`extract_fields` tool) — the Maps hint block
  tells the agent to use it.
- Phase 21 (vertical router, `TaskShape` enum, `mew-agent/src/router.rs`) —
  `TaskShape` lives in `mew-memory`, the router re-exports it.
  The new `LocalLeadGen` variant lives there.
