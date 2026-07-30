import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import './style.css';

document.querySelector<HTMLDivElement>('#app')!.innerHTML = `
  <div id="chat-container">
    <div id="chat-list"></div>
    <form id="chat-form">
      <input id="chat-input" type="text" placeholder="Type a message..." autocomplete="off" />
      <button type="submit">Send</button>
    </form>
    <div id="status-indicator" style="padding: 8px; font-size: 12px; color: #666; text-align: center;">Status: Idle</div>
  </div>
`;

const chatList = document.querySelector<HTMLDivElement>('#chat-list')!;
const chatForm = document.querySelector<HTMLFormElement>('#chat-form')!;
const chatInput = document.querySelector<HTMLInputElement>('#chat-input')!;

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

chatForm.addEventListener('submit', async (e) => {
  e.preventDefault();
  const text = chatInput.value.trim();
  if (!text) return;

  appendMessage(text, 'user');
  chatInput.value = '';

  try {
    const response = await invoke<string>('send_message', { text, history });
    appendMessage(response, 'agent');
    
    // Add to history
    history.push({ role: 'user', content: text });
    history.push({ role: 'assistant', content: response });
    
    // Keep history bounded (e.g. last 10 turns = 20 messages)
    if (history.length > 20) {
      history.splice(0, history.length - 20);
    }
  } catch (error) {
    appendMessage(`Error: ${error}`, 'agent');
  }
});

listen<string>('agent-state', (event) => {
  const statusEl = document.querySelector<HTMLDivElement>('#status-indicator')!;
  statusEl.textContent = `Status: ${event.payload}`;
});
