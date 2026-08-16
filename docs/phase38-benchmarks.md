# Phase 38 — AX Tree Perception Tuning & Fast-Path Snapshot

**Status:** Implemented. `cargo test -p mew-cdp` (29 tests pass, 5 new
SnapshotConfig) and `cargo test -p mew-perception` (6 tests pass, all
new cap tests) and `cargo test -p mew-agent` (236 tests pass, 3 new
`max_ax_tree_nodes` round-trip tests) are all green. Live Maps
enumeration latency comparisons below are operator-run (see checklist
at the bottom).

## What changed

The agent's per-iteration perception block in `mew_agent::agent::run_inner`
used a single hardcoded 5-second per-call timeout with up to 4 retries
and a full `wait_for_page_settled` between every retry — a worst case
of ~67 seconds per perception step. For Google Maps panel opens the
panel content is in the DOM in <500 ms, but the agent was waiting 5 s
on every attempt and burning 60+ seconds per 15-listing task on
over-provisioned timeouts.

Phase 38 introduces a `SnapshotConfig` preset system keyed off
`NavigationKind`, plus an optional cap on the AX tree node count, plus
a per-iteration `PERCEPTION:` log line so regressions are auditable
without a profiler.

### Crate-level changes

#### `mew-cdp` — `SnapshotConfig` + presets

`mew-cdp/src/lib.rs` now exposes:

- `pub struct SnapshotConfig { pub timeout_secs: u64, pub max_retries:
  u8, pub settle_between_retries: bool }` — the three knobs the
  perception block actually needs.
- `SnapshotConfig::full_load()` — `{ 5, 4, true }`. Pre-Phase-38 default.
  For `FullNavigation` (real page loads).
- `SnapshotConfig::spa_transition()` — `{ 2, 2, false }`. For
  `SpaTransition` (Maps / Yelp / TripAdvisor panel opens).
- `SnapshotConfig::no_change()` — `{ 1, 1, false }`. For `NoChange`
  (in-page scroll / click on a stable page).
- `snapshot_config_for(NavigationKind) -> SnapshotConfig` —
  centralized mapping so the agent loop and any future callers stay
  in sync with the preset definitions.

Total worst-case perception latency per preset (timeout-only,
ignoring the page-settle wait which is bounded by the page itself):

| Preset             | First call | +1 retry | +1 settle | Total worst |
| ------------------ | ---------- | -------- | --------- | ----------- |
| `full_load`        | 5 s        | 5 s      | up to 10 s| ~30 s       |
| `spa_transition`   | 2 s        | 2 s      | 0 s       | 4 s         |
| `no_change`        | 1 s        | 1 s      | 0 s       | 2 s         |

#### `mew-cdp` — `NavigationKind` audit (rule 6 flag)

`mew_cdp::NavigationKind` and the `decide_kind` / `classify_navigation`
helpers from phase 31 are present in the crate, but the corresponding
agent.rs wiring (work items 3-5 of phase 31) was never applied: the
agent loop still uses the boolean `is_navigation`. Phase 38 does not
silently retrofit phase 31; it re-derives the `NavigationKind`
locally inside `run_inner` using `mew_cdp::decide_kind` with the
spa-domain list from `sensitive_platforms.spa_domains()`. The bridge
is documented inline as a phase-31 follow-up.

#### `mew-perception` — cap on serialized tree size

`mew_perception::diff::serialize_full_tree` is now a thin wrapper
around a new `serialize_full_tree_capped(node, cap: Option<usize>)`.
The cap counts every emitted line; the root is always emitted so a
truncated tree still carries the page's root-context line. The
underlying `TreeNode` is never modified — the diff path and the
resilience hook still walk the full tree.

A 0 cap reproduces pre-Phase-38 behavior byte-for-byte. The recursion
helper short-circuits on `children.len() > remaining` so the cap is
safe on wide-but-shallow trees (the Maps panel shape).

#### `mew-agent` — `max_ax_tree_nodes` config field

`AgentConfig` gains `pub max_ax_tree_nodes: Option<usize>` (default
`None` = unlimited). Set in `config.yaml` as:

```yaml
agent:
  max_ax_tree_nodes: 800   # recommended for Maps tasks
```

`load_config` clamps a misconfigured `Some(0)` back to `None` so a
typo can't silently produce empty snapshots.

#### `mew-agent` — perception block wiring

`run_inner` now:

1. Derives `nav_kind: NavigationKind` from the URL delta using
   `mew_cdp::decide_kind` and the `spa_domains` list.
2. Selects `snap_config = snapshot_config_for(nav_kind)`.
3. Uses `snap_config.timeout_secs` / `snap_config.max_retries` instead
   of the hardcoded `5s` / `4`.
4. Conditionally calls `wait_for_page_settled` between retries only
   when `snap_config.settle_between_retries` is true.
5. Uses `serialize_full_tree_capped` with the configured cap.
6. Adjusts the full-snapshot diff-replacement heuristic for
   `SpaTransition` (lower threshold, `||` instead of `&&` per phase
   38 spec); `FullNavigation` and `NoChange` keep the pre-Phase-38
   `50 && 50` threshold byte-for-byte.
7. Writes a `PERCEPTION:` line to the transcript on every iteration
   with `kind`, `latency_ms`, `nodes`, `snapshot_bytes`. The
   `latency_ms` is wall-clock from the start of the perception
   block; `nodes` is the underlying tree size (pre-cap) so a
   truncated snapshot is visibly distinct from an unbounded one.

## Acceptance criteria status

- [x] **`cargo test -p mew-cdp` passes**, including a unit test that
      `SnapshotConfig::spa_transition()` returns the expected values
      (`tests::snapshot_config_spa_transition_uses_fast_path`) and
      `SnapshotConfig::full_load()` returns the pre-Phase-38 values
      (`tests::snapshot_config_full_load_preserves_pre_phase38_baseline`).
- [x] **`cargo build` succeeds with zero warnings** in the new code.
      (The workspace has pre-existing warnings in `mew-cdp`,
      `mew-agent`, `mew-cli`, and `mew-ui` that are not part of
      phase 38 — see rule 6 in `work.md` for the "don't touch old
      phases" rule.)
- [ ] **Live Maps enumeration: average perception latency per
      `SpaTransition` panel open drops by ≥ 40% vs. the pre-Phase-38
      baseline** (measured by `PERCEPTION: latency_ms` lines in the
      transcript). Operator-run, see checklist below.
- [ ] **With `max_ax_tree_nodes: 800`, a Maps panel snapshot still
      contains phone, address, rating, and website refs** — verified
      by running `extract_fields` on the capped tree and confirming
      ≥ 80% field extraction rate. Operator-run.
- [ ] **No regression on `FullNavigation` tasks**: `github.com`
      navigation still uses `full_load()` config (5s timeout, 4
      retries). Operator-run; verifiable in the transcript by
      grepping `PERCEPTION: kind=FullNavigation`.

## Operator-run live benchmark checklist

To fill in the live-numbers rows below:

1. **Baseline (pre-Phase-38)** — checkout the pre-phase-38 commit,
   run a 15-listing Maps enumeration in a known city, capture the
   transcript, parse every `Observation: N bytes` line and the
   `tokio::time::Duration::from_secs(5)` perception wall-clock. Average
   perception latency is the headline number.
2. **Phase 38 (no cap)** — check out the phase-38 commit, leave
   `max_ax_tree_nodes` unset (so the cap doesn't interfere),
   repeat the same 15-listing run. Average perception latency
   should drop by ≥ 40% — the speedup comes entirely from the
   `SnapshotConfig::spa_transition()` preset (2s timeout, no
   settle).
3. **Phase 38 (with cap)** — set `max_ax_tree_nodes: 800` in
   `config.yaml`, repeat. The perception latency should match
   the no-cap run; the change shows up in the `snapshot_bytes`
   column of the `PERCEPTION:` lines (now bounded by the cap
   instead of growing with the underlying tree size) and in the
   `extract_fields` success rate (must remain ≥ 80% on phone /
   address / rating / website).

| Metric                                    | Baseline (pre-38) | Phase 38 (no cap) | Phase 38 (cap=800) |
| ----------------------------------------- | ----------------- | ----------------- | ------------------ |
| Avg `PERCEPTION: latency_ms` per panel    |                   |                   |                    |
| 95th-pct `latency_ms`                     |                   |                   |                    |
| Max `latency_ms`                          |                   |                   |                    |
| Avg `nodes` (underlying tree)             |                   |                   |                    |
| Avg `snapshot_bytes` (post-cap)           |                   |                   |                    |
| `extract_fields` success rate (15 runs)   |                   |                   |                    |

## Notes for the reviewer

- The pre-Phase-38 hardcoded 5s/4 retries were tuned for a "page
  where the AX tree is being populated just after
  `wait_for_page_settled` returned `settled=true`" failure mode
  (see the in-line comment at `agent.rs:2002`). The
  `spa_transition()` preset drops to 2s/2 retries, which still
  gives two full attempts at 2s each = 4s total worst case —
  enough for a transient-empty-AX-tree case the old code was
  over-provisioning against.
- The `no_change()` preset (1s/1 retry) is intentionally minimal:
  on a `NoChange` iteration the tree barely changed, so if the
  first fast call missed it, the second won't either. The 1s
  budget is what `chromiumoxide::cdp::browser_protocol::accessibility::GetFullAxTreeParams`
  takes to return in the steady state on a populated tree.
- The cap is applied during serialization only. The diff path
  still sees the full `TreeNode`, so the diff heuristic isn't
  affected by the cap. The `extract_fields` tool also walks
  the underlying tree, so the cap doesn't change what the
  tool can find.
