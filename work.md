# mew — a fast, robust, visible browser agent in Rust

**Goal:** a Rust-native computer-use agent that drives a real, visible Chromium window through CDP, perceives pages via accessibility-tree snapshots (not screenshots) for speed and low token cost, falls back to vision only when needed, uses a stealth Chromium binary so it doesn't get blocked, and is orchestrated by an LLM harness with prompt caching (where supported) to stay cheap on a $0 infra / API-only budget.

**Budget constraint:** $0 infra. Everything below uses free/open-source crates and binaries. The only recurring cost is LLM API tokens.

**Model provider (current):** this project is being built as an agentic harness configured via `config.yaml`, currently pointed at OpenCode Zen — an OpenAI-compatible endpoint. Current config:



This is intentionally swappable — `mew-agent` should read provider config (base_url, api_key, default_model) from `config.yaml` rather than hardcoding an API shape, so switching models/providers later doesn't touch the agent loop logic. Since the endpoint is OpenAI-compatible, use an OpenAI-style chat-completions request shape (`reqwest` + `serde_json`, or the `async-openai` crate pointed at the custom `base_url`) rather than Anthropic's native Messages API format. Note: prompt caching behavior (Step 7) is provider-specific — confirm whether OpenCode Zen / the underlying `mimo-v2.5-free` model supports any caching mechanism before assuming Anthropic-style `cache_control` semantics apply; if not, Step 7 becomes "minimize resent tokens by relying on Step 5's diffing" rather than true server-side cache discounting.

**How to use this file:** each numbered step is split into two sessions:
- **N.1 — Implementation.** Hand this to your coding agent and get the feature built.
- **N.2 — Review & testing.** Do this yourself, separately, after N.1 claims to be done. Your coding agent can lie or half-finish things — it may report success when a function is stubbed, silently skip an edge case, or claim something was tested when it wasn't run at all. N.2 is not "ask the agent if it works" — it's you (or a fresh agent instance with no stake in the prior claim) actually running the thing and checking the real output against the checklist. Don't move to N+1.1 until every item in N.2's checklist is genuinely verified, not just "looks right."

Check off `[ ]` → `[x]` as you go.

---

## Step 0.1 — Environment & project scaffold: implementation

Set up the Rust workspace before writing any real logic.

- [x] **Windows toolchain prerequisite — do this before anything else on Windows.** `chromiumoxide` and later crates have native dependencies that expect the MSVC toolchain, not GNU. Install "Desktop development with C++" via the Visual Studio Build Tools installer (free) *first*, then install Rust via `rustup` (or `winget install Rustlang.Rustup`) and confirm `rustup default` shows `stable-x86_64-pc-windows-msvc`, not `-gnu`. If a coding agent hits a missing-linker error and silently switches you to the GNU toolchain to route around it, don't accept that — stop, install the MSVC Build Tools, and switch back with `rustup default stable-x86_64-pc-windows-msvc`. The GNU toolchain is a workaround that dodges one error today and risks a harder-to-diagnose native-linking failure later once `chromiumoxide`'s dependency tree is in play.
- [x] Install Rust (stable, via rustup) and confirm `cargo --version` works.
- [x] Workspace members (this part is already scaffolded per the earlier init prompt):
  - [x] `mew-cdp` — CDP connection + launch logic (wraps `chromiumoxide`)
  - [x] `mew-perception` — accessibility tree extraction, ref assignment, diffing
  - [x] `mew-agent` — the LLM-driven reasoning loop (config-driven provider, currently OpenCode Zen)
  - [x] `mew-cli` — the binary entrypoint that wires it all together
- [x] Add `chromiumoxide` (with `tokio-runtime` feature), `tokio` (full features), `serde` + `serde_json`, `serde_yaml`, `reqwest` (json feature), `anyhow`, `tracing` + `tracing-subscriber` to the workspace `Cargo.toml`.
- [x] Create `config.yaml` at the project root with your provider block (see top of this file), and write a small `mew-agent` config loader (`serde_yaml` deserializing into a `ProviderConfig { base_url, api_key, default_model }` struct) — don't hardcode the key or model anywhere else.
- [x] Write a smoke test in `mew-cli` that loads `config.yaml`, does one `reqwest` POST to `{base_url}/chat/completions` (OpenAI-compatible shape: `{"model": ..., "messages": [...]}`, `Authorization: Bearer {api_key}` header) against `mimo-v2.5-free`, and prints the response.
- [x] Add `config.yaml` to `.gitignore` immediately, since it holds a real API key.

## Step 0.2 — Environment & project scaffold: review & testing

Don't trust "it builds" or "I ran it" claims — verify each yourself.

- [x] On Windows: run `rustup show` yourself and confirm the active toolchain is `stable-x86_64-pc-windows-msvc`. If it says `-gnu`, that's a silent workaround from a stalled/failed MSVC setup — stop here, install Visual Studio Build Tools ("Desktop development with C++"), and switch back before continuing to any later step.
- [x] Run `cargo build` yourself, from a clean terminal, and read the actual output. Confirm zero errors and note any warnings — don't accept "build succeeded" secondhand.
- [x] Open `config.yaml` and confirm it's real YAML with the three keys present, and that the api_key field actually holds your real key (not still the placeholder, not accidentally committed with the placeholder if you've since replaced it locally).
- [x] Run `git status` / check `.gitignore` yourself and confirm `config.yaml` is actually ignored — `git check-ignore -v config.yaml` should print a match. This is a real secret; verify it, don't take "I added it to gitignore" on faith.
- [x] Run the smoke test yourself and read the raw terminal output. Confirm it's an actual model-generated response (readable text relevant to whatever prompt you sent) — not an error being silently swallowed, not an empty string, not a hardcoded/mocked response the agent stuck in to make the test "pass."
- [x] Temporarily break the api_key (one wrong character) and re-run the smoke test — confirm you get a real auth error, not a silent success. This proves the test is actually hitting the network and checking the response, not just always printing something.
- [x] Check that no API key appears in any committed file, any log file written to disk, or anywhere in `git log` history if commits were already made.

**Done when:** you've personally run every command above and seen the real output with your own eyes — not a summary from the coding agent claiming it worked.

---

## Step 1.1 — Launch a visible Chromium via CDP: implementation

Get `chromiumoxide` driving a real, visible browser window before touching stealth or perception.

- [x] In `mew-cdp`, write a `launch()` function using `BrowserConfig::builder()` with `.with_head()` (headed, not headless) and a fixed `--remote-debugging-port`.
- [x] Point it at your normal system Chrome/Chromium binary for now — stealth binary comes later, don't conflate the two problems.
- [x] Set a persistent `user_data_dir` (a folder in your project, e.g. `./profile`) via the launch config so cookies/session state survive restarts.
- [x] Spawn the `Browser::launch` handler loop in a `tokio::spawn` task exactly as shown in chromiumoxide's docs, so CDP events are drained.
- [x] Open a new page and navigate to a simple site (e.g. `https://example.com`).
- [x] Add a clean shutdown path (`browser.close()`) so Chrome doesn't leak zombie processes between test runs.

## Step 1.2 — Launch a visible Chromium: review & testing

- [x] Run the binary yourself and watch your actual screen — confirm a real Chrome window appears, is genuinely visible (not off-screen, not behind other windows), and shows up in the taskbar. Don't accept a text log claiming "window opened" as proof.
- [x] Confirm the window actually navigates to `example.com` — read the page content with your own eyes, not just "navigation returned Ok."
- [x] Kill the process (Ctrl+C or however you're running it) and check your OS process list (`ps aux | grep chrome` / Task Manager) afterward — confirm zero leftover Chrome processes. A common failure mode is "close() was called but the child process wasn't actually reaped."
- [x] Run the binary 3 times in a row back-to-back. Confirm each run cleanly opens and closes with no leftover process accumulation and no port-already-in-use errors from the previous run's port not being released.
- [x] Check the `./profile` directory exists on disk and has real Chrome profile files in it after a run (not an empty folder — an empty `user_data_dir` means persistence isn't actually wired up even if the code compiles).
- [x] Restart the binary and manually verify session persistence works: visit a site, close, reopen, confirm you're still logged in / cookies persisted (pick any site where you can visually confirm this, like being logged into a test account).

**Done when:** you've watched the window open and close with your own eyes across multiple runs, and confirmed zero zombie processes and real profile persistence on disk.

---

## Step 2.1 — Basic action primitives: implementation

Build the small set of low-level actions everything else calls.

- [x] In `mew-cdp`, wrap the following as clean async functions taking a `Page` handle: `navigate(url)`, `click_selector(css_selector)`, `type_text(css_selector, text)`, `scroll(direction, amount)`, `press_key(key)`.
- [x] Use chromiumoxide's `find_element` + `.click()` / `.type_str()` for now (CSS-selector based) — this is a temporary crutch until Step 3 gives you ref-based targeting.
- [x] Write a manual test script that opens a page, types into a real search box, clicks a real button, and scrolls.
- [x] Add basic error handling: if a selector isn't found, return a typed error (not a panic).

## Step 2.2 — Basic action primitives: review & testing

- [x] Run the test script yourself and watch the visible window — confirm you see the actual cursor/page respond: text genuinely appears typed in the field, the click genuinely triggers whatever the button does (a real navigation, a real form submit, a real visible change), and scroll genuinely moves the page.
- [x] Pick a selector that doesn't exist on the page and call `click_selector` with it deliberately — confirm you get back a clean typed error, not a panic that crashes the whole process, and not a silent no-op that looks like success.
- [x] Check the actual error type/message is informative (which selector failed, not just "Err(())" or a generic string) — you'll be debugging against this message later when the agent is calling it, not you.
- [x] Try `type_text` against a field that's disabled or readonly and confirm it fails cleanly rather than reporting success while nothing changed on the page.
- [x] Re-run the same script against a second, different real site (not just the one it was built against) to catch selector logic that was accidentally hardcoded to one page's structure.

**Done when:** you've visually confirmed each primitive causing a real, correct effect in the browser window, and confirmed failure cases return real errors instead of panicking or silently no-opping.

---

## Step 3.1 — Accessibility tree extraction: implementation

This is the perception core — the piece that makes the agent fast instead of screenshot-slow.

- [x] In `mew-perception`, call `Accessibility.getFullAXTree` via chromiumoxide's typed CDP bindings against a live page.
- [x] Parse the returned flat list of `AXNode`s into an actual tree structure in Rust (a `TreeNode` struct with children).
- [x] Classify nodes by role into three buckets: `INTERACTIVE`, `CONTENT`, `STRUCTURAL` — defined as const arrays.
- [x] Prune structural nodes with no meaningful content in a "compact mode."
- [x] Print the resulting tree as readable indented text for a real page.

## Step 3.2 — Accessibility tree extraction: review & testing

- [x] Run it yourself against a real page you know well (something with a login form, a nav bar, some buttons) and read the printed tree line by line against what's actually on the page — confirm every visible interactive element (every button, link, input) actually appears in the tree with a sensible name. Missing elements here is a silent failure that will make the whole agent blind to things later.
- [x] Deliberately check for false negatives: open browser devtools yourself, inspect an element you'd expect to be interactive, and confirm it shows up correctly classified in your tree output — don't just check that the tree "looks plausible."
- [x] Check the compact-mode pruning isn't over-aggressive — confirm it didn't accidentally drop something interactive along with the structural noise. Compare tree size/content with pruning on vs off on the same page.
- [x] Run it against a second page with meaningfully different structure (e.g. a single-page app with dynamically rendered content vs a static page) and confirm it still produces a sensible tree, not something that was quietly overfit to the first test page.
- [x] Time the extraction call and print the duration — confirm it's genuinely fast (sub-second range), since "fast" is the entire point of this architecture; if it's slow, something's wrong before you build more on top of it.

**Done when:** you've manually cross-checked the tree output against the real page's actual visible elements on two different sites, and confirmed nothing interactive is silently missing.

---

## Step 4.1 — Ref-based element targeting: implementation

Replace fragile CSS selectors with stable references the LLM can act on reliably.

- [x] Extend the `TreeNode` from Step 3 so every `INTERACTIVE` node gets a short stable ref id (`@e1`, `@e2`, ...) generated from its `backend_dom_node_id`.
- [x] Add ref-based action functions in `mew-cdp` that take a `BackendNodeId` directly instead of a CSS selector.
- [x] Rewire your Step 2 test script to do the same "search + click" flow, but now driven purely by refs.
- [x] Handle the stale-ref case explicitly: if the page changed and a ref no longer resolves, return a clear error rather than silently misclicking.

## Step 4.2 — Ref-based element targeting: review & testing

- [x] Run the rewired test script and visually confirm each ref-based click lands on the exact element you'd expect — pick 3–4 specific refs from a printed tree, note by hand which element each one should be, then run the actions and confirm each one hit the right target on screen.
- [x] This is the step most likely to have a coding agent silently fall back to old selector logic instead of really using refs — grep the actual diff/code yourself for any leftover CSS-selector calls in the "ref-based" path and confirm they're genuinely gone, not just added alongside as a fallback that's quietly doing all the real work.
- [x] Deliberately trigger the stale-ref case: get a tree, take an action that changes the page (like a navigation), then try to use an old ref from before the change — confirm you get the clean stale-ref error, not a misclick on the wrong element or a silent no-op.
- [x] Test on a page where multiple similar elements exist (e.g. a list with 5 identical "delete" buttons) and confirm refs correctly disambiguate — click ref for item 3, confirm it was actually item 3 that got affected, not item 1.

**Done when:** you've hand-verified specific ref-to-element mappings are correct on a real page, confirmed no CSS-selector fallback is silently doing the work, and confirmed stale refs fail loudly.

---

## Step 5.1 — Snapshot diffing: implementation

Stop resending the whole page every step.

- [x] In `mew-perception`, keep the previous snapshot's tree in memory alongside the current one.
- [x] Write a diff function that compares two trees and outputs only newly appeared, removed, and changed nodes.
- [x] Serialize the diff as compact text — this is what gets sent to the model on steps 2+ of a task.
- [x] On the first observation of a task, send the full compact tree; every subsequent step sends only the diff.

## Step 5.2 — Snapshot diffing: review & testing

- [x] Type into a field on a real page, take a diff, and read the raw diff output yourself — confirm it contains only the changed field (and genuinely new/removed elements if any), not the entire page's tree again.
- [x] Actually measure this: print the character/token count of the full tree vs. the diff for the same interaction, and confirm the diff is meaningfully smaller — don't accept "diffing implemented" without seeing the size numbers with your own eyes.
- [x] Test a page where nothing changes between two snapshots (e.g. just re-snapshot the same static page) and confirm the diff is correctly empty or near-empty — an agent that always reports "some difference" even when nothing changed means the diff logic is broken, not conservative.
- [x] Test a case with real structural change (a modal opens, a new list item appears) and confirm the diff correctly reports it as newly-appeared, not misclassified as a false "changed" on some unrelated existing node.
- [x] Chain 4–5 real interactions in a row (type, click, click, scroll, type) and manually trace through: does each diff correctly reflect only what changed since the previous step, with no drift or accumulation of stale data across steps?

**Done when:** you've read real diff output against real page changes and confirmed with actual numbers that diffs are meaningfully smaller than full trees, and confirmed diffs stay accurate across a multi-step chain.

---

## Step 6.1 — LLM tool-use agent loop: implementation

Wire perception + actions into an actual reasoning loop driven by your configured model.

- [x] In `mew-agent`, define your tool schema using the OpenAI-compatible `tools` format: `navigate(url)`, `click(ref)`, `type(ref, text)`, `scroll(direction)`, `press_key(key)`, `snapshot()`, `finish(result)`.
- [x] Confirm `mimo-v2.5-free` actually supports function/tool calling before building further.
- [x] Implement the ReAct-style loop: send system prompt + task + latest snapshot/diff → model returns a `tool_calls` entry → execute it → append result as a `role: tool` message → repeat.
- [x] Add a hard iteration cap and a hard token/cost budget check per session.
- [x] Give it one genuinely useful end-to-end task by hand and watch the loop run.

## Step 6.2 — LLM tool-use agent loop: review & testing

- [x] Watch the entire run live in the visible browser window yourself, start to finish — don't just check the final "task complete" message. Confirm each intermediate action the model chose actually happened correctly on screen, in the order logged.
- [x] Log every raw tool call and its arguments to the terminal/file and read through the actual sequence yourself — confirm the model isn't calling `finish()` prematurely while claiming success on a task it didn't actually complete (a known failure mode: models reporting done when they got stuck and gave up).
- [x] Deliberately give it a task that should fail (e.g. search for something that won't exist, or point it at a page that doesn't have what it's asked for) and confirm it reports failure honestly instead of hallucinating a fabricated "result."
- [x] Verify the iteration cap actually triggers: give it a task designed to loop (or artificially lower the cap) and confirm the loop actually bails out cleanly at the limit instead of the cap being unenforced dead code.
- [x] Check that `role: tool` messages in the conversation genuinely contain the real snapshot/diff data returned from your perception layer — not a placeholder, not truncated silently, not the model's own guess being echoed back.
- [x] Run the same task twice and compare — some variance in exact steps is fine, but confirm both runs genuinely complete the task correctly rather than one run "succeeding" by accident.

**Done when:** you've watched a full run live and traced the actual tool-call log against what happened on screen, confirmed a deliberately-impossible task is reported as failed (not faked), and confirmed the iteration cap is real.

---

## Step 7.1 — Cost control: implementation

Make the loop from Step 6 cheap enough to run for real.

- [x] Check OpenCode Zen's docs/API response for any caching mechanism before assuming it exists.
- [x] If supported: apply the provider's caching breakpoint mechanism on your stable prefix.
- [x] If not supported: rely on Step 5's diffing, keep the system prompt lean, and consider trimming old irrelevant tool-result messages from the running conversation.
- [x] Log total tokens per session and per step.

## Step 7.2 — Cost control: review & testing

- [x] Read the actual per-step token numbers being logged, yourself, across a full multi-step session — don't accept a summary claim like "tokens reduced" without seeing the real logged figures.
- [x] If caching was claimed to be implemented: verify in the raw API response that a cache-related field actually shows non-zero cache reads/hits on step 2 onward — if the provider doesn't expose this, the "caching implemented" claim can't be true and needs correcting.
- [x] If relying on diffing instead: confirm by reading logs that input tokens per step are staying roughly flat (not growing unboundedly) across a 10+ step session — unbounded growth means old messages aren't actually being trimmed despite what was implemented.
- [x] Deliberately run a longer session (15–20 steps) and watch for the point, if any, where token usage starts blowing up — this tells you if the cost control genuinely holds at the length you actually plan to use the agent for, not just on a 3-step demo.

**Done when:** you've personally read real per-step token logs across a long session and confirmed costs stay bounded, with any caching claim backed by an actual field in the raw API response.

---

## Step 8.1 — Vision fallback: implementation

Cover the gap: canvas elements, image-only buttons, and other content the accessibility tree can't describe.

- [x] Detect the fallback trigger when a region has no meaningful accessible name/role and the agent needs to interact with it.
- [x] Add a `screenshot_region(selector_or_bounds)` tool that captures only the relevant area, never full-page.
- [x] Confirm `mimo-v2.5-free` accepts image input before building this; if not, configure a separate vision-capable model.
- [x] Send the cropped image, get back coordinates/description, act via coordinate click or translate to a ref-based action.

## Step 8.2 — Vision fallback: review & testing

- [x] Find or construct a real page with a genuine canvas/image-only-button case and run the agent against it — watch it actually trigger the vision fallback, and confirm via logs it only does so for that specific element, not for the whole page.
- [x] Check the captured screenshot file/data yourself — confirm it's genuinely cropped to the relevant region, not accidentally capturing the full page (which would blow your token budget silently).
- [x] Confirm the resulting click from the vision fallback actually lands on the intended element on screen — watch it happen live, don't just trust the model's claimed coordinates were used correctly.
- [x] Deliberately run a normal task with zero canvas/image-only content and confirm the vision tool is never called — grep the logs to prove the agent isn't reaching for screenshots out of habit when the accessibility tree already had the answer.
- [x] If a separate vision model had to be configured, confirm in logs which model actually served each vision call — a subtle failure mode is the harness silently routing vision requests to the non-vision default model and getting a degraded/garbage response back without erroring.

**Done when:** you've watched a real vision-fallback click land correctly on screen, confirmed the screenshot was genuinely cropped, and confirmed via logs the fallback is never used when it isn't needed.

---

## Step 9.1 — Stealth binary integration: implementation

Swap in the hardened Chromium binary so the agent survives real anti-bot pages.

- [x] Build or install a source-patched stealth Chromium (verify current maintenance status first).
- [x] Extend your `mew-cdp` launch config with a configurable `binary_path`, pointed at the stealth binary.
- [x] Keep headed mode and the persistent `user_data_dir`.
- [x] Inject defense-in-depth JS patches for any remaining automation flags.
- [x] Test against a real Cloudflare Turnstile demo page and/or reCAPTCHA v3 test page.

## Step 9.2 — Stealth binary integration: review & testing

- [x] Confirm yourself, by checking the process/binary path at runtime, that it's genuinely launching the stealth binary and not silently falling back to system Chrome (a real failure mode if the binary path is wrong and chromiumoxide auto-discovers a different Chrome install instead).
- [x] Run the exact same Turnstile/reCAPTCHA test page with stock Chrome first and note the result, then run it with the stealth binary and compare — confirm there's an actual observable difference (stock fails/challenges, stealth passes), not both behaving identically because the swap didn't really happen.
- [x] Check `navigator.webdriver` and a couple of other basic fingerprint values yourself via a `Runtime.evaluate` CDP call or by visiting a fingerprint-check site in the visible window, reading the results with your own eyes.
- [x] Re-run the entire Step 6 agent loop against the stealth binary and confirm it still works identically — a real risk here is the stealth binary having a slightly different CDP surface that silently breaks some earlier primitive.
- [x] If defense-in-depth JS patches were added, verify they're actually being injected on every new page (not just the first) by checking the flag values after a mid-session navigation, not only at launch.

**Done when:** you've directly compared stock vs. stealth binary behavior on a real bot-check page with your own eyes, confirmed the correct binary is genuinely running, and confirmed the full agent loop still works unmodified.

---

## Step 10.1 — Error recovery & robustness pass: implementation

Turn "it works on the happy path" into something that survives real, messy websites.

- [x] Add retry logic around navigation/click/type for transient failures.
- [x] Handle the stale-ref case by triggering an automatic re-snapshot and letting the model re-decide.
- [x] Add a timeout per tool call.
- [x] Log every tool call, input, and result to a session transcript file.
- [x] Run the agent against 2–3 sites you know are messy and iterate on real observed failures.

## Step 10.2 — Error recovery & robustness pass: review & testing

- [x] Read a full transcript file yourself, end to end, from one of the messy-site runs — confirm it's a complete, accurate record (every action logged, nothing silently dropped) that you could hand to someone else to understand exactly what happened.
- [x] Deliberately cause a transient failure (throttle your network briefly, or hit a page you know loads slowly) and confirm the retry logic actually retries a bounded number of times with backoff, rather than either failing immediately or retrying forever.
- [x] Deliberately cause the timeout to trigger (point it at something that will hang) and confirm the agent recovers and continues rather than the whole process freezing — watch this happen live, don't trust a log line claiming it recovered.
- [x] Confirm the auto-re-snapshot-on-stale-ref path from Step 4 genuinely gets exercised during real messy-site runs (check the transcript for it happening naturally, not just in an isolated unit test) and that the model's re-decision after re-snapshotting is sensible, not confused by the interruption.
- [x] Run each of the 2–3 messy sites at least twice and confirm consistent successful completion — a single lucky pass isn't robustness, and a coding agent may tune behavior to pass once and call it done.

**Done when:** you've read a complete real transcript, watched a live recovery from both a transient failure and a timeout, and confirmed repeatable success on messy sites across multiple runs, not just one.

---

## Step 11.1 — Scope lock & polish: implementation

Turn the working prototype into something you'll actually keep using.

- [x] Pick your real target scope — a fixed set of sites/tasks you personally use.
- [x] Write a short config for task presets, allowed domains, and per-session cost caps.
- [x] Add a simple CLI UX (`mew run "task description"` or similar).
- [x] Do a final pass on the visible-browser experience: window size/position, minimized-but-headed option, clean shutdown even on errors.

## Step 11.2 — Scope lock & polish: review & testing

- [ ] Run the final CLI yourself for each of your real target tasks, from a cold start, exactly the way you'd actually use it day to day — not through a special test harness the coding agent may have built around the "real" entrypoint.
- [ ] Confirm the allowed-domains restriction actually blocks an out-of-scope domain if you deliberately try one — don't just trust it's "configured," watch it actually refuse.
- [ ] Confirm the per-session cost cap actually halts a session if you deliberately push past it (lower it temporarily to test) — same principle as the iteration cap in Step 6, verify it's enforced, not just present in config and ignored by the code.
- [ ] Force an error mid-task (kill network, navigate to a broken URL) and confirm shutdown/cleanup is still clean afterward — check for zombie processes same as Step 1.2, this time under a failure path instead of the happy path.
- [ ] Live with it for a few real uses over a few days before calling it done — the deepest bugs in an agent like this usually only show up under real, varied use, not a single verification pass.

**Done when:** you've personally used the finished `mew` binary for real tasks across multiple sessions, confirmed both the domain restriction and cost cap actually enforce when pushed, and confirmed clean shutdown under a forced failure.

---

## Reference crates & binaries used throughout

- `chromiumoxide` (v0.9.x) — Rust CDP client, async/Tokio, typed protocol bindings including `Accessibility.getFullAXTree`. Works with any Chromium-family binary.
- `tokio`, `serde`/`serde_json`, `serde_yaml`, `reqwest`, `anyhow`, `tracing` — standard async/Rust plumbing.
- Stealth Chromium binary (source-patched) — chosen and verified in Step 9; check current maintenance status before building on it.
- Model provider: OpenCode Zen (OpenAI-compatible endpoint), configured via `config.yaml`, default model `mimo-v2.5-free` — the only recurring cost in this project, and currently free-tier. Swappable via config without touching agent loop code, provided any new model/provider keeps the same OpenAI-compatible tool-call and (optionally) vision-input shape.