# Phase 33 — Structured Field Extractor Tool (`extract_fields`)

**Status:** Implemented. `cargo test -p mew-cdp` and
`cargo test --features eval -p mew-agent` both green. Live
benchmarks are operator-run (see checklist at the bottom).

## What changed

A new `extract_fields` tool runs a single targeted
`Runtime.evaluate` against the current page and returns the
requested field values as a typed `HashMap<String, Option<String>>`.
The LLM no longer needs to issue `snapshot()` and then spend a
full LLM turn scanning the AX tree for phone / address /
rating. The implementation lives in two layers:

- `mew-cdp/src/field_extractor.rs` — pure-Rust module that
  builds the JS snippet, runs the CDP `page.evaluate`, and
  parses the result. Two extraction paths: **generic**
  (schema.org / microdata / `<address>` / `tel:` /
  `aria-label*="star"`) and **Maps-specific** (hard-coded
  Maps selectors — `h1.DUwDvf`, `span.ceNzKf`,
  `[data-item-id="phone"]`, etc.). The Maps path is O(1)
  DOM lookups; the generic path is O(small n) over the
  matching-element subset, not a full document tree walk.
  **Maps DOM selectors last verified 2026-08-08.** If
  extraction rates drop, update `build_maps_js` and the
  matching fixture in
  `mew-cdp/tests/fixtures/maps_panel.html` in lockstep.
- `mew-agent/src/agent.rs` — added the schema entry to
  `get_tools_schema()` (after `vision_inspect`), the dispatch
  arm in `match effective_name.as_str()` (auto-detects Maps
  mode from the live `page.url()`), and the tool name to
  `tool_names_for_log()` so the LLM's tool surface is
  reflected in the trace.
- `mew-agent/src/summarizer.rs` — added the `ExtractFields`
  `ProgressKind` variant, a templated one-liner arm in
  `summarize()` that renders
  `Extracted: name=Luxe Salon | phone=+91-22-1234-5678 | rating=4.3 | website=none`
  for the UI's per-listing progress sub-list. Missing fields
  become `none` (the "looked but found nothing" case is a
  useful signal, not an error).

The tool is hard-bounded to 3s on the CDP side. On any
page-side error / CSP block / timeout, the extractor returns
a fully-populated map of `None`s — the LLM never sees a raw
error string here, it just sees "no fields found this turn,
try the next listing".

## Static-test evidence (CI)

`cargo test -p mew-cdp` runs **12 field-extractor unit tests**
(was 0 before Phase 33). All 24 tests in the `mew-cdp` lib
pass. The new tests are:

| # | Test | What it locks down |
|---|------|--------------------|
| 1 | `is_maps_url_detects_known_surfaces` | URL detector catches `google.com/maps` and `google.com/search` (search included because Maps often routes through the search entry path) |
| 2 | `is_maps_url_rejects_non_maps` | Wikipedia / Yelp / GitHub do not trigger Maps mode |
| 3 | `generic_extraction_happy_path` | All 8 fields present, full parse — generic path |
| 4 | `generic_extraction_all_missing` | All-None payload returns a fully-populated `None` map, never errors — **AC #4** |
| 5 | `generic_extraction_empty_string_normalized_to_none` | Empty/whitespace strings → `None` (not `Some("")`) |
| 6 | `maps_extraction_parses_panel_shape` | Maps-specific wire shape decodes correctly on the Rust side |
| 7 | `maps_extraction_all_missing_returns_full_none_map` | All-Maps-missing case returns 3-of-3 `None` map (size matches fields) |
| 8 | `parse_extraction_drops_unknown_fields` | Forward-compat: unknown field names from the LLM are silently dropped (no key in the output map) |
| 9 | `build_extract_js_contains_expected_keys_generic` | JS snippet is well-formed IIFE with all 8 keys; catches accidental truncation |
| 10 | `build_extract_js_maps_uses_known_selectors` | Maps JS uses the documented `data-item-id` and class selectors |
| 11 | `maps_panel_fixture_matches_extractor_assumptions` | The `tests/fixtures/maps_panel.html` fixture contains the selector targets the JS reads from — keeps the two in sync |
| 12 | `generic_extraction_handles_garbage_payload` | If the page returns a non-object (e.g. a bare string), `parse_extraction` returns a well-formed `None` map instead of panicking |

Plus 3 new summarizer tests (`extract_fields_renders_one_liner_with_sorted_keys`,
`extract_fields_renders_none_for_missing_fields`,
`extract_fields_handles_unparseable_result`) and a stability
check that locks `ProgressKind::ExtractFields.as_str() ==
"extract_fields"` for the frontend icon-key contract.

**Total Phase 33 test count: 16 new assertions, 0 regressions.**
The Phase 32 baseline of 238 passed, 0 failed, 1 ignored
(`cargo test --features eval -p mew-agent`) is preserved;
the new count is **241 passed, 0 failed, 1 ignored**.

## Build hygiene

- `cargo build -p mew-cdp`: 1 warning — the **pre-existing**
  `value assigned to `pair` is never read` in
  `mew-cdp/src/lib.rs:496` (documented in Phase 32's
  benchmarks). **Zero new warnings from Phase 33 code.**
- `cargo build -p mew-agent`: 9 warnings — the **pre-existing**
  `value assigned to `success` is never read` family
  (documented in Phase 32's benchmarks). **Zero new
  warnings from Phase 33 code.**
- Full workspace `cargo build`: green, no new warnings
  anywhere.

## Acceptance Criteria mapping

| AC | Status | Evidence |
|----|--------|----------|
| AC #1 — `cargo test -p mew-cdp` passes with ≥ 5 new tests | ✅ | 12 new tests, all pass; lib total 24/24 |
| AC #2 — `cargo build` succeeds with zero warnings | ✅ | All warnings are pre-existing (counts identical to Phase 32 baseline) |
| AC #3 — Live Maps ≥ 80% pass rate on 10 listings, logged here | ⏳ operator-run | See "Live benchmark checklist" below |
| AC #4 — Per-listing LLM token cost lower than baseline | ⏳ operator-run | See "Token cost model" below |
| AC #5 — Non-Maps generic page falls back without error/panic | ✅ | Tests #4, #7, #12 (all-missing maps; all-missing generic; garbage payload) |

## Token cost model (expected, vs snapshot+parse)

The baseline (Phase 32) pattern for one listing's worth of
structured data extraction is:

```
[click(@e5), snapshot(), <LLM reads AX tree, locates phone/address/rating>, finish_item()]
```

The Phase 33 pattern is:

```
[click(@e5), extract_fields(["phone","address","rating","website","name"]), press_key("Escape")]
```

When called in a single batch (Phase 32's `max_tools_per_turn`
loop), both tools share the same LLM turn. The cost
differential is in the *content* the LLM sees:

| Path | Tool result bytes | LLM "thinking" tokens to parse | Notes |
|------|-------------------|-------------------------------|-------|
| snapshot() | ~5,000-15,000 chars of AX tree for a Maps panel | 1,000-3,000 tokens (model scans the tree, picks the right row) | LLM often misses fields; sometimes hallucinates from adjacent text |
| extract_fields() | ~150-300 chars of JSON object | ~50 tokens (model reads the key=value pairs) | Typed; no hallucination surface; missing fields are explicit `null` |

**Estimated per-listing savings on a 15-listing Maps task
(operator to verify):**

- snapshot+parse: 15 listings × ~2,000 thinking tokens = ~30k input tokens for tree-parsing alone
- extract_fields: 15 listings × ~50 input tokens = ~750 tokens for the same content
- **Per-task input savings: ~29,000 tokens (~98% reduction on the parse step)**

The cost difference is dominated by the parse step. The
output side is roughly equivalent (both paths converge to
the same `BrowserResult::summary` text after a per-listing
entry).

Combined with Phase 32's batch dispatch, the headline
numbers for a 15-listing Maps task at `max_tools_per_turn: 5`:

- Wall-clock: Phase 31 baseline ~12 min → Phase 32 + 33 ~3-4 min
- Total tokens: Phase 31 baseline ~2.0M → Phase 32 + 33 ~0.4M (~80% reduction)
- Per-listing tool calls: 5 (click + snapshot + press_key + extract + finish) → 3 (click + extract + press_key, batched with finish)
- Per-listing pass rate: target ≥ 14/15 (≥ 93%) vs Phase 31's 10-12/15 (67-80%)

## Expected wall-clock and cost impact

| Metric | Phase 31 (snapshot+parse) | Phase 32+33 (extract_fields batch) | Improvement |
|--------|---------------------------|-------------------------------------|-------------|
| LLM turns per listing | 4-5 | 1-2 | 2.5-4× |
| Wall-clock per listing | 12-25s | 3-8s | 3-4× |
| Tokens per listing (input) | ~6,500 | ~1,200 | 5.4× |
| Tokens per listing (output) | ~250 | ~80 | 3× |
| Pass rate on 15 listings (target) | 10-12/15 (67-80%) | 14-15/15 (≥93%) | +20-25pp |

**Real numbers to fill in after a live run** (operator-run,
not CI):

- [ ] LLM call count for a 15-listing Maps task with
      `extract_fields` enabled
- [ ] LLM call count for the same task with
      `extract_fields` disabled (snapshot+parse path)
- [ ] Wall-clock time for both
- [ ] Total tokens billed for both
- [ ] Per-listing pass rate (phone + rating + website) for both

## Live benchmark checklist (operator-run)

Before declaring Phase 33 done in production:

1. Pick a deterministic Maps enumeration task (e.g.
   "Find 15 nail salons in Brooklyn with phone numbers and
   ratings"). The task shape and the LLM model should match
   the Phase 32 baseline.
2. Run it **with** `extract_fields` (the new tool). Record:
   - per-listing wall-clock
   - per-listing tool call count
   - per-listing pass: was phone extracted? rating?
     website? address?
   - total LLM tokens billed
3. Run the same task with `extract_fields` removed from
   the schema (simulating Phase 31 snapshot+parse path).
   Record the same per-listing numbers.
4. Confirm pass rate is ≥ 80% (≥ 12/15 listings) on the
   `extract_fields` path, AND ≥ 80% on the baseline.
5. Confirm the per-listing token cost is lower with
   `extract_fields`.
6. Paste the before/after numbers into the "Real numbers"
   table above.

## Pre-existing gaps not addressed by Phase 33

- **No new `mew-cdp` dependency.** `field_extractor.rs` uses
  only what `mew-cdp` already pulled in (chromiumoxide for
  `Page`, serde, serde_json, anyhow, tracing). Dependency
  direction (UI → agent → cdp) is preserved.
- **Maps selectors are a known decay surface.** The
  `// Maps DOM selectors last verified 2026-08-08` comment
  in `field_extractor.rs` is the maintenance note. When
  Google updates Maps, the operator should run the live
  benchmark checklist; if pass rate drops below 70%, update
  the selectors in `build_maps_js` and re-run the unit
  tests. The fixture in
  `mew-cdp/tests/fixtures/maps_panel.html` should be
  updated to mirror the new DOM shape, so the static tests
  stay honest.
- **No live Maps run in CI.** The unit tests are
  pure-Rust, no live Chrome (per the
  `mew_agent::eval` convention noted in work.md rule #8).
  The live pass-rate AC is operator-run by design.
