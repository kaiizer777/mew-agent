# Evaluation Harness Pass-Rate History

Historical pass rates for `mew-agent` evaluation harness scenarios.

| Phase | Date | Scenarios | Pass Rate | Status | Notes |
|---|---|---|---|---|---|
| Phase 9 | 2026-08-05 | 10 | 100% (10/10) | Pass | Initial synthetic mock-site harness |
| Phase 17 | 2026-08-06 | 3 | 100% (3/3) | Pass | Planner-worker evidence contract & shortcut rejection |

## Phase 17 — Planner-Worker Shortcut Scenarios

- `planner_accept_on_match`: 100% (1/1) — Happy path evidence verification and signature matching.
- `planner_reject_on_mismatch`: 100% (1/1) — Rejection of fake worker signature, eventual failure state after attempt cap.
- `planner_retry_on_stale_evidence`: 100% (1/1) — Rejection of stale iteration evidence, maintaining Pending status.
