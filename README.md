# mew — a fast, robust, visible browser agent in Rust

**mew** is a Rust-native computer-use agent that drives a real, visible Chromium window through the Chrome DevTools Protocol (CDP). It perceives web pages via accessibility-tree snapshots for speed and low token cost, falls back to vision only when necessary, uses a stealth Chromium binary to evade bot detection, and is orchestrated by an LLM harness designed to stay cheap on a $0 infrastructure budget.

## 🚀 Key Features

*   **Fast Perception**: Uses accessibility-tree snapshots and intelligent diffing instead of full-page screenshots to minimize token usage and maximize speed.
*   **Live Steering (v2)**: Features a live chat channel that allows users to interrupt and steer the agent mid-task without resetting its state or losing progress.
*   **Reliable Navigation (v2)**: Includes a robust URL resolution layer to fix common "open X" LLM failures, seamlessly falling back to search when direct navigation fails.
*   **Task Completeness Checks (v2)**: Enforces evidence-based task verification (requiring fresh snapshot diffs) to prevent the agent from falsely reporting partial completions as fully done.
*   **Vision Fallback**: Intelligently falls back to targeted, regional screenshots for canvas elements or image-only buttons when accessibility data is insufficient.
*   **Stealth Mode**: Integrates with patched stealth Chromium binaries to survive real-world anti-bot pages (e.g., Cloudflare Turnstile, reCAPTCHA).
*   **Cost Controlled**: Built with hard iteration caps, token logging, and prompt caching (via OpenCode Zen or similar OpenAI-compatible endpoints) to ensure low recurring costs.
*   **Visible Cursor (v2)**: Optional cosmetic ghost cursor overlay to visually track the agent's intended interactions.
*   **Pacing Guards (v2)**: Configurable site-specific pacing guards to naturally space out repetitive actions and avoid anti-automation triggers.

## 🏗️ Architecture

The workspace is split into focused, modular crates:
*   `mew-cdp` — CDP connection and launch logic (wraps `chromiumoxide`).
*   `mew-perception` — Accessibility tree extraction, stable reference assignment, and state diffing.
*   `mew-agent` — The LLM-driven reasoning loop, state machine, and tool execution.
*   `mew-cli` — The binary entrypoint, CLI UX, and live-chat stdin reader.

## 🛠️ Prerequisites

*   **Rust**: `stable-x86_64-pc-windows-msvc` toolchain (on Windows, ensure Visual Studio C++ Build Tools are installed before rustup).
*   **Chromium**: A local Chromium/Chrome installation (a stealth binary is highly recommended).
*   **LLM Provider**: An OpenAI-compatible API endpoint.

## ⚙️ Configuration

Create a `config.yaml` in the root directory:

```yaml
base_url: "https://your-llm-endpoint/v1"
api_key: "your_api_key_here"
default_model: "your_preferred_model"
visible_cursor: true
```
*(Note: Never commit your `config.yaml`!)*

## 📚 Development Status

This project is built iteratively following a strict "Implementation -> Review & Testing" checklist format (detailed in `work.md` and `v2.md`). 

Core capabilities including CDP navigation, reference targeting, snapshot diffing, ReAct loop, vision fallback, live steering, and evidence-based completion have been successfully implemented and verified against real-world scenarios.

---
*Built with `chromiumoxide`, `tokio`, and standard Rust async plumbing.*
