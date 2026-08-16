#!/usr/bin/env python3
"""
Phase 32 refactor: extract the single-tool dispatch block in agent.rs
into a new method `dispatch_single_tool_call`. The method returns a
`BatchStepOutcome` so the outer batch loop in `run_inner` can decide
whether to continue, break, or return.

The original block (lines 2643-4211 in the pre-refactor file) is
moved verbatim into the new method, with these adjustments:
  - removed the `let call = &calls[0];` (we receive `call` as a param)
  - removed the post-block `if let Some(result) = self.pending_finish_result.take() { return Ok(result); }`
    (the helper returns `BatchStepOutcome::Finish` instead; caller handles return)
  - added BatchStepOutcome::PageChanged in the navigate arm
  - added BatchStepOutcome::Paused in the irreversible-gate arm
"""
from __future__ import annotations
import io
import re
import sys
from pathlib import Path

SRC = Path(r"C:\Users\bari2\Desktop\mew-agent\mew-agent\src\agent.rs")
text = SRC.read_text(encoding="utf-8")
lines = text.splitlines(keepends=False)

# 1-based inclusive start, end (the original 2643..=4211)
START = 2643
END = 4211

block_lines = lines[START - 1:END]
# The block is currently indented at 16 spaces. After extraction into
# a method, we'll re-indent to 8 spaces (one level less than inside
# run_inner). The match-arm internals go down to 20 / 24 spaces; those
# stay the same relative to the new method body.
INDENT_DROP = 8

def reindent(line: str) -> str:
    # Preserve blank lines; drop exactly 8 leading spaces from
    # non-blank lines if present.
    if not line.strip():
        return line
    if line.startswith(" " * INDENT_DROP):
        return line[INDENT_DROP:]
    return line

inner = "\n".join(reindent(l) for l in block_lines)

# Build the new method. We must:
#  - replace the inside-block `let call = &calls[0];` with a
#    comment about the param.
#  - keep all the rest verbatim.
#  - wrap the body in `pub(crate) async fn dispatch_single_tool_call(...) -> BatchStepOutcome { ... }`.
#  - the post-block return (`if let Some(result) = self.pending_finish_result.take() { return Ok(result); }`)
#    is REMOVED from the helper. The finish() arm now sets
#    BatchStepOutcome::Finish instead and the caller returns Ok.
#  - the irreversible-gate arm replaces its `continue;` with a
#    `BatchStepOutcome::Paused` and returns.
#  - the navigate arm, after a successful navigate, sets
#    `BatchStepOutcome::PageChanged` and returns (before the
#    post-match log block). The post-match log still runs for
#    navigate so the LLM sees the result.

# 1. Strip the first line "let call = &calls[0];" — that was the
#    first thing inside the original block. Replace with a comment.
inner_lines = inner.split("\n")
new_inner_lines: list[str] = []
skip_first_let = True
for line in inner_lines:
    if skip_first_let and line.strip() == "let call = &calls[0];":
        skip_first_let = False
        new_inner_lines.append("// `call` is passed in by the caller; no re-binding here.")
        continue
    new_inner_lines.append(line)
inner = "\n".join(new_inner_lines)

# 2. Remove the trailing `if let Some(result) = self.pending_finish_result.take() { return Ok(result); }`
#    block. It is the LAST 4 lines of the original block (3 lines
#    of comment + 1 of code, but actually it's 2 comment + 1 code,
#    plus the `}` that closes the dispatch block). Actually:
#    line 4204: `// Phase 15.1: if the finish() tool handler stashed a`
#    line 4205: `// result (gate open), exit the loop now with that`
#    line 4206: `// result. The transcript logging above is the same`
#    line 4207: `// shape every other tool uses, so the per-tool log`
#    line 4208: `// line still records the finish() call.`
#    line 4209: `if let Some(result) = self.pending_finish_result.take() {`
#    line 4210: `    return Ok(result);`
#    line 4211: `}`
#    We want to remove lines 4209..4211 (3 lines) and keep the comments.
#    The post-match logging above (TOOL CALL log, AgentEvent::Tool emit,
#    emit_progress_line) stays — those are tool-result logging that
#    should run for EVERY tool in the batch, including navigate and
#    finish (the per-tool transcript line is still useful).
inner_lines = inner.split("\n")
# Find the line that starts with "if let Some(result) = self.pending_finish_result.take() {"
target = "if let Some(result) = self.pending_finish_result.take() {"
idx = None
for i, l in enumerate(inner_lines):
    if l.strip() == target:
        idx = i
        break
if idx is None:
    print("ERROR: could not find pending_finish_result block", file=sys.stderr)
    sys.exit(1)
# The next line is `    return Ok(result);` and the line after is `}`.
# Remove all 3 lines.
del inner_lines[idx:idx + 3]
inner = "\n".join(inner_lines)

# 3. Replace the irreversible-gate `continue;` with a return of
#    BatchStepOutcome::Paused. The line in the original is
#    `                        continue;` (24 spaces, then `continue;`).
inner = inner.replace(
    "                        continue;\n                    }\n                }\n\n                // Phase 17.1: pacing guard.",
    "                        // Phase 32: in batch mode, the irreversible\n                        // gate halts the entire batch. The user\n                        // sees the pause prompt; the next outer\n                        // iteration's checkpoint parks on Paused.\n                        return BatchStepOutcome::Paused;\n                    }\n                }\n\n                // Phase 17.1: pacing guard.",
    1,
)

# 4. After a successful navigate, the rest of the batch would act
#    on the wrong page. We set BatchStepOutcome::PageChanged and
#    return AFTER the post-match tool-result log (so the LLM still
#    gets the navigate result and the transcript still has the
#    TOOL CALL line). To do that without rewriting the post-match
#    log block twice, we add a guard variable `page_changed` and
#    check it at the end of the match arm.
#
#    Simpler: add the flag-set right after the navigate's success
#    path. We pick the first place where `tool_result` is set to
#    "Navigated successfully ..." and inject `page_changed = true;`
#    immediately after.
#    But that's brittle. Cleaner: add the flag-set at the END of
#    the navigate arm (the `=>` arm body). We detect that by
#    adding a "post-navigate page-change signal" right after the
#    `let res_json = ...` block within the navigate match arm.
#
#    To keep this surgical, we'll inject the signal right after
#    the navigate arm's final `tool_result = ...` line. We find
#    the line that starts with `tool_result = format!("Navigated successfully`
#    and append a flag-set line right after the line that closes
#    the navigate arm (`}`). The navigate arm ends with a `}` that
#    closes the inner match — but that's hard to find mechanically.
#    Better: instead, set the flag inside the navigate arm at
#    multiple points. But that's invasive.
#
#    SIMPLEST: change the return type of the method to also include
#    a per-iteration `current_url` post-dispatch, and the caller
#    compares against the URL it captured at the start of the
#    iteration. If it changed (and the tool was navigate), break.
#    But that's another variable to thread.
#
#    Let me do the SIMPLEST thing that works: the navigate arm sets
#    a flag `let mut page_changed_in_batch = false;` at the top
#    of the method and assigns `true` immediately after a successful
#    `tool_result = "Navigated successfully ..."`. I'll inject the
#    flag declaration and a marker assignment.
#
#    But this requires multiple injection points (one per
#    `tool_result = "Navigated successfully"` location, which there
#    is only one in the navigate arm).
#
#    ACTUALLY THE SIMPLEST: change the dispatch method to return
#    BatchStepOutcome::PageChanged from the navigate arm by adding
#    `return BatchStepOutcome::PageChanged;` right after the navigate
#    arm's TOOL CALL log line. The TOOL CALL log line is the LAST
#    thing the match arm does. After that, control falls through
#    to the post-match log block (TOOL CALL log + AgentEvent::Tool
#    emit + emit_progress_line). Those are in the run_inner scope,
#    not inside the match.
#
#    So if I add the `return` at the end of the navigate arm,
#    the post-match log block in the method would NEVER run for
#    navigate, which means the per-tool transcript line and the
#    AgentEvent::Tool event would be skipped for navigate. That
#    breaks backward compat.
#
#    Better solution: set a local flag at the end of the navigate
#    arm, and check it AFTER the post-match log block at the very
#    end of the method.

# Inject `let mut page_changed = false;` declaration near the top
# of the method body, right after the comment about `call` param.
inner = inner.replace(
    "// `call` is passed in by the caller; no re-binding here.\n                let call_id = call[\"id\"].as_str().unwrap_or(\"unknown_id\");",
    "// `call` is passed in by the caller; no re-binding here.\n                // Phase 32: navigate sets this so the batch loop breaks\n                // AFTER the per-tool transcript log runs.\n                let mut page_changed = false;\n                let call_id = call[\"id\"].as_str().unwrap_or(\"unknown_id\");",
    1,
)

# Inject `page_changed = true;` right after the line that ends
# the navigate arm's success path. The success path in the
# navigate arm ends with:
#   tool_result = format!(
#       "Navigated successfully (resolved \"{}\" -> \"{}\" via {})",
#       ...
#   );
# followed by a `success = true;` and the `break 'attempt_loop;`.
# We set the flag right after `success = true;`.
inner = inner.replace(
    "                                        tool_result = format!(\n                                            \"Navigated successfully (resolved \\\"{}\\\" -> \\\"{}\\\" via {})\",\n                                            resolution.original_input,\n                                            resolved_url,\n                                            resolution.path.as_str()\n                                        );\n                                        success = true;\n                                        break 'attempt_loop;",
    "                                        tool_result = format!(\n                                            \"Navigated successfully (resolved \\\"{}\\\" -> \\\"{}\\\" via {})\",\n                                            resolution.original_input,\n                                            resolved_url,\n                                            resolution.path.as_str()\n                                        );\n                                        success = true;\n                                        page_changed = true;\n                                        break 'attempt_loop;",
    1,
)

# At the end of the method, after the per-tool transcript log,
# AgentEvent::Tool emit, and emit_progress_line, we check
# `page_changed` and return BatchStepOutcome::PageChanged. The
# post-match log block ends with `self.emit_progress_line(...);`.
# We add the page_changed check + return right after that.
inner = inner.replace(
    "self.emit_progress_line(&effective_name, &effective_args, &tool_result);\n\n                // Phase 15.1:",
    "self.emit_progress_line(&effective_name, &effective_args, &tool_result);\n\n                // Phase 32: navigate changed the page; remaining\n                // tools in the batch would act on the wrong page.\n                // We still logged the per-tool transcript + emitted\n                // AgentEvent::Tool so the LLM sees the result.\n                if page_changed {\n                    return BatchStepOutcome::PageChanged;\n                }\n\n                // Phase 15.1:",
    1,
)

# The finish() arm stashes `self.pending_finish_result` and falls
# through to the post-match log block. After the log block, the
# original code returned Ok(result) — but we removed that. We
# need to return BatchStepOutcome::Finish from the method when
# the finish arm fired. Detect this by checking
# `self.pending_finish_result.is_some()` right after the
# emit_progress_line, alongside the page_changed check.
inner = inner.replace(
    "                if page_changed {\n                    return BatchStepOutcome::PageChanged;\n                }",
    "                if page_changed {\n                    return BatchStepOutcome::PageChanged;\n                }\n                if self.pending_finish_result.is_some() {\n                    return BatchStepOutcome::Finish;\n                }",
    1,
)

# 5. Build the full method header. We use the doc-comment style.
method_doc = """    /// Phase 32: dispatch a single tool call.
    ///
    /// Extracted from `run_inner` so the same dispatch body can
    /// be called multiple times per LLM turn (multi-action batch).
    /// Returns the `BatchStepOutcome` so the caller can decide
    /// whether to continue the batch, break (navigate changed the
    /// page), return (finish was honored), or pause (irreversible
    /// gate).
    pub(crate) async fn dispatch_single_tool_call(
        &mut self,
        page: &Page,
        call: &serde_json::Value,
        obs_summary: &str,
    ) -> BatchStepOutcome {
"""
method_footer = """        return BatchStepOutcome::Continue;
    }
"""

new_method = method_doc + inner + "\n" + method_footer

# 6. Replace the original block in `lines` with: (a) the call site
#    (a thin wrapper that loops calls[0..take_n] and calls the new
#    method), and (b) nothing else from the original block.
#
#    The call-site wrapper:
call_site = """                // Phase 32: multi-action batch dispatch. The LLM
                // may return several tool calls in one response.
                // We dispatch them sequentially before the next
                // LLM round-trip, bounded by `max_tools_per_turn`
                // (default 1 = pre-Phase-32 behavior).
                //
                // Per-tool invariants preserved across the batch:
                //   - pacing guard runs per tool (not per turn);
                //   - the modal-dismiss override is consumed on the
                //     first tool only (subsequent tools see the
                //     post-dismiss page and run the LLM's intent);
                //   - the irreversible-action gate runs per tool
                //     and halts the batch on Pause;
                //   - navigate / finish() break the batch loop
                //     (navigate changes the page; finish ends the
                //     session).
                let take_n = calls
                    .len()
                    .min(self.config.agent.max_tools_per_turn.max(1));
                let mut tools_dispatched_this_turn: usize = 0;
                for batch_idx in 0..take_n {
                    let outcome = self
                        .dispatch_single_tool_call(
                            page,
                            &calls[batch_idx],
                            obs_summary,
                        )
                        .await;
                    tools_dispatched_this_turn += 1;
                    match outcome {
                        BatchStepOutcome::Continue => {}
                        BatchStepOutcome::PageChanged
                        | BatchStepOutcome::Paused => {
                            break;
                        }
                        BatchStepOutcome::Finish => {
                            if let Some(result) =
                                self.pending_finish_result.take()
                            {
                                return Ok(result);
                            }
                            // Defensive: the finish arm should have
                            // stashed a result. If not, fall through
                            // and let the outer loop decide.
                            break;
                        }
                    }
                }
"""

# 7. Replace lines[START-1:END] with call_site (no trailing newline
#    artifact; we'll reconstruct with explicit join).
new_lines = lines[: START - 1] + [call_site] + lines[END:]
new_text = "\n".join(new_lines)

# 8. Append the new method + the BatchStepOutcome enum at the end
#    of the file. We place them right before the closing `}` of
#    impl Agent, but it's simpler to append at the end and let the
#    user review the diff. Actually, the new method belongs inside
#    `impl Agent { ... }`. The end of `impl Agent` is at the line
#    just before `/// Phase 2: render the PLAN: block` doc comment.
#
#    To be safe, find the end of impl Agent (the line that is
#    exactly `}` at column 0 right before the next doc comment).
#    For this codebase, that's line 4221 (`}`).
IMPL_END_LINE = 4221
impl_end_idx = IMPL_END_LINE - 1
# Verify: lines[impl_end_idx] should be "}"
assert lines[impl_end_idx].strip() == "}", f"impl end mismatch: {lines[impl_end_idx]!r}"

# Insert the BatchStepOutcome enum + new method right before
# the impl end.
enum_def = """
/// Phase 32: per-tool outcome from `dispatch_single_tool_call`.
/// The batch loop in `run_inner` reads this to decide whether
/// to continue dispatching, break (page change or pause), or
/// return (finish was honored).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BatchStepOutcome {
    /// The tool dispatched cleanly; the batch loop should
    /// continue to the next tool call (if any).
    Continue,
    /// The tool was `navigate` and changed the page; remaining
    /// tools in the batch must NOT run on the new page.
    PageChanged,
    /// The tool was `finish()` and the caller should return Ok
    /// with the finish result.
    Finish,
    /// The tool hit the irreversible-action gate; the session
    /// is now Paused and the next outer iteration should park.
    Paused,
}

"""
new_lines2 = new_lines[:impl_end_idx] + [enum_def, new_method] + new_lines[impl_end_idx:]

new_text2 = "\n".join(new_lines2)
SRC.write_text(new_text2, encoding="utf-8")
print(f"Refactor done. File length: {len(new_text2.splitlines())} lines")
