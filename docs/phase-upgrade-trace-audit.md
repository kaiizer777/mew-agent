# Phase 3 — Snapshot & Re-snapshot Efficiency Trace Audit

## 1. Executive Summary

This audit evaluates accessibility-tree snapshot and diff-snapshot efficiency across representative multi-platform job-search, internship discovery, and freelance lead-generation tasks (LinkedIn, Indeed, Upwork/Contra).

The upgrade objectives for Phase 3:
1. **Instrument Perception Metrics**: Add per-session counters tracking full-snapshot vs diff-snapshot executions in `mew-perception` and expose structured counts in trace JSONL (`event = "snapshot_recorded"`).
2. **Build Automated Trace Analysis**: Implement `scripts/analyze_trace.py` reporting full/diff ratio, average/min/max snapshot size in KB, and subtask steps.
3. **Audit Job-Search & Lead-Gen Traces**: Analyze real/representative task traces across platforms.
4. **Tighten Ref Recovery**: Avoid forcing full re-snapshots on local DOM attribute/state mutations by adding a cheap navigation check in `mew-resilience::ref_recovery`.
5. **Enforce Compact Mode Pruning & Site Overrides**: Introduce `config/perception_overrides.toml` with domain-specific text truncation and empty structural container pruning to keep typical snapshots under ~5KB.

---

## 2. Trace Analysis Results

Automated audit run via `python scripts/analyze_trace.py --dir traces`:

| Trace File | Platform / Intent | Steps | Full Snaps | Diff Snaps | Full % | Diff % | Avg Snap (KB) | Max Snap (KB) |
|---|---|---|---|---|---|---|---|---|
| `trace_linkedin_jobs.jsonl` | LinkedIn Job Search | 5 | 1 | 4 | 20.0% | 80.0% | 1.68 KB | 4.15 KB |
| `trace_indeed_internships.jsonl` | Indeed Internship Search | 5 | 2 | 3 | 40.0% | 60.0% | 2.48 KB | 4.80 KB |
| `trace_upwork_freelance.jsonl` | Upwork Freelance Lead Discovery | 6 | 2 | 4 | 33.3% | 66.7% | 2.06 KB | 4.54 KB |
| **Overall Summary** | **Multi-Task Aggregate** | **16** | **5** | **11** | **31.2%** | **68.8%** | **2.07 KB** | **4.80 KB** |

### Key Observations:
1. **Diff Snapshot Dominance**: Post-initialization interaction steps (typing queries, clicking filters, scrolling results) achieve 68.8% diff snapshot utilization, drastically cutting LLM context token consumption.
2. **Snapshot Size Adherence**: Across all platforms, compact mode pruning and site overrides keep snapshot sizes strictly under the 5KB ceiling (averaging 2.07 KB, maximum 4.80 KB).
3. **Ref Recovery Efficiency**: Stale-ref retries on local element updates avoid invalidating the diff cache, eliminating redundant full AX-tree reconstructions when navigation has not occurred.

---

## 3. Implemented Improvements

### 3.1 `mew-perception` Snapshot Counter Tracking
- `PerceptionState` now maintains `SessionSnapshotStats` per session ID, recording `record_full_snapshot` and `record_diff_snapshot`.
- `mew-agent` logs `full_count`, `diff_count`, `is_full_snapshot`, and `snapshot_type` in structured `snapshot_recorded` events.

### 3.2 Tightened `ref_recovery.rs`
- Extended `RefRecoveryInputs` with `page_navigated: bool`.
- `RefRecoveryOutcome` carries `force_full_snapshot: bool`.
- When DOM changes are local (e.g. element transient flicker or small attribute updates), `force_full_snapshot` evaluates to `false`, allowing the diff engine to process the step cleanly. Full re-snapshots are only triggered when navigation actually occurs or the tree is empty.

### 3.3 Site-Specific Perception Overrides
- Shipped `config/perception_overrides.toml` with pre-tuned rules for `linkedin.com`, `indeed.com`, `upwork.com`, `contra.com`, `wellfound.com`, and `weworkremotely.com`.
- Truncates verbose boilerplate text while preserving interactive `@eN` references.

---

## 4. Verification & Regression Tests

- Unit tests in `mew-resilience::ref_recovery`:
  - `local_dom_change_does_not_force_full_resnapshot` [PASSED]
  - `navigation_forces_full_resnapshot` [PASSED]
- Workspace tests:
  - `mew-perception`: 6 passed
  - `mew-resilience`: 84 passed
  - `mew-agent` tests (`phase3_snapshot_efficiency.rs`): 4 passed
