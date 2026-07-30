import { invoke } from '@tauri-apps/api/core';
import './style.css';

document.querySelector<HTMLDivElement>('#app')!.innerHTML = `
  <div id="chat-container">
    <div id="chat-list"></div>
    <form id="chat-form">
      <input id="chat-input" type="text" placeholder="Type a message..." autocomplete="off" />
      <button type="submit">Send</button>
    </form>
  </div>
`;

const chatList = document.querySelector<HTMLDivElement>('#chat-list')!;
const chatForm = document.querySelector<HTMLFormElement>('#chat-form')!;
const chatInput = document.querySelector<HTMLInputElement>('#chat-input')!;

function appendMessage(text: string, sender: 'user' | 'agent') {
  const msgEl = document.createElement('div');
  msgEl.className = \`message \${sender}\`;
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
    const response = await invoke<string>('send_message', { text });
    appendMessage(response, 'agent');
  } catch (error) {
    appendMessage(\`Error: \${error}\`, 'agent');
  }
});
