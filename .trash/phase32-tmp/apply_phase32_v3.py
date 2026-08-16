#!/usr/bin/env python3
"""
Phase 32 refactor v3 — robust, single-pass extraction.

Reads the file as a list of lines, identifies the dispatch block
by its unique start comment and its end (the last
`pending_finish_result.take()` + `return Ok(result)` triplet),
reindents the block to be a method body, and inserts the new
method + enum at the end of impl Agent.

The new method takes:
  - &mut self
  - page: &Page
  - call: &serde_json::Value
  - ref_map: &HashMap<String, BackendNodeId>
  - obs_summary: &str

It returns BatchStepOutcome (Continue, PageChanged, Finish, Paused).

The call site in run_inner becomes a thin batch loop.
"""
from __future__ import annotations
import sys
from pathlib import Path

SRC = Path(r"C:\Users\bari2\Desktop\mew-agent\mew-agent\src\agent.rs")
text = SRC.read_text(encoding="utf-8")
lines = text.splitlines(keepends=False)

# 1. Find dispatch block boundaries by line content (search once).
START_MARKER = "// Process first tool call (single action per turn for now)"
END_LINE_1 = "if let Some(result) = self.pending_finish_result.take() {"
END_LINE_2 = "                    return Ok(result);"
END_LINE_3 = "                }"

dispatch_start = None
for i, l in enumerate(lines):
    if l.strip() == START_MARKER:
        dispatch_start = i
        break
if dispatch_start is None:
    sys.exit("START marker not found")

# Find the LAST occurrence of END_LINE_1 (so we don't pick up an
# earlier match if any). The end of the dispatch block is at the
# end of the run_inner tool dispatch.
dispatch_end_1 = None
for i in range(len(lines) - 1, -1, -1):
    if lines[i].strip() == END_LINE_1:
        dispatch_end_1 = i
        break
if dispatch_end_1 is None:
    sys.exit("END_LINE_1 not found")
# Verify END_LINE_2 and END_LINE_3 follow.
if lines[dispatch_end_1 + 1].strip() != END_LINE_2.strip():
    sys.exit(f"END_LINE_2 mismatch: {lines[dispatch_end_1 + 1]!r}")
if lines[dispatch_end_1 + 2].strip() != END_LINE_3.strip():
    sys.exit(f"END_LINE_3 mismatch: {lines[dispatch_end_1 + 2]!r}")
dispatch_end = dispatch_end_1 + 2  # inclusive index of closing `}`

print(f"dispatch_start={dispatch_start + 1} dispatch_end={dispatch_end + 1}")

# 2. Find impl Agent end: the first `}` at column 0 AFTER dispatch_end
# that is followed by a top-level doc comment (free fn).
impl_end = None
for i in range(dispatch_end + 1, len(lines)):
    if lines[i].strip() == "}":
        # Look ahead for a doc comment or a known free fn marker.
        j = i + 1
        while j < len(lines) and not lines[j].strip():
            j += 1
        if j < len(lines) and lines[j].lstrip().startswith("///"):
            impl_end = i
            break
        if j < len(lines) and lines[j].lstrip().startswith("/// Phase 2:"):
            impl_end = i
            break
if impl_end is None:
    sys.exit("impl Agent end not found")
print(f"impl_end={impl_end + 1}")

# 3. Extract the dispatch block.
block = lines[dispatch_start:dispatch_end + 1]

# 4. Reindent: drop 4 leading spaces from each non-blank line so
#    the block becomes a method body at 12 spaces (was 16).
def reindent(line: str) -> str:
    if not line.strip():
        return line
    if line.startswith("    "):
        return line[4:]
    return line

inner_lines = [reindent(l) for l in block]
inner = "\n".join(inner_lines)

# 5. Replace `let call = &calls[0];` with a comment about the param.
inner = inner.replace(
    "    let call = &calls[0];",
    "    // `call` is the specific tool call from the batch.\n"
    "    // The caller passes `&calls[batch_idx]`; we don't re-bind.\n"
    "    // Phase 32: navigate sets this so the caller breaks the\n"
    "    // batch AFTER the per-tool transcript log runs.\n"
    "    let mut page_changed = false;\n"
    "    let call_id = call[\"id\"].as_str().unwrap_or(\"unknown_id\");",
    1,
)

# 6. Remove the trailing `if let Some(result) = ...` block.
inner_lines = inner.split("\n")
target = "if let Some(result) = self.pending_finish_result.take() {"
idx = None
for i, l in enumerate(inner_lines):
    if l.strip() == target:
        idx = i
        break
if idx is None:
    sys.exit("pending_finish_result block not found in inner")
del inner_lines[idx:idx + 3]
inner = "\n".join(inner_lines)

# 7. Replace the irreversible-gate `continue;` with the
#    BatchStepOutcome::Paused return.
old_gate = (
    "                    continue;\n"
    "                }\n"
    "            }\n"
    "\n"
    "            // Phase 17.1: pacing guard. Decide *before* dispatch"
)
new_gate = (
    "                    // Phase 32: in batch mode, the irreversible\n"
    "                    // gate halts the entire batch. The user\n"
    "                    // sees the pause prompt; the next outer\n"
    "                    // iteration's checkpoint parks on Paused.\n"
    "                    return BatchStepOutcome::Paused;\n"
    "                }\n"
    "            }\n"
    "\n"
    "            // Phase 17.1: pacing guard. Decide *before* dispatch"
)
if old_gate not in inner:
    # Find the actual continue; to debug
    for i, l in enumerate(inner.split("\n")):
        if "continue;" in l:
            ctx = inner.split("\n")[max(0, i - 3):min(len(inner), i + 6)]
            for j, c in enumerate(ctx):
                print(f"  ctx{j}: {c!r}")
            print("---")
            break
    sys.exit("irreversible-gate context not found")
inner = inner.replace(old_gate, new_gate, 1)

# 8. After a successful navigate, set page_changed = true.
old_nav = "                                    success = true;\n                                    break;"
new_nav = (
    "                                    success = true;\n"
    "                                    // Phase 32: navigate changed\n"
    "                                    // the page; the rest of the\n"
    "                                    // batch must not act on it.\n"
    "                                    page_changed = true;\n"
    "                                    break;"
)
# There are multiple `success = true; break;` blocks (navigate,
# click retry, type retry). We need only the navigate one. The
# navigate one is preceded by a "Navigated successfully"
# tool_result format. Find the navigate-specific one.
navigate_marker = (
    '                                    tool_result = format!(\n'
    '                                        "Navigated successfully '
)
idx = inner.find(navigate_marker)
if idx < 0:
    sys.exit("navigate success marker not found")
# Find the next "success = true;" after this marker.
search_from = idx
nav_success_idx = inner.find("success = true;", search_from)
if nav_success_idx < 0:
    sys.exit("navigate success not found")
# Find the next "break;" after nav_success_idx.
nav_break_idx = inner.find("break;", nav_success_idx)
if nav_break_idx < 0:
    sys.exit("navigate break not found")
# Replace "success = true;" with the new version, leaving the
# `break;` alone (it's already there).
inner = (
    inner[:nav_success_idx]
    + "                                    success = true;\n"
    + "                                    // Phase 32: navigate changed\n"
    + "                                    // the page; the rest of the\n"
    + "                                    // batch must not act on it.\n"
    + "                                    page_changed = true;"
    + inner[nav_success_idx + len("success = true;"):]
)

# 9. Add the post-dispatch outcome check at the end of the method
#    body, right after the existing `self.emit_progress_line(...)`.
old_post = (
    "self.emit_progress_line(&effective_name, &effective_args, &tool_result);\n"
    "\n"
    "            // Phase 15.1: if the finish() tool handler stashed a"
)
new_post = (
    "self.emit_progress_line(&effective_name, &effective_args, &tool_result);\n"
    "\n"
    "            // Phase 32: navigate changed the page; remaining\n"
    "            // tools in the batch would act on the wrong page.\n"
    "            // We still logged the per-tool transcript +\n"
    "            // emitted AgentEvent::Tool so the LLM sees the\n"
    "            // result of the last tool in the batch.\n"
    "            if page_changed {\n"
    "                return BatchStepOutcome::PageChanged;\n"
    "            }\n"
    "            if self.pending_finish_result.is_some() {\n"
    "                return BatchStepOutcome::Finish;\n"
    "            }\n"
    "\n"
    "            // Phase 15.1: if the finish() tool handler stashed a"
)
if old_post not in inner:
    sys.exit("post-dispatch emit_progress_line context not found")
inner = inner.replace(old_post, new_post, 1)

# 10. Build the full method.
method_doc = (
    "    /// Phase 32: dispatch a single tool call.\n"
    "    ///\n"
    "    /// Extracted from `run_inner` so the same dispatch body can\n"
    "    /// be called multiple times per LLM turn (multi-action\n"
    "    /// batch). Returns the `BatchStepOutcome` so the caller can\n"
    "    /// decide whether to continue the batch, break (navigate\n"
    "    /// changed the page), return (finish was honored), or\n"
    "    /// pause (irreversible gate).\n"
    "    pub(crate) async fn dispatch_single_tool_call(\n"
    "        &mut self,\n"
    "        page: &Page,\n"
    "        call: &serde_json::Value,\n"
    "        ref_map: &std::collections::HashMap<\n"
    "            String,\n"
    "            chromiumoxide::cdp::browser_protocol::dom::BackendNodeId,\n"
    "        >,\n"
    "        obs_summary: &str,\n"
    "    ) -> BatchStepOutcome {\n"
)
method_footer = (
    "        BatchStepOutcome::Continue\n"
    "    }\n"
)
new_method = method_doc + inner + "\n" + method_footer

# 11. Build the call-site replacement (the batch loop).
call_site_lines = [
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
    "                for batch_idx in 0..take_n {",
    "                    let outcome = self",
    "                        .dispatch_single_tool_call(",
    "                            page,",
    "                            &calls[batch_idx],",
    "                            &ref_map,",
    "                            obs_summary,",
    "                        )",
    "                        .await;",
    "                    match outcome {",
    "                        BatchStepOutcome::Continue => {}",
    "                        BatchStepOutcome::PageChanged",
    "                        | BatchStepOutcome::Paused => break,",
    "                        BatchStepOutcome::Finish => {",
    "                            if let Some(result) =",
    "                                self.pending_finish_result.take()",
    "                            {",
    "                                return Ok(result);",
    "                            }",
    "                            break;",
    "                        }",
    "                    }",
    "                }",
]

# 12. Replace the dispatch block (dispatch_start..=dispatch_end)
#     with the call-site replacement.
new_lines = lines[:dispatch_start] + call_site_lines + lines[dispatch_end + 1:]

# 13. Insert the new method + the BatchStepOutcome enum at the
#     end of impl Agent (impl_end in the ORIGINAL file, which
#     is now at a different index in new_lines because the call
#     site is shorter than the dispatch block).
#
# Compute the offset.
removed_count = (dispatch_end + 1) - dispatch_start
added_count = len(call_site_lines)
delta = added_count - removed_count
impl_end_new = impl_end + delta

# Verify the new impl_end is still a `}`.
assert new_lines[impl_end_new].strip() == "}", \
    f"new impl end mismatch: {new_lines[impl_end_new]!r}"

# Build the enum + new method.
enum_def = (
    "    /// Phase 32: per-tool outcome from\n"
    "    /// `dispatch_single_tool_call`. The batch loop in\n"
    "    /// `run_inner` reads this to decide whether to continue\n"
    "    /// dispatching, break (page change or pause), or return\n"
    "    /// (finish was honored).\n"
    "    #[derive(Debug, Clone, Copy, PartialEq, Eq)]\n"
    "    pub(crate) enum BatchStepOutcome {\n"
    "        /// The tool dispatched cleanly; the batch loop should\n"
    "        /// continue to the next tool call (if any).\n"
    "        Continue,\n"
    "        /// The tool was `navigate` and changed the page;\n"
    "        /// remaining tools in the batch must NOT run on the\n"
    "        /// new page.\n"
    "        PageChanged,\n"
    "        /// The tool was `finish()` and the caller should\n"
    "        /// return Ok with the finish result.\n"
    "        Finish,\n"
    "        /// The tool hit the irreversible-action gate; the\n"
    "        /// session is now Paused and the next outer iteration\n"
    "        /// should park.\n"
    "        Paused,\n"
    "    }\n"
    "\n"
)
enum_def_lines = enum_def.split("\n")
# Note: enum_def ends with "\n", so split("\n") gives a trailing
# empty string. Filter it.
enum_def_lines = [l for l in enum_def_lines if l != ""]
# Actually we want the blank line separator between enum and method.
# Re-build carefully.
enum_lines = [
    "    /// Phase 32: per-tool outcome from",
    "    /// `dispatch_single_tool_call`. The batch loop in",
    "    /// `run_inner` reads this to decide whether to continue",
    "    /// dispatching, break (page change or pause), or return",
    "    /// (finish was honored).",
    "    #[derive(Debug, Clone, Copy, PartialEq, Eq)]",
    "    pub(crate) enum BatchStepOutcome {",
    "        /// The tool dispatched cleanly; the batch loop should",
    "        /// continue to the next tool call (if any).",
    "        Continue,",
    "        /// The tool was `navigate` and changed the page;",
    "        /// remaining tools in the batch must NOT run on the",
    "        /// new page.",
    "        PageChanged,",
    "        /// The tool was `finish()` and the caller should",
    "        /// return Ok with the finish result.",
    "        Finish,",
    "        /// The tool hit the irreversible-action gate; the",
    "        /// session is now Paused and the next outer iteration",
    "        /// should park.",
    "        Paused,",
    "    }",
    "",
]

method_lines = new_method.split("\n")
# new_method ends with "\n", so split("\n") gives a trailing empty.
# Filter.
method_lines = [l for l in method_lines if l != ""]

new_lines2 = (
    new_lines[:impl_end_new]
    + enum_lines
    + method_lines
    + new_lines[impl_end_new:]
)

SRC.write_text("\n".join(new_lines2) + "\n", encoding="utf-8")
print(f"Done. File now has {len(new_lines2)} lines")
