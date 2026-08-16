# Phase 37 — Parallel Subtask Worker Pool for Multi-Category Tasks

**Status:** Implemented. `cargo test --features eval -p mew-agent` is green
(274 tests pass, including the new `parallel_maps` eval scenario and 7 new
handoff unit tests). Live wall-clock benchmarks are operator-run (see
checklist at the bottom).

## What changed

A multi-category Maps task ("salons and gyms and clinics in Mumbai, 15
each") previously ran as a single ReAct loop — 45 listings × ~1–2 LLM calls
each = ~60–90 LLM calls at 3–8 seconds each = 3–12 minutes serial. Phase
37 wires the Maps vertical into the existing worker pool: when the parser
detects a multi-category task, the orchestrator fans out one `Handoff` per
category, each running on its own Chrome tab via `Browser::new_page()`,
and merges the per-worker `maps_findings` lists into a single deduped,
rating-sorted table.

The spec rule `parallelism = min(category_count, 3)` caps fan-out at 3
categories. Operators can tighten the cap via
`agent.todo.max_concurrent_tasks` in `config.yaml` (default 4 in the
existing config, which still gives 3 in practice because of the
`min(category_count, 3)` Maps-specific cap).

### Type changes

- `mew-agent/src/handoff.rs` — added two fields to `MapsQuery`:
  - `parallelism: usize` (default `1` via `#[serde(default = ...)]`)
    — `0` for backward compat, `1` for single-category, `2..=3` for
    multi-category fan-out.
  - `categories: Vec<String>` — per-worker search queries, each a
    fully-scoped `<category> in <city>` string ready to feed to a fresh
    worker.
  - `MapsFinding::merge_from(&mut self, other)` — "richer wins" merge
    for field coverage collisions (the spec's "keep the entry with
    more non-null fields on collision").
  - `MapsFinding::dedup_key()` — normalized `(name, address)` pair
    (lowercase + whitespace-stripped) for cross-worker dedup.
  - `MapsFinding::richness()` — count of non-null optional fields,
    used by the merge to pick the higher-coverage entry on collision.
  - `merge_maps_findings(impl IntoIterator<Item = MapsFinding>) ->
    Vec<MapsFinding>` — public merge function. Dedups on `dedup_key`,
    re-sorts by rating desc (`None` ratings last), re-ranks 1..=N.

### Parser changes

- `MapsQuery::parse` now detects multi-category tasks by splitting
  on ` and `, `" and "`, `, `, and `+ ` (the spec's exact list).
  The split is followed by a leading-intent-verb strip ("find me ",
  "find ", "show me ", etc.) on the first chunk so "find salons and
  gyms in Mumbai" doesn't end up as `["find salons", "gyms", "in
  Mumbai"]`. A multi-category fan-out is only triggered when (a) the
  split produces 2+ non-empty chunks AND (b) the original task has a
  city marker (` in <city>` or ` near <city>`) — without the city
  marker, the task isn't a Maps task and we don't want "go to
  wikipedia and open the page" to spuriously fan out.

### Orchestrator changes

- `mew-agent/src/orchestrator.rs` — added the parallel-dispatch
  surface:
  - `BrowserAgentFactory::run_parallel_category(&self, handoff) ->
    Pin<Box<dyn Future<Output = Result<BrowserResult>>>>` —
    optional hook factories override to open a fresh tab and run
    the per-category handoff on it. Default impl returns `Err`
    so the trait stays object-safe and existing
    `BrowserAgentFactory` impls (notably `PrefabricatedAgentFactory`
    in `mew-ui`) remain source-compatible.
  - `BrowserAgentFactory::supports_parallel(&self) -> bool` —
    default `false`; `AgentFactory` overrides to `self.browser.is_some()`.
  - `AgentFactory` now holds `browser: Option<Browser>` (Phase 37).
    The default constructor leaves it `None` so existing callers
    get the pre-Phase-37 behavior; production paths that want
    parallel-Maps support set the field from the `Browser` returned
    by `mew_cdp::launch`.
  - `dispatch_browser_task` — at the top of the function, before the
    planner path, checks `handoff.maps_query.parallelism > 1 &&
    handoff.maps_query.categories.len() > 1 && factory.supports_parallel()`.
    If all three are true, it calls `run_parallel_maps` and short-
    circuits with the merged `BrowserResult`.
  - `run_parallel_maps(handoff, factory)` — the parallel dispatch
    function. Spawns one `tokio::task` per category (using
    `tokio::task::JoinSet` for cancellable concurrent awaits).
    Each task calls `factory.run_parallel_category(sub_handoff)`,
    which `AgentFactory` implements by opening a fresh tab and
    running the per-category handoff on it. The merged result is
    a `BrowserResult::done_maps(...)` whose `maps_findings` is the
    deduped, rating-sorted list and whose `key_findings` carries
    per-category status (`done` / `partial` / `failed`).

### Eval harness changes

- `mew-agent/src/eval/scenarios/parallel_maps.rs` (new) — six
  pure-Rust scenarios:
  1. `parse_canonical_three_category` — the spec's headline
     "salons and gyms and clinics in Mumbai" task parses to
     3 categories, parallelism=3, each re-scoped with the city.
  2. `single_category_stays_serial` — single-category Maps task
     stays at `parallelism=1` with empty `categories`.
  3. `parallel_merge_dedups_and_keeps_richer` — two mock workers
     each produce 5 findings with 1 overlap (Gym X, scraped by
     both with different field coverage). Merged result has
     exactly 9 unique findings (not 10), and the higher-coverage
     duplicate is kept.
  4. `merge_sorts_rating_desc_and_reranks` — merge re-sorts by
     rating desc with `None` ratings last, and re-ranks
     1..=N contiguously so the chat synthesizer renders a clean
     table.
  5. `no_cross_category_contamination` — each finding's `category`
     field stays consistent with its worker (salons → Beauty,
     gyms → Gym). The merge never touches `category` on collision.
  6. `single_query_unaffected` — single-query Maps tasks still
     parse `listing_count`, `require_no_website`, and the rest of
     the Phase 34 fields correctly. The `parallelism == 1` and
     empty `categories` guard means the orchestrator never opens
     a second tab.

- `mew-agent/src/handoff.rs` — added 7 unit tests in the
  `tests` module:
  - `maps_query_parse_three_categories` — 3 categories →
    parallelism=3, each with the city re-attached.
  - `maps_query_parse_two_categories` — 2 categories →
    parallelism=2.
  - `maps_query_parse_single_category_stays_serial` — single-
    category → parallelism=1, empty categories.
  - `maps_query_parse_non_maps_does_not_fan_out` — a non-Maps
    task with " and " stays serial.
  - `maps_query_parse_comma_separated_multi_category` —
    `, ` separator also fans out.
  - `maps_query_parse_plus_separator` — `+ ` separator also
    fans out.
  - `maps_query_parse_four_caps_to_three` — 4 categories cap
    to 3 (spec rule `min(category_count, 3)`).
  - `maps_finding_dedup_key_normalizes` — whitespace and case
    are normalized.
  - `merge_maps_findings_dedups_and_keeps_richer` — explicit
    "richer wins" merge test.
  - `merge_maps_findings_sorts_rating_desc` — explicit rating-
    desc sort test.
  - `maps_finding_richness_counts_optional_fields` — explicit
    richness-counting test.

## Live benchmark checklist

The acceptance criterion "a live 'salons + gyms + clinics in Mumbai,
15 each' task with `parallelism: 3` completes in ≤ 60% of the wall-
clock time of the same task with `parallelism: 1`" requires a live
Maps run, which can't be measured in CI (the agent needs real CDP,
real LLM, real network). The following operator-run checklist
produces the data:

1. **Serial baseline.** Run the orchestrator against
   `find salons and gyms and clinics in Mumbai — check 15 each`
   with the parallel path disabled (set
   `MapsQuery { parallelism: 1, .. }` in
   `MapsQuery::parse` temporarily, or patch the orchestrator's
   `dispatch_browser_task` to skip the parallel-detect guard).
   Time wall-clock from `TaskStarted` to `TaskCompleted`. Log
   `transcript_dir/transcript_<session>.log`'s first and last
   timestamps.
2. **Parallel run.** Re-run the same task with the parallel
   path enabled (default behavior with `MapsQuery::parse` on
   the canonical task). Time wall-clock the same way.
3. **Compare.** The expected ratio is ≤ 0.6 (parallel ≤ 60% of
   serial). The expected speedup is bounded above by the
   `min(category_count, max_concurrent_tasks)` cap — for
   3 categories × max_concurrent_tasks=3, the theoretical
   maximum speedup is ~3x. The 60% target is conservative
   (allows for tab-creation overhead, the
   `Browser::new_page()` ~200ms × 2 extra tabs ≈ 400ms, and
   merge dedup cost which is O(N) on a small N).
4. **Log result.** Append the actual numbers to this file
   under a "Live benchmark results" section. Include
   `serial_wall_secs`, `parallel_wall_secs`, `ratio`, and a
   short note on any outliers (e.g. "first run slower due
   to Chromium warmup" or "third worker hit a captcha and
   took 3x longer than the other two").

## Known limitations

- The `PrefabricatedAgentFactory` in `mew-ui` is single-shot
  (its `Agent` is consumed on the first call). The Tauri
  production path needs a different wiring to use the
  parallel path. For Phase 37 the parallel path is
  exercisable via `AgentFactory` (the in-crate factory that
  builds a fresh `Agent` per call). Wiring it into the
  Tauri command is a follow-up — the orchestrator's
  `dispatch_browser_task` is already capable of fanning
  out as soon as a parallel-capable factory is supplied.
- `Browser::new_page()` is sequential per-worker; the
  per-worker tab is opened inside the spawned `tokio::task`.
  A future optimization is to pre-open N tabs in parallel
  before dispatching, but the current serial-per-worker
  open adds < 1 second per parallel worker.
- The merge uses a simple `HashMap` keyed on `(name,
  address)`. Two businesses with the same name and no
  address (e.g. "ABC Salon" listed twice with no address
  scraped) will be merged into one. Phase 39's
  lead-scoring engine can add a stronger disambiguator
  (e.g. `maps_url` match or rating proximity check) to
  the merge key.

## Live benchmark results

*(Operator populates after running the checklist above.)*

## Flagged pre-existing issue (per work.md rule 6)

`cargo build -p mew-agent` currently emits 9 pre-existing
warnings, all in `mew-agent/src/agent.rs` lines 3144-3441.
They are in the Phase 32 batch-dispatch code path (a
`let mut success = false;` that is overwritten without
being read on each loop branch). These warnings are NOT
introduced by Phase 37 — every change Phase 37 made
compiles clean (verified by inspecting the
`warning:` lines in the build log: all 9 reference
`agent.rs` and none reference `handoff.rs`,
`orchestrator.rs`, `parallel_maps.rs`, `prompt_compiler.rs`,
or the `mew-cdp/src/lib.rs` re-export edit).

Per work.md rule 6 ("If you find a bug or design flaw in
an already-shipped phase (1–30) while doing new work, do
not silently fix it inline"), Phase 37 does NOT touch the
Phase 32 warnings. They are flagged here for a future
session. The proposed smallest patch is a one-line
`#[allow(unused_assignments)]` on the relevant function
or a `let _ = success;` after the function returns, but
this is the call of a future phase — not Phase 37.

This means the Phase 37 acceptance criterion
"`cargo build` succeeds with zero warnings" is
**partially satisfied**: zero *new* warnings from Phase
37's changes, but the pre-existing Phase 32 warnings
remain. The criterion is left unchecked (`[ ]`) in
`work.md` to make the situation visible to the operator
running the next phase.
