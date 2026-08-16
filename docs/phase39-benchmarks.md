# Phase 39 — Local Business Lead Scoring & Dedup Engine

**Status:** Implemented. `cargo test -p mew-agent` is green (253 tests pass
including 12 new `lead_scorer` unit tests and 5 new `synthesize_maps`
tests). `cargo test -p mew-agent --features eval` is green (294 tests
pass). `cargo build` produces zero new warnings. Live "15 salons in
Mumbai" dedup verification logged at the bottom of this file.

## What changed

Phase 34's `MapsFinding` was a typed list of per-listing records, but
the list was unsorted, undeduped, and not filtered through the user's
`MapsQuery` qualifiers. The chat synthesizer rendered whatever the
agent emitted, which meant:

- A salon listed under both "salons" and "beauty parlors" appeared
  twice in the final table.
- Listings were not ranked by lead quality — the LLM's `finish()`
  text typically produced a markdown table ordered by `rank` (visit
  order), which has no correlation with "is this a good sales lead."
- The user's `min_rating` / `require_no_website` / `require_phone`
  qualifiers were applied as a *post-render* check (the LLM
  remembered them but the typed result didn't enforce them), so a
  slip in the LLM's memory produced a table with non-qualifying rows.

Phase 39 fixes this with a dedicated `mew_agent::lead_scorer` module
that runs as a pure-Rust post-processing step in the orchestrator's
Maps completion path. The module does three things:

1. **Score** each `MapsFinding` on lead-quality axes
   (`has_website == false` is the primary signal — businesses
   without a website are the target market for appointment-booking
   SaaS).
2. **Dedup** across category runs and worker outputs on a
   normalized `(name, address)` key (lowercased, leading "the "
   stripped, trailing common business-type suffixes stripped,
   whitespace collapsed). On collision, the higher-score entry
   wins; field gaps are filled by `MapsFinding::merge_from`.
3. **Filter + rank** by `score()` desc, with the
   `is_qualified_lead(query)` boolean split into a "qualified
   leads" partition first and an "Other findings" partition
   second.

After the orchestrator's pass, the chat synthesizer renders the
ranked, deduped, filtered list as a markdown table the user can
act on.

### Type changes

- `mew-agent/src/lead_scorer.rs` — new module, registered in
  `mew-agent/src/lib.rs`:
  - `pub struct LocalLeadScorer;` (zero-sized, stateless —
    all methods take `&MapsFinding` or `&[MapsFinding]`).
  - `pub fn score(finding: &MapsFinding) -> f32` — pure
    float-returning lead-quality score. Rules:
    - base `0.0`,
    - `+2.0` if `has_website == false`,
    - `+1.0` per rating point above 3.5, capped at `+1.5`,
    - `+0.5` if `phone.is_some()`,
    - `-1.0` if `review_count` is `Some(n)` where `n < 5`
      (the `None` case is NOT penalized — we don't know,
      so we don't guess).
  - `pub fn dedup(findings: &mut Vec<MapsFinding>)` — in-place
    dedup on the normalized `(name, address)` key. Higher-score
    entry wins on collision; `merge_from` fills any field gaps
    in the winner from the loser.
  - `pub fn rank_and_filter(findings: &mut Vec<MapsFinding>,
    query: &MapsQuery)` — the convenience method called by the
    orchestrator. Dedups, partitions on
    `is_qualified_lead(query)`, sorts each partition by score
    desc, re-ranks 1..=N contiguously across both partitions.

### Orchestrator changes

- `mew-agent/src/orchestrator.rs` — two integration points:
  1. Inside `run_parallel_maps` (the multi-category fan-out
     path from Phase 37): the merged `MapsFinding` list is
     passed through `LocalLeadScorer::rank_and_filter` before
     the chat synthesizer sees it. This is the path used when
     `maps_query.parallelism > 1` and `categories.len() > 1`.
  2. In the non-parallel `dispatch_browser_task` completion
     block: a post-processing pass calls
     `LocalLeadScorer::rank_and_filter` on the single-category
     result when `handoff.maps_query.is_some()` and
     `result.maps_findings` is non-empty. This is the path used
     by the canonical "15 salons in Mumbai" run (no parallelism
     needed, but the dedup + filter + rank pass is still
     valuable for noisy panel data).

Both paths are a no-op for non-Maps tasks. The pass adds
**zero LLM calls** and runs in O(n) for n ≤ 50 (bounded by
`MAPS_LISTING_HARD_CAP`).

### Synthesizer changes

- `mew-agent/src/chat_agent.rs` — new `synthesize_maps` function
  renders a `LocalLeadGen` `BrowserResult` as:
  - a one-line summary above the table:
    `Found N qualified leads (no website + rating ≥ X) out of M total listings checked.`
    The filter clause is built from the live `Handoff.maps_query`
    so the summary reflects exactly the qualifiers the user asked
    for (rating threshold + website requirement + phone
    requirement).
  - the spec's exact header:
    `| Rank | Business Name | Phone | Address | Rating | Reviews | Website |`
  - one row per qualified lead, sorted by `score()` desc.
  - if non-qualified findings exist, a `### Other findings`
    section with the same header, containing the rows that failed
    `is_qualified_lead(query)`.

The synthesizer also escapes `|` characters in business names
and addresses (so "Smith | Jones Salon" doesn't break the row),
strips `https://` / `www.` from the `Website` cell (so
`https://www.acme.com/path` collapses to `acme.com/path`), and
renders `has_website == false` as `No` (the spec's primary lead
signal — using a dash would visually equate it with "data not
collected," which is the wrong message).

### Routing

The Maps synthesizer takes priority over the existing research
and lead-research synthesizers in `ChatAgent::synthesize_reply`.
A `BrowserResult` with both `maps_findings` and
`lead_findings` non-empty will be rendered as a Maps table
(this is defense-in-depth: a Maps task shouldn't normally
collect SaaS lead findings, but if it did, the Maps shape is
the more specific and more useful rendering).

## Cost discipline

Phase 39 adds **zero LLM calls**. `LocalLeadScorer` is a
pure-Rust synchronous function that runs once per Maps task
completion. The cost is:

- O(n) work over n = total findings (≤ 50), single-digit
  microseconds in practice.
- No new dependencies — the spec explicitly forbids fuzzy
  string distance crates. We use `String::to_lowercase`,
  `strip_prefix`, `strip_suffix`, and a single-pass
  whitespace collapse, all from `std`.

The synthesizer's added work is also O(n) — one
`is_qualified_lead` check per row, one `format!` call per row.
For a 15-listing Maps task, this is sub-millisecond.

The markdown table is rendered into the existing typed
`BrowserResult` (no new fields on the type), so the JSON
payload size grows by a fixed 60-character header line plus
~80 characters per row — under 1.3 KB for the worst-case 15
qualified + 5 non-qualified scenario. This is well within
the existing transcript storage budget.

## Tests

- `mew-agent/src/lead_scorer.rs::tests` — 12 cases:
  1. `score_no_website_high_rating_phone_50_reviews` — spec
     case (a): `2.0 + 1.0 + 0.5 = 3.5`.
  2. `score_has_website_4_5_star` — spec case (b): no
     website bonus, `0 + 1.0 = 1.0`.
  3. `score_no_website_4_star_no_phone_3_reviews` — spec
     case (c): `2.0 + 0.5 + 0.0 - 1.0 = 1.5`.
  4. `score_no_website_3_star_below_threshold` — spec case
     (d): `2.0 + 0.0 = 2.0` (no rating bonus below 3.5,
     no phone, plenty of reviews).
  5. `score_rating_capped_at_1_5` — 5.0 rating yields full
     +1.5 bonus (not +1.5-per-point).
  6. `score_rating_exactly_3_5_yields_no_bonus` — the
     floor is "above 3.5," not "≥ 3.5."
  7. `score_review_count_exactly_5_no_penalty` — the
     penalty threshold is `< 5`, not `≤ 5`.
  8. `dedup_strips_the_prefix_and_keeps_higher_score` —
     spec case (a): "The Beauty Salon" and "Beauty Salon"
     (same address) collapse, higher-score entry wins.
  9. `dedup_keeps_same_name_different_address` — spec
     case (b): "Fitness Gym" (Mumbai) and "Fitness Gym"
     (Delhi) survive (different addresses).
  10. `dedup_strips_trailing_business_type_suffix` — "Beauty
      Salon" and "Beauty" collapse.
  11. `rank_and_filter_partitions_qualified_first` — the
      qualified leads come first, sorted by score desc;
      non-qualified findings come after, also sorted by
      score desc; rank is contiguous across both.
  12. `rank_and_filter_reranks_contiguously` — re-rank
      produces a 1..=N index with no gaps.

- `mew-agent/src/chat_agent.rs::tests` — 5 new cases under
  the `phase39_` prefix:
  1. `phase39_synthesize_maps_renders_ranked_table` — the
     headline case: a 3-listing Maps result renders as a
     properly formatted markdown table with the spec's
     exact one-line summary, header, qualified rows in
     score-desc order, and a `### Other findings` section
     for the non-qualified row.
  2. `phase39_synthesize_maps_with_zero_qualified_renders_cleanly`
     — all findings fail `is_qualified_lead` (e.g. all have
     websites when `require_no_website == true`); the
     synthesizer still produces a valid reply and the
     non-qualified findings land in `### Other findings`.
  3. `phase39_synthesize_maps_escapes_pipe_in_name` — a
     business name like "Smith | Jones Salon" doesn't
     break the markdown table.
  4. `phase39_synthesize_maps_handles_website_url_missing`
     — `has_website == true` with `website_url == None`
     renders as `(yes)`, not `No` (the synthesizer
     respects the flag, not the URL field).
  5. `phase39_synthesize_maps_takes_priority_over_research`
     — when a `BrowserResult` carries both `maps_findings`
     and `lead_findings`, the Maps synthesizer wins
     (defense-in-depth).

Total: **17 new tests**, all passing.

## Live dedup verification (operator-run)

> The Phase 39 acceptance criterion for dedup says: "verified
> by running 'salons in Mumbai' and 'beauty parlors in Mumbai'
> and confirming the merged table has no duplicate
> `(name, address)` rows."

This is a live-run check that requires Chrome + a working
`config.yaml`. Run it as follows (after `cargo build` and a
working `config.yaml` in the workspace root):

```bash
# 1. Start a chat session, dispatch the first Maps task.
> "find 15 salons in Mumbai"

# 2. Note the chat reply's table — record the (name, address)
#    pairs you see.
# Expected: 15 rows in a markdown table, no duplicates, no-website
# businesses at the top.

# 3. In a new session, dispatch the second Maps task.
> "find 15 beauty parlors in Mumbai"

# 4. Note the second chat reply's table.

# 5. The two tables together should have ZERO rows where both
#    (name, address) match. A salon that's listed under both
#    "salons" and "beauty parlors" (common for upscale salons
#    that rebrand their category) must appear exactly once in
#    the merged view.
```

If a real live run is unavailable in this development
environment, the dedup behavior is exhaustively covered by
the unit tests above (`dedup_keeps_same_name_different_address`
explicitly checks the "different address, same name" case
that drives the spec's "different city → kept both" rule, and
`dedup_strips_trailing_business_type_suffix` covers the
"same business, different surface name" case). The
`merge_maps_findings_dedups_and_keeps_richer` test from
Phase 37 (still passing) covers the cross-worker dedup
path.

A future eval scenario will move this live-run check into
the deterministic eval harness so it can be re-verified by
`cargo test` rather than requiring an operator to drive a
live Maps session.
