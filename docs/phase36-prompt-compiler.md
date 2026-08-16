# Phase 36 — Agent-Optimized Task Prompt Compiler

**Status:** Implemented. `cargo test -p mew-agent` (222 tests, 1
pre-existing ignored) and `cargo test -p mew-agent --features eval`
(262 tests) both green. `cargo build --workspace` succeeds with
zero new warnings. Live-transcript spot-check passes on three
realistic user inputs — see
`mew-agent/examples/phase36_prompt_compiler.rs` for the runnable
verification harness.

## What changed

Users naturally write task prompts for human readers: "narrate as
you go", "move quickly but visibly", "don't batch or skip the
click-through", "use declare_subtasks". None of those directives
have a counterpart in the agent's tool-calling model — there is
no narration tool, speed is a `PacingGuard` config concern (Phase
17.1), batching is an LLM round-trip optimization (Phase 32) the
user can't see, and the Maps vertical *forbids* `declare_subtasks`
(Phase 34). When these directives reach the LLM as part of the
task string, the LLM either hallucinates a tool that doesn't
exist, or wastes an LLM turn trying to comply.

Phase 36 adds a pre-flight prompt compiler that normalizes the
raw user task into an agent-executable form *before* either
message is constructed. The compiler is a pure-Rust, deterministic
rule set — no LLM call, no `regex` crate, no new dependency. It
runs on the agent's own thread at construction time and is total
(empty input is a valid compilation result).

## Module surface

`mew_agent::prompt_compiler` is the new module. The full public
surface is:

```rust
pub struct CompiledTask {
    pub agent_task: String,
    pub execution_hints: Vec<String>,
    pub strip_warnings: Vec<String>,
}

pub fn compile(
    task: &str,
    shape: TaskShape,
    query: Option<&MapsQuery>,
) -> CompiledTask;
```

The `Option<&MapsQuery>` parameter is reserved for future
per-vertical hint customization. The current Maps hint set is
shape-driven (the Phase 34 spec calls for the hints to fire
"regardless of what the user wrote"), so the constructor passes
`None` and the parameter is currently unused. Locking the public
signature now means a future revision can extend without breaking
call sites.

## Rule set

In execution order, matching case-insensitively against the
input. Each rule strips its phrases and records a
`strip_warning`. Counts ("first 15", "each of the 15") are
deliberately not touched — Phase 35's
`agent::detect_enumeration_task` needs them to be intact for the
`in_page_history_cap` auto-override.

1. **Narration directives** — "narrate as you go", "narrate
   briefly", "narrate each step", "describe what you see", "say
   out loud", "tell me what you're doing", "explain each step".
   Warning: `stripped: narration directive`.
2. **Speed directives** — "move quickly", "be fast", "move
   slowly", "take your time". Warning: `stripped: speed
   directive (config concern, not prompt concern)`.
3. **Anti-batch directives** — "don't batch", "do not batch",
   "don't skip the click-through", "do not skip the
   click-through", "one at a time". Warning: `stripped:
   anti-batch directive (per-step by design)`.
4. **`declare_subtasks` mention** — "use declare_subtasks", "call
   declare_subtasks", "use declare subtasks", "call declare
   subtasks". On match, an additional execution hint is emitted
   telling the LLM explicitly *not* to call `declare_subtasks`.
   Warning: `stripped: declare_subtasks mention (forbidden by
   vertical hint)`.
5. **Count normalization** — explicit no-op. Counts survive
   because no rule above targets numeric phrases.

The cleaned string has its whitespace collapsed (runs of spaces /
tabs → single space, leading/trailing trimmed) and a final
stranded-punctuation cleanup so "salons in Mumbai, narrate as
you go" becomes "salons in Mumbai" (the comma is consumed
together with the stripped phrase, not left dangling).

## Per-vertical execution hints

Only `TaskShape::LocalLeadGen` injects hints today — the Phase 34
Maps pack. Four lines, in this order:

1. "Use extract_fields(fields=['phone','address','rating','website','name']) immediately after clicking each listing."
2. "Use press_key(key='Escape') to close business panels and return to search results."
3. "Do NOT call declare_subtasks." (or, when the user mentioned
   `declare_subtasks`, a more pointed "Do NOT call
   declare_subtasks for this task — use extract_fields and
   finish() with structured output directly.")
4. "Call finish() with a markdown table (columns: Name | Phone
   | Address | Rating | Website) after all listings are checked."

Other shapes (JobSearch, LeadGen, GenericResearch, GenericBrowse)
inject no hints — their vertical packs already carry the
necessary guidance through the `PLAN:` block that
`run_preflight_plan` appends, and adding compiler-side hints
would just duplicate the instruction set.

The block is appended to the system prompt in the same style
the `PLAN:` block uses:

```
<existing system prompt text>

EXECUTION HINTS (pre-flight compiler):
- <hint 1>
- <hint 2>
- ...
```

## Constructor wiring

`Agent::new_with_tracing` now runs the compiler before building
either message. The integration is intentionally minimal:

1. `let compiled = crate::prompt_compiler::compile(&task, infer_task_shape_from_task(&task), None);`
2. `messages[0].content` is built as `system_prompt` plus the
   `EXECUTION HINTS:` block (omitted entirely when
   `execution_hints` is empty, so every non-Maps shape stays
   byte-for-byte equivalent to the pre-Phase-36 system prompt).
3. `messages[1].content` is `format!("Task: {}", compiled.agent_task)`.
4. Each `strip_warning` is logged to the transcript in the same
   `[ts] [session_id] EVENT: ...` format used by `STATE:`,
   `PREFLIGHT:`, and `TOOL CALL:`. The format is
   `[ts] [session_id] PROMPT-COMPILE: <warning>`.

The constructor uses the existing `infer_task_shape_from_task`
helper rather than plumbing a `MapsQuery` from the chat-side
router. The chat-side `ChatAgent::build_handoff_inner` is the
place where `MapsQuery::parse` runs; by the time the Agent is
constructed the query is already baked into the `Handoff`. The
constructor is self-contained — it just needs the task string and
a shape — and the spec's hint injection is shape-driven, not
query-driven, so this is the right factoring.

## Test coverage

`prompt_compiler.rs` carries an inline `#[cfg(test)] mod tests`
with 10 cases:

* The four spec-mandated acceptance cases (Maps + narration
  with all 4 hint assertions; declare_subtasks strip with the
  specific "do NOT" hint assertion; GenericBrowse passthrough
  with empty warnings/hints; empty-string no-panic).
* Anti-batch strip with warning.
* Speed strip with warning.
* Count preservation (Phase 35 contract: "first 15" survives
  "narrate as you go" stripping).
* Whitespace collapse.
* Case-insensitive matching (the original casing of the
  surrounding text is preserved).
* Non-Maps shape does not get Maps hints even when the user
  mentions "Google Maps" by name.

The `mew-agent/examples/phase36_prompt_compiler.rs` example is a
live-transcript spot-check harness covering the three realistic
inputs the spec calls out (acceptance criterion #4):

1. `"Open Google Maps, check 15 salons in Mumbai, narrate as you go, don't batch"` — two `PROMPT-COMPILE:` lines (narration + anti-batch).
2. `"Search gyms in Mumbai, use declare_subtasks, move quickly"` — two `PROMPT-COMPILE:` lines (speed + declare_subtasks).
3. `"Find businesses in Delhi"` — no `PROMPT-COMPILE:` lines, clean passthrough.

Run with `cargo run -p mew-agent --example phase36_prompt_compiler`.
The harness reads the per-call transcript file the constructor
wrote and asserts the expected `PROMPT-COMPILE:` audit lines
appear in the right format.

## Cost discipline

Per the budget-discipline rule from `work.md` line 16, every
phase that adds LLM calls must state its expected token/cost
impact. Phase 36 adds **zero LLM calls**. The compiler is a
pure-Rust function executed once per session at construction
time. The system-prompt length *increases* by the four
`EXECUTION HINTS:` lines on `LocalLeadGen` tasks only — roughly
~400 characters of static text. That maps to ~100 tokens at the
GPT-style 4-chars-per-token ratio, which is on the same order
as a single line of the existing `PLAN:` block and well within
the per-task budget. Token counters in the
`mew-agent/examples/phase36_prompt_compiler.rs` example do not
diverge from the pre-Phase-36 baseline for any non-Maps shape.

## Files touched

- **New:** `mew-agent/src/prompt_compiler.rs` (the module).
- **New:** `mew-agent/examples/phase36_prompt_compiler.rs` (live-transcript verification harness).
- **New:** `docs/phase36-prompt-compiler.md` (this file).
- **Edited:** `mew-agent/src/lib.rs` — added `pub mod prompt_compiler;`.
- **Edited:** `mew-agent/src/agent.rs` — `Agent::new_with_tracing` now calls `compile()` and appends the `EXECUTION HINTS:` block to the system prompt, plus the `PROMPT-COMPILE:` transcript lines.
- **Edited:** `work.md` — all Phase 36 checkboxes flipped per the file's no-formatting-change rule.
