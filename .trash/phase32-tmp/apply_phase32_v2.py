#!/usr/bin/env python3
"""
Phase 32 refactor (v2): extract the single-tool dispatch block in
agent.rs into a new method `dispatch_single_tool_call`.

The block (lines 2303-3732 in the current file) is moved verbatim
into a new async method on `impl Agent`. The method takes:
  - &mut self
  - page: &Page
  - call: &serde_json::Value
  - ref_map: &HashMap<String, BackendNodeId>
  - obs_summary: &str

It returns a `BatchStepOutcome` enum (Continue, PageChanged, Finish,
Paused) so the caller can decide whether to continue the batch,
break (navigate), return (finish), or pause (irreversible gate).

The BatchStepOutcome enum is defined inside `impl Agent` (as a
pub(crate) associated item) so it lives in the same scope as the
agent.

The call site in `run_inner` is replaced with a thin batch loop:

    let take_n = calls.len().min(
        self.config.agent.max_tools_per_turn.max(1)
    );
    for batch_idx in 0..take_n {
        match self.dispatch_single_tool_call(
            page, &calls[batch_idx], &ref_map, obs_summary
        ).await {
            BatchStepOutcome::Continue => {}
            BatchStepOutcome::PageChanged | BatchStepOutcome::Paused => break,
            BatchStepOutcome::Finish => {
                if let Some(r) = self.pending_finish_result.take() {
                    return Ok(r);
                }
                break;
            }
        }
    }
"""
from __future__ import annotations
import sys
from pathlib import Path

SRC = Path(r"C:\Users\bari2\Desktop\mew-agent\mew-agent\src\agent.rs")
text = SRC.read_text(encoding="utf-8")
lines = text.splitlines(keepends=False)

# Inclusive line numbers in the current file.
DISPATCH_START = 2303
DISPATCH_END = 3732

# impl Agent end (the `}` at column 0 after the last method).
IMPL_AGENT_END = 3743

# Sanity-check the boundaries.
assert lines[IMPL_AGENT_END - 1].strip() == "}", \
    f"impl end mismatch: line {IMPL_AGENT_END}: {lines[IMPL_AGENT_END - 1]!r}"
assert lines[DISPATCH_START - 1].strip().startswith("// Process first tool call"), \
    f"dispatch start mismatch: {lines[DISPATCH_START - 1]!r}"
assert lines[DISPATCH_END - 1].strip() == "}", \
    f"dispatch end mismatch: line {DISPATCH_END}: {lines[DISPATCH_END - 1]!r}"

# 1. Read the dispatch block.
block = lines[DISPATCH_START - 1:DISPATCH_END]

# 2. Re-indent: drop 4 leading spaces from each non-blank line so
#    the block becomes a method body at the correct indent level
#    (8 spaces, not 16).
INDENT_DROP = 4
def reindent(line: str) -> str:
    if not line.strip():
        return line
    if line.startswith(" " * INDENT_DROP):
        return line[INDENT_DROP:]
    return line

inner = "\n".join(reindent(l) for l in block)

# 3. Replace `let call = &calls[0];` with a comment about the param
#    (the method receives `call` from the caller).
inner = inner.replace(
    "    let call = &calls[0];",
    "    // `call` is the specific tool call from the batch.\n"
    "    // The caller passes `&calls[batch_idx]`; we don't re-bind.",
    1,
)

# 4. Remove the trailing `if let Some(result) = self.pending_finish_result.take() { return Ok(result); }`
#    block. It's the LAST 3 lines of the dispatch block. The finish
#    arm already stashes the result in `self.pending_finish_result`;
#    the helper returns BatchStepOutcome::Finish instead of returning
#    directly. The caller handles the return.
inner_lines = inner.split("\n")
target = "if let Some(result) = self.pending_finish_result.take() {"
idx = None
for i, l in enumerate(inner_lines):
    if l.strip() == target:
        idx = i
        break
if idx is None:
    print("ERROR: pending_finish_result block not found", file=sys.stderr)
    sys.exit(1)
del inner_lines[idx:idx + 3]
inner = "\n".join(inner_lines)

# 5. Replace the irreversible-gate `continue;` with BatchStepOutcome::Paused.
#    The original is 28-space-indented `continue;` (was 24 before,
#    we dropped 4). Find the exact context and replace.
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
    print("ERROR: irreversible-gate context not found", file=sys.stderr)
    # Debug: dump context around the 'continue;' line
    for i, l in enumerate(inner_lines):
        if "continue;" in l:
            print(f"  inner line {i}: {l!r}", file=sys.stderr)
            for j in range(max(0, i - 3), min(len(inner_lines), i + 6)):
                print(f"    line {j}: {inner_lines[j]!r}", file=sys.stderr)
    sys.exit(1)
inner = inner.replace(old_gate, new_gate, 1)

# 6. Set `page_changed = true;` after a successful navigate.
old_nav = (
    "                                    success = true;\n"
    "                                    break;"
)
new_nav = (
    "                                    success = true;\n"
    "                                    // Phase 32: navigate\n"
    "                                    // changed the page; the\n"
    "                                    // rest of the batch must\n"
    "                                    // not act on it.\n"
    "                                    page_changed = true;\n"
    "                                    break;"
)
if old_nav not in inner:
    print("ERROR: navigate success context not found", file=sys.stderr)
    # Debug: find all success = true;
    for i, l in enumerate(inner_lines):
        if "success = true;" in l:
            print(f"  inner line {i}: {l!r}", file=sys.stderr)
            for j in range(max(0, i - 3), min(len(inner_lines), i + 6)):
                print(f"    line {j}: {inner_lines[j]!r}", file=sys.stderr)
            print("---", file=sys.stderr)
    sys.exit(1)
inner = inner.replace(old_nav, new_nav, 1)

# 7. Declare `let mut page_changed = false;` right after the
#    per-method comment.
inner = inner.replace(
    "    // `call` is the specific tool call from the batch.\n"
    "    // The caller passes `&calls[batch_idx]`; we don't re-bind.\n"
    "    let call_id = call[\"id\"].as_str().unwrap_or(\"unknown_id\");",
    "    // `call` is the specific tool call from the batch.\n"
    "    // The caller passes `&calls[batch_idx]`; we don't re-bind.\n"
    "    // Phase 32: navigate sets this so the caller breaks the\n"
    "    // batch AFTER the per-tool transcript log runs.\n"
    "    let mut page_changed = false;\n"
    "    let call_id = call[\"id\"].as_str().unwrap_or(\"unknown_id\");",
    1,
)

# 8. Add the post-dispatch outcome check at the end of the method
#    body, right after the existing `self.emit_progress_line(...)`.
old_post = "self.emit_progress_line(&effective_name, &effective_args, &tool_result);\n\n    // Phase 15.1:"
new_post = (
    "self.emit_progress_line(&effective_name, &effective_args, &tool_result);\n\n"
    "    // Phase 32: navigate changed the page; remaining tools\n"
    "    // in the batch would act on the wrong page. We still\n"
    "    // logged the per-tool transcript + emitted AgentEvent::Tool\n"
    "    // so the LLM sees the result of the last tool in the\n"
    "    // batch.\n"
    "    if page_changed {\n"
    "        return BatchStepOutcome::PageChanged;\n"
    "    }\n"
    "    if self.pending_finish_result.is_some() {\n"
    "        return BatchStepOutcome::Finish;\n"
    "    }\n\n"
    "    // Phase 15.1:"
)
if old_post not in inner:
    print("ERROR: post-dispatch emit_progress_line context not found", file=sys.stderr)
    # Debug
    for i, l in enumerate(inner_lines):
        if "emit_progress_line" in l:
            print(f"  inner line {i}: {l!r}", file=sys.stderr)
            for j in range(max(0, i - 2), min(len(inner_lines), i + 5)):
                print(f"    line {j}: {inner_lines[j]!r}", file=sys.stderr)
    sys.exit(1)
inner = inner.replace(old_post, new_post, 1)

# 9. Build the full method (with doc comment + signature + body + footer).
method_doc = (
    "    /// Phase 32: dispatch a single tool call.\n"
    "    ///\n"
    "    /// Extracted from `run_inner` so the same dispatch body can\n"
    "    /// be called multiple times per LLM turn (multi-action\n"
    "    /// batch). Returns the `BatchStepOutcome` so the caller can\n"
    "    /// decide whether to continue the batch, break (navigate\n"
    "    /// changed the page), return (finish was honored), or pause\n"
    "    /// (irreversible gate).\n"
    "    pub(crate) async fn dispatch_single_tool_call(\n"
    "        &mut self,\n"
    "        page: &Page,\n"
    "        call: &serde_json::Value,\n"
    "        ref_map: &std::collections::HashMap<String, chromiumoxide::cdp::browser_protocol::dom::BackendNodeId>,\n"
    "        obs_summary: &str,\n"
    "    ) -> BatchStepOutcome {\n"
)
method_footer = (
    "        BatchStepOutcome::Continue\n"
    "    }\n"
)
new_method = method_doc + inner + "\n" + method_footer

# 10. Build the call-site replacement (the batch loop).
call_site = (
    "                // Phase 32: multi-action batch dispatch.\n"
    "                // The LLM may return several tool calls in one\n"
    "                // response. We dispatch them sequentially before\n"
    "                // the next LLM round-trip, bounded by\n"
    "                // `max_tools_per_turn` (default 1 = pre-Phase-32\n"
    "                // behavior).\n"
    "                //\n"
    "                // Per-tool invariants preserved across the batch:\n"
    "                //   - pacing guard runs per tool (not per turn);\n"
    "                //   - the modal-dismiss override is consumed on\n"
    "                //     the first tool only (subsequent tools see\n"
    "                //     the post-dismiss page and run the LLM's\n"
    "                //     intent);\n"
    "                //   - the irreversible-action gate runs per tool\n"
    "                //     and halts the batch on Pause;\n"
    "                //   - navigate / finish() break the batch loop\n"
    "                //     (navigate changes the page; finish ends\n"
    "                //     the session).\n"
    "                let take_n = calls\n"
    "                    .len()\n"
    "                    .min(self.config.agent.max_tools_per_turn.max(1));\n"
    "                for batch_idx in 0..take_n {\n"
    "                    let outcome = self\n"
    "                        .dispatch_single_tool_call(\n"
    "                            page,\n"
    "                            &calls[batch_idx],\n"
    "                            &ref_map,\n"
    "                            obs_summary,\n"
    "                        )\n"
    "                        .await;\n"
    "                    match outcome {\n"
    "                        BatchStepOutcome::Continue => {}\n"
    "                        BatchStepOutcome::PageChanged\n"
    "                        | BatchStepOutcome::Paused => break,\n"
    "                        BatchStepOutcome::Finish => {\n"
    "                            if let Some(result) =\n"
    "                                self.pending_finish_result.take()\n"
    "                            {\n"
    "                                return Ok(result);\n"
    "                            }\n"
    "                            break;\n"
    "                        }\n"
    "                    }\n"
    "                }\n"
)

# 11. Replace the dispatch block (DISPATCH_START..=DISPATCH_END)
#     with the call-site replacement.
new_lines = (
    lines[: DISPATCH_START - 1]
    + [call_site]
    + lines[DISPATCH_END:]
)

# 12. Insert the new method + the BatchStepOutcome enum at the
#     end of impl Agent (just before the closing `}` at line
#     IMPL_AGENT_END in the original file, which is now at a
#     different index because step 11 changed the file).
# We need to find the new index of the impl Agent closing `}`
# AFTER step 11. The line that was at IMPL_AGENT_END is now at
# index (IMPL_AGENT_END - 1) - (DISPATCH_END - DISPATCH_START) + len(call_site)
# but `call_site` is a single string in the array, not split into
# lines, so the count is off by the number of lines in call_site.
# Easier: search for the first `}` at column 0 that comes after
# the dispatch block.
impl_end_idx = None
for i in range(len(new_lines)):
    if new_lines[i].strip() == "}":
        # Check that the next non-blank line is a top-level doc comment
        # for a free function, not another method.
        j = i + 1
        while j < len(new_lines) and not new_lines[j].strip():
            j += 1
        if j < len(new_lines) and new_lines[j].lstrip().startswith("///"):
            # This is a free-function doc comment. The `}` is the
            # end of `impl Agent`.
            impl_end_idx = i
            break

if impl_end_idx is None:
    print("ERROR: could not find impl Agent end", file=sys.stderr)
    sys.exit(1)

# Build the BatchStepOutcome enum (placed right before the new method).
enum_def = (
    "    /// Phase 32: per-tool outcome from\n"
    "    /// `dispatch_single_tool_call`. The batch loop in\n"
    "    /// `run_inner` reads this to decide whether to continue\n"
    "    /// dispatching, break (page change or pause), or return\n"
    "    /// (finish was honored).\n"
    "    #[derive(Debug, Clone, Copy, PartialEq, Eq)]\n"
    "    pub(crate) enum BatchStepOutcome {\n"
    "        /// The tool dispatched cleanly; the batch loop\n"
    "        /// should continue to the next tool call (if any).\n"
    "        Continue,\n"
    "        /// The tool was `navigate` and changed the page;\n"
    "        /// remaining tools in the batch must NOT run on\n"
    "        /// the new page.\n"
    "        PageChanged,\n"
    "        /// The tool was `finish()` and the caller should\n"
    "        /// return Ok with the finish result.\n"
    "        Finish,\n"
    "        /// The tool hit the irreversible-action gate; the\n"
    "        /// session is now Paused and the next outer\n"
    "        /// iteration should park.\n"
    "        Paused,\n"
    "    }\n"
    "\n"
)

new_lines2 = (
    new_lines[:impl_end_idx]
    + [enum_def, new_method]
    + new_lines[impl_end_idx:]
)

SRC.write_text("\n".join(new_lines2) + "\n", encoding="utf-8")
print(f"Done. File now has {len(new_lines2)} lines")
