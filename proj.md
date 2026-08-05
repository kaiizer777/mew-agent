# mew — Project Documentation

A Rust-native, visible, cost-controlled browser agent driven by a two-agent LLM harness. All ten phases of the work plan are shipped.

---

## 1. Executive summary

`mew` is a Rust-native computer-use agent that drives a real, visible Chromium window through the Chrome DevTools Protocol (CDP). It is designed around three pillars:

1. **Speed and cost.** The agent perceives pages through accessibility-tree snapshots and intelligent diffs, not full-page screenshots. Token spend stays in the low hundreds per step on a typical task, and a 100-step iteration cap with optional `max_cost` budget keeps every run inside an explicit ceiling. End-of-task summary and pre-flight decomposition calls are cached and reused where possible.
2. **Real-world bot evasion.** A stealth Chromium binary (`stealth-browser/chrome.exe`), source-patched `navigator.webdriver` masking, profile retention for authenticated sessions, site-aware pacing guards, and a sensitive-platform entry path that uses a search-engine referrer instead of a bare direct nav.
3. **Human-in-the-loop on the hard cases.** The Tauri shell is a single chat surface, the agent pauses for visible-window CAPTCHA solving by default, irreversible actions (`submit order`, `send payment`, `delete`, `post publicly`) force a `Paused` checkpoint for confirmation, and the user can steer mid-task through an `mpsc` channel without resetting the session. The opt-in `CaptchaSolver` trait (2captcha / anti-captcha / capmonster) is the documented extension point for users who explicitly want unattended runs.

The architecture moved from a single blurred agent to a **two-agent split** in Phase 3: `ChatAgent` (intent routing + result synthesis) and `BrowserAgent` (the ReAct loop), connected by typed `Handoff` and `Result` structs. Every `ChatAgent → BrowserAgent → ChatAgent` round trip ends with a user-facing chat message — the orchestrator converts factory errors to `BrowserResult::failure` so even a Chrome-launch failure produces a chat reply. All error paths route through the `error_message` layer, so raw `Err` / panic / JSON surfaces never reach the user as-is.

With v3 the project ships a Tauri 2 desktop shell, a `Channel<any>` for live events (State / Tool / Summary / ProgressLine), typed `OrchestratorEvent`s mapped to Tauri events by a `TauriSink`, and one chat surface instead of a separate transcript panel. Production hardening (Phase 10) is complete: tracing is quiet by default, the config schema is documented, and the three motivating scenarios all pass end-to-end.

---

## 2. Crate ecosystem

The workspace is a Cargo workspace with seven crates and a `mew-resilience` library that's pulled in through `mew-agent`. The dependency graph is one-way: UI depends on agent; agent depends on resilience, nav, perception, cdp; perception and cdp have no agent deps.

```
mew-ui (Tauri 2)
   │
   ▼
mew-agent ───── mew-cli
   │  │
   │  ├── mew-resilience
   │  ├── mew-nav
   │  ├── mew-perception
   │  └── mew-cdp
```

### 2.1 `mew-cdp` — browser control & window geometry

The lowest layer. Wraps `chromiumoxide` and adds everything mew needs on top of it.

- **Stealth launch.** `BrowserConfig::builder().with_head()` spawns the local Chromium binary in headed mode. The default `config.browser.binary_path` points at `stealth-browser/chrome.exe` — a source-patched binary that survives Cloudflare Turnstile and reCAPTCHA v3 better than vanilla Chrome. Defense-in-depth JS injection strips `navigator.webdriver` and other automation flags on every page load.
- **Action primitives.** Async functions for `navigate(url)`, `click(ref)`, `type_text(ref, text)`, `scroll()`, and `screenshot_region(x, y, w, h)` for the vision fallback.
- **Window docking.** Direct HWND reparenting broke on Chrome 139 / DirectComposition, so the dock math is now pure CDP: `Browser.getWindowForTarget` + `Browser.setWindowBounds` with a screen-aware arithmetic module (`compute_dock_rect_screen_aware`) that clamps the dock rect to the monitor and accounts for the Tauri window's screen-space position. Chromium docks flush right of the Tauri window and scales correctly across multi-monitor setups.
- **Ghost cursor.** A lightweight CSS+JS overlay (`window.__mewCursor`) injected via `Page.addScriptToEvaluateOnNewDocument`. `position: fixed; pointer-events: none;` so it tracks the agent's intended click target with a ripple animation, without intercepting real DOM clicks. Zero overhead when `visible_cursor: false`.
- **Screencast capture.** `mew_cdp::capture_screenshot` polls the page every 500ms and returns a JPEG. The Tauri shell pushes each frame to the `agent-screencast-frame` event. (Periodic `Page.captureScreenshot` is used instead of `Page.startScreencast` because the latter delivers frames at unpredictable cadence and the periodic poll is simpler to throttle.) **Phase X: 4K live preview.** The viewport is 3840×2160 (4K UHD) and the screencast is captured at native 4K with JPEG q=85, every 5th frame. The 5x nth-frame skip lands the preview at 1-2 fps — fine for a "live preview" pane (the chat surface carries the textual per-step detail) and keeps the per-frame CPU encode cost manageable. The CSS `image-rendering: high-quality` hint tells the browser to use a Lanczos-style scaler when the 4K source is downscaled to the ~880px preview pane. The agent's interaction model (AX-tree refs, `@eX` click targets) is resolution-independent, so the larger viewport does not change which elements the agent can target — it only makes the preview pane sharper.

### 2.2 `mew-perception` — accessibility-tree extraction & diffing

The agent's primary sensory input is the browser's accessibility tree, not a screenshot.

- **`Accessibility.getFullAXTree` extraction.** Calls the typed CDP binding, parses the flat `AXNode` array into an in-memory hierarchical `TreeNode` struct.
- **Semantic classification.** Nodes are bucketed as `INTERACTIVE`, `CONTENT`, or `STRUCTURAL`. "Compact mode" aggressively prunes structural noise (empty `<div>` wrappers, hidden subtrees) while preserving every interactive element. The LLM gets an ultra-lean textual representation, typically 1–3KB per snapshot for a typical page.
- **Stable ref assignment.** Each interactive element gets a short, stable reference ID derived from its `backend_dom_node_id` (e.g. `@e1`, `@e12`). The LLM targets actions purely via refs (`click(@e12)`), which is far more robust than CSS selectors — refs survive class churn, attribute changes, and most JS re-renders.
- **Diffing engine.** Caches the prior snapshot and serializes only the added / removed / changed nodes for subsequent steps. This is the single biggest token-cost win in the system: a `click` that triggers a small DOM update ships a tiny diff to the LLM, not the full re-snapshot.
- **Stale-ref recovery.** When the LLM hands back a `@eX` that no longer exists (the page navigated, a re-render changed the backend IDs), the error path in `mew-resilience::ref_recovery` triggers a bounded re-snapshot + retry. The LLM never sees a raw "ref stale" error.

### 2.3 `mew-nav` — URL Resolution Layer + sensitive-platform routing

A pure-Rust library, no `serde_yaml` dep (uses `toml`).

- **Three-branch resolver.**
  1. **Hardcoded map** — `instagram` → `https://www.instagram.com`, `slack` → `https://app.slack.com`, etc. (Shipped with the workspace.)
  2. **Direct `.com` probe** — `https://{x}.com` with a strict 4-second timeout (`DIRECT_GUESS_PROBE_TIMEOUT`). Catches "go to anthropic" → `https://anthropic.com`.
  3. **Google fallback** — `https://www.google.com/search?q=…` if both above miss.
- **Sensitive-platform routing.** Loads `config/sensitive_platforms.toml` and routes matched domains through a `ResolutionPath`:
  - `Direct` — same as not being in the table.
  - `ViaSearch` — `navigate("instagram")` becomes `https://www.google.com/search?q=instagram`. The LLM clicks the organic result to actually land on the target. This dodges the "browser appears on instagram.com from nowhere" anti-bot classifier.
  - `ViaSearchConfirm` — like `ViaSearch` but with a "login" keyword appended, nudging results toward the canonical sign-in URL (used for LinkedIn).
- **Match rules.** Bare host, lowercased, `www.` stripped. `*.example.com` matches a single subdomain level. Exact-host entries take precedence over wildcards.
- **Pre-seeded entries.** instagram.com, www.instagram.com, twitter.com, x.com, facebook.com, www.facebook.com, linkedin.com, www.linkedin.com, tiktok.com, www.tiktok.com. All marked `known_to_challenge_bots = true` so downstream code (pacing, telemetry) reads the flag.

Why a separate file? `mew-nav` is a pure-Rust library; pulling `serde_yaml` (only used by `mew-agent`'s config) into it would be the wrong dependency direction. `toml` is a tiny zero-config dep. Operators can edit the list without touching `config.yaml`.

### 2.4 `mew-resilience` — six failure-mode detectors + mock fixtures

The 2026 consensus on what actually breaks production browser agents: element-reference / selector drift, ambiguous screenshots, silent login / session loss, popup / modal interruptions, rate-limit / block pages, and irreversible actions taken without confirmation. `mew-resilience` hardens each of these.

| Failure mode | Detector module | Behavior |
| --- | --- | --- |
| Stale `@eX` ref | `ref_recovery.rs` | Bounded re-snapshot + retry. LLM never sees a raw stale-ref error. |
| Modal / cookie banner | `modal_interrupts.rs` | Detect in the AX tree. Auto-dismiss or force as the LLM's first required action. |
| Mid-task login loss | `session_loss.rs` | Detect when a login form appears where a dashboard was expected. Surface explicitly to the user, don't let the agent flounder. |
| Rate-limit / 429 / Cloudflare block | `rate_limit.rs` | Extend `pacing.rs` to detect and trigger exponential backoff + retry instead of treating the empty page as a normal page. |
| Irreversible action | `irreversible_actions.rs` | Allowlist / classifier for `submit order`, `send payment`, `delete`, `post publicly`. Forces a `Paused` checkpoint for confirmation, reusing the existing `Paused` state. |
| Vision ambiguity | `vision_confidence.rs` | `screenshot_region` returns a bounding box + confidence. Re-prompt or ask the user when confidence is low. |

All six are unit-tested against `mock_fixtures.rs` — pure-Rust `TreeNode` constructors for "cookie-banner page", "fake logged-out page", "fake 429 page", etc. — so the resilience suite runs in CI without a live browser.

### 2.5 `mew-agent` — the LLM brain, the two-agent split, and the orchestrator

The decision-making core. The crate is organized around a typed pipeline, not free functions.

#### 2.5.1 The two-agent split

```
ChatAgent                          BrowserAgent
- classify_intent                  - run_inner ReAct loop
- build_handoff (Handoff)          - perceive → act
- synthesize_reply (Result)        - emit ProgressLine
                                   - completeness gate
                                   - finish() → BrowserResult
```

`ChatAgent` is in `mew_agent::chat_agent`. It has its own system prompt (`CHAT_AGENT_SYSTEM_PROMPT`), distinct from `BrowserAgent`'s. A test asserts the two prompts do not share the browser-side "COMPLETENESS PROTOCOL" phrase, so a future copy-paste gets caught.

#### 2.5.2 `Handoff` and `Result`

```rust
struct Handoff {
    task_description: String,
    subtasks: Vec<SubTask>,           // populated by planner
    constraints: Vec<Constraint>,     // reserved for sensitive-platform routing
    originating_message_id: String,   // stamped on every trace event
}

enum BrowserStatus { Done, Partial, Failed }
struct BrowserResult {
    status: BrowserStatus,
    summary: String,
    key_findings: Vec<KeyFinding>,
    final_snapshot_signature: Option<String>,
    raw_transcript_ref: Option<PathBuf>,
}
```

`ChatAgent::build_handoff` runs the deterministic planner and populates `subtasks`. `ChatAgent::synthesize_reply` takes a `BrowserResult` and produces the user-facing text — three branches (`Done` with "N of M sub-tasks completed" footer, `Partial` with an "Outstanding" list of per-subtask reasons, `Failed` with reason + summary).

The orchestrator catches factory errors and converts them to `BrowserResult::failure("unknown-session", format!("browser task could not start: {e}"), None)`, so even a Chrome-launch failure produces a chat reply.

#### 2.5.3 Deterministic pre-flight planning

`mew_agent::planner::plan(&task)` runs *before* the first LLM call. It splits a compound instruction ("go to X" + "do Y") into a typed `Vec<SubTask>` and injects a `PLAN (pre-flight decomposition):` block into every subsequent system prompt. The `CompletenessTracker` reads the same `Vec<SubTask>` to gate `finish()` — a task can only be marked done when fresh snapshot evidence confirms the outcome.

This is the core fix for the original Bug #1: the LLM no longer has to spontaneously realize that "go to instagram and text my friend hi" is two subtasks. The plan is on the page from turn one.

#### 2.5.4 Live step summarization

`mew_agent::summarizer::summarize` produces a one-line human note for every tool dispatch. Templated path is zero-LLM (navigate/click/type/scroll/etc. all have a template). The end-of-task summary is a single LLM call with `max_tokens: 200`, fired only when the `finish()` gate is open; on any HTTP / parse / model error it falls back to the raw `finish()` text — never silent.

`LiveProgress` is a ring buffer capped at `live_lines_cap` (default 5, hard-clamped 1..=1000). Total line count is preserved on the task card so the `more_steps_suffix` "…and N more steps" UI helper renders correctly. Verbosity (`Concise` / `Detailed`) filters at the source.

#### 2.5.5 State machine

`SessionState` transitions explicitly through `Stopped → Running → Paused → Done | Failed`. `checkpoint()` is the function the ReAct loop calls before every tool dispatch; if state is `Paused`, it awaits. `Paused` is the single re-use point for irreversible-action confirmation, CAPTCHA handoff, and human debugging.

#### 2.5.6 URL Resolution + PacingGuard + CompletenessTracker

- **URL Resolution** calls into `mew-nav`. See §2.3.
- **PacingGuard** (`pacing.rs`) manages streaks. Consecutive identical actions in a tight loop (e.g. 5 `click`s back-to-back with no different action type between them) trigger a `PacingDecision` that injects a random delay in `[min_delay_ms, max_delay_ms]`. One-off and mixed actions never get paced. Opt-in via `pacing.enabled: true` — default off so existing task patterns aren't silently slowed.
- **CompletenessTracker** (`completeness.rs`) owns the `Vec<SubTask>` and the `gate_open()` check. `finish()` only opens the gate when fresh snapshot evidence (`last_snapshot_signature` matching the LLM-supplied one) confirms the real-world outcome. This is the fix for the "false completion" anomaly where the LLM claims success and the agent believes it.

#### 2.5.7 Long-horizon research loop

`mew_agent::research` is the Phase 7 module.

- **`ResearchPlanner`** reads `config/research_platforms.toml` and emits a typed `ResearchPlan` with one platform per subtask, an `entry_hint`, a `step_budget`, a `time_budget_secs`, and a `default_query`.
- **`FindingStore`** is a shared, deduplicated list across platform subtasks. Findings carry `(platform, role, email?, url?)` plus a timestamp. The synthesis step renders one consolidated list of `role + email/URL per finding`, not fragments.
- **Falsifiable-commitment checkpoint.** Before marking a subtask done, the agent states what evidence it expects (e.g. "an email matching x@y.com" or "a working application URL"). `CompletenessTracker` verifies that evidence is actually present in the latest snapshot, not just trusting the LLM's self-report.
- **Per-platform budget guard.** On `step_budget` or `time_budget_secs` overrun, the subtask is marked `Exhausted` and the loop moves on. A platform with no result after the budget is *not* a failure — only the loop returning without a consolidated answer is.
- **Pre-seeded platforms.** LinkedIn, Indeed, Wellfound, WeWorkRemotely, RemoteOK. Glassdoor, Monster, Y Combinator's `workatastartup.com` are commented out — uncomment to opt in.

#### 2.5.8 Tracing layer

`mew_agent::tracing_layer` adds a per-session JSONL layer. Quiet by default in production builds; opt-in via `MEW_TRACING_DIR` env var. Captures every LLM request / response, every tool call, every snapshot signature, every URL Resolution branch, every `SessionHandle` state transition, and the exact handoff boundary in `mew-ui` (the typed `OrchestratorEvent` that crossed from frontend to agent).

The `browser_task_result_delivered` event is the positive tripwire that replaced the pre-Phase-1.5 `browser_task_result_dropped` name — a stale `select(.event == "browser_task_result_delivered")` in a trace file is unambiguous evidence of a post-Phase-1.5 run.

#### 2.5.9 Evaluation harness

`mew_agent::eval` is the Phase 9 & Phase 17 module — a pure-Rust mock-page scenario runner. No live Chrome, no LLM calls, no network. Scenarios are typed values that carry the user task, the page state the agent would see, the expected terminal state, and which of the six Phase 6 failure modes the scenario is *known* to trip. The runner calls `ChatAgent::synthesize_reply` and asserts the handoff contract: right task dispatched, result reflected in chat reply, failure paths still produce a user-facing message. Reusable assertions live in `eval::assertions`.

Phase 17 extends the harness with planner-worker contract evaluation (`eval/scenarios/planner_worker_shortcut.rs`), covering three MUST-HAVE scenarios: (1) Happy path (worker signature matches planner, transitions to Done with evidence and attempts == 1), (2) Worker shortcut (fake worker signature rejected as StaleEvidence, remains Pending, retries up to attempt cap and transitions to Failed), and (3) Stale evidence (worker reuses signature from past iteration, rejected as StaleEvidence, todo remains Pending). Dedicated helper binary `mew-cli/src/bin/phase17_planner_eval.rs` provides standalone execution.

Wired into CI as `cargo test --features eval -p mew-agent`. Pass-rate over time is logged in `docs/eval-history.md`.


#### 2.5.10 CAPTCHA / challenge handling

`mew_agent::captcha_solver` and `mew_agent::captcha_telemetry` together implement Phase 8.

- **Challenge-page detector** recognizes Cloudflare Turnstile, reCAPTCHA v2/v3, and hCaptcha in the AX tree and classifies them distinctly from a normal page.
- **Default behavior on detection:** pause the session (`Paused` state) and message the user clearly.
- **`CaptchaSolver` trait.** The pluggable extension point. Shipped implementations cover 2captcha, anti-captcha, and capmonster; the `solve_kinds` field in `config.yaml` lists which kinds the chosen provider supports.
- **Opt-in `agent.captcha` config block.** `enabled: false` by default. `provider`, `api_key_env`, and `per_session_cap` are the operator-facing knobs.
- **Local-only telemetry.** Per-domain challenge counts in `<data_dir>/captcha_telemetry.json`. No data leaves the machine.
- **`known_to_challenge_bots` flag in `sensitive_platforms.toml`.** Per-domain, used by pacing and telemetry. Pacing slows before the navigation and the telemetry baseline is seeded at the higher "expected" level.

### 2.6 `mew-cli` — headless entry point

The CLI is still useful for debugging and the no-UI examples (`stdin_chat`, `test_stale`, `test_messy`, `test_stealth`, `test_timeout`, `debug_github`). Every example writes a session transcript under `mew-ui/src-tauri/transcripts/` so a CI run can diff transcripts against a golden.

### 2.7 `mew-ui` — the Tauri 2 shell

The desktop shell. The frontend is a single TypeScript file (`src/main.ts`) with a small CSS module (`src/style.css`).

- **Single chat surface.** `#chat-container` owns the full viewport. User messages right-align; everything else left-aligns. No separate transcript panel.
- **MessageKind type system.** `user | chat_reply | task_started | task_progress | task_completed | task_failed`. Each kind has its own CSS class. `task_started` / `task_completed` / `task_failed` are cards with a semantic left-border accent. `task_progress` is a dim monospace line under the active task.
- **Collapsible "view details".** Native `<details>` element on every `task_*` card, closed by default. Streams new rows in as events land when the user has it open. The "result" row is added when `TaskCompleted` fires.
- **"Working · N steps" header pill.** Driven by the live `Channel<any>` stream. `State` / `Tool` / `Summary` events bump the counter; `TaskCompleted` transitions to `Done · N steps` / `Failed · N steps`; the pill fades to `Idle` after 1.8s.
- **Verbosity toggle.** A header button that flips a `verbosity` state and re-renders every task's live progress sub-list. Defense-in-depth re-filter in case a future backend forgets to truncate.
- **Live events via `Channel<any>`.** Tauri `Channel`s are used instead of `app.emit` for high-frequency events because the former guarantee ordering under high throughput. The four event types handled in `onEvent.onmessage`: `State` (state transition), `Tool` (tool call), `Summary` (LLM-generated end-of-step note), `ProgressLine` (templated live progress line).
- **Tauri commands.** `send_message`, `pause_session`, `resume_session`, `get_config_summary`. `send_message` is the request path; if a session is active, the user message is pushed directly into the `mpsc` steering bus and `acknowledge_steering` is called immediately so the user sees "Got it, the agent will adjust." in the chat list in the same frame.
- **TauriSink.** The orchestrator's `TurnSink` impl for Tauri events. Maps `OrchestratorEvent` variants to the right `app.emit` calls:
  - `TaskStarted` → `chat-task-started`
  - `SteeringAcknowledged` → `chat-steering-ack`
  - `BrowserResultReady` → `browser-result-ready` (debug / tracing)
  - `ChatReply` → `chat-reply`
  - `TaskCompleted` → `chat-task-completed`
- **Screencast popover.** Receives `agent-screencast-frame` JPEGs from the periodic `Page.captureScreenshot` poll. Smaller, floating container than the old "Live Preview" panel. Dismissing it doesn't affect the agent.
- **`error_message` layer.** All raw `Err` / panic / JSON surfaces in the Tauri command handlers are funneled through `error_message::for_user` so the chat reply the user sees is a plain-language sentence, never a stack trace.

---

## 3. End-to-end task flow

A complete lifecycle of a user message, in eleven steps:

1. **User input (`mew-ui`).** The user types *"Go to instagram and text my friend hi"* in the chat box.
2. **Intent classification.** `mew-ui` sends the string to the LLM forcing the `classify_intent` tool. The LLM returns `Intent::BrowserTask(task)`.
3. **Handoff construction.** `ChatAgent::build_handoff` runs `mew_agent::planner::plan(&task)`, which deterministically decomposes the compound instruction into a `Vec<SubTask>` (e.g. `[{ navigate("instagram") }, { type_text("hi") }]`). The `Handoff` is typed and stamped with `originating_message_id`.
4. **Session init.** `mew-agent` spawns a Tokio task, registers a new `SessionHandle` transitioning `Stopped → Running`.
5. **Docking mechanics.** `mew-cdp` spawns the stealth Chromium instance. The Tauri event loop captures the host's screen coordinates, triggers `compute_dock_rect_screen_aware`, and issues a CDP bounds update. Chromium docks flush right of the Tauri window.
6. **URL Resolution.** The LLM outputs `navigate("instagram")`. The routing layer checks `sensitive_platforms.toml` — `instagram.com` matches `via_search` — and returns `https://www.google.com/search?q=instagram`. The LLM clicks the organic result and lands on instagram.com. (Bare direct nav would have tripped the anti-bot classifier; this is the Bug #1 fix.)
7. **Perception loop.** `mew-perception` grabs the AX tree, prunes structural noise, assigns `@eX` refs, computes the diff vs. the prior snapshot, and ships the diff to the LLM context.
8. **Action & execution.** The LLM evaluates the tree and dispatches `type_text(@e12, "hi")`. The JS ghost cursor visibly glides across Chromium; `mew-cdp` triggers the real DOM dispatch. A `ProgressLine` ("Typed 'hi' into @e12") lands in the live progress sub-list and the header pill bumps to "Working · 7 steps".
9. **Steering interruption (optional).** The user types *"Actually, tell John I'll be 5 minutes late"*. Because a session is active, this string bypasses intent classification and flows directly into the active `mpsc` channel. `TauriSink::acknowledge_steering` emits `chat-steering-ack` so the user sees "Got it, the agent will adjust." in the same frame. On the next `checkpoint()`, the LLM observes the appended `role: user` message and alters the plan.
10. **Evidence & verification.** The LLM calls `finish()`. The Completeness Verifier forces one last AX-tree diff loop. The fresh snapshot signature must match what the LLM claimed, otherwise the gate stays closed.
11. **Completion.** The state machine hits `Done`. `ChatAgent::synthesize_reply` produces the user-facing text. `TauriSink` emits `chat-reply` and `chat-task-completed`. The chat list shows the synthesized reply as a left-aligned message; the `task_started` card transitions to a `task_completed` card with the "result" row appended to its `<details>` panel. The session handle is dropped.

If any step in 2–10 fails, the orchestrator converts the failure into `BrowserResult::failure(reason, summary)` and the user still sees a `chat-reply` — never silent. Raw error surfaces are rewritten through the `error_message` layer before they reach the user.

---

## 4. Phase history

### Phase 1 — Core foundation (v1)

- **Windows MSVC toolchain.** Mandated `stable-x86_64-pc-windows-msvc` to stabilize native deps in `chromiumoxide` and avoid GNU linking failures.
- **Perception over vision.** Built the AX-tree parser, abandoned full-page screenshots.
- **Base tool schemas.** `navigate`, `click`, `type`, `scroll`, and the `screenshot_region` vision fallback for canvas / image-only elements.
- **Stealth init.** Adopted stealth binaries and `user_data_dir` profile retention to bypass reCAPTCHA and persist authentication across launches.
- **Tracing layer.** Structured `tracing` crate coverage of the full `mew-agent` ReAct loop: every LLM request / response, tool call, snapshot signature. Span instrumentation around the URL Resolution Layer's three branches, `SessionHandle` state transitions, and the `mew-ui` → `mew-agent` handoff boundary.
- **Live reproductions.** Bug #1 (both phrasings) and Bug #2 reproduced with tracing on; full transcripts captured under `transcripts/`.

### Phase 1.5 — Minimal bug-2 wire fix (hot-patch, pre-Phase 3 hardening)

The minimum code change that closes the user-visible "result computed but never emitted" gap. `TauriSink` maps `ChatReply` to `chat-reply`; the frontend's `listen<string>('chat-reply', ...)` pushes the payload into the chat list. Replaced the `browser_task_result_dropped` tripwire with the positive `browser_task_result_delivered` so a stale `select(.event == "browser_task_result_delivered")` in a trace file is unambiguous evidence of a post-Phase-1.5 run.

### Phase 2 — Reliability & steering (v2)

- **Session state machine.** Replaced the naive blocking loop with `SessionState` (`Stopped` / `Running` / `Paused` / `Done` / `Failed`) and the `mpsc` steering side-channel.
- **URL Resolution pipeline.** Three-branch resolver (map → probe → fallback) extended with the sensitive-platform routing table and the `ResolutionPath::ViaSearch` / `ViaSearchConfirm` variants.
- **Sensitive-platform routing table.** `config/sensitive_platforms.toml` with 10 seeded entries (instagram, twitter, x, facebook, linkedin, tiktok families, `www.` variants).
- **Pre-flight planning.** `mew_agent::planner` runs deterministically before the first LLM call, seeds `CompletenessTracker`, and writes a `PREFLIGHT:` line + `PLAN (pre-flight decomposition):` block to the system prompt.
- **Regression fixture + tests.** `phase2_instagram_regression.rs` asserts both instagram phrasings produce equivalent terminal state. 17 planner tests + 11 nav tests.
- **10/10 pass rate** on the original failing prompt, logged in `docs/bug-1-fix-verification.md`.

### Phase 3 — Desktop shell (v3)

- **Tauri 2 integration.** Scaffolded `mew-ui`. Side-stepped unreliable Windows reparenting by adopting pure CDP `Browser.setWindowBounds` for docking.
- **Stream segregation.** One-shot `app.emit` for session lifecycle events; ordered Tauri `Channel<any>` for the live event stream.
- **Intent classification.** A single input field that routes to conversational chat or browser task without the user noticing the boundary.
- **Two-agent split.** `ChatAgent` + `BrowserAgent` + typed `Handoff` / `Result` structs. `mew_agent::chat_agent.rs`, `mew_agent::handoff.rs`, `mew_agent::orchestrator.rs`. `ChatAgent::synthesize_reply` is the only place a user-facing chat message is produced. The bug-2 root cause ("final result computed but never `app.emit`'d") is fixed by construction.
- **Mid-task steering ack channel.** `orchestrator::acknowledge_steering` + `SteeringAcknowledged` event + `TauriSink::chat-steering-ack` + frontend listener — the user sees "Got it, the agent will adjust." in the same frame as their steering message.
- **Live round trip verified.** `phase3_round_trip.rs` exercises the full `ChatAgent → BrowserAgent → ChatAgent` flow and asserts equivalent end state for both `Done` and `Failed` results.

### Phase 4 — UI overhaul

- **Single chat surface.** Removed the raw transcript panel component, the `tab-transcript` / `tab-preview` tab buttons, and the full-height `Live Preview` panel.
- **MessageKind system.** `user | chat_reply | task_started | task_progress | task_completed | task_failed`. Each kind has its own CSS class and semantic left-border accent.
- **Collapsible "view details".** Native `<details>` on every `task_*` card.
- **"Working · N steps" header pill.** Replaces the old full-dump transcript.
- **Docking / cursor visuals unaffected.** The cursor / docking code lives in `mew-cdp`; none of it was touched.
- **Instagram e2e re-tested clean.** The motivating scenario produces a single chat message, no raw JSON, no transcript panel.

### Phase 5 — Live step summarization

- **Templated summaries.** Zero-LLM one-liner per tool dispatch. 26 unit tests cover every common tool in both verbosity modes.
- **Live `ProgressLine` event.** New `AgentEvent::ProgressLine { timestamp_secs, text, kind, success }` variant on the existing `mpsc::UnboundedSender<AgentEvent>` channel. Wire-compatible via `#[serde(tag = "type")]`.
- **End-of-task LLM call.** Single `end_of_task_summarize` call with `max_tokens: 200`, fired only when the finish() gate is open. Falls back to the raw `finish()` text on any error.
- **Ring buffer cap.** `LiveProgress` ring buffer, default 5, hard-clamped 1..=1000. Total count preserved.
- **Verbosity toggle.** `SummarizationConfig::verbosity` (`Concise` | `Detailed`). Frontend has a header button to flip and re-render every task's live progress sub-list.

### Phase 6 — Resilience core

- **Stale ref → re-snapshot retry.** Bounded. LLM never sees a raw stale-ref error.
- **Modal interrupts → auto-dismiss.** Detected in the AX tree.
- **Session loss → explicit surfacing.** Login form where dashboard was expected.
- **Rate-limit → exponential backoff.** 429 / Cloudflare block pages.
- **Irreversible actions → Paused checkpoint.** Allowlist / classifier.
- **Vision ambiguity → re-prompt.** Bounding box + confidence on `screenshot_region`.
- **Mock-page fixtures.** Pure-Rust `TreeNode` constructors for every failure mode, used by both the resilience unit tests and the Phase 9 eval scenarios.

### Phase 7 — Long-horizon task planning

- **`ResearchPlanner`.** Reads `config/research_platforms.toml`; emits `ResearchPlan` with one platform per subtask.
- **Falsifiable-commitment checkpoint.** Per subtask, the agent states what evidence it expects ("an email matching x@y.com" or "a working application URL"). `CompletenessTracker` verifies that evidence is actually present in the latest snapshot, not just trusting the LLM's self-report.
- **`FindingStore`.** Cross-platform, deduplicated.
- **"No result on this platform" ≠ "task failed".** Move to the next platform, only report overall failure once all are exhausted.
- **Final synthesis.** One consolidated answer (role + email/URL per finding).
- **Per-platform budgets.** `step_budget` and `time_budget_secs` per row in `research_platforms.toml`. On overrun, the subtask is `Exhausted`.
- **End-to-end benchmarks.** 2–3 real job-search scenarios logged in `docs/phase7-benchmarks.md` with success rate + time.

### Phase 8 — Obstacle & CAPTCHA handling

- **Challenge-page detector.** Recognizes Cloudflare Turnstile, reCAPTCHA v2/v3, hCaptcha in the AX tree.
- **Pause-and-message default.** On detection, session goes to `Paused` and the user is messaged clearly.
- **`CaptchaSolver` trait.** Pluggable. Shipped implementations: 2captcha, anti-captcha, capmonster. `solve_kinds` in `config.yaml` lists which kinds the chosen provider supports.
- **Opt-in `agent.captcha` config block.** `enabled: false` by default. `provider`, `api_key_env`, `per_session_cap` are the operator-facing knobs.
- **`known_to_challenge_bots` flag in `sensitive_platforms.toml`.** Per-domain, used by pacing and telemetry. Pacing slows before the navigation and the telemetry baseline is seeded at the higher "expected" level.
- **Local-only telemetry.** Per-domain challenge counts in `<data_dir>/captcha_telemetry.json`.
- **README ethical / ToS boundary.** Documents the 2026 consensus path and the per-domain cost / ToS risk of automated solving.

### Phase 9 — Evaluation harness

- **Pure-Rust scenario runner.** `mew_agent::eval`. Mock-page `TreeNode` fixtures, no live Chrome, no LLM, no network.
- **Reusable handoff assertions.** `eval::assertions` — every `ChatAgent → BrowserAgent → ChatAgent` round trip is graded: right task dispatched, result reflected in chat reply, failure paths still produce a user-facing message.
- **CI gate.** `cargo test --features eval -p mew-agent` is wired into CI; future changes can't silently regress the Bug #1 / #2 fixes.
- **Pass-rate tracking.** `docs/eval-history.md`.

### Phase 10 — Production hardening & release polish

- **Error-messaging pass.** All raw `Err` / panic / JSON surfaces in `mew-ui` Tauri commands route through `error_message::for_user(&err, action)` so the user always sees a plain-language sentence, not a stack trace.
- **Consolidated config schema.** `sensitive_platforms.toml`, `captcha` block, `summarization.verbosity`, per-platform budgets, and the `CaptchaSolver` provider fields are all documented in one place in the README and the proj doc.
- **Quiet-by-default tracing.** JSONL layer only fires when `MEW_TRACING_DIR` is set; production builds don't ship the per-step trace cost.
- **Latency profiling + caching.** End-of-task summary LLM call and pre-flight decomposition are cached and reused where possible. The `end_of_task_llm_summary` flag in `config.yaml` opts out cleanly.
- **Final README / docs pass.** README reflects the two-agent architecture, the new UI, the CAPTCHA solver trait, and the eval-harness CI gate. proj.md is the long-form design doc.
- **Three motivating-scenario acceptance pass.**
  1. *"Visit instagram and text my friend hi"* works directly through the via-search entry path.
  2. Any browser task shows a clean chat summary in the UI, never raw JSON.
  3. A multi-platform job-search task returns one consolidated list of roles + emails/URLs.

---

## 5. Configuration reference

The full config schema, with defaults and rationale:

```yaml
opencode_zen:
  base_url: <OpenAI-compatible endpoint>          # e.g. https://api.xiaomimimo.com/v1
  api_key: <secret>                                # never commit
  default_model: <model name>                      # e.g. mimo-v2.5
  max_iterations: 100                              # hard cap on ReAct loop iterations per task
  max_tokens: 740000                               # optional total context cap
  max_cost: <USD>                                  # optional hard cost cap

browser:
  binary_path: <path to chrome.exe>                # stealth binary preferred
  visible_cursor: false                            # ghost cursor overlay; default off

agent:
  allowed_domains: [list]                          # safety whitelist
  task_presets: { name: "task string" }            # named task shortcuts
  pacing:
    enabled: false
    min_delay_ms: 800
    max_delay_ms: 2500
    streak_threshold: 2
  summarization:
    verbosity: concise                             # concise | detailed
    live_lines_cap: 5
    end_of_task_llm_summary: true
  research:
    enabled: true
    platforms_file: config/research_platforms.toml
  captcha:
    enabled: false                                 # off by default
    solve_kinds: []                                # populated by the chosen provider
    provider: <name>                               # 2captcha | anti-captcha | capmonster | ...
    api_key_env: <env var>                         # env var holding the provider key
    per_session_cap: 5
```

`config/sensitive_platforms.toml` and `config/research_platforms.toml` are the two operator-editable data tables. See their inline comments for the full schema.

---

## 6. Operating notes

- **Profile retention.** `user_data_dir` is the Tauri shell's `mew-ui/src-tauri/profile/`. Logged-in IG / Google cookies persist across launches. Don't commit this directory.
- **Captured transcripts.** Every example run writes a session transcript under `mew-ui/src-tauri/transcripts/`. Use these to diff golden transcripts in CI.
- **`MEW_TRACING_DIR`.** Set to an absolute writable path before launching the agent to enable the JSONL tracing layer. Without it no trace file lands. Production builds ship with this off.
- **Eval history.** `docs/eval-history.md` is the append-only pass-rate log. Update after every CI run.
- **Phase docs.** `docs/bug-1-root-cause.md`, `docs/bug-2-root-cause.md`, `docs/bug-1-fix-verification.md`, `docs/phase3-handoff.md`, `docs/phase4-ui-overhaul.md`, `docs/phase5-summarization.md`, `docs/phase7-benchmarks.md`, `docs/phase8-captcha-handling.md` cover the design decisions and live verification artifacts for each phase.

---

*For the authoritative checkbox state of every phase, see `work.md`. For the quick-start and current project status, see `README.md`.*
