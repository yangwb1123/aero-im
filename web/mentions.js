// mentions.js — composer @-mention autocomplete popover (extracted from app.js).
//
// Owns the mention dropdown state machine: detect a trailing `@query`, fetch +
// cache room members, render the popover, and splice the chosen id back into the
// composer input. Depends only on the shared spine (state / els / avatar helper)
// and the api client.
//
// The composer keydown/input handlers stay in app.js and drive this module via
// its public surface:
//   • maybeShowMentionMenu()  — re-evaluate on input
//   • moveMention(delta)      — arrow-key navigation
//   • pickMention()           — commit the highlighted entry
//   • closeMentionMenu()      — dismiss
//   • isMentionMenuOpen()     — whether the menu is currently open

import { api } from './api.js';
import { state, els, avatarStyleFromId } from './context.js';

const mentionState = { open: false, items: [], index: 0, anchor: 0, pop: null };

export function isMentionMenuOpen() { return mentionState.open; }

export async function maybeShowMentionMenu() {
  const input = els.composerInput;
  const pos = input.selectionStart || input.value.length;
  const before = input.value.slice(0, pos);
  const m = before.match(/(?:^|\s)@([A-Za-z0-9]{0,12})$/);
  if (!m || !state.currentRoomId) { closeMentionMenu(); return; }
  const query = m[1];
  mentionState.anchor = pos - m[0].length;
  let members = state.roomMembers && state.roomMembers.get && state.roomMembers.get(state.currentRoomId);
  if (!members) {
    try {
      members = await api.listRoomMembers(state.currentRoomId);
      state.roomMembers = state.roomMembers || new Map();
      state.roomMembers.set(state.currentRoomId, members);
      for (const p of members) state.participants.set(p.id, p);
    } catch { members = []; }
  }
  const q = query.toLowerCase();
  const filtered = (members || [])
    .filter((p) => (p.display_name || '').toLowerCase().includes(q) || p.id.toLowerCase().includes(q))
    .slice(0, 8);
  if (!filtered.length) { closeMentionMenu(); return; }
  mentionState.items = filtered;
  mentionState.index = 0;
  openMentionMenu();
}

function openMentionMenu() {
  closeMentionMenu();
  const pop = document.createElement('div');
  pop.className = 'mention-pop';
  for (let i = 0; i < mentionState.items.length; i++) {
    const p = mentionState.items[i];
    const b = document.createElement('button');
    b.type = 'button';
    b.className = 'mention-item' + (i === mentionState.index ? ' active' : '');
    const av = document.createElement('span');
    av.className = 'mention-avatar';
    av.setAttribute('style', avatarStyleFromId(p.id));
    av.textContent = (p.display_name || '?')[0];
    const nm = document.createElement('span');
    nm.className = 'mention-name';
    nm.textContent = p.display_name || p.id.slice(0, 8);
    const kn = document.createElement('span');
    kn.className = 'mention-kind';
    kn.textContent = p.kind;
    b.appendChild(av); b.appendChild(nm); b.appendChild(kn);
    b.addEventListener('mousedown', (e) => { e.preventDefault(); mentionState.index = i; pickMention(); });
    pop.appendChild(b);
  }
  document.body.appendChild(pop);
  const r = els.composerInput.getBoundingClientRect();
  pop.style.bottom = (window.innerHeight - r.top + 6) + 'px';
  pop.style.left = (r.left + 18) + 'px';
  mentionState.pop = pop;
  mentionState.open = true;
}

export function closeMentionMenu() {
  if (mentionState.pop && mentionState.pop.parentNode) mentionState.pop.parentNode.removeChild(mentionState.pop);
  mentionState.pop = null;
  mentionState.open = false;
}

export function moveMention(delta) {
  const n = mentionState.items.length;
  if (!n) return;
  mentionState.index = (mentionState.index + delta + n) % n;
  openMentionMenu();
}

export function pickMention() {
  const p = mentionState.items[mentionState.index];
  if (!p) return;
  const input = els.composerInput;
  const cursor = input.selectionStart || input.value.length;
  const before = input.value.slice(0, cursor);
  const after = input.value.slice(cursor);
  const newBefore = before.replace(/@[A-Za-z0-9]{0,12}$/, '@' + p.id + ' ');
  input.value = newBefore + after;
  const pos = newBefore.length;
  input.setSelectionRange(pos, pos);
  els.composerSend.disabled = !input.value.trim();
  closeMentionMenu();
  input.focus();
}
