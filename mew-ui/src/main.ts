import { invoke, Channel } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import './style.css';

// ===========================================================================
// Phase 4: single chat surface, no transcript panel.
//
// Every interaction — user messages, ChatAgent replies, browser task
// lifecycle (started / progress / completed / failed) — is rendered in
// one chat list. The right-hand "Live Transcript" panel is gone. Per
// task, the raw transcript is still available, but behind a
// collapsible "view details" affordance on the task message itself
// rather than on a separate, always-visible panel.
//
// The `MessageKind` enum is the single source of truth for how a
// message is rendered. Adding a new visual treatment means adding a
// variant here + a CSS class. Adding a new event channel means
// adding a `listen()` call that funnels into `pushMessage`.
// ===========================================================================

type TodoStatus = 'pending' | 'done' | 'skipped' | 'failed' | 'exhausted';

interface TodoRow {
  id: string; // todo_id
  intent: string;
  status: TodoStatus;
  attempts: number;
  evidence?: any;
  rejected_reason?: string;
  is_running?: boolean;
}

const todoStore = new Map<string, TodoRow>(); // key: `${task_id}::${todo_id}`
const taskTodos = new Map<string, string[]>(); // key: task_id, value: array of todo_ids

type MessageKind =
  | 'user'
  | 'chat_reply'
  | 'task_started'
  | 'task_progress'
  | 'task_completed'
  | 'task_failed'
  | 'todo_list'
  | 'todo_rejected';

interface ChatMessage {
  /** Stable id, used as the DOM key and as the parent linkage for
   *  progress / completed / failed children. */
  id: string;
  kind: MessageKind;
  /** Visible one-liner for the chat list. */
  text: string;
  /** For `task_*` messages, the parent task id so children can be
   *  visually grouped and looked up later (currently used for the
   *  collapsible details). Null for top-level messages. */
  parentTaskId?: string;
  /** Free-form structured payload kept for "view details". */
  details?: Array<{ label: string; value: string }>;
  /** Monotonic counter so we can render the "Working · N steps"
   *  header pill. */
  stepCount?: number;
  /** Phase 5: live progress lines for this task. Capped at
   *  `liveLinesCap`; older lines are dropped from the visible
   *  list but the total count is preserved so the
   *  "…and N more steps" suffix is correct. */
  liveLines?: LiveProgressLine[];
  /** Phase 5: total number of progress lines the agent has
   *  emitted for this task — including ones we collapsed out
   *  of the visible cap. Used for the header meta line. */
  liveLineTotal?: number;
}

interface LiveProgressLine {
  /** Server-supplied kind tag — one of the strings from
   *  `summarizer::ProgressKind::as_str`:
   *  navigate / click / type / scroll / press_key / snapshot /
   *  vision_inspect / declare / mark_done / mark_skipped /
   *  mark_failed / finish / other. Used to color the bullet. */
  kind: string;
  /** Templated one-liner. */
  text: string;
  /** Unix seconds. */
  timestampSecs: number;
  /** True when the underlying tool call did NOT start with
   *  "ERROR:". The CSS uses this to red the line. */
  success: boolean;
}

interface HistoryMessage {
  role: 'user' | 'assistant';
  content: string;
}

const history: HistoryMessage[] = [];
const messages: ChatMessage[] = [];
let nextMessageId = 1;
const mintId = (): string => `m${nextMessageId++}`;
let activeTaskId: string | null = null;
let activeTaskSteps = 0;
// Phase 5: cap on the visible live progress lines per
// task. Mirrors `agent.summarization.live_lines_cap` in
// config.yaml (default 5). Older lines are collapsed into
// a single "…and N more steps" entry.
const liveLinesCap = 5;
// Phase 5: verbosity toggle. `concise` (default) hides the
// tool's first arg snippet; `detailed` shows it. The
// backend already sends the right string for the current
// verbosity — this toggle re-filters the visible set when
// the user clicks it.
type Verbosity = 'concise' | 'detailed';
let verbosity: Verbosity = 'concise';

// ---------------------------------------------------------------------------
// DOM scaffold
// ---------------------------------------------------------------------------

document.querySelector<HTMLDivElement>('#app')!.innerHTML = `
  <div id="app-shell">
    <section id="chat-pane" class="pane pane-chat">
      <div class="header">
        <div class="title">Mew Agent</div>
        <div class="header-controls">
          <div id="status-indicator" class="status-indicator status-idle">Idle</div>
          <button id="verbosity-btn" class="ctrl-btn verbosity-btn" title="Toggle progress verbosity" aria-label="Toggle verbosity">Concise</button>
          <button id="pause-btn" class="ctrl-btn" disabled>Pause</button>
          <button id="resume-btn" class="ctrl-btn" disabled>Resume</button>
        </div>
      </div>
      <div id="chat-list" class="chat-list">
        <div class="message chat_reply" data-kind="chat_reply">
          <div class="message-body">Welcome to mew-agent. Type a request to begin.</div>
        </div>
      </div>
      <div class="input-area">
        <form id="chat-form">
          <input id="chat-input" type="text" placeholder="Type a message..." autocomplete="off" />
          <button type="submit" id="send-btn">Send</button>
        </form>
      </div>
    </section>
    <aside id="preview-pane" class="pane pane-preview" aria-label="Live browser preview">
      <div class="preview-header">
        <div class="preview-title">
          <span class="preview-status-dot" id="preview-status-dot" aria-hidden="true"></span>
          <span class="preview-title-text">Live Preview</span>
          <span id="preview-status-pill" class="preview-status-pill">Idle</span>
        </div>
        <div class="preview-meta" id="preview-meta">Waiting for first frame…</div>
      </div>
      <div class="preview-stage">
        <img id="live-preview-img" class="live-preview-img" alt="Live browser preview" />
        <div id="preview-placeholder" class="preview-placeholder">
          <div class="preview-placeholder-dot"></div>
          <div class="preview-placeholder-text">The browser's view will appear here once a task starts.</div>
        </div>
      </div>
    </aside>
  </div>
`;

const chatList = document.querySelector<HTMLDivElement>('#chat-list')!;
const chatForm = document.querySelector<HTMLFormElement>('#chat-form')!;
const chatInput = document.querySelector<HTMLInputElement>('#chat-input')!;
const statusIndicator = document.querySelector<HTMLDivElement>('#status-indicator')!;
const pauseBtn = document.querySelector<HTMLButtonElement>('#pause-btn')!;
const resumeBtn = document.querySelector<HTMLButtonElement>('#resume-btn')!;
const verbosityBtn = document.querySelector<HTMLButtonElement>('#verbosity-btn')!;
const livePreviewImg = document.querySelector<HTMLImageElement>('#live-preview-img')!;
const previewPlaceholder = document.querySelector<HTMLDivElement>('#preview-placeholder')!;
const previewStatusDot = document.querySelector<HTMLSpanElement>('#preview-status-dot')!;
const previewStatusPill = document.querySelector<HTMLSpanElement>('#preview-status-pill')!;
const previewMeta = document.querySelector<HTMLDivElement>('#preview-meta')!;
let previewFrameCount = 0;

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

function pushMessage(msg: ChatMessage): void {
  messages.push(msg);
  const el = renderMessage(msg);
  chatList.appendChild(el);
  chatList.scrollTop = chatList.scrollHeight;
}

function renderMessage(msg: ChatMessage): HTMLDivElement {
  const el = document.createElement('div');
  el.className = `message ${kindClass(msg.kind)}`;
  el.dataset.kind = msg.kind;
  el.dataset.id = msg.id;
  if (msg.parentTaskId) {
    el.dataset.parentTaskId = msg.parentTaskId;
  }

  const body = document.createElement('div');
  body.className = 'message-body';
  body.textContent = msg.text;
  el.appendChild(body);

  if (msg.kind === 'task_started' || msg.kind === 'task_completed' || msg.kind === 'task_failed') {
    el.appendChild(renderDetails(msg));
    // Phase 5: live progress list under each task card. Renders
    // the most recent N lines and a "…and M more steps"
    // collapse. The list lives under the body so the user
    // sees the running activity *next to* the task card
    // rather than as separate `task_progress` chat messages
    // (which would flood the list).
    el.appendChild(renderLiveProgress(msg));
  }

  if (msg.kind === 'task_started') {
    const meta = document.createElement('div');
    meta.className = 'message-meta';
    const total = msg.liveLineTotal ?? 0;
    meta.textContent = total > 0
      ? `Agent is working · ${total} step${total === 1 ? '' : 's'} so far.`
      : 'Agent is working on this.';
    el.appendChild(meta);
  } else if (msg.kind === 'task_completed') {
    const meta = document.createElement('div');
    meta.className = 'message-meta';
    const steps = msg.stepCount ?? 0;
    meta.textContent = `Completed in ${steps} step${steps === 1 ? '' : 's'}.`;
    el.appendChild(meta);
  } else if (msg.kind === 'task_failed') {
    const meta = document.createElement('div');
    meta.className = 'message-meta';
    meta.textContent = 'Did not complete.';
    el.appendChild(meta);
  }

  return el;
}

function renderDetails(msg: ChatMessage): HTMLDetailsElement {
  const details = document.createElement('details');
  details.className = 'message-details';
  const summary = document.createElement('summary');
  summary.textContent = 'View details';
  details.appendChild(summary);

  const content = document.createElement('div');
  content.className = 'message-details-content';
  const rows = msg.details ?? [];
  if (rows.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'message-details-empty';
    empty.textContent = 'No raw transcript was captured for this task.';
    content.appendChild(empty);
  } else {
    for (const row of rows) {
      const rowEl = document.createElement('div');
      rowEl.className = 'message-details-row';
      const label = document.createElement('span');
      label.className = 'message-details-label';
      label.textContent = row.label;
      const value = document.createElement('span');
      value.className = 'message-details-value';
      value.textContent = row.value;
      rowEl.appendChild(label);
      rowEl.appendChild(value);
      content.appendChild(rowEl);
    }
  }
  details.appendChild(content);
  return details;
}

// Phase 5: render the live progress list under a task card.
// Mirrors the backend's `LiveProgress` buffer: a fixed cap
// (last 5 lines) with older lines collapsed into a single
// "…and N more steps" entry. The list is part of the card
// itself — no separate `task_progress` chat messages — so a
// 30-step task does not flood the chat list.
//
// When `verbosity === 'concise'`, lines whose text starts
// with a long quoted arg (e.g. `Typed "very long message"`)
// are truncated to 60 chars. Detailed keeps them as the
// backend sent them. The backend already filters by
// verbosity at the source; this is a defense-in-depth
// re-filter so toggling verbosity on the client re-renders
// the visible set without a backend round trip.
function renderLiveProgress(msg: ChatMessage): HTMLDivElement {
  const wrap = document.createElement('div');
  wrap.className = 'live-progress';
  const lines = msg.liveLines ?? [];
  const total = msg.liveLineTotal ?? lines.length;
  if (lines.length === 0 && total === 0) {
    const empty = document.createElement('div');
    empty.className = 'live-progress-empty';
    empty.textContent = 'Waiting for first step…';
    wrap.appendChild(empty);
    return wrap;
  }
  for (const ln of lines) {
    wrap.appendChild(renderLiveProgressLine(ln));
  }
  const collapsed = total - lines.length;
  if (collapsed > 0) {
    const more = document.createElement('div');
    more.className = 'live-progress-more';
    more.textContent = `…and ${collapsed} more step${collapsed === 1 ? '' : 's'}`;
    wrap.appendChild(more);
  }
  return wrap;
}

function renderLiveProgressLine(ln: LiveProgressLine): HTMLDivElement {
  const el = document.createElement('div');
  el.className = `live-progress-line kind-${ln.kind}${ln.success ? '' : ' failed'}`;
  const dot = document.createElement('span');
  dot.className = 'live-progress-dot';
  const text = document.createElement('span');
  text.className = 'live-progress-text';
  text.textContent = verbosity === 'concise' ? truncateForConcise(ln.text) : ln.text;
  el.appendChild(dot);
  el.appendChild(text);
  el.title = `${ln.kind} @ ${ln.timestampSecs}s`;
  return el;
}

function truncateForConcise(s: string): string {
  // The backend already truncates long arg text in concise
  // mode. This is a small re-cap on the client to defend
  // against a backend that hasn't been updated or a future
  // server that doesn't truncate. We match the backend's
  // limits loosely (40 for the typed text, 80 for URLs).
  if (s.length <= 80) return s;
  return s.slice(0, 79) + '…';
}

function kindClass(kind: MessageKind): string {
  switch (kind) {
    case 'user':
      return 'user';
    case 'chat_reply':
      return 'chat_reply';
    case 'task_started':
      return 'task_started';
    case 'task_progress':
      return 'task_progress';
    case 'task_completed':
      return 'task_completed';
    case 'task_failed':
      return 'task_failed';
    case 'todo_list':
      return 'todo_list';
    case 'todo_rejected':
      return 'todo_rejected';
  }
  return '';
}

function appendToTaskDetails(taskId: string, row: { label: string; value: string }): void {
  const task = messages.find((m) => m.id === taskId);
  if (!task) return;
  task.details = task.details ?? [];
  task.details.push(row);
  // Lazy DOM update: only re-render the existing <details> if it's
  // already in the DOM and the user has opened it. Otherwise the
  // next renderMessage will pick up the row.
  const taskEl = chatList.querySelector<HTMLDivElement>(`[data-id="${taskId}"]`);
  if (!taskEl) return;
  const details = taskEl.querySelector<HTMLDetailsElement>('.message-details');
  if (!details || !details.open) return;
  const content = details.querySelector<HTMLDivElement>('.message-details-content');
  if (!content) return;
  const empty = content.querySelector('.message-details-empty');
  if (empty) empty.remove();
  const rowEl = document.createElement('div');
  rowEl.className = 'message-details-row';
  const label = document.createElement('span');
  label.className = 'message-details-label';
  label.textContent = row.label;
  const value = document.createElement('span');
  value.className = 'message-details-value';
  value.textContent = row.value;
  rowEl.appendChild(label);
  rowEl.appendChild(value);
  content.appendChild(rowEl);
}

function updateTaskMeta(taskId: string): void {
  const task = messages.find((m) => m.id === taskId);
  if (!task) return;
  task.stepCount = activeTaskSteps;
  const taskEl = chatList.querySelector<HTMLDivElement>(`[data-id="${taskId}"]`);
  if (!taskEl) return;
  const meta = taskEl.querySelector<HTMLDivElement>('.message-meta');
  if (!meta) return;

  const todos = taskTodos.get(taskId) || [];
  const N = todos.length;
  let T = 0;
  for (const tid of todos) {
    const status = todoStore.get(`${taskId}::${tid}`)?.status;
    if (status && ['done', 'skipped', 'failed', 'exhausted'].includes(status)) {
      T++;
    }
  }

  if (task.kind === 'task_started') {
    if (N > 0) {
      meta.textContent = `Working · ${T} of ${N} todos`;
    } else {
      const total = task.liveLineTotal ?? activeTaskSteps;
      meta.textContent = total > 0
        ? `Agent is working · ${total} step${total === 1 ? '' : 's'} so far.`
        : 'Agent is working on this.';
    }
  } else if (task.kind === 'task_completed') {
    if (N > 0) {
      meta.textContent = `Completed · ${T} of ${N} todos`;
    } else {
      const total = task.liveLineTotal ?? activeTaskSteps;
      meta.textContent = `Completed in ${total} step${total === 1 ? '' : 's'}.`;
    }
  }
}

function updateStatus(state: string): void {
  statusIndicator.textContent = state;
  statusIndicator.className = 'status-indicator';
  const lower = state.toLowerCase();
  if (lower.includes('idle') || lower === '') {
    statusIndicator.classList.add('status-idle');
    chatInput.placeholder = 'Type a message...';
    pauseBtn.disabled = true;
    resumeBtn.disabled = true;
  } else if (lower.includes('run') || lower.includes('start') || lower.includes('work')) {
    statusIndicator.classList.add('status-running');
    chatInput.placeholder = 'Type to steer agent...';
    pauseBtn.disabled = false;
    resumeBtn.disabled = true;
  } else if (lower.includes('pause')) {
    statusIndicator.classList.add('status-paused');
    chatInput.placeholder = 'Agent paused. Type a message...';
    pauseBtn.disabled = true;
    resumeBtn.disabled = false;
  } else if (lower.includes('fail') || lower.includes('error')) {
    statusIndicator.classList.add('status-failed');
    chatInput.placeholder = 'Type a message...';
    pauseBtn.disabled = true;
    resumeBtn.disabled = true;
  } else if (lower.includes('done')) {
    statusIndicator.classList.add('status-done');
    chatInput.placeholder = 'Type a message...';
    pauseBtn.disabled = true;
    resumeBtn.disabled = true;
  } else {
    statusIndicator.classList.add('status-idle');
  }
  // Mirror the chat pill onto the preview pane so the right side
  // never disagrees with the left. The pill is purely cosmetic
  // (the actual stream health comes from the frame counter and
  // the "Live" status dot).
  if (previewStatusPill && previewStatusDot) {
    if (lower.includes('run') || lower.includes('start') || lower.includes('work')) {
      previewStatusPill.textContent = 'Streaming';
      previewStatusDot.classList.add('is-active');
      previewStatusDot.classList.remove('is-paused', 'is-failed', 'is-done');
    } else if (lower.includes('pause')) {
      previewStatusPill.textContent = 'Paused';
      previewStatusDot.classList.add('is-paused');
      previewStatusDot.classList.remove('is-active', 'is-failed', 'is-done');
    } else if (lower.includes('fail') || lower.includes('error')) {
      previewStatusPill.textContent = 'Failed';
      previewStatusDot.classList.add('is-failed');
      previewStatusDot.classList.remove('is-active', 'is-paused', 'is-done');
    } else if (lower.includes('done')) {
      previewStatusPill.textContent = 'Done';
      previewStatusDot.classList.add('is-done');
      previewStatusDot.classList.remove('is-active', 'is-paused', 'is-failed');
    } else {
      previewStatusPill.textContent = 'Idle';
      previewStatusDot.classList.remove('is-active', 'is-paused', 'is-failed', 'is-done');
    }
  }
}

function startWorkingPill(): void {
  activeTaskSteps = 0;
  if (activeTaskId) {
    const todos = taskTodos.get(activeTaskId) || [];
    const N = todos.length;
    if (N > 0) {
      updateStatus(`Working · 0 of ${N} todos`);
      return;
    }
  }
  updateStatus('Working · 0 steps');
}

function bumpWorkingPill(): void {
  activeTaskSteps += 1;
  if (activeTaskId) {
    const todos = taskTodos.get(activeTaskId) || [];
    const N = todos.length;
    if (N > 0) {
      let T = 0;
      for (const tid of todos) {
        const status = todoStore.get(`${activeTaskId}::${tid}`)?.status;
        if (status && ['done', 'skipped', 'failed', 'exhausted'].includes(status)) {
          T++;
        }
      }
      updateStatus(`Working · ${T} of ${N} todos`);
    } else {
      updateStatus(`Working · ${activeTaskSteps} step${activeTaskSteps === 1 ? '' : 's'}`);
    }
    updateTaskMeta(activeTaskId);
  }
}

function finishWorkingPill(kind: 'done' | 'failed'): void {
  if (kind === 'done') {
    updateStatus(`Done · ${activeTaskSteps} step${activeTaskSteps === 1 ? '' : 's'}`);
  } else {
    updateStatus(`Failed · ${activeTaskSteps} step${activeTaskSteps === 1 ? '' : 's'}`);
  }
  setTimeout(() => {
    updateStatus('Idle');
    activeTaskId = null;
    activeTaskSteps = 0;
  }, 1800);
}

function updateTodoRow(taskId: string, eventTodo: any, rejectedReason?: string) {
  const key = `${taskId}::${eventTodo.id}`;
  let row = todoStore.get(key);
  if (!row) {
    row = {
      id: eventTodo.id,
      intent: eventTodo.intent,
      status: 'pending',
      attempts: 0,
    };
    todoStore.set(key, row);
    
    let list = taskTodos.get(taskId);
    if (!list) {
      list = [];
      taskTodos.set(taskId, list);
    }
    if (!list.includes(eventTodo.id)) {
      list.push(eventTodo.id);
    }
  }
  
  row.status = eventTodo.status ?? row.status;
  row.attempts = eventTodo.attempts ?? row.attempts;
  row.evidence = eventTodo.evidence ?? row.evidence;
  
  if (rejectedReason) {
    row.rejected_reason = rejectedReason;
  }
  
  // Auto-advance Running flag
  const todos = (taskTodos.get(taskId) || []).map(id => todoStore.get(`${taskId}::${id}`)!);
  let foundRunning = false;
  for (const t of todos) {
    const isTerminal = ['done', 'skipped', 'failed', 'exhausted'].includes(t.status);
    if (!isTerminal && !foundRunning) {
      t.is_running = true;
      foundRunning = true;
    } else {
      t.is_running = false;
    }
  }

  // If status became Exhausted or Failed, auto-open details
  if (row.status === 'failed' || row.status === 'exhausted') {
    const taskEl = chatList.querySelector<HTMLDivElement>(`[data-id="${taskId}"]`);
    if (taskEl) {
      const details = taskEl.querySelector<HTMLDetailsElement>('.message-details');
      if (details) {
        details.open = true;
      }
    }
  }

  const el = document.getElementById(`todo-${key}`);
  if (el) {
    el.setAttribute('data-just-changed', 'true');
    setTimeout(() => el.removeAttribute('data-just-changed'), 240);
  }

  renderTodoListForTask(taskId);
  updateTaskMeta(taskId);
}

function renderTodoListForTask(taskId: string) {
  const taskEl = chatList.querySelector<HTMLDivElement>(`[data-id="${taskId}"]`);
  if (!taskEl) return;
  
  let listWrap = taskEl.querySelector<HTMLDivElement>('.todo-list-wrap');
  if (!listWrap) {
    listWrap = document.createElement('div');
    listWrap.className = 'todo-list-wrap';
    // Insert after message body, before details and live progress
    const body = taskEl.querySelector('.message-body');
    if (body && body.nextSibling) {
      taskEl.insertBefore(listWrap, body.nextSibling);
    } else {
      taskEl.appendChild(listWrap);
    }
    
    // Keyboard navigation
    listWrap.addEventListener('keydown', (e) => {
      const rows = Array.from(listWrap!.querySelectorAll('.todo-row')) as HTMLElement[];
      const idx = rows.indexOf(document.activeElement as HTMLElement);
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        if (idx < rows.length - 1) rows[idx + 1].focus();
      } else if (e.key === 'ArrowUp') {
        e.preventDefault();
        if (idx > 0) rows[idx - 1].focus();
      }
    });
  }
  
  listWrap.innerHTML = '';
  const todos = taskTodos.get(taskId) || [];
  for (const tid of todos) {
    const row = todoStore.get(`${taskId}::${tid}`);
    if (row) {
      listWrap.appendChild(renderTodoRow(taskId, row));
    }
  }
}

function renderTodoRow(taskId: string, row: TodoRow): HTMLElement {
  const el = document.createElement('div');
  el.className = 'todo-row';
  el.id = `todo-${taskId}::${row.id}`;
  el.tabIndex = 0; // Focusable
  el.dataset.status = row.status;
  
  if (row.is_running) {
    el.dataset.running = 'true';
  }

  const statusCol = document.createElement('div');
  statusCol.className = 'todo-row-status';
  let icon = '○';
  if (row.is_running) icon = '◐';
  if (row.status === 'done') icon = '●';
  if (['skipped', 'failed', 'exhausted'].includes(row.status)) icon = '□';
  statusCol.textContent = icon;

  const intentCol = document.createElement('div');
  intentCol.className = 'todo-row-intent';
  
  const intentText = document.createElement('div');
  intentText.className = 'todo-row-intent-text';
  intentText.textContent = row.intent;
  intentCol.appendChild(intentText);

  if (row.status === 'done' && row.evidence) {
    const ev = document.createElement('div');
    ev.className = 'todo-row-evidence';
    ev.textContent = 'Evidence matched';
    intentCol.appendChild(ev);
  }

  if (['failed', 'exhausted'].includes(row.status) && row.rejected_reason) {
    const reason = document.createElement('div');
    reason.className = 'todo-row-reason';
    reason.textContent = row.rejected_reason;
    intentCol.appendChild(reason);
  }

  el.appendChild(statusCol);
  el.appendChild(intentCol);

  el.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      const taskEl = chatList.querySelector<HTMLDivElement>(`[data-id="${taskId}"]`);
      if (taskEl) {
        const details = taskEl.querySelector<HTMLDetailsElement>('.message-details');
        if (details) {
          details.open = !details.open;
        }
      }
    }
  });

  return el;
}

// ---------------------------------------------------------------------------
// Event wiring
// ---------------------------------------------------------------------------

pauseBtn.addEventListener('click', async () => {
  try {
    const res = await invoke<string>('pause_session');
    pushMessage({
      id: mintId(),
      kind: 'chat_reply',
      text: `[System] ${res}`,
    });
  } catch (err) {
    pushMessage({
      id: mintId(),
      kind: 'chat_reply',
      text: `[System Error] ${err}`,
    });
  }
});

resumeBtn.addEventListener('click', async () => {
  try {
    const res = await invoke<string>('resume_session');
    pushMessage({
      id: mintId(),
      kind: 'chat_reply',
      text: `[System] ${res}`,
    });
  } catch (err) {
    pushMessage({
      id: mintId(),
      kind: 'chat_reply',
      text: `[System Error] ${err}`,
    });
  }
});

// Phase 5: verbosity toggle. Flips the visible line cap
// rendering between "concise" (truncated, no quoted arg
// snippet) and "detailed" (longer text with the arg
// snippet). The backend already filters at the source;
// this is a client-side re-filter so toggling re-renders
// the visible set without a backend round trip.
verbosityBtn.addEventListener('click', () => {
  verbosity = verbosity === 'concise' ? 'detailed' : 'concise';
  verbosityBtn.textContent = verbosity === 'concise' ? 'Concise' : 'Detailed';
  verbosityBtn.setAttribute(
    'aria-label',
    `Toggle progress verbosity (current: ${verbosity})`
  );
  // Re-render the live progress sub-list of every task
  // that has one. We replace the `.live-progress` child
  // in place so we don't re-render the whole chat list.
  for (const task of messages) {
    if (task.kind === 'task_started' || task.kind === 'task_completed' || task.kind === 'task_failed') {
      const taskEl = chatList.querySelector<HTMLDivElement>(`[data-id="${task.id}"]`);
      if (!taskEl) continue;
      const oldProgress = taskEl.querySelector<HTMLDivElement>('.live-progress');
      if (oldProgress) {
        const fresh = renderLiveProgress(task);
        oldProgress.replaceWith(fresh);
      }
    }
  }
});

// Phase 4: the live AgentEvent channel becomes the source of truth
// for per-iteration progress. We translate State / Tool / Summary
// events from the ReAct loop into task_progress lines and into the
// "Working · N steps" pill, then re-route them into the active
// task's "view details" collapsible.
const onEvent = new Channel<any>();
onEvent.onmessage = (event) => {
  if (event.type === 'State') {
    const data = event.data;
    if (!activeTaskId) return;
    bumpWorkingPill();
    appendToTaskDetails(activeTaskId, {
      label: `state @ ${data.timestamp_secs}s`,
      value: `${data.from} → ${data.to} (${data.kind})`,
    });
  } else if (event.type === 'Tool') {
    const data = event.data;
    if (!activeTaskId) return;
    bumpWorkingPill();
    appendToTaskDetails(activeTaskId, {
      label: `tool @ ${data.timestamp_secs}s`,
      value: `${data.name}(${truncate(data.args, 120)})`,
    });
  } else if (event.type === 'Summary') {
    const data = event.data;
    if (!activeTaskId) return;
    pushMessage({
      id: mintId(),
      kind: 'task_progress',
      parentTaskId: activeTaskId,
      text: truncate(data.text, 240),
    });
    appendToTaskDetails(activeTaskId, {
      label: `summary @ ${data.timestamp_secs}s`,
      value: data.text,
    });
  } else if (event.type === 'ProgressLine') {
    // Phase 5: live progress line. Append to the active
    // task's `liveLines` array (capped at `liveLinesCap`)
    // and re-render the live progress sub-list. We do NOT
    // push a separate `task_progress` chat message — the
    // live progress sub-list under the task card is the
    // canonical surface, and pushing a chat message per
    // step would flood the chat list (the exact problem
    // Phase 5 is solving).
    const data = event.data;
    if (!activeTaskId) return;
    appendLiveProgressLine(activeTaskId, {
      kind: data.kind,
      text: data.text,
      timestampSecs: data.timestamp_secs,
      success: data.success !== false,
    });
  }
};

/**
 * Append a `LiveProgressLine` to a task's `liveLines`,
 * respecting the cap. Updates the DOM in place so the
 * user sees the new line without a full re-render. Used
 * by the `onEvent.ProgressLine` handler above.
 */
function appendLiveProgressLine(taskId: string, ln: LiveProgressLine): void {
  const task = messages.find((m) => m.id === taskId);
  if (!task) return;
  task.liveLines = task.liveLines ?? [];
  task.liveLines.push(ln);
  task.liveLineTotal = (task.liveLineTotal ?? 0) + 1;
  // Cap the visible set.
  while (task.liveLines.length > liveLinesCap) {
    task.liveLines.shift();
  }
  // Update the DOM in place. We re-build the `.live-progress`
  // child rather than mutating individual <div> children so
  // the "…and N more steps" suffix stays correct on every
  // emission.
  const taskEl = chatList.querySelector<HTMLDivElement>(`[data-id="${taskId}"]`);
  if (!taskEl) return;
  const oldProgress = taskEl.querySelector<HTMLDivElement>('.live-progress');
  if (oldProgress) {
    const fresh = renderLiveProgress(task);
    oldProgress.replaceWith(fresh);
  }
  // Update the "Working · N steps" meta line.
  updateTaskMeta(taskId);
}

function truncate(s: string, n: number): string {
  if (s.length <= n) return s;
  return s.slice(0, n - 1) + '…';
}

chatForm.addEventListener('submit', async (e) => {
  e.preventDefault();
  const text = chatInput.value.trim();
  if (!text) return;

  const userMsg: ChatMessage = { id: mintId(), kind: 'user', text };
  pushMessage(userMsg);
  chatInput.value = '';

  try {
    const response = await invoke<string>('send_message', {
      text,
      history,
      onEvent,
    });
    // The synchronous return for the chat path carries the same
    // text the backend just emitted on `chat-reply`. The chat
    // path goes through the same listener; we still append a
    // local copy to handle the case where the listener raced
    // ahead of the return — `pushMessage` keeps the messages
    // array append-only and the DOM renders in order.
    if (response) {
      pushMessage({
        id: mintId(),
        kind: 'chat_reply',
        text: response,
      });
      history.push({ role: 'user', content: text });
      history.push({ role: 'assistant', content: response });
      while (history.length > 20) history.shift();
    }
  } catch (error) {
    pushMessage({
      id: mintId(),
      kind: 'chat_reply',
      text: `Error: ${error}`,
    });
    updateStatus('Failed');
  }
});

// ---------------------------------------------------------------------------
// Backend event listeners
// ---------------------------------------------------------------------------

listen<{ task_id: string; todo: any }>('todo-state-changed', (event) => {
  updateTodoRow(event.payload.task_id, event.payload.todo);
  if (activeTaskId === event.payload.task_id) {
    bumpWorkingPill();
  }
});

listen<{ task_id: string; todo_id: string; reason: string }>('todo-rejected', (event) => {
  updateTodoRow(event.payload.task_id, { id: event.payload.todo_id }, event.payload.reason);
});

listen<string>('agent-state', (event) => {
  updateStatus(event.payload);
});

// Phase 4: a "browser task started" event. We now render this as
// a dedicated `task_started` card with a collapsible details panel.
// The card stays in the chat list as the parent for the live
// `task_progress` lines that follow. When `task_completed` or
// `task_failed` fires, we mark the card's kind and update its
// meta line in place — no need to re-render the whole list.
listen<{ originating_message_id: string; task_description: string }>(
  'chat-task-started',
  (event) => {
    const taskId = mintId();
    activeTaskId = taskId;
    startWorkingPill();
    pushMessage({
      id: taskId,
      kind: 'task_started',
      text: `Working on: ${event.payload.task_description}`,
      details: [
        { label: 'task id', value: event.payload.originating_message_id },
        { label: 'status', value: 'in progress' },
      ],
    });
  }
);

// Phase 3: a steering acknowledgement event. Rendered as a plain
// `chat_reply` line so it reads naturally alongside the agent's
// own messages.
listen<{ originating_message_id: string; text: string }>(
  'chat-steering-ack',
  () => {
    pushMessage({
      id: mintId(),
      kind: 'chat_reply',
      text: 'Got it, the agent will adjust.',
    });
  }
);

// Phase 4: typed task completion. The Rust side emits this on the
// `chat-task-completed` topic right after the synthesized
// `chat-reply` lands. We mutate the existing `task_started` card
// in place rather than appending a duplicate — the user already
// sees the synthesized chat reply, so the card just becomes a
// success badge.
listen<{
  originating_message_id: string;
  status: 'Done' | 'Failed';
  step_count: number;
  summary: string;
}>('chat-task-completed', ({ payload }) => {
  const taskId = activeTaskId;
  if (taskId) {
    const task = messages.find((m) => m.id === taskId);
    if (task) {
      task.kind = payload.status === 'Done' ? 'task_completed' : 'task_failed';
      task.stepCount = payload.step_count;
      task.details = task.details ?? [];
      task.details.push({ label: 'result', value: payload.summary });
    }
    const taskEl = chatList.querySelector<HTMLDivElement>(`[data-id="${taskId}"]`);
    if (taskEl) {
      taskEl.classList.remove('task_started');
      taskEl.classList.add(payload.status === 'Done' ? 'task_completed' : 'task_failed');
      taskEl.dataset.kind = payload.status === 'Done' ? 'task_completed' : 'task_failed';
      const body = taskEl.querySelector<HTMLDivElement>('.message-body');
      if (body) body.textContent = payload.summary;
      const meta = taskEl.querySelector<HTMLDivElement>('.message-meta');
      if (meta) {
        const n = payload.step_count;
        meta.textContent = payload.status === 'Done'
          ? `Completed in ${n} step${n === 1 ? '' : 's'}.`
          : 'Did not complete.';
      }
    }
  }
  finishWorkingPill(payload.status === 'Done' ? 'done' : 'failed');
});

// Phase 1.5: minimal Bug #2 fix. The Rust side of `run_browser_task`
// emits the browser agent's final result on `chat-reply`. We push
// that into the chat list as a `chat_reply` and append to history
// so the classifier has context next turn. Accepts the bare-string
// form for backward compatibility with pre-Phase-3 backend builds.
listen<{ originating_message_id: string; text: string } | string>(
  'chat-reply',
  (event) => {
    const text =
      typeof event.payload === 'string'
        ? event.payload
        : event.payload.text;
    pushMessage({
      id: mintId(),
      kind: 'chat_reply',
      text,
    });
    history.push({ role: 'assistant', content: text });
    while (history.length > 20) history.shift();
  }
);

// ---------------------------------------------------------------------------
// Live preview — throttled to one paint per animation frame.
// The right-hand pane stays mounted at all times; we just light
// up the "Live" pill and hide the placeholder once the first
// frame lands, then keep a running frame counter so the user
// can see at a glance that the stream is healthy.
// ---------------------------------------------------------------------------

let pendingFrame: string | null = null;
let frameRequested = false;

listen<string>('agent-screencast-frame', (event) => {
  pendingFrame = event.payload;
  if (!frameRequested) {
    frameRequested = true;
    requestAnimationFrame(() => {
      if (pendingFrame) {
        livePreviewImg.src = 'data:image/jpeg;base64,' + pendingFrame;
        previewFrameCount += 1;
        // First frame: drop the placeholder and light up the pill.
        if (previewPlaceholder && !previewPlaceholder.hasAttribute('hidden')) {
          previewPlaceholder.setAttribute('hidden', '');
        }
        previewStatusPill.textContent = 'Live';
        previewStatusPill.classList.add('is-live');
        previewStatusDot.classList.add('is-live');
        previewMeta.textContent = `${previewFrameCount} frame${previewFrameCount === 1 ? '' : 's'} received`;
      }
      frameRequested = false;
    });
  }
});
