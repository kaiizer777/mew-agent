#!/usr/bin/env python3
"""
Phase 32 in-place refactor:
Wrap the existing single-tool-dispatch block (lines 2643-4211 in
the pre-refactor file) in a `for batch_idx in 0..take_n { ... }` loop.
The block runs N times per LLM turn, where N = min(calls.len(),
max_tools_per_turn). Default 1 = unchanged behavior.

The block uses `ref_map` from the surrounding run_inner scope
(it's the AX tree ref map from the perception block). The
loop re-uses the same `ref_map` because it does not change
between tool dispatches in the same turn.

The match arm sets a `batch_should_break: bool` flag in three
cases:
  - irreversible-action gate (instead of `continue;`).
  - navigate success (signals that the rest of the batch must
    not run on the new page).
  - finish() (the existing `pending_finish_result` check also
    handles this; we set the flag too for symmetry).
The loop checks the flag at the end of each iteration and
breaks if set.
"""
from __future__ import annotations
import re
import sys
from pathlib import Path

SRC = Path(r"C:\Users\bari2\Desktop\mew-agent\mew-agent\src\agent.rs")
text = SRC.read_text(encoding="utf-8")
lines = text.splitlines(keepends=False)

# Pre-refactor inclusive line numbers.
START = 2643
END = 4211

# 1. Read the block.
block = lines[START - 1:END]

# 2. Re-indent: add 4 spaces to every non-blank line.
def reindent(line: str) -> str:
    if not line.strip():
        return line
    return "    " + line

new_block = [reindent(l) for l in block]

# 3. Replace `let call = &calls[0];` with `let call = &calls[batch_idx];`
#    in the reindented block.
new_block_text = "\n".join(new_block)
new_block_text = new_block_text.replace(
    "                let call = &calls[0];",
    "                let call = &calls[batch_idx];",
    1,
)

# 4. Replace the irreversible-gate `continue;` with setting the
#    batch_should_break flag. The original line is
#    `                        continue;` (24 spaces then continue;).
#    After re-indent it's 28 spaces. Match the 28-space version.
new_block_text = new_block_text.replace(
    "                            continue;\n                        }\n                    }\n\n                    // Phase 17.1: pacing guard.",
    "                            // Phase 32: in batch mode, the\n                            // irreversible gate halts the entire\n                            // batch. The user sees the pause\n                            // prompt; the next outer iteration's\n                            // checkpoint parks on Paused.\n                            batch_should_break = true;\n                            continue;\n                        }\n                    }\n\n                    // Phase 17.1: pacing guard.",
    1,
)

# 5. Inject `batch_should_break = true;` after a successful
#    navigate. The success path in the navigate arm ends with
#    `break 'attempt_loop;`. Set the flag right before that.
new_block_text = new_block_text.replace(
    "                                        success = true;\n                                        break 'attempt_loop;",
    "                                        success = true;\n                                        // Phase 32: navigate changed the page; remaining\n                                        // tools in this batch must not act on it.\n                                        batch_should_break = true;\n                                        break 'attempt_loop;",
    1,
)

# 6. Convert the new_block_text back to a list of lines.
new_block = new_block_text.split("\n")

# 7. Build the loop wrapper. The wrapper is inserted at the same
#    START position.
loop_wrapper_prefix = [
    "                // Phase 32: multi-action batch dispatch.",
    "                // The LLM may return several tool calls in one",
    "                // response. We dispatch them sequentially before",
    "                // the next LLM round-trip, bounded by",
    "                // `max_tools_per_turn` (default 1 = pre-Phase-32",
    "                // behavior).",
    "                //",
    "                // Per-tool invariants preserved across the batch:",
    "                //   - pacing guard runs per tool (not per turn);",
    "                //   - the modal-dismiss override is consumed on",
    "                //     the first tool only (subsequent tools see",
    "                //     the post-dismiss page and run the LLM's",
    "                //     intent);",
    "                //   - the irreversible-action gate runs per tool",
    "                //     and halts the batch on Pause;",
    "                //   - navigate / finish() break the batch loop",
    "                //     (navigate changes the page; finish ends the",
    "                //     session).",
    "                let take_n = calls",
    "                    .len()",
    "                    .min(self.config.agent.max_tools_per_turn.max(1));",
    "                let mut batch_should_break = false;",
    "                for batch_idx in 0..take_n {",
]

# 8. After the end of the block (the closing `}` that closes the
#    dispatch block at line 4211), we need to add the loop-close
#    `}` and the post-loop check for batch_should_break.
#    Actually the original block already ends with the pending_
#    finish_result return; we keep that, then add the
#    batch_should_break check right after, then close the for.
loop_wrapper_suffix = [
    "                    // Phase 32: after the per-tool dispatch,",
    "                    // check whether the batch should stop early",
    "                    // (irreversible gate, navigate, or finish).",
    "                    // The check is after the per-tool transcript",
    "                    // log + AgentEvent::Tool emit so the LLM",
    "                    // still gets the result of the last tool in",
    "                    // the batch.",
    "                    if batch_should_break {",
    "                        break;",
    "                    }",
    "                }",
]

# 9. Assemble. The new content at the START position is:
#    loop_wrapper_prefix + new_block + loop_wrapper_suffix
new_block_full = loop_wrapper_prefix + new_block + loop_wrapper_suffix

# 10. Replace the original block with the new wrapper.
new_lines = lines[: START - 1] + new_block_full + lines[END:]

SRC.write_text("\n".join(new_lines) + "\n", encoding="utf-8")
print(f"Done. File now has {len(new_lines)} lines")
