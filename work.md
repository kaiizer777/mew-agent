# mew v3 — desktop UI, live chat routing, docked browser

**Context:** `mew` v1 (core agent: CDP driving, accessibility-tree perception, ref-based
actions, LLM ReAct loop, stealth, error recovery) and v2 (state machine, live mid-task
steering channel, URL resolution, task-completeness gating, visible cursor, pacing guard)
are both done. `mew-agent`, `mew-cdp`, `mew-perception` are working, tested Rust crates.
What's missing is a UI: a chat panel next to a real, visible, docked Chromium window,
where plain chat gets answered directly and browser-intent messages get routed into the
existing agent loop — including messages sent *while* the agent is mid-task, using the
live-chat channel v2 already built and verified.

This file does not re-litigate anything v1/v2 already solved. It only covers what's new:
the desktop shell, the routing layer, and the window-docking mechanism.

**Format is unchanged from v1/v2:** N.1 = implementation session, N.2 = review/testing
session, done separately, by you, with your own eyes. Don't move to N+1.1 until N.2 is
genuinely checked off. Each step is scoped to one sitting.

**Phases in this file, 1 through 6:**

| Phase | Covers |
|---|---|
| 1 | Tauri shell + workspace wiring (window on screen, IPC round-trip, structured-output check) |
| 2 | Intent routing (chat vs. browser-task classification) |
| 3 | Agent session lifecycle from the UI (launch, mid-task steering, clean session-end) |
| 4 | Live transcript streaming via Channels |
| 5 | Chromium window docking via CDP |
| 6 | UI polish & real-use pass |

---

## Guide for the coding agent working this file

Hand this whole section to your coding agent before it touches Step 1.1.

1. **One step per session, in order.** Implement exactly the checklist items in the
   current N.1 — nothing from a later step, nothing "while I'm in here." If you notice
   something later steps will need, note it in a comment, don't build it early.
2. **Never assume a library/API behaves a certain way — check the actual crate docs,
   the actual CDP response, or the actual current Tauri docs before writing code
   against it.** This file already flags the specific unverified assumptions (Step
   1.2's structured-output check, Step 4's Channel API shape); treat any other gap you
   hit the same way — verify, don't guess.
3. **Don't mark a checklist item `[x]` on your own judgment.** Implementation items
   get checked off by you after building them; review items only get checked off by
   the human actually running the thing and looking at real output. If you're not sure
   something is genuinely done, say so plainly instead of checking the box.
4. **No silent fallbacks.** If a planned approach doesn't work (a tool call fails
   validation, a window call errors, an IPC call doesn't fire), stop and report it —
   don't quietly swap in a workaround (e.g. reparenting, polling instead of events,
   free-text intent parsing) that this file already ruled out for a reason.
5. **Reuse existing v1/v2 code paths instead of rebuilding them.** The state machine,
   the live-chat channel, the transcript logging, the URL resolution — all already
   exist and are tested. This file's job is wiring a UI to them, not reimplementing
   them. If you find yourself writing agent-loop logic from scratch, stop and check
   whether it already exists in `mew-agent`.
6. **Keep the diff small and legible per session.** A session that touches every crate
   in the workspace for a one-line feature is a sign of scope creep — the whole point
   of the N.1/N.2 split is that a human can actually review what changed.
7. **When a step's own file text names a real gotcha (the CDP bounds/state exclusivity
   note, the `emit()` ordering warning), treat it as a hard constraint, not a
   suggestion** — it was put there because it broke something in research, not as
   color commentary.

---

## Decisions this file assumes (researched, not guessed — read once before starting)

- **Stack: Tauri 2, not Electron.** `mew-agent`/`mew-cdp`/`mew-perception` are existing
  Rust crates — Tauri's backend *is* Rust, so the UI layer calls them directly as a
  library dependency. Electron would force a sidecar/IPC bridge to a separate Rust
  process for no benefit.
- **Chromium is a sibling OS window, docked via CDP `Browser.setWindowBounds` —
  never reparented, never screencast.** Two alternatives were investigated and ruled
  out:
  - *Reparenting the Chrome HWND into the Tauri window* (`SetParent`/`SetWindowLongPtr`)
    broke upstream in Chrome 139 (2025) due to `WS_EX_NOREDIRECTIONBITMAP`/
    DirectComposition changes. Even the documented workaround (`WS_EX_LAYERED` +
    `SetLayeredWindowAttributes`) is described by Chromium's own team as fragile,
    undocumented "tribal knowledge." Not a foundation to build on.
  - *CDP screencast streamed into an `<img>`/canvas* (the Browserbase / AWS AgentCore /
    Mastra Studio pattern) exists because those products drive *headless/cloud*
    browsers with no real window to show. `mew` already has a real, visible, locally
    interactive Chromium — screencasting it away would add latency and throw away
    native scrolling/selection/zoom for no reason.
  - What's left, and what this file uses: `chromiumoxide` exposes
    `Browser.setWindowBounds` / `Browser.getWindowForTarget` as typed CDP bindings
    (`chromiumoxide::cdp::browser_protocol::browser::{SetWindowBoundsParams,
    GetWindowBoundsParams}`). This is real CDP, cross-platform (CDP delegates the
    actual OS window call internally), and needs zero `windows`-crate / raw Win32
    code, so it never touches the broken reparenting path at all.
- **Intent routing: one LLM call per message, structured output via a
  `classify_intent` tool call — not free-text parsing, not a second model.**
  Structured-output/enum classification is the documented 2026 best practice for
  small (2-label) routing decisions: cheaper and more consistent than a fine-tuned
  classifier for this label count, and avoids a second model/provider dependency.
  **However:** whether OpenCode Zen's raw `/chat/completions` proxy supports
  `response_format: {type: "json_schema"}` is *not confirmed* — that capability is
  documented for the OpenCode SDK/TUI product, not verified for the bare REST
  endpoint `mew-agent` calls with `reqwest`. Step 6 of v1 already proved tool-calling
  works against this exact provider/model. So: route intent through a
  `classify_intent(intent, reply)` **tool call** (schema-guaranteed via the mechanism
  already proven to work), not through `response_format`. Verify this assumption
  in 18.2 before building anything downstream on it — same discipline v2 used for
  the caching-support question in its own Step 7.
- **Rust → frontend streaming: Channels for the transcript/status feed, plain
  `emit()` events only for one-shot notifications.** Tauri's own docs flag that
  `app.emit()` under rapid, high-frequency emission can deliver out of order if
  listeners are async — explicitly recommending the Channel API for ordered,
  high-throughput data. The live agent transcript (state transitions, tool calls,
  streamed status) is exactly that case; a single "task finished" ping is not.
  Mixing these up is the most likely subtle bug in this phase — keep them separate
  from the start.

---

## Step 1.1 — Tauri shell + workspace wiring: implementation

Get a Tauri window on screen that can call into the existing `mew-agent` crate, before
any routing or docking logic exists.

- [x] Scaffold a Tauri 2 project (`cargo create-tauri-app` or manual) as a new workspace
  member, e.g. `mew-ui`, alongside the existing `mew-cdp` / `mew-perception` /
  `mew-agent` / `mew-cli` members — not a separate repo. Add `mew-agent` (and whatever
  of `mew-cdp` it needs) as a path dependency in `mew-ui/src-tauri/Cargo.toml`.
- [x] Pick a frontend stack for the chat UI (plain HTML/JS, or a framework if you
  already have a preference — this file doesn't mandate one). Build a minimal chat
  list + text input, no styling polish yet.
- [x] Wire one real Tauri command end to end: `send_message(text: String) -> String`
  that, for now, just echoes the text back — proves the JS ↔ Rust IPC round-trip
  works before any LLM or agent logic is added.
- [x] Confirm `mew-agent`'s existing code compiles as a dependency inside the Tauri
  binary (native deps like `chromiumoxide` sometimes need feature-flag adjustments
  when pulled into a new binary target — surface and fix any of that now, not later).
- [x] Set the main Tauri window's default size/position to occupy the left half of a
  typical screen (a fixed reasonable default is fine — dynamic multi-monitor handling
  is out of scope for this step).

## Step 1.2 — Tauri shell + workspace wiring: review & testing

- [ ] Run the Tauri app yourself (`cargo tauri dev` or equivalent) and watch a real
  window open on screen — confirm it's genuinely the left-half-of-screen size/position
  you configured, not a default centered window that the config silently didn't apply
  to.
- [ ] Type into the chat input, send it, and confirm the echoed response actually comes
  back through real IPC — check the browser devtools network/console yourself (Tauri
  apps can be inspected like any webview) to see the real `invoke` call and its
  response, not just trust the UI showing *something*.
- [ ] Confirm `mew-agent` really compiled in as a dependency: temporarily call one
  trivial function from it (e.g. a config loader) from a Tauri command and confirm it
  executes for real — read actual output, don't just trust a clean `cargo build`.
- [ ] **Resolve the structured-output assumption now, before Step 2 depends on it.**
  Write a tiny standalone test that sends one `reqwest` call to OpenCode Zen with a
  single tool defined (`classify_intent(intent: enum["chat","browser_task"], reply:
  string)`) and a forced tool choice, and read the raw JSON response yourself. Confirm
  the model actually returns a well-formed `tool_calls` entry matching the schema, not
  free text, not a malformed call, not silent refusal. If this fails, decide now
  whether to retry with a different `tool_choice` setting or fall back to a different
  approach — don't carry an unverified assumption into Step 2.
- [ ] Close the app and confirm no orphaned processes (the underlying webview host,
  any dev-server process) are left running — same zombie-process discipline as v1
  Step 1.2, now applied to the new binary.

**Done when:** you've watched a real Tauri window open at the size/position you set,
confirmed a real IPC round-trip with your own eyes in devtools, confirmed `mew-agent`
genuinely compiles and runs inside this new binary, and confirmed — with a real raw API
response you read yourself — whether tool-call-based structured output actually works
against OpenCode Zen.

---

## Step 2.1 — Intent routing: implementation

Every chat message needs to become either a direct reply or a routed agent task, using
whichever mechanism Step 1.2 just confirmed actually works.

- [ ] In `mew-agent` (or a new small `mew-router` module — your call), implement a
  `classify(message: &str, conversation_context: &[Message]) -> Intent` function where
  `Intent` is an enum `{ Chat(String), BrowserTask(String) }` — the classification call
  returns both the routing decision *and* the direct reply/rephrased task in one round
  trip, not two separate calls.
- [ ] Pass recent conversation history into the classification call, not just the
  single latest message — "open it" only makes sense as a browser-task if the prior
  turn named a site. Scope how much history you pass (last few turns is plenty; don't
  send the whole growing transcript into every classification call).
- [ ] Wire the `send_message` Tauri command from Step 1 to actually call `classify()`:
  on `Intent::Chat(reply)`, return the reply directly to the frontend. On
  `Intent::BrowserTask(task)`, hand off to Step 3's agent-session logic (stub this
  hand-off for now if Step 3 isn't built yet — just log that a browser task was
  detected and what task string was extracted).
- [ ] Handle the classification call itself failing (network error, malformed
  response) with a clear typed error surfaced to the frontend — don't let a
  classification failure silently swallow the user's message.

## Step 2.2 — Intent routing: review & testing

- [ ] Send 10+ varied real messages you'd actually type — plain small talk, clear
  browser tasks, and deliberately ambiguous ones ("check that for me", "open it",
  "what about the other one") — and read the actual classification decision for each
  one against what you'd expect. This is the step most likely to look "basically fine"
  on obvious cases while quietly getting the ambiguous ones wrong — don't skip the
  ambiguous set.
- [ ] Confirm conversation context is genuinely being used: have a two-turn exchange
  where turn 1 names a site and turn 2 says only "open it" — confirm it correctly
  routes as a browser task referencing the right site, not misclassified as plain chat
  for lack of context.
- [ ] Confirm a normal chat message never accidentally triggers Chromium to launch —
  send several genuinely conversational messages in a row and watch that no browser
  window opens, no agent session starts, nothing happens beyond a chat reply.
- [ ] Deliberately break the network (or point `base_url` at something invalid) mid-
  session and confirm the classification failure surfaces as a real visible error in
  the chat UI, not a silently dropped message or a frozen input box.
- [ ] Read the raw request/response for a few of these classification calls yourself
  (log them if not already) — confirm the reply text returned alongside `Intent::Chat`
  is a genuine, sensible reply and not a placeholder or the model's confused attempt
  to also call the classify tool when it shouldn't have.

**Done when:** you've read real classification outcomes across obvious and ambiguous
real messages, confirmed context from prior turns is actually used, confirmed plain
chat never triggers the browser, and confirmed classification failures are visible, not
silent.

---

## Step 3.1 — Agent session lifecycle from the UI: implementation

Connect a classified browser task to a real running `mew-agent` session, reusing v2's
state machine and live-chat channel rather than rebuilding either.

- [ ] On the first `Intent::BrowserTask` in a chat, spin up a real `mew-agent` session
  in a background Tokio task (via `tauri::async_runtime::spawn` or equivalent), exactly
  as `mew-cli` already does — same `SessionHandle`, same `checkpoint()`/state-machine
  machinery from v2 Step 12, same `mpsc::channel<UserMessage>` from v2 Step 13. This
  step is wiring, not new agent logic — resist rewriting anything `mew-agent` already
  does.
- [ ] Store the running session's `SessionHandle` and the channel's `Sender` half in
  Tauri's managed state (`app.manage(...)`), keyed by a session/chat id, so subsequent
  Tauri commands in the same chat can reach the same running session.
- [ ] Route every *subsequent* message in the same chat, while a session is active,
  straight to that session's existing `Sender` — bypassing Step 2's classifier
  entirely while a task is running. This is the actual point of v2 Step 13: the user
  should be able to say anything mid-task and have it steer the running agent, not get
  reclassified as idle chat.
- [ ] Decide and implement the exact "session is done" transition: when the agent
  reaches `Done`/`Failed`/`Stopped` (v2's `SessionState`), clear it from managed state
  so the *next* message goes back through Step 2's classifier instead of trying to
  steer a dead session.
- [ ] Surface a minimal but real status signal to the frontend for now (even just one
  `emit()` on state transitions is fine here — the proper Channel-based transcript
  stream is Step 4). The goal of this step is a correct, working session lifecycle;
  polished streaming comes next.

## Step 3.2 — Agent session lifecycle from the UI: review & testing

- [ ] Send a real multi-step browser task from the chat UI and confirm a real, visible
  Chromium window actually launches (position/docking isn't wired yet — that's Step
  22, a floating window is fine for now) and the task actually runs to completion,
  driven entirely from the UI, not the CLI.
- [ ] While that task is running, send a follow-up message from the same chat input
  ("also check the weather" or similar) and confirm — by reading the transcript file
  v2 already produces — that it was genuinely appended to the running session's
  conversation, not reclassified as a fresh chat message and not silently dropped.
  This is the actual UI-level proof of the thing you originally asked for.
- [ ] Let a task finish, confirm the session is genuinely cleared from managed state
  (not just that the UI *looks* idle) by sending a new plain-chat message afterward and
  confirming it goes through Step 2's classifier again rather than trying to steer a
  finished session — check logs for which path it took.
- [ ] Kill the Tauri app mid-task (force-quit, not graceful) and confirm no orphaned
  Chromium process is left running afterward — check your process list yourself, same
  standard as every prior zombie-process check in v1/v2.
- [ ] Start two separate chat sessions back-to-back (not simultaneously — sequentially)
  and confirm the second one starts clean, with no state bleeding over from the first
  session's managed-state entry.

**Done when:** you've driven a full real multi-step task from the UI, personally
interrupted it mid-task from the same chat input and confirmed via transcript that it
was incorporated (not dropped or restarted), confirmed clean session-end transitions
back to the classifier, and confirmed no orphaned processes after a hard kill.

---

## Step 4.1 — Live transcript streaming via Channels: implementation

Replace Step 3's placeholder status ping with the real ordered, high-throughput stream
the UI needs to feel alive — using Tauri's Channel API, not raw `emit()`, per the
ordering-risk finding noted at the top of this file.

- [ ] Add a Tauri Channel parameter to the session-start command (per Tauri's
  documented pattern: `on_event: tauri::ipc::Channel<T>` passed in from the frontend
  alongside the task text). Define a serializable event enum covering at minimum:
  state transitions (from v2's `SessionState`), each tool call + result, and the final
  per-subtask completion summary (from v2 Step 15).
  - Note: the current constraint is that a Channel must be created and passed in by
    the frontend at command-invocation time — confirm this against whatever Tauri
    version you land on when you get here, since IPC APIs are actively evolving; don't
    assume the exact call shape without checking current docs at implementation time.
- [ ] Have the running agent session push every relevant event onto this channel as it
  happens — reuse v2's existing transcript-logging call sites (state transitions are
  already logged with timestamps per v2 Step 12; tool calls are already logged per v1
  Step 10) as the trigger points, rather than inventing a second, separate
  instrumentation pass.
- [ ] On the frontend, render the incoming stream as a live-updating transcript/status
  area distinct from the plain chat bubbles — this is "what the agent is doing right
  now," not a chat message.
- [ ] Keep the one-shot `emit()` from Step 3 only for things that are genuinely
  one-off and don't need ordering guarantees (e.g. "a new session started") — don't
  migrate everything to Channels reflexively if it doesn't need the ordering
  guarantee, but don't leave the high-frequency transcript stream on `emit()` either.

## Step 4.2 — Live transcript streaming via Channels: review & testing

- [ ] Run a genuinely long, many-step real task and watch the live transcript area
  update in real time on screen — confirm events appear in the correct order start to
  finish, with no visible out-of-order jumps (this is the exact failure mode Tauri's
  own docs warn `emit()` is prone to under rapid emission — confirm the Channel switch
  actually avoids it, don't just assume it does because you used the "right" API).
- [ ] Deliberately compare: temporarily route the same event stream through plain
  `emit()` instead of the Channel, fire a burst of rapid events, and observe whether
  you can reproduce out-of-order delivery — then switch back and confirm the Channel
  version doesn't exhibit it. This is the one claim in this whole step worth actually
  falsifying rather than trusting the docs' word for it.
- [ ] Confirm the transcript stream shown in the UI genuinely matches the on-disk
  transcript file from v2/v1 for the same session — spot-check several entries side by
  side, don't just eyeball that "something is streaming."
- [ ] Trigger a mid-task steering message (as in Step 3.2) again, this time watching
  the live transcript — confirm the injected user message and the agent's next action
  both appear in the live stream in the correct order relative to what was already
  running, not just correctly logged to disk after the fact.
- [ ] Close and reopen the chat mid-task (if your UI allows navigating away) and
  confirm reattaching to the live stream doesn't duplicate, drop, or reorder events —
  or, if reattachment isn't supported yet, confirm that's a clean known limitation
  rather than a silent corruption of the stream.

**Done when:** you've watched a long real session stream live in correct order,
actually reproduced the `emit()` ordering problem once for comparison and confirmed the
Channel-based version doesn't have it, and confirmed the live stream matches the
on-disk transcript for the same session.

---

## Step 5.1 — Chromium window docking via CDP: implementation

Position the agent's Chromium window against the Tauri window using
`Browser.setWindowBounds`, per the researched decision at the top of this file — no
Win32, no reparenting.

- [ ] In `mew-cdp`, add a function that computes the target Chromium window rect from
  the Tauri window's current outer position/size (query via Tauri's window API) plus
  your chosen split (e.g. Chromium occupies the region to the right of the Tauri
  window, full screen height). This is pure arithmetic — no OS calls yet.
- [ ] Call `Browser.getWindowForTarget` to get the browser's `WindowID`, then
  `Browser.setWindowBounds` with the computed rect, right after a session's Chromium
  instance launches (this can reuse or extend v1's existing launch path in `mew-cdp` —
  don't duplicate the launch logic).
  - Gotcha confirmed in CDP's own protocol definition: `windowState`
    (minimized/maximized/fullscreen) and `left`/`top`/`width`/`height` cannot be set
    in the same `setWindowBounds` call — sending both errors out. Your bounds-only
    calls in this step are fine as long as you never also pass a state field; keep
    that in mind if a later step adds a "maximize"/"restore" affordance.
- [ ] In the Tauri app, listen for the main window's resize/move events
  (`tauri::WindowEvent::Resized` / `Moved`) and, when a Chromium session is active,
  recompute the target rect and re-call `set_window_bounds` so the two stay docked
  live rather than only aligning once at launch.
- [ ] Debounce the resize-triggered re-positioning (a raw per-pixel-event call to
  `set_window_bounds` on every intermediate resize frame is wasteful and may visibly
  lag) — a short debounce (e.g. reposition once movement pauses briefly, or throttle to
  a few times a second) is enough; don't over-engineer this.

## Step 5.2 — Chromium window docking via CDP: review & testing

- [ ] Launch a real task and watch both windows on screen — confirm Chromium actually
  lands docked against the Tauri window at launch, not overlapping it or appearing in
  an unrelated default position.
- [ ] Resize and move the Tauri window around the screen while a Chromium session is
  active and watch Chromium visibly follow/resize to stay docked — confirm this is
  actually live, not something that only worked once at launch and silently stopped
  updating.
- [ ] Confirm the debounce is real and reasonable: watch for visible lag or stutter
  during a drag-resize, and confirm `set_window_bounds` isn't firing on every single
  intermediate frame (check call frequency in logs if unsure) — but also confirm it's
  not so heavily debounced that the docking feels broken/delayed.
- [ ] Test at more than one screen size/resolution if you have access to one, or at
  least at two different manually-set Tauri window sizes — confirm the rect math
  genuinely adapts rather than being hardcoded to whatever size you tested first.
- [ ] Confirm the agent's actual perception/action layer (accessibility tree, ref
  clicks) still works correctly after the window has been resized/repositioned mid-
  session — a real risk here is coordinate-dependent logic (if any survived from v1's
  vision-fallback coordinate clicks) silently breaking after a bounds change that
  perception-based actions wouldn't notice.

**Done when:** you've watched Chromium dock against the Tauri window at launch and
visibly follow it live through resize/move on screen, confirmed the debounce is neither
laggy nor excessive, and confirmed the agent's real actions still land correctly after
a mid-session repositioning.

---

## Step 6.1 — UI polish & real-use pass: implementation

Same spirit as v1's Step 11 and v2's pacing/cursor steps — turn a working prototype
into something pleasant to actually use daily.

- [ ] Add a visible, unambiguous state indicator in the UI (idle / classifying /
  running / paused / done / failed) driven by the real `SessionState` values already
  streaming in from Step 4 — not a separate, hand-maintained UI state that can drift
  out of sync with the real agent state.
- [ ] Add a clear affordance for the v2 `pause()`/`resume()` machinery — a real button
  in the UI, not just a channel message typed as text — for the case where the user
  wants to freeze the agent and take manual control of the browser rather than just
  steer it.
- [ ] Handle the empty/idle state, error states, and a genuinely long-running task
  gracefully in the UI (loading affordance, no frozen-looking input box).
- [ ] Do a final pass on window defaults (Tauri window size/position, Chromium's
  initial docked size) so a cold start looks intentional, not like leftover dev
  defaults.

## Step 6.2 — UI polish & real-use pass: review & testing

- [ ] Use the finished app for several real tasks across a few real sessions, cold
  start each time, the way you'd actually use it day to day — not through a special
  dev harness.
- [ ] Confirm the state indicator genuinely tracks reality: deliberately pause a
  session via the new button and confirm the indicator reflects `Paused` correctly and
  promptly, then resume and confirm it flips back.
- [ ] Force a failure (bad task, broken network) and confirm the UI shows a real,
  legible failed state rather than hanging on "running" forever or showing a raw
  unhandled error.
- [ ] Force-quit the app mid-task one more time, now against the fully wired system,
  and re-confirm zero orphaned Chromium processes — this check earns re-verification
  every time new lifecycle code is added, per the same standard as every earlier phase.
- [ ] Live with it for a few real days of actual use before calling it done — per v1's
  own closing note, the deepest bugs in a system like this surface under real, varied
  use, not a single pass.

**Done when:** you've used the finished app for real tasks across multiple real
sessions, confirmed the state indicator and pause/resume affordance genuinely reflect
the real agent state, confirmed graceful failure display, and re-confirmed clean
shutdown under a forced kill on the fully integrated system.