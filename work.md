# mew — work plan (phases 11–18)

The "main agent plans, browser agent executes" outer loop, on top of the
shipped two-agent split. The shipped `ChatAgent` / `BrowserAgent` modules
become the inner loop. A new outer **Planner** in the Tauri shell decomposes
the user message into typed `Todo`s, dispatches them to a long-lived,
supervised **Browser Agent** worker, and only accepts "done" on per-todo
evidence — not on the LLM's self-report.

Goal: the worker cannot shortcut by emitting a single `done` and walking
away. Every `Todo` must (a) execute its declared actions, (b) capture a
fresh snapshot, and (c) have its `evidence_signature` cross-checked
against the snapshot the planner captured independently.

Eight phases, each 1–2 focused PRs. The new flow is **additive** —
the existing `ChatAgent → BrowserAgent` round trip in `orchestrator.rs`
keeps working under a `planner_enabled` config flag.

---

## Conventions

- **Files to touch** is non-exhaustive; it lists the ones expected to
  change, not every `.rs`/`.ts`.
- Every phase ends with **Acceptance** — a concrete thing you can
  verify in CI or in a 5-minute manual test.
- Every phase has a **Reuses** note pointing at the existing code
  the new code should build on, not duplicate.
- Phases ship in order. Skipping a phase breaks the next one.

---

## Phase 11 — Todo schema and decomposition contract

**Goal:** Define the typed `Todo` schema, the planner's job-to-todos
decomposition contract, and the per-todo evidence model. Pure data +
contract. No Tauri, no LLM, no network.

**Reuses:**
- `mew_agent::planner::Plan` and the deterministic decomposition pass.
- `mew_agent::completeness::SubTask` / `SubTaskStatus` — Todo is a
  superset; existing subtask states (`Pending | Done | Skipped | Failed |
  Exhausted`) become todo states unchanged.

**Files to touch:**
- `mew-agent/src/todo.rs` (new) — `Todo`, `TodoId`, `TodoStatus`,
  `Evidence`, `AcceptanceCriterion`, `TodoBudget`.
- `mew-agent/src/lib.rs` — re-export.
- `mew-agent/src/planner.rs` — `decompose_to_todos(&Handoff) -> Vec<Todo>`.
- `mew-agent/src/completeness.rs` — alias `TodoStatus = SubTaskStatus`
  (or a thin newtype, depending on taste). Do not extend the enum;
  todo-level outcomes (Done, Skipped, Failed, Exhausted) are
  identical to subtask outcomes.

**Schema sketch (for shared understanding, not literal code):**
```rust
pub struct Todo {
    // ID format MUST match `mew_agent::planner::slugify` output:
    // e.g. "navigate-instagram-1", "send-hi-2". The existing
    // `mark_subtask_done` tool looks ids up by exact-match string,
    // so reusing the same slug format lets the worker re-use the
    // tool for per-todo evidence calls without an id-translation
    // shim. Newtype wrapper so the compiler catches accidental
    // mixing of todo ids with subtask ids.
    pub id: TodoId,                          // e.g. "navigate-instagram-1"
    pub intent: String,                      // human description
    pub acceptance: Option<AcceptanceCriterion>, // what "done" means (single, optional)
    pub depends_on: Vec<TodoId>,             // ordering
    pub status: TodoStatus,                  // reuses SubTaskStatus
    pub evidence: Option<Evidence>,          // filled when terminal
    pub attempts: u8,                        // bounded retries, default 3
    pub budget: TodoBudget,                  // step / time caps
}

pub enum AcceptanceKind {
    UrlAt,           // planner expects page URL to equal `value`
    TextInSnapshot,  // planner expects AX-tree text to contain `value`
    ElementPresent,  // planner expects an interactive element with `value` as label
    AnySnapshot,     // planner only requires a fresh snapshot, no semantic check
}

pub struct AcceptanceCriterion {
    pub kind: AcceptanceKind,
    pub value: String,
}

pub struct Evidence {
    pub todo_id: TodoId,
    pub worker_signature: String,            // the worker's reported signature
    pub planner_signature: String,           // the planner's independently re-computed signature
    pub verified_at_secs: u64,
}
```

**Why `worker_signature` AND `planner_signature` both live in `Evidence`:**
The worker's `mark_subtask_done` already records the signature it
observed. The new rule is the *planner* must independently re-hash
the AX-tree (or a typed summary of it) that the worker hands back
as part of `TodoResult`, and the two must match. Storing both lets
a reviewer diff "what the worker said" vs "what the planner saw"
without re-running the task. Both signatures use the existing
`len:{:08x}` format from `agent.rs::run_inner` — the
`DefaultHasher` over the obs text — so the wire format is
unchanged.

**Checklist:**
- [x] Define `Todo`, `TodoId` (newtype, `Deref<Target=str>`),
      `TodoStatus` (= `SubTaskStatus`), `AcceptanceCriterion`,
      `AcceptanceKind`, `Evidence`, `TodoBudget` in
      `mew-agent/src/todo.rs`
- [x] Derive `Debug, Clone, Serialize, Deserialize` on every public
      type; add `#[serde(tag = "type")]` on `AcceptanceKind` so the
      wire format survives a future variant addition
- [x] `TodoId::from_slug(&str, index: usize) -> TodoId` —
      thin wrapper over the existing `mew_agent::planner::slugify`
      so a todo id is byte-identical to the corresponding subtask id
      for the same description+index
- [x] Extend `mew_agent::planner::plan()` with
      `decompose_to_todos(&Handoff) -> Vec<Todo>` — reuses the
      existing deterministic `and` / `then` / `,` split and seeds
      each piece with a `Todo` whose `id` matches the corresponding
      `DeclareItem.id`
- [x] Add `acceptance` heuristic: for a todo whose description
      starts with "navigate", seed `Some(UrlAt(value=resolved_url))`;
      for "type"/"send", seed `Some(ElementPresent(value=expected_text))`;
      for everything else, seed `Some(AnySnapshot)` — never `None`,
      because a planner-side acceptance rule is the whole point of
      Phase 12
- [x] Add `tests/todo_schema.rs`: JSON round-trip,
      `TodoId::from_slug` produces the same string as
      `planner::slugify` for the same input, slug idempotency
      (`"a b c" -> "a-b-c" -> "a-b-c"`)
- [x] Doc comment on `Todo` explaining the invariant: `status ==
      Done ⇒ evidence.is_some() ∧ evidence.planner_signature ==
      evidence.worker_signature`
- [x] Update `mew-agent/src/lib.rs` to re-export the new module

**Acceptance:**
- `cargo test -p mew-agent` passes with the new tests.
- `decompose_to_todos` on the existing `phase2_instagram_regression`
  input ("go to instagram and text my friend hi") produces a
  `Vec<Todo>` of length 2 with ids of the form
  `["go-to-instagram-0", "text-my-friend-hi-1"]` (or whatever the
  existing `slugify` produces for those clauses — the test pins
  the exact strings, not a hand-waved format).
- No existing public API in `mew-agent` changes shape.

---

## Phase 12 — Per-todo evidence gate (the no-shortcut rule)

**Goal:** Make it impossible for a worker to claim `Done` without
planner-verifiable evidence. The worker hands back its
`last_snapshot_signature` *and* the AX-tree text it observed; the
planner re-hashes the AX-tree text with the same `len:{:08x}`
function `agent.rs::run_inner` uses and only accepts `Done` when
the two signatures match.

**Reuses:**
- `mew_agent::completeness::MarkOutcome::StaleEvidence` — the
  evidence-rejection pattern is already there for subtasks; lift it
  to todos.
- `mew_agent::agent::run_inner`'s `len:{:08x}` signature function
  (the same `DefaultHasher` over the obs text at
  `mew-agent/src/agent.rs:1826-1838`). The worker and the planner
  use *the same* function, so equal inputs always produce equal
  signatures.
- `mew_perception::TreeNode` and the AX-tree text the worker
  already produces per snapshot.

**Why the planner doesn't run its own CDP snapshot:**
The planner lives in the Tauri shell, not next to the browser. To
capture an independent snapshot it would need its own CDP
connection — a second browser instance, doubling the resource cost
and tripping the anti-bot profile-sharing detector. Instead, the
worker hands the *raw* AX-tree text (not just a hash) as part of
`TodoResult`, and the planner hashes it locally. The worker can lie
about the tree only by also lying about the hash, which the
planner's local re-hash catches.

**Files to touch:**
- `mew-agent/src/todo.rs` — add
      `planner_signature(obs_text: &str) -> String` (the same
      `len:{:08x}` function, lifted to a public util so the worker
      and planner can't drift) and
      `verify_evidence(worker_sig: &str, obs_text: &str) -> Result<String, EvidenceMismatch>`.
- `mew-agent/src/agent.rs` — when the worker's `BrowserResult`
      arrives, the orchestrator calls `verify_evidence` per `Todo`
      before marking it `Done`.
- `mew-agent/tests/evidence_gate.rs` (new) — golden scenarios.
- `mew-agent/src/handoff.rs` (or a new `TodoResult` struct) — the
      `TodoResult` carries both `last_snapshot_signature: String`
      and `last_obs_text: String` so the planner has the raw input
      to re-hash.

**The rule (in code, not prose):**
```rust
fn mark_done(todo: &mut Todo, worker_sig: &str, obs_text: &str)
            -> MarkOutcome {
    if todo.evidence.is_some() {
        return MarkOutcome::AlreadyTerminal;
    }
    let planner_sig = planner_signature(obs_text);
    if worker_sig != planner_sig {
        return MarkOutcome::StaleEvidence {
            worker: worker_sig.to_string(),
            planner: planner_sig,
        };
    }
    todo.evidence = Some(Evidence {
        todo_id: todo.id.clone(),
        worker_signature: worker_sig.to_string(),
        planner_signature: planner_sig,
        verified_at_secs: now_secs(),
    });
    todo.status = SubTaskStatus::Done;
    MarkOutcome::MarkedDone { /* ... */ }
}
```

**Checklist:**
- [x] Add `verify_evidence` in `todo.rs` — pure function, no IO, no LLM
- [x] Wire `verify_evidence` into `mark_done` path in `mew-agent/src/todo.rs` / orchestrator — call happens *before* the `Done` transition
- [x] On `EvidenceMismatch`, emit a typed `OrchestratorEvent::TodoRejected { todo_id, worker_signature, planner_signature }`
- [x] Bound retries: `attempts` field, default 3
- [x] Add a golden test: worker emits `done` with signature `abc`, planner signature `xyz` → outcome `StaleEvidence`, todo stays `Pending`, retry counter increments
- [x] Add a positive test: matching signatures → `MarkedDone`, todo transitions, `evidence` field populated
- [x] Add a third test: same signature but `evidence_iteration` is older than the last `mark_done` call → reject as `StaleEvidence` (the "model re-uses old evidence" shortcut)

**Acceptance:**
- `cargo test -p mew-agent` includes 3 new passing tests covering
  accept, reject, and stale-iteration cases.
- The "worker emits one `done` and walks away" shortcut now requires
  the worker to also fabricate the AX-tree text that hashes to the
  planner's signature — which means fabricating evidence. The
  transcript shows the fabricated text in the worker's
  `TodoResult.last_obs_text`; a reviewer can read the text and see
  the lie.
- A test asserts that a worker which hands back
  `last_obs_text: ""` (empty string) is rejected as
  `StaleEvidence` — the empty-string shortcut fails closed.

---

## Phase 13 — Browser Agent as a long-lived supervised worker

**Goal:** Promote the current `agent::Agent` (ReAct loop) from
"spawned once per session, dies with the session" to "a long-lived
worker that accepts one todo at a time and is supervised by a planner."

**Reuses:**
- `mew_agent::agent::Agent` — the ReAct loop body, refactored
  but not re-implemented.
- `mew_agent::session::SessionHandle` — state machine, unchanged.
- `mew_agent::orchestrator::BrowserAgentFactory` — already abstracts
  "build a browser agent" so the planner can compose with it.

**Files to touch:**
- `mew-agent/src/worker.rs` (new) — `BrowserAgentWorker`:
      long-lived, owns a `BrowserAgentFactory` output, exposes
      `submit(Todo) -> oneshot::Receiver<TodoResult>` and
      `signal(SupervisorSignal) -> ()`.
- `mew-agent/src/supervisor.rs` (new) — `SupervisorSignal` enum:
      `Pause | Resume | Cancel | Replan(Vec<Todo>)`. Steering is
      *not* a supervisor signal — it goes through the existing
      `mpsc::Sender<UserMessage>` path the legacy flow already uses.
      (Two paths for the same concern invites drift; one path is
      enough.)
- `mew-agent/src/worker_pool.rs` (new) — `WorkerPool` with a single
      worker for v1. The API takes a `Vec<BrowserAgentWorker>` so
      Phase 18 can grow it without changing call sites; v1 calls
      just pass a 1-element vec.
- `mew-agent/src/agent.rs` — small refactor: extract the ReAct loop
      body so it can be invoked by the worker without rebuilding
      `SessionHandle` each time. The ReAct loop's
      `tool_dispatch → snapshot → continue` shape must remain, but
      the loop boundary itself must `tokio::select!` on the
      supervisor signal so a `Cancel` can break it between tool
      calls (not in the middle of one — a tool call is atomic by
      contract).

**Architecture (one paragraph):**
The Tauri shell holds an `Arc<WorkerPool>` in `AppState`. The
planner calls `worker_pool.submit(todo)`, which returns a
`oneshot::Receiver<TodoResult>`. The worker, in a `tokio::spawn`'d
task, runs the ReAct loop scoped to that todo only. Inside the
loop, between tool calls, the worker `tokio::select!`s on the
supervisor signal so a `Cancel` can break out cleanly. The
planner, in its own task, `tokio::select!`s on the receiver, the
supervisor signal, and a per-todo deadline timer. On deadline,
the planner sends `Cancel`; on user steering, the planner routes
through the existing `mpsc::Sender<UserMessage>` (so the LLM sees
the steering message in the next turn, same as the legacy path);
on completion, the planner calls the Phase 12 evidence gate.

**Checklist:**
- [x] Define `SupervisorSignal` in `mew-agent/src/supervisor.rs`:
      `Pause | Resume | Cancel | Replan(Vec<Todo>)` with a
      `signal_id: u64` monotonic counter so the worker can ignore
      stale signals. The worker holds a `signal_id: u64` watermark
      and discards any signal with `id <= watermark`
- [x] Add `BrowserAgentWorker` in `mew-agent/src/worker.rs`:
      `submit(Todo) -> oneshot::Receiver<TodoResult>` and
      `signal(SupervisorSignal) -> ()`. The `submit` future
      panics with `panic!("await the previous receiver first")` if the previous submit's
      `Receiver` has not yet been awaited — caller bug, fail
      loudly
- [x] The worker runs a *scoped* ReAct loop: only the current todo
      is in `CompletenessTracker`; pre-existing `Handoff.subtasks` are
      filtered to just the active todo
- [x] Inside the ReAct loop, the worker's `await self.tool_dispatch(...)`
      call is wrapped in a `tokio::select!` against the supervisor
      signal. `Cancel` causes the loop to return a
      `TodoResult::Cancelled` *without* consuming the next tool
      dispatch — the current tool's effect on the page is whatever
      it was, but no further tool calls run
- [x] Add `WorkerPool` in `mew-agent/src/worker_pool.rs` — the public
      API is `submit` / `signal` / `shutdown`. Internally v1 holds
      `Vec<BrowserAgentWorker>` of length 1; Phase 18 grows it
- [x] `WorkerPool::submit` is the gate that enforces
      `agent.todo.max_inflight` — if the pool is full, return
      `Err(PoolError::Busy)` and the planner surfaces it as
      `BrowserResult::failure("backpressure", ...)`. The single-
      worker v1 implements this with an in-flight flag; multi-
      worker v2 is a count comparison
- [x] Refactor `mew-agent/src/agent.rs` so the ReAct loop body is
      callable with a pre-built `SessionHandle` and a pre-filtered
      `Handoff` (don't tear down and rebuild session per todo)
- [x] Add `tests/worker_lifecycle.rs`:
      `submit_then_complete`, `submit_then_cancel_mid_loop`,
      `submit_then_deadline`, `submit_then_steering_via_existing_mpsc`,
      `submit_twice_without_awaiting_first_receiver_panics`,
      `submit_while_pool_shutting_down_returns_err`,
      `cancel_signal_with_stale_id_is_ignored`
- [x] Add `#[tracing::instrument(skip_all, fields(todo_id, signal_id))]`
      on the worker's submit/signal/complete paths so the per-todo
      trace is its own span. `skip_all` on the ReAct body to avoid
      logging the entire prompt

**Acceptance:**
- A planner can submit, observe completion, and submit again on the
  same `BrowserAgentWorker` without rebuilding it.
- Cancelling a todo mid-ReAct does not corrupt the next todo's
  `SessionHandle` (verifiable via `phase3_round_trip` style test).
- A second `submit` while the first receiver is unawaited panics
  with a message that says "await the previous receiver first" —
  this is a programmer error and fail-loud is correct.
- Existing `phase2_instagram_regression` still passes — the refactor
  is internal.

---

## Phase 14 — Tauri command surface for the planner

**Goal:** Expose the planner as a Tauri command, with the worker pool
in `AppState`, and the per-todo evidence flow wired to the existing
`TauriSink`. The frontend gets new commands without breaking the
existing `send_message` path.

**Reuses:**
- `mew-ui/src-tauri/src/lib.rs::AppState` and `ActiveSession` —
      extended, not replaced.
- `mew_agent::orchestrator::TauriSink` — extended with new event
      mappings.

**Files to touch:**
- `mew-ui/src-tauri/src/lib.rs` — add `WorkerPool` to `AppState`,
      add `start_task` / `pause_todo` / `resume_todo` /
      `cancel_todo` / `replan` / `stop_task` commands.
- `mew-agent/src/orchestrator.rs` — extend `OrchestratorEvent` with
      `TodoStateChanged { task_id, todo }` and `TodoRejected {
      task_id, todo_id, reason }` variants.
- `mew-ui/src-tauri/src/lib.rs` (`TauriSink` impl) — map the new
      events to `app.emit("todo-state-changed", ...)` and
      `app.emit("todo-rejected", ...)`.
- `mew-ui/src-tauri/Cargo.toml` — add `tokio-util` for
      `CancellationToken`.

**Architectural rule (no exceptions, call it out in code review):**
No new Tauri command may call `chromiumoxide` directly. Every
browser interaction goes through the `mew_cdp` crate, same as
the legacy flow. The pinned `chromiumoxide = 0.9.1` in the
workspace `Cargo.toml` is consumed by `mew-cdp` only; the
Tauri shell must not add a transitive dep on it. The PR template
should reject any change that violates this.

**Tauri command contract (in `lib.rs`):**
```rust
/// `task_id` is generated server-side and returned in `TaskHandle`.
/// The frontend tracks it from then on; it is *not* derived from
/// the todo id (one task has many todos).
#[tauri::command]
async fn start_task(
    app: AppHandle,
    state: State<'_, AppState>,
    message: String,
    history: Vec<FrontendMessage>,
) -> Result<TaskHandle, String>;

#[tauri::command]
async fn pause_todo(state: State<'_, AppState>, task_id: String, todo_id: String)
    -> Result<(), String>;
#[tauri::command]
async fn resume_todo(state: State<'_, AppState>, task_id: String, todo_id: String)
    -> Result<(), String>;
#[tauri::command]
async fn cancel_todo(state: State<'_, AppState>, task_id: String, todo_id: String)
    -> Result<(), String>;
#[tauri::command]
async fn replan(state: State<'_, AppState>, task_id: String) -> Result<(), String>;

/// Phase 18 ships with this; Phase 14 declares the signature
/// only and stubs the body so the frontend can wire the button.
#[tauri::command]
async fn stop_task(state: State<'_, AppState>, task_id: String) -> Result<(), String>;
```

**Why `task_id` is on every command (not just `start_task`):**
A single Tauri session can have multiple in-flight tasks (the
user opened two chat sessions, or sent a new message while the
previous task is still running). The `WorkerPool` keys its
internal state by `task_id`, not by `todo_id`, because two
concurrent tasks could in principle have a todo with the same
id (`navigate-instagram-1`) — ids are local to a task, not
global. Frontend always carries the `task_id` it got from
`start_task`'s return value.

**The gotcha (call it out so it doesn't bite in code review):**
Tauri 2 `State<'_, T>` is *not* `'static`. You cannot move it into a
`tokio::spawn` future. The pattern is: capture the `AppHandle`, then
inside the spawned task, re-fetch state via `app.state::<AppState>()`.
The existing `mew-ui` code already does this in the steering path —
extend the pattern, don't reinvent it.

**Checklist:**
- [x] Add `WorkerPool` to `AppState` as `Arc<WorkerPool>`; initialize
      in the `setup` hook after config load. Initialization is
      `tokio::spawn`-friendly: if it fails (e.g. config invalid),
      `start_task` returns the error, not a panic
- [x] Add `start_task` command: classifier → decompose to todos →
      submit first todo → return `TaskHandle { task_id, todos }` so
      the UI can render the list immediately. `task_id` is a UUIDv4
      generated by `uuid::Uuid::new_v4()`, not a hash of the task
- [x] Add `pause_todo`, `resume_todo`, `cancel_todo` commands — all
      forward a `SupervisorSignal` to the worker pool, scoped by
      `task_id`
- [x] Add `replan` command: cancel the current todo, ask the
      `planner::decompose_to_todos` to re-run with the original task
      plus the failure context, re-submit. Replanning preserves
      completed todos (their evidence is still valid) and only
      re-derives the pending tail
- [x] Add `stop_task` command with a stub body that just returns
      `Err("not yet implemented")`; Phase 18 fills it in
- [x] Extend `OrchestratorEvent` with `TodoStateChanged { task_id,
      todo }` and `TodoRejected { task_id, todo_id, reason }` —
      `#[serde(tag = "type")]` keeps wire compat with the legacy
      frontend listeners
- [x] Extend `TauriSink` impl: `TodoStateChanged → "todo-state-changed"`,
      `TodoRejected → "todo-rejected"`. Both events also carry
      `task_id` in the payload so the frontend reducer can
      multi-task
- [x] Wire `start_task` so the existing `ChatAgent → BrowserAgent`
      flow is the **fallback** when `config.agent.planner_enabled`
      is `false` (default during Phase 14; flip to `true` in Phase 16)
- [x] Add `error_message::for_user` mapping for every new error path
      (no `?` operator propagating raw `anyhow::Error` to the
      frontend). Add a unit test that asserts every new `Result<_, String>`
      Tauri command's error path is *not* a JSON dump

**Acceptance:**
- The 6 new commands are listed in the Tauri config and the
  frontend can `invoke()` them.
- With `planner_enabled: false`, the existing `send_message` flow
  is byte-identical to before this phase. A `git diff` on the
  legacy code path shows zero changes.
- Cancelling a todo emits a `todo-rejected` event the frontend can
  listen for, and the worker is immediately ready for the next todo.
- Two parallel `start_task` calls produce two distinct `task_id`s
  and the worker pool's per-task state is independent.

---

## Phase 15 — Per-todo UI checklist surface

**Goal:** Replace the single "Working · N steps" pill with an
explicit, live per-todo checklist rendered in the chat. Each todo
gets a row with its id, intent, status icon, and an inline progress
counter. The collapsible "view details" expands to show the per-todo
trace.

**Reuses:**
- `mew-ui/src/main.ts::MessageKind` — extend with `todo_list`
      variant.
- The existing `<details>` collapsible in the task card.
- The existing `liveLines` ring buffer on `ChatMessage` for the
  per-todo sub-progress.

**Files to touch:**
- `mew-ui/src/main.ts` — new `TodoRow` interface, `todo_list`
      MessageKind variant, new `listen("todo-state-changed", ...)` and
      `listen("todo-rejected", ...)` handlers, `updateTodoRow()`
      reducer.
- `mew-ui/src/style.css` — `.todo-row` styles, status icons
      (use Unicode glyphs; no icon font dep), `[data-status="done"]`
      / `[data-status="rejected"]` color tokens.

**The rendering rule (per row, derived from `TodoStatus`):**
- `Pending` → empty circle `○`, dim text.
- `Running` → half-filled circle `◐`, ink-blue accent, live line
  counter. (`Running` is *not* a `TodoStatus`; it's a UI flag
  derived from "is this todo currently the in-flight one for its
  task?")
- `Done` → filled circle `●`, green accent, single line of evidence
  text.
- `Skipped` / `Failed` / `Exhausted` → hollow square `□`, gray,
  reason inline.

**`Rejected` is not a `TodoStatus`.** It is a per-attempt
annotation that lives in the `TodoRejected` event payload. When
the planner exhausts `attempts` without matching evidence, the
todo's *terminal* status becomes `Failed` (or `Exhausted` if the
heuristic chose so). The UI row for a `Failed` todo shows the
most recent rejection reason inline. This keeps the state
machine clean: `TodoStatus` is the single source of truth for
"is this todo done?", and rejection is a transient event
annotation.

**Checklist:**
- [x] Define `TodoRow { id, intent, status: TodoStatus, attempts,
      evidence?, rejected_reason? }` in `main.ts`. `TodoStatus` is
      the strict subset
      `'pending' | 'done' | 'skipped' | 'failed' | 'exhausted'` —
      `Running` is a UI flag, not a status
- [x] Extend `MessageKind` with `'todo_list' | 'todo_rejected'`
- [x] The state store keys rows by `(task_id, todo_id)` so two
      concurrent tasks don't trample each other's todo rows
- [x] Add `listen('todo-state-changed', ...)` and route into
      `updateTodoRow(state, event)` reducer (immutable update
      keyed by `task_id + todo_id`)
- [x] Add `listen('todo-rejected', ...)` and route into the same
      reducer with `rejected_reason` populated; the *status* does
      not change unless the rejection exhausts `attempts`, in
      which case the matching `TodoStateChanged` event follows
- [x] Add `renderTodoRow(row: TodoRow) -> HTMLElement` that
      returns a single DOM node — no innerHTML strings, no
      `dangerouslySetInnerHTML`, no eval
- [x] CSS: `.todo-row` is a 28px-tall flex row with a fixed-width
      status column (28px) and a 1fr intent column
- [x] CSS: status colors use the existing `--ink-50 / --ink-300 /
      --accent / --success / --danger` tokens — do not invent new
      colors
- [x] CSS: a `[data-just-changed]` attribute drives a 240ms
      background flash on state change so the user sees the
      transition, not the snapshot
- [x] When a todo transitions to `Failed` / `Exhausted` (i.e.
      attempts exhausted), auto-open the task card's `<details>`
      so the user sees the trace
- [x] The header pill ("Working · N steps") becomes "Working · T of
      N todos" — N = total, T = terminal count (`done + skipped +
      failed + exhausted`)
- [x] Keyboard: arrow keys move focus between todo rows in the
      currently focused task card; Enter toggles the inline
      details
- [x] `Running` row highlight auto-advances when one todo finishes
      and the next one starts — implement by computing `Running`
      in the reducer as "the in-flight todo for this task,
      according to the most recent `TodoStateChanged` event"

**Acceptance:**
- A 3-todo task renders as 3 rows in the chat, all in `Pending`
  initially.
- Submitting the first todo animates the row to `Running`, then
  `Done` on evidence match, and the header pill updates.
- On rejection, the row shows the rejection reason inline but
  *stays in `Pending`* until `attempts` is exhausted. The trace
  `<details>` opens only on the final failure transition, not on
  every intermediate rejection.
- Two concurrent tasks in the chat show two separate todo lists
  with independent progress.

---

## Phase 16 — Planner outer loop, opt-in via config

**Goal:** Wire the planner end-to-end: classify → decompose →
supervise → evidence-gate → synthesize. Behind a
`config.agent.planner_enabled: true` flag, off by default, so the
existing flow keeps working for a full release cycle.

**Reuses:**
- `mew_agent::chat_agent::ChatAgent::classify_intent` — entry point
      for the outer loop.
- `mew_agent::chat_agent::ChatAgent::synthesize_reply` — final
      synthesis, unchanged.
- The Phase 14 commands and the Phase 15 UI.

**Files to touch:**
- `mew-agent/src/planner.rs` — `Planner::run(task: Handoff, pool:
      &WorkerPool, sink: &dyn TurnSink) -> BrowserResult`.
- `mew-agent/src/orchestrator.rs` — in `run_turn`, branch on
      `planner_enabled`: if true, hand off to `Planner::run`; else,
      the existing path.
- `mew-agent/src/lib.rs` — export `Planner`.
- `config.yaml` — add `agent.planner_enabled: false` (default off).
- `mew-ui/src-tauri/src/lib.rs::start_task` — read the config flag
      and dispatch accordingly.

**The outer loop, in plain English:**
1. `classify_intent(user_message)` → `Intent::BrowserTask(task)`.
2. `planner::decompose_to_todos(&Handoff)` → `Vec<Todo>`.
3. For each `Todo` in topological order:
   - `worker_pool.submit(todo.clone()).await` →
     `oneshot::Receiver<TodoResult>`.
   - `tokio::select!` on receiver, supervisor signal, and
     per-todo deadline.
   - On `Done`: call Phase 12 evidence gate; on match, advance;
     on mismatch, retry up to `attempts`.
   - On `Rejected`: surface as `TodoRejected` event, optionally
     re-plan the remaining todos via `planner::decompose_to_todos`.
   - On deadline: send `SupervisorSignal::Cancel`, mark
     `Exhausted { reason: "deadline" }`, advance.
4. After all todos terminal, build a `BrowserResult` from the
   per-todo outcomes (Done / Partial / Failed mirrors the existing
   per-subtask rollup).
5. `synthesize_reply` produces the user-facing text — same path as
  before.

**Checklist:**
- [x] Implement `Planner::run` in `planner.rs` per the algorithm
      above — pure orchestrator, no LLM calls inside (LLM work is
      scoped to the worker per todo)
- [x] Branch in `orchestrator::run_turn` on
      `planner_enabled`: existing path or `Planner::run`
- [x] `config.yaml`: `agent.planner_enabled: false` by default with
      an inline comment explaining the rollout
- [x] `start_task` reads the flag at command entry, not per-todo,
      so a long task doesn't switch modes mid-flight
- [x] On per-todo failure, the planner's "replan" decision is
      itself deterministic (heuristic, not LLM): if the failed
      todo's `acceptance` was `AnySnapshot` and 3 retries failed,
      mark `Exhausted` and move on; otherwise `Replan` once
- [x] End-of-task `synthesize_reply` includes a per-todo
      summary table in the chat, not just a one-liner — the user
      should see "T1 done · T2 done · T3 skipped (deadline)"
- [x] Update `phase2_instagram_regression` and `phase3_round_trip`
      to be mode-agnostic: both must pass with
      `planner_enabled: true` and `planner_enabled: false`
- [x] Update `phase5_live_progress` so the per-todo `ProgressLine`
      stream is the new mode-of-truth (and the old "N steps" pill
      still works under the legacy mode)

**Acceptance:**
- Flip `planner_enabled: true` in `config.yaml`, restart the
  Tauri app, run the three motivating scenarios from `proj.md`,
  all three pass:
  1. *"Visit instagram and text my friend hi"* — todos: `navigate
     instagram`, `send hi`; both `Done` with evidence.
  2. *"Any browser task"* — the chat summary shows per-todo status.
  3. *"Multi-platform job search"* — the existing Phase 7
     `ResearchPlanner` output is *itself* a todo, with one
     sub-todo per platform. The per-platform sub-todos in turn
     run on the worker; the `max_iterations: 100` cap from
     `config.yaml` is checked per-worker-task, not per-task
     (verify in the trace file — a 5-platform search hits ~500
     iterations, not 100, with the per-worker cap reset between
     platforms).
- Flip back to `false`; the legacy `ChatAgent → BrowserAgent`
  flow still passes all existing examples.

**The iteration-cap gotcha (call it out before the PR):**
The shipped `max_iterations: 100` is enforced inside
`agent.rs::run_inner` *per ReAct loop*. In legacy mode, one
user message = one ReAct loop = one cap. In planner mode, one
user message = N todos, each with its own ReAct loop. If the
cap is left as a global single value, a 5-todo research task
trips it after 20 iterations per todo. The fix is to
re-initialize the iteration counter per `Todo` (i.e. when the
worker calls `submit` it passes a fresh `Handoff` whose
`max_iterations` comes from the per-todo `TodoBudget`, not from
the global config). The checklist item below is the regression
test for this.

**Additional checklist item (replaces the bullet above):**
- [ ] Regression test: a 3-todo task that would have hit the
      100-iteration cap in legacy mode completes cleanly in
      planner mode because each todo's counter resets at submit
      time. Assert: the trace shows three separate iteration
      spans, each capped at `TodoBudget.max_iterations`, not one
      shared counter

---

## Phase 17 — Evaluation harness for the planner-worker contract

**Goal:** Extend `mew_agent::eval` with scenarios that *fail* the
planner-worker contract if the worker shortcuts. The eval scenarios
are the regression net for the "no shortcut" claim.

**Reuses:**
- `mew_agent::eval::assertions` — the existing reusable handoff
      assertions.
- `mew_resilience` mock-page fixtures — same as the Phase 6 unit
      tests.

**Files to touch:**
- `mew-agent/src/eval/scenarios/planner_worker_shortcut.rs` (new) —
      three scenarios: accept-on-match, reject-on-mismatch, retry-
      on-stale-evidence.
- `mew-agent/src/eval/assertions.rs` — new
      `assert_todo_done(todo, evidence)` and
      `assert_todo_rejected(todo, reason)` helpers.
- `mew-agent/src/eval/harness.rs` — wire the new scenarios behind
      the `eval` feature flag (already opt-in).
- `mew-agent/src/eval/runner.rs` — run all planner scenarios in
      sequence, report pass/fail per scenario.
- `docs/eval-history.md` — append a Phase 17 section with the
      pass-rate (should be 100% on first commit; future regressions
      land here).

**Three must-have scenarios:**
1. **Happy path.** Worker reports `Done` with signature matching
   planner's. Assert: todo transitions to `Done`, evidence
   populated, `attempts == 1`.
2. **Worker shortcut.** Worker reports `Done` with a *fake*
   signature. Assert: todo stays `Pending`, `attempts == 2` after
   retry, eventual `Rejected` on second mismatch.
3. **Stale evidence.** Worker re-uses a signature from a previous
   todo. Assert: rejected as `StaleEvidence`, todo `Pending`.

**Checklist:**
- [ ] `eval/scenarios/planner_worker_shortcut.rs` with the three
      scenarios above
- [ ] `eval/assertions.rs::assert_todo_done` checks
      `status == Done ∧ evidence.is_some() ∧ evidence.worker ==
      evidence.planner`
- [ ] `eval/assertions.rs::assert_todo_rejected` checks
      `status != Done ∧ attempts > 1 ∧ rejected_reason.is_some()`
- [ ] `eval/harness.rs` runs the new scenarios and reports
      per-scenario pass/fail
- [ ] `docs/eval-history.md` has a Phase 17 row: `phase 17: 3/3
      passing` (or a regression table if anything's red)
- [ ] CI: `cargo test --features eval -p mew-agent` is the same
      single command that catches Phase 9 regressions *and* Phase
      17 regressions
- [ ] Add `mew-cli/src/bin/phase17_planner_eval.rs` example so a
      developer can run just the planner scenarios without the
      full eval gate
- [ ] Document the eval scenarios in `proj.md` §2.5.9 ("Evaluation
      harness") — add a Phase 17 paragraph

**Acceptance:**
- `cargo test --features eval -p mew-agent` includes the 3 new
  scenarios, all green.
- A deliberate regression (comment out the evidence check in
  Phase 12) causes exactly the 3 new scenarios to fail, and they
  fail with messages that point at the right module.

---

## Phase 18 — Production hardening for the planner

**Goal:** Trace logging, error paths, and a small ergonomic escape
hatch so the planner is safe to leave on by default.

**Reuses:**
- The existing `mew_agent::tracing_layer` (JSONL, opt-in via
      `MEW_TRACING_DIR`).
- The existing `error_message::for_user` layer.
- The existing `Config` schema doc in `proj.md` §5.

**Files to touch:**
- `mew-agent/src/tracing_layer.rs` — add
      `trace_todo_lifecycle(todo_id, event)` spans; emit on submit,
      on every `mark_todo_done` call, on every rejection.
- `mew-agent/src/error_message.rs` (if not already there) — add
      `todo_rejected(todo, reason)` and
      `planner_disabled_fallback(reason)` mappings.
- `mew-agent/src/budget.rs` — wire per-todo `TodoBudget` to the
      existing `pacing` and `summarization.budget` configs.
- `mew-ui/src-tauri/src/lib.rs` — surface the
      `planner_disabled_fallback` warning as a one-time chat
      message so the user knows which mode is active.
- `proj.md` — update the config schema, the architecture diagram,
      and the "Project status" section to reflect Phase 11-18.
- `README.md` — flip `planner_enabled` to `true` in the example
      config and document the per-todo UI.

**The escape hatches (must-have, do not skip):**
- **Kill switch:** a `stop_task(task_id)` Tauri command that
  cancels the active todo, sends a final `TodoStateChanged` event
  with `status: Failed { reason: "stopped by user" }`, and tears
  down the per-task state in the pool. Wired to a red "Stop"
  button next to the header pill.
- **Per-todo timeout knob:** `agent.todo.default_budget_secs` in
  `config.yaml`, default 120s, hard-clamped 5..=600.
- **Backpressure (per-task, not per-worker):** the worker pool is
  single-worker in v1, so worker-level backpressure is trivial
  (one todo in flight per task at most). What actually needs
  guarding is the number of *concurrent tasks* in the pool.
  `agent.todo.max_concurrent_tasks` (default 4) caps how many
  tasks the pool accepts; a 5th `start_task` returns
  `BrowserResult::failure("backpressure", "Too many concurrent
  tasks; wait for one to finish.", None)`. Phase 18's
  multi-worker grow-up revisits this knob.

**Out of scope for Phase 18 (deferred / explicit non-goals):**
- **Mode-visible greeting:** the user just set
  `planner_enabled` in `config.yaml`; they know which mode is
  active. A system greeting that says "Planner mode: on" is
  nice-to-have noise, not a safety property. The header pill's
  "T of N todos" text already tells the user the planner is
  supervising. Defer until a user actually asks for it.

**Checklist:**
- [ ] `tracing_layer.rs::trace_todo_lifecycle` is called on every
      todo submit / mark / reject with `task_id`, `todo_id`, and
      a `phase` tag (`submit | mark_done | mark_rejected |
      mark_failed | mark_exhausted | cancel`); the JSONL line
      is one per event
- [ ] `error_message::todo_rejected` returns a plain-language
      sentence, not a JSON dump. Add a unit test: a fixture
      `EvidenceMismatch { worker: "len:0123abcd", planner:
      "len:0123abce" }` produces a sentence containing the
      planner's signature and the words "did not match" — no
      JSON, no Rust path
- [ ] `config.yaml` documents the `agent.todo` block:
      `enabled` (default false), `default_budget_secs` (default
      120, hard-clamp 5..=600), `max_attempts` (default 3),
      `max_concurrent_tasks` (default 4),
      `replan_max_per_task` (default 1)
- [ ] `stop_task` Tauri command cancels the active todo,
      sends a final `TodoStateChanged` event with
      `status: Failed { reason: "stopped by user" }`, and
      tears down the per-task state in the pool — no orphan
      `tokio::spawn`s. A test fires `stop_task` mid-ReAct and
      asserts the worker task is fully joined (not leaked)
      before `stop_task` returns
- [ ] Frontend: the header pill becomes a stop button while
      a task is active; clicking it invokes `stop_task` and
      the pill returns to `Idle`. Disabled (gray) when no task
      is active so the user can't fire it accidentally
- [ ] `proj.md` §5 (Configuration reference) and §2 (Crate
      ecosystem) reflect the new module; the architecture
      diagram in §3 shows the planner as a third box
- [ ] `README.md` example config has `planner_enabled: true`
      with a comment about the rollout status
- [ ] `docs/phase17-planner-eval.md` (new) — the eval
      scenarios and their golden outcomes
- [ ] `docs/phase18-planner-hardening.md` (new) — the
      trace format, the kill switch, the backpressure rule

**Acceptance:**
- A 5-minute manual test: enable planner mode, run a 3-todo
  task, hit "Stop" mid-flight, see the chat show "task
  cancelled", restart the app, and the legacy mode still
  works.
- `MEW_TRACING_DIR=./trace cargo run --bin phase17_planner_eval`
  produces a JSONL file with one line per todo lifecycle event,
  every line carrying both `task_id` and `todo_id`.
- The three motivating scenarios from `proj.md` pass on the
  first try in planner mode, the chat summary shows the
  per-todo rollup, and the trace file lets a developer replay
  the supervisor's decisions.

---

## Cross-phase invariants (verify before each PR merge)

- [ ] `cargo test -p mew-agent` is green at every phase boundary.
- [ ] `cargo test --features eval -p mew-agent` is green at
      Phase 17+.
- [ ] `mew-ui` builds with `npm run build` and produces a bundle.
- [ ] `phase2_instagram_regression`, `phase3_round_trip`, and
      `phase7_benchmarks` examples still pass in *both* planner
      modes (off and on).
- [ ] No new `unwrap()` in the supervisor or worker paths —
      every error routes through `error_message::for_user`.
- [ ] No `println!` or `eprintln!` in `mew-ui/src-tauri` —
      every log goes through `tracing` or `tauri-plugin-log`.
- [ ] No new `unsafe` block anywhere in `mew-agent`.
- [ ] `git diff` of `mew-cdp`, `mew-perception`, `mew-nav`,
      `mew-resilience` shows zero changes from this work plan.
      (The four browser-perception crates are read-only from
      this point forward; if a phase genuinely needs to touch
      one, that's a sign the phase is too aggressive — split it.)

## Deliberate non-goals (do not "improve" these in any phase)

- **Multi-worker pool (N > 1).** v1 ships with one worker. The
  API is shaped for growth, but no phase before Phase 18
  *completes* multi-worker. Don't add it speculatively.
- **Cryptographic signatures.** The `len:{:08x}` is a hash, not
  a signature. The worker can collission-attack it if it
  controls both the obs text and the signature; the defense is
  the *AX-tree text in the result* (a reviewer can read the
  text), not a stronger hash. Upgrading to BLAKE3 or HMAC
  doesn't help because the attacker controls the input. If a
  real audit threat model emerges, add a third-party
  perception verifier — not a better hash.
- **LLM-based replanning.** The "replan" path in Phase 16 is
  deterministic. An LLM-driven replanner is a separate
  project; don't bolt it on.
- **Replacing the legacy `ChatAgent → BrowserAgent` flow.** The
  legacy path stays supported indefinitely; the new path is
  opt-in. A "delete the old path" PR will be rejected.
- **Per-`Todo` LLMs.** Every todo in a task runs on the same
  worker with the same LLM config. Per-todo model
  specialization (small model for navigate, big model for
  write) is a future optimization, not Phase 11–18 work.

---

## Phase status (append-only log)

| Phase | Title | Status |
| --- | --- | --- |
| 1 | Core foundation (v1) | shipped |
| 1.5 | Bug-2 wire fix | shipped |
| 2 | Reliability & steering (v2) | shipped |
| 3 | Desktop shell (v3) | shipped |
| 4 | UI overhaul | shipped |
| 5 | Live step summarization | shipped |
| 6 | Resilience core | shipped |
| 7 | Long-horizon research loop | shipped |
| 8 | Obstacle & CAPTCHA handling | shipped |
| 9 | Evaluation harness | shipped |
| 10 | Production hardening | shipped |
| 11 | Todo schema and decomposition contract | shipped |
| 12 | Per-todo evidence gate | shipped |
| 13 | Browser Agent as long-lived supervised worker | shipped |
| **14** | **Tauri command surface for the planner** | **not started** |
| **15** | **Per-todo UI checklist surface** | **not started** |
| **16** | **Planner outer loop, opt-in via config** | **not started** |
| **17** | **Evaluation harness for the planner-worker contract** | **not started** |
| **18** | **Production hardening for the planner** | **not started** |
