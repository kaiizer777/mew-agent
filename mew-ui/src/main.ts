import { invoke, Channel } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import './style.css';

document.querySelector<HTMLDivElement>('#app')!.innerHTML = `
  <div id="split-layout">
    <div class="panel" id="chat-container">
      <div class="header">Onyx Chat</div>
      <div id="chat-list">
        <div class="message agent">Welcome to mew-agent. Type a request to begin.</div>
      </div>
      <form id="chat-form">
        <input id="chat-input" type="text" placeholder="Type a message..." autocomplete="off" />
        <button type="submit" id="send-btn">Send</button>
      </form>
      <div class="controls">
        <button id="pause-btn" disabled>Pause</button>
        <button id="resume-btn" disabled>Resume</button>
      </div>
      <div style="margin-top: 1rem; display: flex; justify-content: center;">
        <div id="status-indicator" class="status-indicator status-idle">Idle</div>
      </div>
    </div>
    <div class="panel" id="transcript-container">
      <div class="tabs">
        <button id="tab-transcript" class="tab active">Live Transcript</button>
        <button id="tab-preview" class="tab">Live Preview</button>
      </div>
      <div id="transcript-list"></div>
      <div id="preview-container" style="display: none; flex: 1; align-items: center; justify-content: center; overflow: hidden; background: #000; border-radius: 8px;">
        <img id="live-preview-img" style="max-width: 100%; max-height: 100%; object-fit: contain;" />
      </div>
    </div>
  </div>
`;

const chatList = document.querySelector<HTMLDivElement>('#chat-list')!;
const chatForm = document.querySelector<HTMLFormElement>('#chat-form')!;
const chatInput = document.querySelector<HTMLInputElement>('#chat-input')!;
const transcriptList = document.querySelector<HTMLDivElement>('#transcript-list')!;
const previewContainer = document.querySelector<HTMLDivElement>('#preview-container')!;
const livePreviewImg = document.querySelector<HTMLImageElement>('#live-preview-img')!;
const tabTranscript = document.querySelector<HTMLButtonElement>('#tab-transcript')!;
const tabPreview = document.querySelector<HTMLButtonElement>('#tab-preview')!;
const statusIndicator = document.querySelector<HTMLDivElement>('#status-indicator')!;
const pauseBtn = document.querySelector<HTMLButtonElement>('#pause-btn')!;
const resumeBtn = document.querySelector<HTMLButtonElement>('#resume-btn')!;

tabTranscript.addEventListener('click', () => {
  tabTranscript.classList.add('active');
  tabPreview.classList.remove('active');
  transcriptList.style.display = 'flex';
  previewContainer.style.display = 'none';
});

tabPreview.addEventListener('click', () => {
  tabPreview.classList.add('active');
  tabTranscript.classList.remove('active');
  previewContainer.style.display = 'flex';
  transcriptList.style.display = 'none';
});

interface FrontendMessage {
  role: string;
  content: string;
}

const history: FrontendMessage[] = [];

function appendMessage(text: string, sender: 'user' | 'agent') {
  const msgEl = document.createElement('div');
  msgEl.className = `message ${sender}`;
  msgEl.textContent = text;
  chatList.appendChild(msgEl);
  chatList.scrollTop = chatList.scrollHeight;
}

function appendTranscript(text: string) {
  const msgEl = document.createElement('div');
  msgEl.className = 'transcript-item';
  msgEl.textContent = text;
  transcriptList.appendChild(msgEl);
  transcriptList.scrollTop = transcriptList.scrollHeight;
}

function updateStatus(state: string) {
  statusIndicator.textContent = state;
  statusIndicator.className = 'status-indicator'; // reset
  
  const lowerState = state.toLowerCase();
  if (lowerState.includes('idle')) {
    statusIndicator.classList.add('status-idle');
    chatInput.placeholder = "Type a message...";
    pauseBtn.disabled = true;
    resumeBtn.disabled = true;
  } else if (lowerState.includes('run') || lowerState.includes('start')) {
    statusIndicator.classList.add('status-running');
    chatInput.placeholder = "Type to steer agent...";
    pauseBtn.disabled = false;
    resumeBtn.disabled = true;
  } else if (lowerState.includes('pause')) {
    statusIndicator.classList.add('status-paused');
    chatInput.placeholder = "Agent paused. Type a message...";
    pauseBtn.disabled = true;
    resumeBtn.disabled = false;
  } else if (lowerState.includes('fail') || lowerState.includes('error')) {
    statusIndicator.classList.add('status-failed');
    chatInput.placeholder = "Type a message...";
    pauseBtn.disabled = true;
    resumeBtn.disabled = true;
  } else if (lowerState.includes('done')) {
    statusIndicator.classList.add('status-done');
    chatInput.placeholder = "Type a message...";
    pauseBtn.disabled = true;
    resumeBtn.disabled = true;
  } else {
    // Default / classifying
    statusIndicator.classList.add('status-idle');
  }
}

pauseBtn.addEventListener('click', async () => {
  try {
    const res = await invoke<string>('pause_session');
    appendTranscript(`[System] ${res}`);
  } catch (err) {
    appendTranscript(`[System Error] ${err}`);
  }
});

resumeBtn.addEventListener('click', async () => {
  try {
    const res = await invoke<string>('resume_session');
    appendTranscript(`[System] ${res}`);
  } catch (err) {
    appendTranscript(`[System Error] ${err}`);
  }
});

const onEvent = new Channel<any>();
onEvent.onmessage = (event) => {
  if (event.type === 'State') {
    const data = event.data;
    appendTranscript(`[${data.timestamp_secs}] STATE: ${data.from} -> ${data.to} (${data.kind})`);
    updateStatus(data.to);
  } else if (event.type === 'Tool') {
    const data = event.data;
    appendTranscript(`[${data.timestamp_secs}] TOOL: ${data.name}\nArgs: ${data.args}\nResult: ${data.result}`);
  } else if (event.type === 'Summary') {
    const data = event.data;
    appendTranscript(`[${data.timestamp_secs}] SUMMARY:\n${data.text}`);
  }
};

chatForm.addEventListener('submit', async (e) => {
  e.preventDefault();
  const text = chatInput.value.trim();
  if (!text) return;

  appendMessage(text, 'user');
  chatInput.value = '';

  try {
    const response = await invoke<string>('send_message', { text, history, onEvent });
    appendMessage(response, 'agent');
    
    history.push({ role: 'user', content: text });
    history.push({ role: 'assistant', content: response });
    
    if (history.length > 20) {
      history.splice(0, history.length - 20);
    }
  } catch (error) {
    appendMessage(`Error: ${error}`, 'agent');
    updateStatus('Failed');
  }
});

listen<string>('agent-state', (event) => {
  updateStatus(event.payload);
});

listen<string>('agent-screencast-frame', (event) => {
  livePreviewImg.src = "data:image/jpeg;base64," + event.payload;
});
