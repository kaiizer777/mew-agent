# mew — Personal-use setup guide

> A daily-use personal tool, not a project you have to hand-configure
> every Monday morning. This guide walks through the full setup flow
> end-to-end: from a fresh `git clone` to "I run `mew run-preset
> daily_remote_jobs` and read the digest when I get back."

---

## 1. What you get

A `mew` install that you can walk away from:

* `mew run-preset daily_remote_jobs` — visits LinkedIn / Indeed /
  Wellfound / WeWorkRemotely / RemoteOK, returns a consolidated chat
  reply + a Markdown digest at `<data_dir>/digests/YYYY-MM-DD.md`.
* `mew run-preset daily_freelance_leads` — same shape, but on Upwork /
  Contra / WeWorkRemotely-Contract / Freelancer.com / Guru.
* `mew run-preset weekly_full_search` — both verticals in one run, with
  per-platform budget caps.
* `mew list-presets` — print the available preset catalog to stdout.
* A typed "lessons learned" surface (`get_job_freelance_lessons` Tauri
  command) that grows over time as the agent figures out which job
  boards actually work well for you.

The agent pauses for confirmation on `Paused` checkpoints (irreversible
actions, 2FA prompts, account restrictions), writes a digest of every
run, and remembers what worked.

---

## 2. One-time setup (15 minutes)

### 2.1 Build & install

```powershell
# From the repo root
cargo build --workspace --release
```

The release build produces `target/release/mew.exe` (CLI) and
`target/release/app.exe` (Tauri shell). Drop the `mew-ui/src-tauri/`
target on your `PATH` or wire a desktop shortcut to `app.exe`.

### 2.2 Set up Google credentials

`mew` uses "Sign in with Google" wherever a job board or freelance
platform supports it. Set two env vars before launching:

```powershell
$env:GOOGLE_EMAIL = "you@gmail.com"
$env:GOOGLE_PASSWORD = "<your-app-password-or-normal-password>"
```

> **Use a Google app password, not your normal password.** Google
> blocks "less secure app" logins on accounts without 2FA enabled.
> The Tauri shell's profile retention (`user_data_dir`) caches the
> session, so after the first successful sign-in you can drop the
> env vars and the agent stays logged in.

`config.yaml` references these by env-var name (never by value):

```yaml
agent:
  credentials:
    google:
      email_env: GOOGLE_EMAIL
      password_env: GOOGLE_PASSWORD
```

### 2.3 Configure the rest of `config.yaml`

The minimum to get started:

```yaml
opencode_zen:
  base_url: <your-LLM-endpoint>
  api_key: <secret>
  default_model: <model name>
  max_iterations: 500
  max_tokens: 1740000

browser:
  binary_path: c:\Users\bari2\Desktop\mew-agent\stealth-browser\chrome.exe
  visible_cursor: false

agent:
  # Phase 16: outer Planner loop. Recommended for personal use
  # (multi-platform fanout + per-subtask budget caps).
  planner_enabled: true

  # Phase 6: auto-apply. DEFAULT DISABLED. When you turn it
  # on, the agent will draft application text on Paused
  # checkpoints — `require_confirmation_per_application:
  # true` (the default) pauses for your approval on every
  # application. Set `false` only when you've read
  # docs/phase6-auto-apply.md and the platform's ToS.
  auto_apply:
    enabled: false
    require_confirmation_per_application: true
    max_applications_per_run: 10

  # Phase 8: per-run results digest. The daily digest file
  # is the on-disk record of what the agent found. Enable
  # to keep a personal archive.
  digests:
    enabled: true
    output_dir: null  # Tauri resolves to <app_data_dir>/digests/

  # Phase 8: structured preset overrides. Empty by default —
  # the seed presets (daily_remote_jobs, daily_freelance_leads,
  # weekly_full_search) cover the common case.
  task_preset_overrides: {}

  pacing:
    enabled: true
    min_delay_ms: 800
    max_delay_ms: 2500
    streak_threshold: 2

  research:
    enabled: true
    max_concurrent_workers: 3
    max_concurrent_per_domain: 1
    verify_links: true
```

The shipped `config.yaml` already has all of this — only the
`agent.digests.enabled` flag and the `agent.auto_apply.enabled` flag
need flipping.

### 2.4 Verify the install

```powershell
mew list-presets
# Expected output:
# Available presets:
#   - daily_freelance_leads — Daily freelance client lead search across ...
#   - daily_remote_jobs — Daily Monday-morning remote job search across ...
#   - weekly_full_search — Weekly comprehensive sweep: both job and freelance ...
```

If you see the three presets, the structured-preset path is wired up.
If the CLI complains "Preset 'X' not found", the
`mew_agent::presets` module isn't being picked up — re-run
`cargo build --workspace` and check the warnings.

---

## 3. Daily-use flow

### 3.1 Trigger a run

```powershell
mew run-preset daily_remote_jobs
```

The agent:

1. Builds a typed `Handoff` with one `SubTask` per remote job board.
2. Launches headed Chrome (so you can see it work / solve CAPTCHAs).
3. Streams partial findings to the chat as each platform completes.
4. On the `Paused` checkpoint, drafts application text per
   finding (only if `auto_apply.enabled: true`).
5. Synthesizes one consolidated chat reply.
6. Writes a Markdown digest to
   `<app_data_dir>/digests/YYYY-MM-DD.md`.
7. Records any high-confidence lessons it picked up about which
   platforms worked well.

### 3.2 Walk away

You can close the chat surface after the run starts — the agent
keeps working. When you come back:

* The chat surface shows the consolidated reply.
* The digest file shows the same content + a "## YYYY-MM-DD HH:MM:SS"
  header for grep-ability.
* The `applications_sent.json` dedup store (Phase 6) records every
  application submitted, so re-running the same preset on Tuesday
  won't double-apply to Monday's postings.

### 3.3 Resume a paused session

The agent pauses on:

* **CAPTCHA / challenge page** — solve it in the visible Chrome
  window, then type "continue" in the chat box. The agent
  re-attempts the page.
* **2FA prompt** — approve on your phone, then type "continue".
* **Irreversible action** (`apply_to_job`, `send_proposal`,
  `send_connection_request`) — the chat list shows the draft text;
  approve with "submit" or "skip".
* **Account restriction** ("we've restricted your account") — the
  chat list shows a clear non-alarming message; the run continues on
  the next platform.

For a "stuck" session (worker hasn't checked in for a few minutes),
the Tauri shell's `pause_session` / `resume_session` commands
unblock it.

---

## 4. What to expect from `auto_apply`

The Phase 6 auto-apply layer has three modes:

| `auto_apply.enabled` | `require_confirmation_per_application` | Behavior |
| --- | --- | --- |
| `false` (default) | n/a | Pure no-op. The `Paused`-checkpoint irreversible-action gate still fires, but the agent does not draft application text. |
| `true` | `true` (default) | The agent drafts application text on every `Paused` checkpoint, surfaces it in the chat, and waits for your explicit approval. |
| `true` | `false` | "Fire and forget." The agent auto-submits every draft, respecting the dedup store + per-run cap. **Read `docs/phase6-auto-apply.md` and the platform's ToS before flipping this.** |

The dedup store at `<data_dir>/applications_sent.json` is keyed on
`(platform, posting_identifier)` so re-running the same preset on
Tuesday never double-applies to Monday's postings. The
`max_applications_per_run: 10` cap is a hard per-session ceiling —
even with `require_confirmation_per_application: false`, the agent
will not submit more than 10 in a single run.

---

## 5. Reviewing past runs

The on-disk digest at `<app_data_dir>/digests/YYYY-MM-DD.md` is the
authoritative record. The Tauri shell's `get_job_freelance_lessons`
command surfaces the high-confidence lessons the agent has picked up
across runs:

```json
[
  {
    "id": "lesson-001",
    "task_shape": "JobSearch",
    "platform": "LinkedIn",
    "lesson_text": "Use the via_search entry path; direct nav gets a CAPTCHA on every fresh session.",
    "confidence_score": 1.0,
    "times_helped": 4
  }
]
```

The Tauri UI renders these as a "What I've learned" pane in the
chat list. Filtered to `JobSearch` + `LeadGen` only — the
cross-vertical `get_recent_lessons` command is still available for
debugging.

---

## 6. Resuming paused sessions

When the agent pauses, the chat list shows a `Paused` card with
the reason and the next step:

* **CAPTCHA handoff** — solve in the visible Chrome window, then
  type "continue" (or click `Resume` in the Tauri UI).
* **2FA prompt** — approve on your phone, then "continue".
* **Irreversible action** — chat list shows the draft application
  text; click `Submit` or `Skip` per finding.
* **Account restriction** — chat list shows the platform + a clear
  "account restricted; moving on" message; the run continues on
  the next platform automatically.
* **Resume from a budget cap** — the chat list shows "N of M
  sub-tasks completed" with the outstanding list; the agent moves
  to the next platform on its own.

For a "stuck" session (worker hasn't checked in for 5+ minutes), the
Tauri shell's `pause_session` / `resume_session` Tauri commands
explicitly unblock it. The session's `SessionHandle` keeps the
authenticated-domain state, so resuming doesn't re-trigger Google
sign-in.

---

## 7. FAQ

**Q: I closed the chat surface mid-run. Did I lose the run?**
A: No. The agent keeps working; the in-progress `task_started` card
   is still on disk (the Tauri shell's `app_data_dir` keeps it). When
   you reopen the chat, the run's final consolidated reply is
   already in the chat list.

**Q: Can I run two presets in parallel?**
A: No, and you shouldn't. The per-domain concurrency cap (default
   1) is the anti-ban mechanism — two workers on `linkedin.com` at
   once is a known account-flag trigger. Run them sequentially:
   `mew run-preset daily_remote_jobs` Monday morning, `mew run-preset
   daily_freelance_leads` Monday afternoon.

**Q: How do I see what the agent did on a specific platform?**
A: The chat list's per-task `task_started` card has a collapsible
   `<details>` panel with the live progress lines (typed text,
   clicks, snapshots). The session's transcript is at
   `<app_data_dir>/transcripts/<session_id>.jsonl`.

**Q: I want to add a custom preset.**
A: Add it to `config.yaml` under `agent.task_preset_overrides`:
   ```yaml
   task_preset_overrides:
     my_custom:
       name: my_custom
       summary: my own preset
       shape: daily_remote_jobs
   ```
   The Tauri `list_presets` command and the `mew list-presets` CLI
   will both surface it.

**Q: The agent paused on a CAPTCHA. I solved it, but the agent
  didn't resume.**
A: Type "continue" in the chat box. The agent's `mpsc` steering
   bus picks it up on the next checkpoint, the snapshot evidence
   re-verifies the platform state, and the loop continues.
