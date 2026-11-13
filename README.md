# Mew — A Fast, Robust, Visible Browser Agent in Rust

**Mew** is a Rust-native computer-use agent that drives a visible Chromium window via the Chrome DevTools Protocol (CDP). Rather than relying on slow, expensive full-page vision models for every step, Mew perceives pages through fast accessibility-tree snapshots and diffs, falling back to targeted regional screenshots only for visual-only or canvas elements. Built to run locally on a $0 budget, it pairs a stealth Chromium runtime with a clean Tauri 2 desktop UI docked alongside the live browser.

## How It Works

Mew separates conversation from execution using a structured multi-agent harness:

- **ChatAgent:** Classifies intent, handles conversation, constructs browser task handoffs, and synthesizes findings into clean replies.
- **Planner & Supervisor Pool:** Splits complex instructions into budgeted subtasks executed by supervised workers, managing health heartbeats, pause/resume signals, and automatic context handoffs.
- **BrowserAgent:** Drives Chromium via a ReAct loop, reading incremental accessibility-tree diffs (`@eX` references), dispatching actions, and verifying completion.

## Core Capabilities

- **Accessibility-First Perception:** Extracts `Accessibility.getFullAXTree` diffs to minimize tokens and latency, triggering targeted `screenshot_region` fallbacks only for unlabelled icons or canvases.
- **Stealth Navigation & Pacing:** Enters high-friction sites (Instagram, X, LinkedIn, TikTok) organically via search referrers, using a stealth Chromium binary and human-like action jitter to avoid automated blocks.
- **Human-in-the-Loop Obstacles:** Detects Turnstile, reCAPTCHA, and hCaptcha in the accessibility tree, immediately pausing to alert you in chat so you can solve challenges in the open window before resuming.
- **Production Resilience & Safety:** Automatically recovers from stale DOM elements, clears modal popups, detects unexpected logouts, applies exponential backoff on HTTP 429s, breaks repetitive loops, and requires explicit user confirmation before irreversible actions (payments, orders, deletions, or public posts).
- **Multi-Platform Research:** Fans deep queries out across platforms (LinkedIn, Indeed, Wellfound) under independent budgets, deduplicating results into a single report via a local `FindingStore`.
- **Episodic Memory:** Records navigation lessons with confidence scoring and time decay (`mew-memory`), injecting past insights into future prompts to avoid repeating mistakes.
- **Docked Desktop UI:** Features live one-line step summaries, mid-task steering without restarting, and an optional visual ghost cursor tracking clicks in real time.

## System Overview

Mew is organized as a modular Rust workspace:

- **`mew-cdp`** — CDP wrapper (`chromiumoxide`) for stealth launch, window docking, ghost-cursor overlays, and screencasts.
- **`mew-perception`** — Accessibility-tree extraction, `@eX` mapping, semantic classification, and diffing.
- **`mew-nav`** — Three-tier URL resolution and sensitive-platform entry routing.
- **`mew-resilience`** — Real-time recovery for stale refs, modals, rate limits, auth drops, and confirmation gates.
- **`mew-memory`** — Episodic lesson storage, confidence decay, and prompt-injection retrieval.
- **`mew-agent`** — Core orchestration harness (`ChatAgent`, `BrowserAgent`, `Planner`, `ResearchPlanner`, worker pool).
- **`mew-ui` & `mew-cli`** — Docked Tauri 2 desktop app and companion CLI runner.

## Ethical Design

Built around visible, human-supervised automation, Mew uses organic pacing, human-in-the-loop CAPTCHA handoffs, and confirmation checkpoints to respect site boundaries and keep users in control.
