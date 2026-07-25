// smart_replies.js — AI-suggested reply buttons.
//
// After a new message arrives, the backend's `/api/rooms/:id/suggest-replies`
// endpoint generates 3 short reply options grounded in the conversation.
// This module fetches them and renders clickable chips below the message.
//
// Self-contained: loaded as its own <script type="module">, auto-initialised.
// Shares the singleton `ws` / `state` / `api` with the other modules.

import { api, ApiError } from './api.js';
import { state, ws } from './context.js';

let activeSuggestions = null; // { container, messageId, buttons[] }

/// Fetch suggestions and render them below `messageEl`.
export async function showSuggestions(roomId, messageId, messageEl) {
  // Clear any previous suggestions
  hideSuggestions();

  if (!roomId || !messageEl) return;

  try {
    const res = await api(`/api/rooms/${roomId}/suggest-replies`, {
      method: 'POST',
      body: JSON.stringify({ k: 10 }),
    });

    if (!res || !res.suggestions) return;

    // Parse markdown numbered list: "1. First\n2. Second\n3. Third"
    const lines = res.suggestions
      .split('\n')
      .map(l => l.replace(/^\d+\.\s*/, '').trim())
      .filter(l => l.length > 0);

    if (lines.length === 0) return;

    // Create suggestion container
    const container = document.createElement('div');
    container.className = 'smart-replies';
    container.style.cssText = 'display:flex;gap:6px;flex-wrap:wrap;margin-top:6px;';

    const buttons = [];
    for (const text of lines.slice(0, 3)) {
      const btn = document.createElement('button');
      btn.className = 'smart-reply-btn';
      btn.textContent = text.length > 60 ? text.slice(0, 57) + '…' : text;
      btn.style.cssText =
        'padding:4px 10px;border:1px solid #ccc;border-radius:12px;' +
        'background:#fff;cursor:pointer;font-size:12px;color:#333;' +
        'transition:background .15s;';
      btn.addEventListener('mouseenter', () => { btn.style.background = '#e8f4fd'; });
      btn.addEventListener('mouseleave', () => { btn.style.background = '#fff'; });
      btn.addEventListener('click', () => {
        // Send the selected reply as a chat message
        const roomId = state.currentRoom;
        if (roomId && ws.sendMessage) {
          ws.sendMessage(roomId, [{ type: 'text', text }]);
        }
        hideSuggestions();
      });
      container.appendChild(btn);
      buttons.push(btn);
    }

    messageEl.appendChild(container);
    activeSuggestions = { container, messageId, buttons };
  } catch (e) {
    // API not available / AI not configured — silently ignore
    if (!(e instanceof ApiError && e.status === 502)) {
      console.warn('smart_replies: fetch failed', e);
    }
  }
}

/// Remove the suggestion buttons.
export function hideSuggestions() {
  if (activeSuggestions) {
    if (activeSuggestions.container.parentNode) {
      activeSuggestions.container.parentNode.removeChild(activeSuggestions.container);
    }
    activeSuggestions = null;
  }
}

/// Call this from app.js when a new message is rendered.
/// `messageEl` is the DOM element for the message bubble.
export function onNewMessage(roomId, messageId, messageEl) {
  // Only show suggestions for messages from other people
  if (messageEl && state?.me?.id) {
    showSuggestions(roomId, messageId, messageEl);
  }
}
