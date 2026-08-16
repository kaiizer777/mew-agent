# 20 Project Ideas That Solve Real Pain — One Per Stack

Most "project idea" lists are recycling the same beginner exercises: todo apps, chat apps, URL shorteners, weather widgets, blog backends. This list is the opposite. Every idea below is anchored to a documented pain I could verify in surveys, postmortems, or Reddit pain threads, scoped tight enough that a single developer can ship an MVP in a few weeks, and chosen because the language has a real, defensible reason to be the right tool. Pain first, language second, vibe never.

Three filters were applied to every idea:

1. **Pain is documented, not imagined.** A 2025 Atlassian DevEx survey of 3,500 developers showed 50% lose 10+ hours per week to organisational inefficiencies, with finding information as the #1 friction point [1]. A Stack Overflow survey of 49,000 developers found 66% frustrated by AI outputs that are "almost right, but not quite" and 45% saying debugging AI code takes longer than writing it [2]. These are the kind of pain signals that justify a tool, not vague "this would be cool" lists.
2. **Non-generic.** No todo apps, no weather apps, no chat apps, no Trello clones. Each idea lives in a narrow enough niche that a focused builder can win it.
3. **Language is the point, not the coincidence.** The idea actually benefits from that language's strengths (single static binary, single-threaded CPU, ecosystem, etc.) rather than being a project that could be built in anything.

---

## Rust — 5 ideas

Rust's edge is single static binary, memory safety, predictable performance, and a sweet spot in tools that other languages can't quite get right.

### 1. `vuln-scan` — a pre-commit, zero-telemetry npm/pnpm supply-chain scanner

The 2025 npm ecosystem had a brutal year: 18 high-profile packages (chalk, debug, ansi-styles) compromised in September via a maintainer phishing attack, followed by the Shai-Hulud worm and the s1ngularity campaign — 187 packages total, billions of weekly downloads affected [3][4]. Existing scanners (Socket, Snyk, Dependabot) require cloud accounts and telemetry; CVE Lite CLI is a closer match but it's still a Node tool with its own supply-chain exposure.

`vuln-scan` is a single Rust static binary that reads your `package-lock.json` / `pnpm-lock.yaml` / `yarn.lock`, checks every entry against a vendored offline-first advisory DB (OSV + npm advisories), explains the dependency path in plain English, and prints the exact upgrade command. No account, no telemetry, no network egress unless you opt in. Why Rust: ironic counter — a Rust binary is the only way to scan Node projects without depending on Node. Single 8MB binary, sub-second scans, runs cleanly in pre-commit hooks.

### 2. `cargo-buckle-up` — one-command Rust DX accelerator

Rust developers' #1 complaint is compile time, with 55% waiting more than 10 seconds for incremental rebuilds after a small change [5]. The fixes exist — sccache, mold linker, cargo-nextest, rust-analyzer target-dir isolation — but configuring them properly is a weekend of `.cargo/config.toml` archaeology [6][7].

`cargo-buckle-up` is an opinionated CLI: `buckle-up` in any Rust project auto-installs sccache, configures mold as the linker, sets `CARGO_INCREMENTAL=0` for CI, isolates the rust-analyzer target dir, switches the test runner to nextest, and shows you a 30-second before/after timing diff. Why Rust: meta-tooling for Rust itself. Single binary, zero config drift, plays nicely with existing Cargo workspaces. The non-generic angle: it doesn't compete with the underlying tools; it just glues them together and proves the savings.

### 3. `ledger-local` — local-first personal finance desktop & mobile app

Reddit has a thread titled exactly "I couldn't find a local-first budgeting app that didn't require a server or a subscription" [8]. Actual Budget is the closest OSS option, but it's TypeScript/Electron with 26K stars and a non-trivial stack [9]. Most alternatives push bank linking, cloud sync, or both.

`ledger-local` is a Tauri (Rust) + SQLite + sync-via-WebDAV personal finance app. Your data lives in a local SQLite file; you point it at any WebDAV share (Nextcloud, your NAS, even Dropbox via rclone) for cross-device sync; you own everything. Double-entry bookkeeping under the hood, plain-text import, OFX/QIF, no bank linking unless you want it via the optional SimpleFIN plug-in. Why Rust: Tauri ships a 4MB binary, encrypted at rest, runs on iOS/Android/macOS/Linux/Windows from the same code, no Electron. Non-generic because it is the rare finance app that says "no" to the cloud.

### 4. `ws-orbit` — a WebSocket reliability proxy for iOS / Safari / flaky networks

The #1 software-engineer pain point tracked by one Reddit pain-mining tool is WebSocket on Safari/iOS, with multiple specific failure modes documented: NSURLSession's WebSocket implementation chokes on split-message frames (Fin=0), connections drop when the device screen locks, and corporate VPN/proxy stacks break them in subtle ways [10][11][12]. The current workaround is to scatter reconnection logic, setTimeout hacks, and polling fallbacks through every client.

`ws-orbit` is a small Rust proxy you put between your WS backend and clients. It splits long messages into iOS-friendly chunks, sends keepalive pings on a separate channel, transparently downgrades to long-polling when the client looks like a locked iPhone, and replays missed messages on reconnect. Why Rust: `tokio-tungstenite` is production-grade; the memory footprint is tiny enough to run as a sidecar in any k8s pod. Non-generic because the iOS WebSocket pain is so specific that almost no general-purpose tool covers it well.

### 5. `vault-lite` — a single-binary secrets manager for 5-to-50-person teams

HashiCorp Vault is the de-facto standard, but it's notoriously hard to operate and not free in the way most teams want anymore. SOPS is the popular lightweight alternative (encrypted YAML in Git), but it has no audit log, no per-environment access control, and no UI [13]. Infisical is the main OSS challenger and it's a Docker stack that asks you to run a database and a worker [14]. There's a real middle-ground gap.

`vault-lite` is one Rust binary that gives small teams: AES-GCM encrypted-at-rest, audit log of every read, per-environment ACLs (dev/staging/prod), short-lived tokens, a tiny embedded UI, and a CLI that feels like `kubectl` not like enterprise IAM. Why Rust: encrypted at rest, single static binary, no JVM, no Docker stack, deploys in 30 seconds. Non-generic because it intentionally does not try to be Vault — no dynamic DB creds, no PKI, no HSM support. Just the 80% use case that 95% of small teams actually need.

---

## Go — 5 ideas

Go's edge is single static binary, fast compile, real concurrency, and a sweet spot in services that need to be deployed as one file to a server, a Docker container, or a k8s sidecar.

### 1. `oasmock` — stateful, OpenAPI-driven API mock server in one binary

WireMock is the gold standard for stateful API mocking, but it's Java, requires a JVM, and has heavy ops overhead. Prism is the modern alternative but it's Node, stateless, and not great at simulating real back-end behavior. Developers building frontends, integration tests, or contract tests want stateful, persisted scenarios — "after I POST /users, the next GET /users/123 should return what I posted" — without a JVM and without an account.

`oasmock` is a Go binary: point it at your OpenAPI spec, it generates a fully stateful mock that supports realistic scenarios (CRUD round-trips, ID generation, validation against the spec), persists state in a single SQLite file, and ships with a built-in admin UI. Why Go: classic single-binary service pattern, goroutines for parallel request handling, easy to embed in CI. Non-generic because "give me a stateful mock backend" is a real daily pain for front-end and platform teams, and the existing options force them to choose between JVM-weight and statelessness.

### 2. `webhookd` — a self-hostable webhook delivery gateway

Every SaaS that exposes webhooks ends up hand-rolling retry logic, exponential backoff, signature verification, dedup, dead-letter handling, and a replay UI. Hookdeck and Svix solve this as a service, but a self-hostable OSS version is missing.

`webhookd` is one Go binary: register endpoints via API, fan out events with HMAC signing, exponential backoff with jitter, idempotency keys for dedup, dead-letter persistence, and a small web UI to inspect and replay any failed event. Why Go: built for exactly this kind of HTTP-heavy, long-running, concurrent I/O service. The whole thing deploys in a 15MB binary, runs in a homelab, and replaces several hundred lines of fragile Python in every SaaS codebase. Non-generic because it is the rare "infrastructure primitive" that almost every modern team reinvents badly.

### 3. `redactr` — a log PII / secret redaction sidecar

A persistent data-pro pain: devs ship logs to Datadog, Splunk, or just journald, and accidentally include API keys, credit-card numbers, emails, and IPs. SOC2 and GDPR make this a real problem, not a theoretical one. Cloud-only log redaction services exist, but on-prem and sidecar options for self-hosted stacks are scarce.

`redactr` is a Go binary you run as a sidecar (or a `kubectl logs` wrapper) that streams logs from a source (file, journald, syslog, Docker container, K8s pod) and rewrites them with a pluggable detector pipeline (regex, denylist, ML-ish entropy for tokens) before they reach their destination. Why Go: classic syslog/journald sidecar pattern; goroutines for streaming; cross-compiles to a 10MB static binary. Non-generic because it is the rare tool that solves a real compliance pain for self-hosters without sending your logs to a third party.

### 4. `relaymail` — a self-hostable transactional email service for indie SaaS

Indie devs shipping SaaS end up choosing between Mailgun/Postmark (paid per email, vendor lock-in), SES (cheap but has a deliverability learning cliff and a few weeks of account-review purgatory), or mailcow/Stalwart/Postal (heavy, often a full Docker stack). There is no "small, single-binary, sane defaults, just works for my five products" option.

`relaymail` is one Go binary: SMTP receive + HTTP API, per-domain DKIM/SPF/DMARC autoconfig, a small admin UI, bounce/spam handling, and a built-in feedback loop for AOL/Gmail/Outlook. Why Go: networking-heavy, single binary, ships as a tiny container. Non-generic because it is the rare email server that does not assume you want to run an entire mail stack. The MVP: a working SMTP relay for one or two domains, deliverability that doesn't embarrass you.

### 5. `ai-billing-proxy` — an AI-inference proxy with cost caps, fallbacks, and a request log

A real recurring pain: every team that uses OpenAI, Anthropic, or open-source LLMs in production ends up building a thin proxy to (a) cap spend per team/user, (b) route to a fallback model when the primary is rate-limited, (c) log every request and response for debugging and prompt iteration, and (d) do A/B splits between prompt versions. Helicone, Portkey, and OpenPipe are SaaS for this; the self-host gap is real.

`ai-billing-proxy` is one Go binary: drop-in OpenAI/Anthropic-compatible API surface, per-key spend limits with hard kill switches, model-failover chains, full request/response capture to SQLite, and a tiny dashboard to inspect traffic and tag prompts. Why Go: classic proxy pattern, goroutine fan-out for failover, single binary. Non-generic because the "I keep rewriting this proxy every three months" pain is universal and the only well-known options are SaaS.

---

## Node.js / TypeScript — 5 ideas

Node's edge is the biggest package ecosystem in software, native browser symmetry, and the strongest position in anything that touches the web stack.

### 1. `git-issues` — a Git-native GitHub Issues replacement that lives in your repo

GitHub Issues has real, well-documented problems for solo developers and small teams: contentless +1 spam, custom field gaps, an over-busy notifications page, the inability to filter "accepted" issues, the slowness of the web UI, vendor lock-in, and maintainer burnout from the recent surge of low-quality AI-generated issues and PRs [15][16]. Linear and Jira exist, but they are SaaS, expensive, and not git-native. There's no widely-used tool that stores issues as plain Markdown files inside the repo, with optional GitHub sync, a clean TUI, and zero server.

`git-issues` is a Node CLI + optional local web UI: each issue is a `.md` file under `.issues/`, with YAML frontmatter for status/assignee/priority, git version-controlled, searchable with ripgrep, and able to push/pull a mirror to GitHub Issues for those who want it. Why Node: the markdown/git toolchain is overwhelmingly JS; this would not be ergonomic in Rust. Non-generic because the failure mode of GitHub Issues specifically is documented and a git-native replacement has a defensible long-term market.

### 2. `tinkerbay` — a browser-based hardware dev tool with WebSerial / WebUSB + recordable sessions

Hardware tinkerers (Arduino, ESP32, Raspberry Pi Pico, custom microcontrollers) suffer from a fragmented toolchain: Arduino IDE is aging, PlatformIO is powerful but heavy, serial monitors are separate apps, and capturing a session for later debugging or sharing is painful. A browser can now talk directly to USB and serial devices via the WebSerial and WebUSB APIs, and there is no widely-used dev tool that exploits this.

`tinkerbay` is a browser app: connect to a serial/USB device, get a real REPL, record the full session (with timestamps), annotate it, and replay it locally. Optional scripting layer to send canned command sequences. Why Node: `serialport` and `webusb` are mature in the npm ecosystem; the recording/replay layer is a few hundred lines of TypeScript. Non-generic because it's a very specific niche that the Arduino/PlatformIO world has not addressed well, and a browser-based approach eliminates the install step that turns off newcomers.

### 3. `co-canvas` — a zero-server, file-based, real-time collaborative canvas

Excalidraw, tldraw, and Miro all require an account (or at least a hosted server) for collaboration. tldraw can save locally, but real-time multi-user collaboration without a server is rare outside of CRDT demos. There is a real demand for a "share a URL, both ends see the same canvas, the file lives on disk" tool that doesn't need a hosting account.

`co-canvas` is a single static web page (no server, no backend) that uses Yjs (or Automerge) CRDTs, persists state in a single JSON file on each client's disk, and exchanges updates via WebRTC data channels seeded by a short share code. No account, no server, no telemetry. Why Node: this is the strongest ecosystem for CRDT libraries and WebRTC peer setup. Non-generic because the specific constraint — "no server at all" — is what the existing tools refuse to do.

### 4. `promptbench` — a self-hostable AI prompt versioning + eval runner

Stack Overflow's 2025 survey shows 45% of devs say debugging AI-generated code takes more time than writing it manually, and 66% are frustrated by outputs that are "almost right" [2]. A big part of the fix is iterating on prompts against a fixed eval set, but the existing tools (PromptLayer, Helicone, LangSmith) are SaaS and want you to ship your prompts and traces to their cloud. There's no widely-used self-hostable alternative that ties prompt versions, eval results, and CI integration together.

`promptbench` is a Node app: a YAML-defined set of prompt variants and test cases, a runner that hits any OpenAI-compatible API, a results UI showing per-variant pass rates and token costs, and a GitHub Action that gates PRs on eval regressions. Why Node: the dev audience is JS-first; integrates cleanly with GitHub Actions, Vercel, etc. Non-generic because prompt-iteration is increasingly a real engineering practice, not a magic trick, and a self-hostable tool has a place in any team that won't ship prompts to a third party.

### 5. `syncdrop` — a local-first, e2e-encrypted file sync with a modern UX

Syncthing works and is solid, but its UI is dated and there is no first-class mobile experience. iCloud, OneDrive, Dropbox, and Google Drive are SaaS and expensive for the storage you actually get. There is a clear gap for a Syncthing-quality engine wrapped in a 2025-era UI, with first-class iOS/Android, no central account, and built-in versioning.

`syncdrop` is a Tauri (or Electron) app on top of libsync, with a clean modern UI, optional end-to-end encryption keys you control, mobile apps, and a versioning snapshot every N minutes. Why Node: the front-end is overwhelmingly a TS/React project; the heavy lifting is in well-tested Rust. Non-generic because the existing options are either a SaaS you don't trust with your files, or Syncthing with a UI from 2014.

---

## Python — 5 ideas

Python's edge is the data/ML ecosystem, fast prototyping, and the strongest position in anything that touches notebooks, scripts, and small web services.

### 1. `datasift` — a local, interactive CSV/Excel cleaner with replayable transformations

Data professionals routinely report spending 80% of their time cleaning data and 20% analyzing it, and they call out the "lack of tools that mirror production environments for local development" as a recurring theme [17][18]. OpenRefine is the closest OSS tool, but it's Java, hasn't had a major UI refresh in years, and has a learning cliff.

`datasift` is a Python app (FastAPI backend + Tauri/Electron front-end, but the logic is pure Python): open a CSV/Parquet/Excel file, get auto-detected schema and column types, apply transformations via a point-and-click UI, and — the non-generic part — every transformation is logged as a YAML step that can be saved, replayed, diffed, and checked into Git. Why Python: this is pure data ecosystem; the transformation library should use DuckDB, Polars, or pandas. Non-generic because the "replay" feature is what the existing tools miss — a year from now you can re-run the exact same cleaning pipeline on a new export.

### 2. `notefold` — an adaptive note-taking app for ADHD that hides what you don't use

A Reddit analysis of 9,300+ "I wish there was an app" posts, plus multiple other studies, name "managing daily life with ADHD" as a top recurring theme, with the specific failure pattern that people start productivity apps and abandon them as the tool becomes overwhelming or loses its novelty [19][20]. The fix is not more features; it's the opposite. The best note app for an ADHD user is the one with the fewest visible features at any given moment.

`notefold` is a Python backend (FastAPI + SQLite) plus a Tauri front-end, with ML that watches which features you actually use, surfaces only those in the UI, and progressively hides everything else behind a single "show everything" toggle. Why Python: heuristic/ML layer is trivial; the value is in the restraint. Non-generic because the product thesis is anti-feature, which is genuinely rare in the productivity-app category.

### 3. `recipe-strip` — a CLI that turns a recipe URL into a clean Markdown / ingredients-only view

In the same 9,300-post Reddit analysis, the "cooking & recipes" category had an average post length of 223 words (the second highest of any category) because users are "furious" about bloated recipe sites buried under ads and SEO life stories [20]. The existing recipe apps (Paprika, Whisk, AnyList) all require accounts and full app installs. The "I just want the recipe" use case is underserved.

`recipe-strip` is a Python CLI: paste a recipe URL, get back a clean Markdown file with title, ingredients, steps, total time, and a JSON-LD metadata block. Built on `httpx`, `selectolax`, and a small ML model to handle sites without structured data. Why Python: the data extraction is a classic Python strength. Non-generic because it's not a recipe app — it's a "get me out of this hellscape" utility, and it can be piped into any existing note system.

### 4. `housemanual` — a self-hostable "house manual" for renters and landlords

The same Reddit pain-mining work identified "renters face significant frustration in finding housing due to fraudulent listings, scams, and lack of trustworthy information" as a recurring theme, and a related sub-pain: once you have a place, tracking maintenance, deposits, lease terms, and landlord communication is chaos [19]. There is no good tool on either side. Property-management software targets landlords with multiple units; consumer apps target home-buyers.

`housemanual` is a Django app that gives a renter and a landlord a shared workspace per property: maintenance tickets with photo upload, document storage (lease, move-in checklist, photos), a message log, and a clear history that's exportable for legal disputes. Self-hostable, runs on a $5 VPS. Why Python: Django's batteries-included admin is built for this exact CRUD pattern. Non-generic because the renter side specifically is a deeply underserved buyer.

### 5. `papertalk` — a local PDF Q&A tool for research papers (on-device, privacy-first)

Researchers and grad students routinely work with 100-page PDFs and either re-read them or copy-paste into a cloud AI. The cloud AI is convenient, but it leaks unpublished work. Adobe's AI features are expensive and lock you in. The on-device LLM ecosystem is finally good enough (Phi-4-mini, Gemma 2 2B, Qwen2.5-3B-Instruct, all run on a laptop) that a useful local PDF Q&A tool is feasible.

`papertalk` is a Python app: drop in a folder of PDFs, it indexes them locally (ColBERT or a small embedding model), asks a local LLM via Ollama, returns cited answers, and never makes a network call. Why Python: the ML/data ecosystem is unmatched. Non-generic because the on-device + privacy story is the entire point — researchers with unpublished work can use it without legal review.

---

## How to pick from this list

Three quick filters, applied in order:

**1. Pick a pain you have personally observed.** The most reliable signal that an idea is worth building is that you have lost real time to the problem yourself. If you haven't, the idea is still valid, but you'll be guessing at UX and you'll get the second-most-important things wrong.

**2. Pick the language that the idea actually needs.** The list above is built so the language is a reason, not an accident. `vuln-scan` is a Rust binary because the irony of "scan npm from a non-Node binary" is a feature. `oasmock` is Go because single-binary deployment is the whole point. `datasift` is Python because Polars and DuckDB live there. If you can swap the language and the idea still works, the language choice is a coincidence and you may be in the generic zone after all.

**3. Look for the "almost-served" gap.** Some of these ideas look like they overlap with existing tools, and they do. The non-generic move is to pick the specific axis where the incumbent is failing. Actual Budget is open source but is JS-stacked and central-DB. Vault is comprehensive but heavy. Excalidraw is great but requires an account. The good ideas pick one axis where the existing tool is weakest and ship a tool that does that one thing well.

A non-generic project is one where, if you tell a friend what you're building, they say "oh I needed that yesterday," not "oh cool, is that a wrapper around OpenAI?"

---

## References

[1] Atlassian, "State of Developer Experience 2025" — https://www.atlassian.com/teams/software-development/state-of-developer-experience-2025

[2] Stack Overflow 2025 Developer Survey / productivity paradox — https://stackoverflow.co/teams/resources/2025-stack-overflow-developer-survey-for-leaders/productivity-paradox/

[3] Cymulate, "npm Under Siege: Evolving Supply Chain Threats" — https://cymulate.com/blog/npm-under-siege-supply-chain-attacks/

[4] CyberProof, "A Guide to the Recent Node Package Manager (NPM) Compromise" — https://www.cyberproof.com/blog/guide-to-recent-node-package-manager-npm-compromise/

[5] Rust Blog, "Rust Compiler Performance Survey 2025 Results" — https://blog.rust-lang.org/2025/09/10/rust-compiler-performance-survey-2025-results/

[6] Microsoft, "Rust Engineering Playbook: Compile-Time and Developer Tools" — https://microsoft.github.io/RustTraining/engineering-book/ch08-compile-time-and-developer-tools.html

[7] Corrode, "Tips For Faster Rust Compile Times" — https://corrode.dev/blog/tips-for-faster-rust-compile-times/

[8] Reddit r/degoogle, "I couldn't find a local-first budgeting app that didn't require a server or a subscription" — https://www.reddit.com/r/degoogle/comments/1ppeeqh/i_couldnt_find_a_localfirst_budgeting_app_that/

[9] Actual Budget (local-first open-source personal finance) — https://actualbudget.org

[10] painonsocial.com, "15 Best Subreddits for Software Engineers: Real Pain Points" — https://painonsocial.com/subreddits/software-engineers

[11] Apple Developer Forums, "iOS 15 WKWebView websocket behaviour" — https://developer.apple.com/forums/thread/685403

[12] GitHub graphql-ws Discussion #290, "Reconnect doesn't work after Safari drops WebSocket connection when the user locks the screen" — https://github.com/enisdenjo/graphql-ws/discussions/290

[13] dev.to, "Is Your API Key Still Running Naked? The Complete 2026 Secrets Management Guide" — https://dev.to/linou518/is-your-api-key-still-running-naked-the-complete-2026-secrets-management-guide-4m7n

[14] wetheflywheel, "HashiCorp Vault Alternatives: 8 Secrets Managers Compared" — https://wetheflywheel.com/en/comparisons/hashicorp-vault-alternatives/

[15] Devache, "GitHub Pain Points — 21 real developer frustrations" — https://devache.com/tech/GitHub

[16] LeadDev, "What's gone wrong at GitHub?" — https://leaddev.com/software-quality/whats-gone-wrong-at-github

[17] LinkedIn, Derek Pauley, "The Hidden Struggles of Data Professionals" — https://www.linkedin.com/pulse/hidden-struggles-data-professionals-what-theyre-really-derek-pauley-vouje

[18] KDnuggets, "Top 7 Python ETL Tools for Data Engineering" — https://www.kdnuggets.com/top-7-python-etl-tools-for-data-engineering

[19] GitHub Gist (scdekov), "Top 10 Real User Problems (from Reddit) and Potential Software Solutions" — https://gist.github.com/scdekov/f355ab96c729f8ac5bfad35c8cc8b13a

[20] Markethunt, "A Redditor Analyzed 9,300+ 'I Wish This Existed' Posts" — https://markethunt.io/insights/reddit-market-validation-analysis
