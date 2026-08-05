# mew — a fast, robust, visible browser agent in Rust

**mew** is a Rust-native computer-use agent that drives a real, visible Chromium window through the Chrome DevTools Protocol (CDP). It perceives pages via accessibility-tree snapshots for speed and low token cost, falls back to vision only when necessary, runs a stealth Chromium binary, and is orchestrated by a two-agent LLM harness (intent-routing **ChatAgent** + ReAct-loop **BrowserAgent**) designed to stay cheap on a $0 infrastructure budget.

All ten phases of the work plan are shipped: from the original accessibility-first perception and the two-agent handoff, through sensitive-platform routing, live step summarization, the resilience core, the long-horizon research loop, CAPTCHA / challenge handling, the pure-Rust evaluation harness wired into CI, and the production hardening pass.

The Tauri shell is a single chat surface docked alongside Chromium: intent classification, live mid-task steering, structured `ChatAgent → BrowserAgent → ChatAgent` handoffs, and live human-readable progress lines instead of raw JSON.

## What mew is, in one sentence

You type something in the chat box — "go to instagram and text my friend hi" or "find remote Rust jobs on LinkedIn and give me a contact email" — and a real Chromium window opens, navigates, clicks, types, and reports back in natural language. Sensitive platforms (Instagram, Twitter, LinkedIn, TikTok, Facebook) are entered through a search-engine referrer instead of a bare direct nav. The agent pauses for human-in-the-loop on CAPTCHA pages. Long-horizon research tasks fan out across multiple platforms, each with its own step + time budget.

## Highlights

- **Accessibility-first perception.** `Accessibility.getFullAXTree` is the primary sensory input; full-page screenshots are reserved for the vision fallback (`screenshot_region`) on canvas / image-only elements. Diffs between snapshots are what the LLM context sees, not full trees.
- **Two-agent architecture.** `ChatAgent` handles intent classification, conversation, and result synthesis. `BrowserAgent` runs the ReAct loop. A typed `Handoff` (`task_description`, `subtasks`, `constraints`, `originating_message_id`) flows ChatAgent → BrowserAgent; a typed `Result` (`status`, `summary`, `key_findings`, `final_snapshot_signature`) flows back. No session ends without a user-facing chat message.
- **Deterministic pre-flight planning.** Compound instructions ("go to X" + "do Y") are split into a typed `Vec<SubTask>` by `mew_agent::planner` *before* the first LLM call. Every subsequent system prompt carries the `PLAN (pre-flight decomposition)` block.
- **Sensitive-platform routing.** Domains listed in `config/sensitive_platforms.toml` (instagram.com, twitter.com, x.com, facebook.com, linkedin.com, tiktok.com and their `www.` variants) are routed through a search-engine entry instead of a bare direct nav. This dodges the "browser appears on instagram.com from nowhere" anti-bot classifier.
- **URL Resolution Layer.** Three branches — hardcoded map → direct `.com` probe (4s timeout) → `google.com/search?q=…` fallback — run before any `navigate` call reaches CDP.
- **Live step summarization.** Every tool dispatch emits a one-line human note ("Opened instagram.com", "Typed 'John' into search box"). Templated for the common tools, LLM-rewritten only at the end-of-task gate. Concise / Detailed verbosity toggle. Live ring buffer capped at the last 5 lines, with total step count preserved.
- **One chat surface, no transcript panel.** The Tauri UI is a single chat list: user messages right-align, everything else left-aligns. Task lifecycle (`task_started` → `task_progress` → `task_completed` / `task_failed`) renders as a card with a semantic left-border. The raw event log is available behind a collapsible "view details" on each task card, not as a separate always-on panel.
- **Resilience core.** Six production failure modes are hardened: stale `@eX` ref → bounded re-snapshot retry; cookie banners / modals / "sign in to continue" → auto-dismiss or force as first required action; mid-task login loss → explicit surfacing; 429 / Cloudflare block pages → exponential backoff; irreversible actions (submit order, send payment, delete, post publicly) → `Paused` checkpoint for confirmation; low-confidence vision fallback → re-prompt or ask the user.
- **Long-horizon research loop.** A typed `ResearchPlanner` reads `config/research_platforms.toml`, fans a goal out across platforms (LinkedIn, Indeed, Wellfound, WeWorkRemotely, RemoteOK by default; glassdoor, monster, YC commented out), runs a `FindingStore` for cross-platform deduplication, enforces per-platform step + time budgets, and emits a single consolidated answer (role + email/URL per finding) at the end.
- **CAPTCHA / challenge handling.** Detects Cloudflare Turnstile, reCAPTCHA v2/v3, hCaptcha. Default behavior on detection: pause the session, message the user, let the human solve it in the visible browser window. Optional opt-in third-party solving-service integration is wired through the `CaptchaSolver` trait (off by default). Local-only telemetry records per-domain challenge counts.
- **Visible ghost cursor.** A fixed-position overlay follows every click target and ripples on dispatch. Zero overhead when disabled (`visible_cursor: false`).
- **Pacing guard.** Site-aware anti-ban jitter. Consecutive identical actions in a tight loop are spaced by a configurable random delay; one-off and mixed actions never get paced. Opt-in (`pacing.enabled: false` by default).
- **Evaluation harness + CI gate.** A pure-Rust mock-page scenario runner in `mew_agent::eval` replays fixed tasks end-to-end (no live Chrome, no LLM calls) and grades the handoff contract: right task dispatched, result reflected in chat reply, failure paths still produce a user-facing message. Wired into CI as `cargo test --features eval` so future changes can't silently regress the Bug #1 / #2 fixes.
- **Production hardening.** All error messages are rewritten through a plain-language `error_message` layer routed via `ChatAgent`. Tracing is quiet by default in production builds (opt in via `MEW_TRACING_DIR`). End-of-task LLM summary and pre-flight decomposition are cached and reused where possible. Final pass against the three motivating scenarios: (1) "visit instagram and text my friend hi" works directly, (2) any browser task shows a clean chat summary in the UI, never raw JSON, (3) a multi-platform job-search task returns one consolidated list of roles + emails/URLs.

## Workspace layout

The workspace is a Cargo workspace with focused, modular crates:

| Crate | Purpose |
| --- | --- |
| `mew-cdp` | CDP connection, stealth launch, window-docking geometry, ghost-cursor injection, screencast capture. Wraps `chromiumoxide`. |
| `mew-perception` | Accessibility-tree extraction, stable `@eX` ref assignment, semantic classification, snapshot diffing. |
| `mew-nav` | URL Resolution Layer + sensitive-platform routing table loader. Pure-Rust library (no `serde_yaml` dep — uses `toml`). |
| `mew-resilience` | The six failure-mode detectors (stale ref, modal interrupts, session loss, rate limit, irreversible actions, vision confidence) + mock-page fixtures for testing without a live browser. |
| `mew-agent` | `ChatAgent` + `BrowserAgent` + `Orchestrator`, `Handoff` / `Result` types, `CompletenessTracker`, `PacingGuard`, `planner`, `summarizer`, `research`, `eval`. |
| `mew-cli` | Headless CLI entrypoint (still useful for debugging, transcript logging, and the no-UI examples). |
| `mew-ui` (Tauri) | Tauri 2 shell, IPC bridge, `TauriSink` (the orchestrator's `TurnSink` impl for Tauri events), intent router, single chat surface, Tauri Channel for live events. |

## Prerequisites

- **Rust**: `stable-x86_64-pc-windows-msvc` (Windows). Visual Studio C++ Build Tools installed before rustup.
- **Chromium**: A local Chromium / Chrome binary. The stealth binary at `stealth-browser/chrome.exe` is preferred — vanilla Chrome trips different anti-bot paths.
- **LLM provider**: An OpenAI-compatible endpoint (e.g. OpenCode Zen). Default config uses `https://api.xiaomimimo.com/v1` with `mimo-v2.5`.
- **Node.js / npm**: For building the Tauri UI frontend (Vite + TypeScript).

## Configuration

`config.yaml` at the workspace root is the main config. The two platform tables (`sensitive_platforms.toml`, `research_platforms.toml`) live in `config/`.

```yaml
opencode_zen:
  base_url: https://api.xiaomimimo.com/v1
  api_key: sk-...your key here...
  default_model: mimo-v2.5
  max_iterations: 100
  max_tokens: 740000

browser:
  binary_path: c:\Users\bari2\Desktop\mew-agent\stealth-browser\chrome.exe
  visible_cursor: false

agent:
  allowed_domains:
    - example.com
    - en.wikipedia.org
    - github.com
    - google.com
    - www.google.com
    - docs.rs
    - crates.io
    - localhost
  pacing:
    enabled: false
    min_delay_ms: 800
    max_delay_ms: 2500
    streak_threshold: 2
  summarization:
    verbosity: concise        # concise | detailed
    live_lines_cap: 5
    end_of_task_llm_summary: true
  research:
    enabled: true
    platforms_file: config/research_platforms.toml
  captcha:
    enabled: false            # off by default. See "Ethical / ToS boundary" below
    solve_kinds: []           # populated by the third-party solver provider at runtime
    provider: <name>          # free-form, e.g. "2captcha", "anti-captcha", "capmonster"
    api_key_env: <env var>    # env var holding the provider key
    per_session_cap: 5
```

Never commit `config.yaml` — `api_key` is a secret. `.gitignore` excludes it by default.

## Running it

### Tauri UI (production path)

```bash
cd mew-ui
npm install
npm run dev               # Vite + Tauri dev shell
# or
npm run build && cargo tauri build
```

A headed Chromium window opens, the chat panel docks to the left of it, and you type in the input box. Every browser task shows up in the chat list as a card with a "Working · N steps" header. The first user message after a quiet period triggers intent classification: conversational chat stays in the chat; anything that smells like "go to X / do Y" gets a `chat-task-started` card and a real Chromium session.

### Headless CLI (debug / no-UI)

```bash
cargo run -p mew-cli --bin agent_test
```

Useful for transcript logging, pacing experiments, and stale-ref / network-failure / chat-channel / completeness smoke tests (the `mew-cli/src/bin/` examples).

### Targeted regression tests

```bash
# Phase 2: instagram phrasings
cargo run --example phase2_instagram_regression -p mew-agent

# Phase 3: ChatAgent → BrowserAgent → ChatAgent round trip
cargo run --example phase3_round_trip -p mew-agent

# Phase 5: live step summarization
cargo run --example phase5_live_progress -p mew-agent

# Phase 6: resilience detectors
cargo run --example phase6_resilience_core -p mew-agent

# Phase 7: long-horizon research loop
cargo run --example phase7_benchmarks -p mew-agent

# Phase 9: evaluation harness + CI gate
cargo test --features eval -p mew-agent
cargo run --example phase9_eval_harness -p mew-agent
```

## Architecture, in 60 seconds

```
┌──────────────────────────────────────────────────────────┐
│  mew-ui (Tauri 2)                                        │
│  - single chat surface                                   │
│  - Channel<any> for live events (State/Tool/Summary/     │
│    ProgressLine)                                         │
│  - Tauri commands: send_message, pause_session, ...      │
└────────────────────────┬─────────────────────────────────┘
                         │  invoke('send_message')           │
                         │  Channel<OrchestratorEvent>       │
                         ▼
┌──────────────────────────────────────────────────────────┐
│  mew-agent::orchestrator                                 │
│  - TauriSink maps OrchestratorEvent → app.emit(...)      │
│    events: chat-task-started, chat-reply,                │
│    chat-task-completed, chat-steering-ack,               │
│    agent-state, browser-result-ready                     │
└────────────────────────┬─────────────────────────────────┘
                         │
       ┌─────────────────┴──────────────────┐
       ▼                                    ▼
  ChatAgent                          BrowserAgent
  - intent classify                  - ReAct loop
  - build_handoff (Handoff)          - perceive → act
  - synthesize_reply (Result)        - emit ProgressLine
                                     - completeness gate
                                     - finish() → BrowserResult
```

The `Handoff` struct (`task_description`, `subtasks`, `constraints`, `originating_message_id`) is the unit ChatAgent hands to BrowserAgent. The `Result` struct (`status: Done | Partial | Failed`, `summary`, `key_findings`, `final_snapshot_signature`, `raw_transcript_ref`) is what BrowserAgent hands back. `ChatAgent::synthesize_reply` turns the `Result` into the user-facing text that lands in the chat list as a `chat-reply` event.

The orchestrator is the only place that produces a user-facing chat message — a `Failed` transition always lands a `ChatReply` with a human-readable reason, never silent. All error paths route through the `error_message` layer, so raw `Err` / panic / JSON surfaces never reach the user as-is.

## Sensitive-platform routing

A new file, `config/sensitive_platforms.toml`, lists domains that need special entry. Pre-seeded entries cover the Instagram, Twitter/X, Facebook, LinkedIn, and TikTok families. Each row carries an `EntryStrategy`:

- `direct` — same as not being in the table. Listed for audit clarity.
- `via_search` — `navigate("instagram")` becomes `https://www.google.com/search?q=instagram`. The LLM has to click the organic result to actually land on the target.
- `via_search_confirm` — like `via_search` but with a "login" keyword appended, nudging the search results toward the canonical sign-in URL (useful for LinkedIn where the LLM usually needs the authenticated surface immediately).

Each row also carries a `known_to_challenge_bots` flag that downstream code (pacing, telemetry) reads to slow down before the navigation and to seed the per-domain challenge baseline.

Why a separate file? `mew-nav` is a pure-Rust library; pulling `serde_yaml` (only used by `mew-agent`'s config) into it would be the wrong dependency direction. `toml` is a tiny zero-config dep. Operators can edit the list without touching `config.yaml`.

**Where the table is loaded from.** `SensitivePlatforms::load_from_default_location` resolves `config/sensitive_platforms.toml` in this order:

1. `MEW_WORKSPACE_DIR` env var, if set — treated as the repo root.
2. A parent-directory walk from `std::env::current_dir()`, bounded at 16 hops.
3. Fall back to an empty table (with a `tracing::debug` log) so pre-Phase-2 setups still run.

The Tauri shell sets `MEW_WORKSPACE_DIR` automatically in its setup hook (it walks up from the executable's path looking for a `Cargo.toml` that contains a `[workspace]` section, and uses the first hit as the workspace root). This makes the reroute work for both dev builds (where CWD is `target/debug/` and the walk would otherwise have to hop several levels) and release bundles (where the binary lives in `Program Files\mew-ui\` or `mew-ui.app/Contents/MacOS/` and has no parent with the file). Operators can also set the env var manually before launching `app.exe` to point at a different config tree.

> **Phase 16.3 note.** The pre-16.3 implementation used `Path::new("config/sensitive_platforms.toml")` directly, which only worked when the process's CWD happened to be the workspace root. The Tauri shell's CWD is `target/debug/`, so the table silently failed to load and `navigate("https://www.instagram.com")` returned "domain not in allowlist" because the resolver fell through to the `already-url` branch instead of converting to a Google search URL. Pin tests: `mew-nav`'s `load_from_default_location_walks_parents` and `load_from_default_location_honors_workspace_dir_env`.

## Obstacle & CAPTCHA handling

mew detects four challenge families in the rendered accessibility tree:

- **Cloudflare Turnstile** (the 2022+ replacement for "I'm Under Attack")
- **Google reCAPTCHA v2** ("I am not a robot" checkbox + image grid)
- **Google reCAPTCHA v3** (invisible / score-based — detected via the v3 script + badge)
- **hCaptcha** (the privacy-focused alternative)

Default behavior on detection: **pause the session, message the user clearly, let the human solve it in the visible browser window.** The chat list shows something like:

> *Cloudflare Turnstile challenge detected on instagram.com — The agent has paused. Please solve the challenge in the browser window, then tell the agent to continue.*

Because mew runs a real, visible, headed browser, the human is the safest solver. The next snapshot detects the challenge is gone and the agent resumes.

Sites known to challenge bots get an extra 1.5–3.0s "settle-in" delay before each navigation. Local-only telemetry records per-domain challenge counts in `<data_dir>/captcha_telemetry.json`. No data leaves the machine.

The optional `CaptchaSolver` trait is the pluggable extension point for unattended runs. The default `enabled: false` keeps the pause-and-message path; an opt-in `provider` + `api_key_env` config block routes through the provider-specific implementation when the user explicitly chooses unattended behavior.

## Ethical / ToS boundary

mew's default behavior on a challenge page — **avoidance, slow pacing, human handoff** — is the 2026 consensus path and the path that keeps you inside the Terms of Service of the sites you visit.

Many sites' ToS **explicitly forbid** automated solving. Cloudflare, Google, and most major hCaptcha-using sites treat solver-API traffic as a violation that can result in:

- Permanent account ban on the offending site
- IP-range level blocking that affects all users behind the same NAT / VPN
- Reputational damage if the account is associated with public-facing work

Solving services also cost money — per-challenge pricing in the $1–3 / 1000 range as of 2026, with reCAPTCHA v2/v3 cheaper than image-grid challenges.

For these reasons, the `agent.captcha` block in `config.yaml` is **off by default**, and enabling it is an *explicit* consent. The `CaptchaSolver` trait ships with implementations for 2captcha, anti-captcha, and capmonster — pick one in `config.yaml` and set the matching `api_key_env`. Per-provider cost and ToS guidance is in `docs/phase8-captcha-handling.md`.

**Enable `enabled: true` only when:**

1. You have read and accept the ToS of every site your agent will visit.
2. You understand the per-challenge cost.
3. You have a legitimate use case (accessibility research, a personal workflow on a site you own).

If your task is to *visit* a public site and *interact* with it normally, **the default behavior is what you want**. Solve the one or two challenges that come up in the visible browser window; do not enable automated solving.

## Project status

All ten phases from `work.md` are **shipped**:

- **Phase 1** — structured tracing, URL Resolution span instrumentation, `SessionHandle` transition spans, intent-router boundary tracing, live reproductions of Bug #1 (both phrasings) and Bug #2 captured.
- **Phase 1.5** — minimum `chat-reply` emit fix, frontend listener, root-cause doc update, replaced `browser_task_result_dropped` tripwire with the positive `browser_task_result_delivered`.
- **Phase 2** — sensitive-platform routing table, `ResolutionPath::ViaSearch` / `ViaSearchConfirm`, pre-flight decomposition via `mew_agent::planner`, regression fixture + 17 planner tests + 11 nav tests, 10/10 pass rate on the original failing prompt logged in `docs/bug-1-fix-verification.md`.
- **Phase 3** — `ChatAgent` / `BrowserAgent` split, typed `Handoff` / `Result`, `synthesize_reply` integration, mid-task steering ack channel, full `ChatAgent → BrowserAgent → ChatAgent` round trip verified live.
- **Phase 4** — single chat surface, `MessageKind` type system, collapsible "view details", "Working · N steps" header pill, ghost-cursor / docking visuals re-verified unaffected, instagram e2e re-tested clean.
- **Phase 5** — live step summarization, `AgentEvent::ProgressLine` channel variant, end-of-task LLM summary through the `Result` struct, ring buffer cap, verbosity toggle.
- **Phase 6** — six failure-mode detectors (`ref_recovery`, `modal_interrupts`, `session_loss`, `rate_limit`, `irreversible_actions`, `vision_confidence`) + mock fixtures.
- **Phase 7** — `ResearchPlanner`, falsifiable-commitment checkpoint per subtask, `FindingStore` cross-platform dedup, "no result on this platform ≠ task failed", consolidated final synthesis, per-platform budgets, end-to-end benchmarks logged in `docs/phase7-benchmarks.md`.
- **Phase 8** — challenge-page detector, pause-and-message default, `CaptchaSolver` trait + 2captcha / anti-captcha / capmonster implementations, `known_to_challenge_bots` flag, local-only telemetry, README ethical / ToS boundary.
- **Phase 9** — `mew_agent::eval` mock-page harness, `cargo test --features eval -p mew-agent` CI gate, handoff-contract assertions, `docs/eval-history.md` pass-rate log.
- **Phase 10** — error-messaging pass through `mew_ui::error_message`, consolidated config schema, quiet-by-default tracing, end-of-task summary + planner cache, README / docs final pass, three motivating-scenario acceptance pass.

See `proj.md` for the full phase-by-phase architecture and design history, and `work.md` for the authoritative checkbox state of every phase.

---

*Built with `chromiumoxide`, `tauri`, `tokio`, `reqwest`, and standard Rust async plumbing.*
