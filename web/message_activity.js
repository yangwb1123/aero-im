// Per-message receipts and block-interaction realtime UI.
//
// One IntersectionObserver marks only messages that actually enter the viewport,
// rather than issuing one request for every row in a large history page. The
// endpoint is idempotent; this client also keeps a session-local attempt set.

import { api } from './api.js';
import { state, ws, els, cssEscape } from './context.js';
import { mergeInteraction, mergeSeenReader, participantLabel } from './message_activity_model.js';

const readersByMessage = new Map();
const interactionsByMessage = new Map();
const attempted = new Set();
let installed = false;
let visibilityObserver = null;

function activitySlot(messageId) {
  const node = els.msgList.querySelector(`[data-msg-id="${cssEscape(messageId)}"]`);
  if (!node) return null;
  let slot = node.querySelector('.msg-activity');
  if (!slot) {
    slot = document.createElement('div');
    slot.className = 'msg-activity';
    node.querySelector('.msg-body')?.appendChild(slot);
  }
  return slot;
}

function paint(messageId) {
  const slot = activitySlot(messageId);
  if (!slot) return;
  slot.replaceChildren();
  const readers = readersByMessage.get(messageId) || [];
  if (readers.length) {
    const names = readers.map((id) => participantLabel(state.participants, id));
    const seen = document.createElement('span');
    seen.className = 'msg-seen';
    seen.textContent = `已读 ${readers.length}`;
    seen.title = names.join('、');
    slot.appendChild(seen);
  }
  const interactions = interactionsByMessage.get(messageId) || [];
  for (const item of interactions.slice(-3)) {
    const chip = document.createElement('span');
    chip.className = 'msg-interaction';
    chip.textContent = `${participantLabel(state.participants, item.participant)} · ${item.actionId}`;
    slot.appendChild(chip);
  }
}

function handleSeen(frame) {
  if (!frame?.message_id || !frame.participant) return;
  readersByMessage.set(
    frame.message_id,
    mergeSeenReader(readersByMessage.get(frame.message_id), frame.participant),
  );
  paint(frame.message_id);
}

function handleInteraction(frame) {
  if (!frame?.message_id) return;
  interactionsByMessage.set(
    frame.message_id,
    mergeInteraction(interactionsByMessage.get(frame.message_id), frame),
  );
  paint(frame.message_id);
}

async function markVisible(node) {
  const messageId = node?.dataset?.msgId;
  if (!messageId || node.classList.contains('pending') || attempted.has(messageId)) return;
  if (node.dataset.senderId === state.me?.id) return;
  attempted.add(messageId);
  try {
    await api.markMessageSeen(messageId);
    handleSeen({ message_id: messageId, participant: state.me?.id });
    const response = await api.listMessageSeen(messageId);
    let readers = readersByMessage.get(messageId) || [];
    for (const item of response?.readers || []) {
      readers = mergeSeenReader(readers, item.participant_id);
    }
    readersByMessage.set(messageId, readers);
    paint(messageId);
  } catch {
    // A transient disconnect should not permanently suppress a later retry.
    attempted.delete(messageId);
    visibilityObserver?.observe(node);
  }
}

function observeNode(node) {
  if (!(node instanceof Element) || !node.matches('.msg[data-msg-id]')) return;
  paint(node.dataset.msgId);
  if (visibilityObserver) visibilityObserver.observe(node);
  else queueMicrotask(() => markVisible(node));
}

export function initMessageActivity() {
  if (installed) return;
  installed = true;
  ws.on('msg:message_seen', handleSeen);
  ws.on('msg:interaction', handleInteraction);

  if ('IntersectionObserver' in window) {
    visibilityObserver = new IntersectionObserver((entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting || entry.intersectionRatio < 0.5) continue;
        visibilityObserver.unobserve(entry.target);
        markVisible(entry.target);
      }
    }, { root: els.msgScroll, threshold: 0.5 });
  }

  for (const node of els.msgList.querySelectorAll('.msg[data-msg-id]')) observeNode(node);
  new MutationObserver((records) => {
    for (const record of records) {
      for (const node of record.addedNodes) {
        if (!(node instanceof Element)) continue;
        if (node.matches('.msg[data-msg-id]')) observeNode(node);
        for (const child of node.querySelectorAll?.('.msg[data-msg-id]') || []) observeNode(child);
      }
    }
  }).observe(els.msgList, { childList: true, subtree: true });
}
