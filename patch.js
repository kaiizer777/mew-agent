const fs = require('fs');

const mainTsPath = './mew-ui/src/main.ts';
let mainTs = fs.readFileSync(mainTsPath, 'utf8');

const styleCssPath = './mew-ui/src/style.css';
let styleCss = fs.readFileSync(styleCssPath, 'utf8');

// 1. Add types and variables
const typesStr = `type TodoStatus = 'pending' | 'done' | 'skipped' | 'failed' | 'exhausted';

interface TodoRow {
  id: string; // todo_id
  intent: string;
  status: TodoStatus;
  attempts: number;
  evidence?: any;
  rejected_reason?: string;
  is_running?: boolean;
}

const todoStore = new Map<string, TodoRow>(); // key: \`\${task_id}::\${todo_id}\`
const taskTodos = new Map<string, string[]>(); // key: task_id, value: array of todo_ids

`;

mainTs = mainTs.replace("type MessageKind =", typesStr + "type MessageKind =");

// 2. Extend MessageKind
mainTs = mainTs.replace(
  "| 'task_completed'\n  | 'task_failed';",
  "| 'task_completed'\n  | 'task_failed'\n  | 'todo_list'\n  | 'todo_rejected';"
);

// 3. Replace updateTaskMeta
const updateTaskMetaOriginal = `function updateTaskMeta(taskId: string): void {
  const task = messages.find((m) => m.id === taskId);
  if (!task) return;
  task.stepCount = activeTaskSteps;
  const taskEl = chatList.querySelector<HTMLDivElement>(\`[data-id="\${taskId}"]\`);
  if (!taskEl) return;
  const meta = taskEl.querySelector<HTMLDivElement>('.message-meta');
  if (!meta) return;
  if (task.kind === 'task_started') {
    // Prefer the live progress total (more accurate — counts
    // the templated progress lines, not the agent's iteration
    // counter) when present.
    const total = task.liveLineTotal ?? activeTaskSteps;
    meta.textContent = total > 0
      ? \`Agent is working · \${total} step\${total === 1 ? '' : 's'} so far.\`
      : 'Agent is working on this.';
  } else if (task.kind === 'task_completed') {
    const total = task.liveLineTotal ?? activeTaskSteps;
    meta.textContent = \`Completed in \${total} step\${total === 1 ? '' : 's'}.\`;
  }
}`;

const updateTaskMetaNew = `function updateTaskMeta(taskId: string): void {
  const task = messages.find((m) => m.id === taskId);
  if (!task) return;
  task.stepCount = activeTaskSteps;
  const taskEl = chatList.querySelector<HTMLDivElement>(\`[data-id="\${taskId}"]\`);
  if (!taskEl) return;
  const meta = taskEl.querySelector<HTMLDivElement>('.message-meta');
  if (!meta) return;

  const todos = taskTodos.get(taskId) || [];
  const N = todos.length;
  let T = 0;
  for (const tid of todos) {
    const status = todoStore.get(\`\${taskId}::\${tid}\`)?.status;
    if (status && ['done', 'skipped', 'failed', 'exhausted'].includes(status)) {
      T++;
    }
  }

  if (task.kind === 'task_started') {
    if (N > 0) {
      meta.textContent = \`Working · \${T} of \${N} todos\`;
    } else {
      const total = task.liveLineTotal ?? activeTaskSteps;
      meta.textContent = total > 0
        ? \`Agent is working · \${total} step\${total === 1 ? '' : 's'} so far.\`
        : 'Agent is working on this.';
    }
  } else if (task.kind === 'task_completed') {
    if (N > 0) {
      meta.textContent = \`Completed · \${T} of \${N} todos\`;
    } else {
      const total = task.liveLineTotal ?? activeTaskSteps;
      meta.textContent = \`Completed in \${total} step\${total === 1 ? '' : 's'}.\`;
    }
  }
}`;

mainTs = mainTs.replace(updateTaskMetaOriginal, updateTaskMetaNew);

// 4. Update the pill header logic in bumpWorkingPill and startWorkingPill
const bumpWorkingPillOriginal = `function bumpWorkingPill(): void {
  activeTaskSteps += 1;
  if (activeTaskId) {
    updateStatus(\`Working · \${activeTaskSteps} step\${activeTaskSteps === 1 ? '' : 's'}\`);
    updateTaskMeta(activeTaskId);
  }
}`;

const bumpWorkingPillNew = `function bumpWorkingPill(): void {
  activeTaskSteps += 1;
  if (activeTaskId) {
    const todos = taskTodos.get(activeTaskId) || [];
    const N = todos.length;
    if (N > 0) {
      let T = 0;
      for (const tid of todos) {
        const status = todoStore.get(\`\${activeTaskId}::\${tid}\`)?.status;
        if (status && ['done', 'skipped', 'failed', 'exhausted'].includes(status)) {
          T++;
        }
      }
      updateStatus(\`Working · \${T} of \${N} todos\`);
    } else {
      updateStatus(\`Working · \${activeTaskSteps} step\${activeTaskSteps === 1 ? '' : 's'}\`);
    }
    updateTaskMeta(activeTaskId);
  }
}`;

mainTs = mainTs.replace(bumpWorkingPillOriginal, bumpWorkingPillNew);

const startWorkingPillOriginal = `function startWorkingPill(): void {
  activeTaskSteps = 0;
  updateStatus('Working · 0 steps');
}`;

const startWorkingPillNew = `function startWorkingPill(): void {
  activeTaskSteps = 0;
  if (activeTaskId) {
    const todos = taskTodos.get(activeTaskId) || [];
    const N = todos.length;
    if (N > 0) {
      updateStatus(\`Working · 0 of \${N} todos\`);
      return;
    }
  }
  updateStatus('Working · 0 steps');
}`;

mainTs = mainTs.replace(startWorkingPillOriginal, startWorkingPillNew);

// 5. Add updateTodoRow reducer and renderTodoRow
const renderTodoRowLogic = \`
function updateTodoRow(taskId: string, eventTodo: any, rejectedReason?: string) {
  const key = \\\`\\\${taskId}::\\\${eventTodo.id}\\\`;
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
  const todos = (taskTodos.get(taskId) || []).map(id => todoStore.get(\\\`\\\${taskId}::\\\${id}\\\`)!);
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
    const taskEl = chatList.querySelector<HTMLDivElement>(\\\`[data-id="\\\${taskId}"]\\\`);
    if (taskEl) {
      const details = taskEl.querySelector<HTMLDetailsElement>('.message-details');
      if (details) {
        details.open = true;
      }
    }
  }

  const el = document.getElementById(\\\`todo-\\\${key}\\\`);
  if (el) {
    el.setAttribute('data-just-changed', 'true');
    setTimeout(() => el.removeAttribute('data-just-changed'), 240);
  }

  renderTodoListForTask(taskId);
  updateTaskMeta(taskId);
}

function renderTodoListForTask(taskId: string) {
  const taskEl = chatList.querySelector<HTMLDivElement>(\\\`[data-id="\\\${taskId}"]\\\`);
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
    const row = todoStore.get(\\\`\\\${taskId}::\\\${tid}\\\`);
    if (row) {
      listWrap.appendChild(renderTodoRow(taskId, row));
    }
  }
}

function renderTodoRow(taskId: string, row: TodoRow): HTMLElement {
  const el = document.createElement('div');
  el.className = 'todo-row';
  el.id = \\\`todo-\\\${taskId}::\\\${row.id}\\\`;
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
      const taskEl = chatList.querySelector<HTMLDivElement>(\\\`[data-id="\\\${taskId}"]\\\`);
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
\`;

mainTs = mainTs.replace(
  "// ---------------------------------------------------------------------------", 
  renderTodoRowLogic + "\\n// ---------------------------------------------------------------------------"
);


// 6. Listeners for todo-state-changed and todo-rejected
const todoListeners = \`
listen<{ task_id: string; todo: any }>('todo-state-changed', (event) => {
  updateTodoRow(event.payload.task_id, event.payload.todo);
  if (activeTaskId === event.payload.task_id) {
    bumpWorkingPill();
  }
});

listen<{ task_id: string; todo_id: string; reason: string }>('todo-rejected', (event) => {
  updateTodoRow(event.payload.task_id, { id: event.payload.todo_id }, event.payload.reason);
});
\`;

mainTs = mainTs.replace(
  "listen<string>('agent-state', (event) => {",
  todoListeners + "\\nlisten<string>('agent-state', (event) => {"
);


// 7. Add styles to style.css
const todoStyles = \`
/* Todo List UI */
.todo-list-wrap {
  margin-top: 0.75rem;
  display: flex;
  flex-direction: column;
  gap: 0.15rem;
  background: var(--surface-base);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 0.35rem;
}

.todo-row {
  display: flex;
  align-items: center;
  min-height: 28px;
  font-size: 0.82rem;
  padding: 0.2rem 0.5rem;
  border-radius: var(--radius-xs);
  transition: background-color 0.24s ease, color 0.15s ease;
  outline: none;
}

.todo-row:focus {
  background: var(--surface-raised);
  box-shadow: 0 0 0 1px var(--border-strong);
}

.todo-row[data-just-changed="true"] {
  background: var(--surface-raised);
}

.todo-row-status {
  width: 28px;
  flex-shrink: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 1.1rem;
}

.todo-row-intent {
  flex: 1;
  display: flex;
  flex-direction: column;
  justify-content: center;
}

.todo-row[data-status="pending"] {
  color: var(--text-muted);
}
.todo-row[data-running="true"] {
  color: var(--accent);
}
.todo-row[data-status="done"] {
  color: var(--state-done);
}
.todo-row[data-status="failed"], .todo-row[data-status="exhausted"] {
  color: var(--state-failed);
}
.todo-row[data-status="skipped"] {
  color: var(--text-muted);
}

.todo-row-evidence {
  font-size: 0.7rem;
  color: var(--state-done);
  opacity: 0.8;
  margin-top: -0.1rem;
}

.todo-row-reason {
  font-size: 0.7rem;
  color: var(--state-failed);
  opacity: 0.8;
  margin-top: -0.1rem;
}
\`;

styleCss = styleCss + "\\n" + todoStyles;

fs.writeFileSync(mainTsPath, mainTs);
fs.writeFileSync(styleCssPath, styleCss);
console.log("Patched files for phase 15");
