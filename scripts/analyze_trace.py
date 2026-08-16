#!/usr/bin/env python3
"""
scripts/analyze_trace.py — Analyze MEW trace logs for perception efficiency and task metrics.

Reports:
  - Full vs Diff snapshot ratio
  - Snapshot size statistics (average, min, max in KB)
  - Steps and snapshot counts per subtask / session
"""

import sys
import os
import json
import glob
import argparse
from typing import List, Dict, Any, Optional

def analyze_trace_file(filepath: str) -> Dict[str, Any]:
    full_count = 0
    diff_count = 0
    snapshot_sizes_bytes = []
    subtasks = {}
    current_subtask = "default"
    total_steps = 0
    session_id = os.path.splitext(os.path.basename(filepath))[0]

    with open(filepath, "r", encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                data = json.loads(line)
            except Exception:
                continue

            event = data.get("event")
            if not event and "fields" in data:
                event = data.get("fields", {}).get("event")

            # Check session_id if present
            if "session_id" in data:
                session_id = data["session_id"]

            # Subtask transitions
            if event == "todo_lifecycle":
                phase = data.get("phase", "")
                todo_id = data.get("todo_id") or data.get("task_id") or "unknown"
                if phase in ("start", "running"):
                    current_subtask = todo_id
                    if current_subtask not in subtasks:
                        subtasks[current_subtask] = {
                            "steps": 0,
                            "full_snapshots": 0,
                            "diff_snapshots": 0,
                            "snapshot_sizes": []
                        }

            # Snapshot recording events
            if event == "snapshot_recorded":
                total_steps += 1
                is_full = data.get("is_full_snapshot", False)
                snap_type = data.get("snapshot_type")
                if snap_type == "full" or is_full or data.get("is_full_replace", False):
                    full_count += 1
                    subtask_is_full = True
                else:
                    diff_count += 1
                    subtask_is_full = False

                obs_bytes = data.get("obs_bytes", 0)
                snapshot_sizes_bytes.append(obs_bytes)

                if current_subtask not in subtasks:
                    subtasks[current_subtask] = {
                        "steps": 0,
                        "full_snapshots": 0,
                        "diff_snapshots": 0,
                        "snapshot_sizes": []
                    }
                subtasks[current_subtask]["steps"] += 1
                if subtask_is_full:
                    subtasks[current_subtask]["full_snapshots"] += 1
                else:
                    subtasks[current_subtask]["diff_snapshots"] += 1
                subtasks[current_subtask]["snapshot_sizes"].append(obs_bytes)

            elif event == "perception_recorded":
                # If snapshot_recorded was not emitted or for additional size checks
                obs_bytes = data.get("snapshot_bytes", 0)
                if not snapshot_sizes_bytes and obs_bytes > 0:
                    snapshot_sizes_bytes.append(obs_bytes)

    total_snapshots = full_count + diff_count
    full_ratio = (full_count / total_snapshots * 100.0) if total_snapshots > 0 else 0.0
    diff_ratio = (diff_count / total_snapshots * 100.0) if total_snapshots > 0 else 0.0

    avg_kb = (sum(snapshot_sizes_bytes) / len(snapshot_sizes_bytes) / 1024.0) if snapshot_sizes_bytes else 0.0
    min_kb = (min(snapshot_sizes_bytes) / 1024.0) if snapshot_sizes_bytes else 0.0
    max_kb = (max(snapshot_sizes_bytes) / 1024.0) if snapshot_sizes_bytes else 0.0
    total_kb = (sum(snapshot_sizes_bytes) / 1024.0) if snapshot_sizes_bytes else 0.0

    return {
        "file": filepath,
        "session_id": session_id,
        "total_snapshots": total_snapshots,
        "full_snapshots": full_count,
        "diff_snapshots": diff_count,
        "full_ratio_pct": full_ratio,
        "diff_ratio_pct": diff_ratio,
        "avg_snapshot_kb": avg_kb,
        "min_snapshot_kb": min_kb,
        "max_snapshot_kb": max_kb,
        "total_snapshot_kb": total_kb,
        "total_steps": total_steps,
        "subtasks": subtasks,
    }

def print_report(results: List[Dict[str, Any]]):
    print("=" * 80)
    print("MEW TRACE ANALYSIS AUDIT REPORT")
    print("=" * 80)
    
    total_all_snapshots = sum(r["total_snapshots"] for r in results)
    total_full = sum(r["full_snapshots"] for r in results)
    total_diff = sum(r["diff_snapshots"] for r in results)
    all_sizes = []
    for r in results:
        for s in r["subtasks"].values():
            all_sizes.extend(s.get("snapshot_sizes", []))

    overall_avg_kb = (sum(all_sizes) / len(all_sizes) / 1024.0) if all_sizes else 0.0
    overall_full_pct = (total_full / total_all_snapshots * 100.0) if total_all_snapshots > 0 else 0.0
    overall_diff_pct = (total_diff / total_all_snapshots * 100.0) if total_all_snapshots > 0 else 0.0

    print(f"Traces Analyzed       : {len(results)}")
    print(f"Total Snapshots       : {total_all_snapshots}")
    print(f"Full Snapshots        : {total_full} ({overall_full_pct:.1f}%)")
    print(f"Diff Snapshots        : {total_diff} ({overall_diff_pct:.1f}%)")
    print(f"Overall Full:Diff     : 1:{total_diff / total_full:.2f}" if total_full > 0 else "Overall Full:Diff : N/A")
    print(f"Overall Avg Snapshot  : {overall_avg_kb:.2f} KB")
    print("-" * 80)

    for i, r in enumerate(results, 1):
        print(f"\n[{i}] File: {os.path.basename(r['file'])} (Session: {r['session_id']})")
        print(f"    Total Snapshots    : {r['total_snapshots']}")
        print(f"    Full / Diff Ratio  : {r['full_snapshots']} Full ({r['full_ratio_pct']:.1f}%) / {r['diff_snapshots']} Diff ({r['diff_ratio_pct']:.1f}%)")
        print(f"    Snapshot Sizes (KB): Avg = {r['avg_snapshot_kb']:.2f} KB | Min = {r['min_snapshot_kb']:.2f} KB | Max = {r['max_snapshot_kb']:.2f} KB")
        print(f"    Total Data Size    : {r['total_snapshot_kb']:.2f} KB across {r['total_steps']} steps")
        
        if r["subtasks"]:
            print("    Subtasks Breakdown :")
            for sub_id, data in r["subtasks"].items():
                s_avg = (sum(data["snapshot_sizes"]) / len(data["snapshot_sizes"]) / 1024.0) if data["snapshot_sizes"] else 0.0
                print(f"      - {sub_id:<20}: {data['steps']} steps (Full: {data['full_snapshots']}, Diff: {data['diff_snapshots']}, Avg: {s_avg:.2f} KB)")
    print("\n" + "=" * 80)

def main():
    parser = argparse.ArgumentParser(description="Analyze MEW tracing logs.")
    parser.add_argument("paths", nargs="*", help="Trace jsonl files or directories containing trace files.")
    parser.add_argument("--dir", dest="trace_dir", help="Directory containing trace files (e.g. MEW_TRACING_DIR).")
    parser.add_argument("--json", action="store_true", help="Output summary as JSON.")
    args = parser.parse_args()

    target_files = []
    
    if args.trace_dir:
        pattern = os.path.join(args.trace_dir, "*.jsonl")
        target_files.extend(glob.glob(pattern))

    for p in args.paths:
        if os.path.isdir(p):
            target_files.extend(glob.glob(os.path.join(p, "*.jsonl")))
        elif os.path.isfile(p):
            target_files.append(p)

    if not target_files:
        env_dir = os.environ.get("MEW_TRACING_DIR")
        if env_dir and os.path.isdir(env_dir):
            target_files.extend(glob.glob(os.path.join(env_dir, "*.jsonl")))

    if not target_files:
        print("No trace files found to analyze. Specify file paths or --dir.", file=sys.stderr)
        sys.exit(1)

    results = [analyze_trace_file(f) for f in sorted(target_files)]

    if args.json:
        print(json.dumps(results, indent=2))
    else:
        print_report(results)

if __name__ == "__main__":
    main()
