// app.js — Aero IM debug client entry point (P2 features wired).
// Auth, rooms, messages, optimistic rendering, typing, read, reactions,
// attachments, search drawer, AI drawer, 1:1 calls, live streaming.
import { api, auth, ApiError } from './api.js';
import { initSmartReplies, onNewMessage } from './smart_replies.js';
import {
  renderMessage,
  renderRoomItem,
  renderOnlineItem,
  renderReactionsInto,
  renderTypingInto,
  initialOf,
  toast,
} from './render.js';
// Shared spine (state / ws / els / DOM helpers / leaf utils) lives in context.js.
import { state, ws, els, cssEscape, scrollToMessage, avatarStyleFromId, hasPendingForRoom } from './context.js';
// Extracted domain modules (see each file header for its public surface).
import { handleCall, wireCallControls } from './calls.js';
import { openEmojiPicker } from './emoji.js';
import { initSearchAi, restoreAiHistory } from './search.js';
import { initNotifications, bumpNotifBadge, refreshNotifBadge, loadNotifications } from './notifications.js';
import { initLive } from './live.js';
import { initMedia } from './media.js';
import { liveHooks, submitBlockInteraction, clearLiveCards, handleStreamEvent } from './livecards.js';
import { maybeShowMentionMenu, moveMention, pickMention, closeMentionMenu, isMentionMenuOpen } from './mentions.js';
import { initModalForms } from './modals.js';
import { initAuthUi } from './auth_ui.js';
import { initChrome } from './chrome.js';
import { installUnhandledRejectionReporting } from './error_reporting.js';
import { recallErrorToast } from './recall_errors.js';
import { syncRoomSidebar } from './room_sync.js';
import { initMessageActivity } from './message_activity.js';
import { clearPendingDelivery, findPendingMatch, initReliableDelivery, pendingTempId, sendOptimistically } from './delivery.js';
import { refreshWsAccessToken } from './ws_auth.js';
import { draftComposerCleared, draftRoomSwitched, initDrafts, resetDrafts } from './drafts.js';
let wsHooksInstalled = false;
// ---------- view switching ----------
function showAuth() { els.viewAuth.hidden = false; els.viewChat.hidden = true; }
function showChat() { els.viewAuth.hidden = true; els.viewChat.hidden = false; }
for (const t of els.tabs) {
  t.addEventListener('click', () => {
    const name = t.dataset.tab;
    for (const x of els.tabs) x.classList.toggle('active', x === t);
    for (const p of els.tabPanels) p.hidden = p.dataset.tabPanel !== name;
  });
}
document.addEventListener('visibilitychange', () => { if (!document.hidden && state.currentRoomId) clearUnread(state.currentRoomId); });
function enterChat() {
  showChat();
  const me = state.me;
  els.meName.textContent = me.display_name || '—';
  els.meEmail.textContent = me.email || me.id || '';
  els.meAvatar.textContent = initialOf(me.display_name || me.email);
  els.meAvatar.setAttribute('style', avatarStyleFromId(me.id));
  hookWs();
  ws.connect(auth.getToken(), me.id, {
    refreshAccessToken: () => refreshWsAccessToken(api, auth, state),
  });
  syncRoomSidebar(forceReauth, () => { refreshRoomList(); updateTitleBadge(); });
  api.rtcConfig().then((c) => { state.rtcConfig = c; }).catch(() => {});
  // Load the gift catalog once so stream cards can render the gift bar; re-render if a room is open.
  api.liveGifts().then((r) => {
    state.giftCatalog = r?.gifts || [];
    if (state.currentRoomId) rerenderCurrentRoom();
  }).catch(() => {});
  // Ask for notification permission once (silent if denied).
  if ('Notification' in window && Notification.permission === 'default') {
    Notification.requestPermission().catch(() => {});
  }
  // Seed the bell badge from the persisted unread-notification count.
  refreshNotifBadge();
}

// ---------- ws ----------
function hookWs() {
  if (wsHooksInstalled) return;
  wsHooksInstalled = true;
  ws.on('auth_expired', forceReauth);
  ws.on('status', (s) => {
    els.wsDot.classList.remove('ws-up', 'ws-down', 'ws-wait');
    if (s === 'up') els.wsDot.classList.add('ws-up');
    else if (s === 'wait' || s === 'connecting') els.wsDot.classList.add('ws-wait');
    else els.wsDot.classList.add('ws-down');
    els.wsDot.title = `WS: ${s}`;
  });
  ws.on('open', () => {
    if (!state.currentRoomId) return;
    ws.joinRoom(state.currentRoomId);
    // Re-watch live streams that survived the disconnect (ROADMAP 方向一): the
    // server drops a participant's stream subscriptions when the socket closes,
    // so without this danmaku/gifts/viewer-count die silently after any blip.
    for (const sid of state.watchedStreams) ws.watchStream(sid);
    // Replay edits/deletes that happened while we were disconnected.
    replayChanges(state.currentRoomId);
  });
  ws.on('msg:message', (f) => handleIncomingMessage(f.message, f.client_message_id));
  ws.on('msg:edited', (f) => handleEdited(f.message));
  ws.on('msg:recalled', (f) => handleRecalled(f.message));
  ws.on('msg:deleted', (f) => handleDeleted(f));
  ws.on('msg:reaction', (f) => handleReaction(f));
  ws.on('msg:read', (f) => handleReadReceipt(f));
  ws.on('msg:typing', (f) => handleTyping(f));
  ws.on('msg:notify', (f) => handleNotify(f));
  ws.on('msg:pin', (f) => handlePin(f));
  ws.on('msg:presence', (f) => handlePresence(f));
  ws.on('msg:membership', (f) => handleMembership(f));
  ws.on('msg:call', (f) => handleCall(f.event));
  ws.on('msg:stream_event', (f) => handleStreamEvent(f.event));
  ws.on('msg:backfill', (f) => handleBackfillTruncated(f));
  ws.on('msg:resync', () => handleResync());
  ws.on('msg:error', (f) => toast(f.code === 'rate_limited' ? '操作太频繁,请稍后重试' : `服务端:${f.msg || f.code || 'error'}`, 'error'));
  ws.on('msg:pong', () => {});
  initMessageActivity();
  initReliableDelivery({
    ws,
    getPendingMap: () => state.pendingByTempId,
    onCanonical: handleIncomingMessage,
    onRestore: restorePendingDelivery,
    onPendingChanged: rerenderCurrentRoom,
  });
}

const CATCHUP_PAGE_SIZE = 100;

async function handleBackfillTruncated(f) {
  if (!f || !f.truncated || !f.next_since) return;
  const resumeAcks = ws.pauseDeliveryAcks();
  let complete = false;
  try {
    if (f.room_id) complete = await pullRoomSince(f.room_id, f.next_since);
    else if (f.stream_id) complete = await pullStreamChatSince(f.stream_id, f.next_since);
  } finally {
    resumeAcks(complete);
  }
}

async function pullRoomSince(roomId, since) {
  let cursor = since;
  while (cursor) {
    let list;
    try { list = await api.listMessages(roomId, { since: cursor, limit: CATCHUP_PAGE_SIZE }); }
    catch { return false; }
    const arr = Array.isArray(list) ? list : [];
    for (const m of arr) handleIncomingMessage(m);
    if (arr.length < CATCHUP_PAGE_SIZE) return true;
    const next = arr.reduce((mx, m) => (m?.id && m.id > mx ? m.id : mx), cursor);
    if (next <= cursor) return false;
    cursor = next;
  }
  return true;
}

async function pullStreamChatSince(streamId, since) {
  const ctrl = state.liveCards.get(streamId);
  if (!ctrl) return true;
  let cursor = since;
  const pageSize = 200;
  while (cursor) {
    let r;
    try { r = await api.streamChatList(streamId, pageSize, cursor); }
    catch { return false; }
    const lines = Array.isArray(r?.chat) ? r.chat : [];
    for (const line of lines) ctrl.addChat(line);
    if (lines.length < pageSize) return true;
    const next = lines.reduce((mx, l) => (l?.id && l.id > mx ? l.id : mx), cursor);
    if (next <= cursor) return false;
    cursor = next;
  }
  return true;
}

async function handleResync() {
  if (state.resyncInFlight) return;
  // Never guess a v2 prefix with MAX-ULID; reconnect into full ordinal replay.
  if (ws.supports('delivery_cursor_v2')) { ws.pauseDeliveryAcks()(false); return; }
  state.resyncInFlight = true;
  const resumeAcks = ws.pauseDeliveryAcks();
  let complete = true;
  try {
    for (const [roomId, arr] of state.messagesByRoom) {
      const last = arr.length ? arr[arr.length - 1]?.id : null;
      if (last && !await pullRoomSince(roomId, last)) {
        complete = false;
        break;
      }
    }
  } finally {
    state.resyncInFlight = false;
    resumeAcks(complete);
  }
}

function handleIncomingMessage(m, clientMessageId = null) {
  if (!m?.id || !m?.room_id) return;
  const isMine = m.sender_id === state.me?.id;
  if (isMine) {
    const key = clientMessageId
      ? pendingTempId(clientMessageId)
      : findPendingMatch(m, state.pendingByTempId, state.me?.id);
    const pending = key ? state.pendingByTempId.get(key) : null;
    if (pending) {
      clearPendingDelivery(pending);
      replaceNodeForMsg(key, m);
      const arr = state.messagesByRoom.get(m.room_id) || [];
      const idx = arr.findIndex((x) => x.id === key);
      if (idx >= 0) arr[idx] = m; else arr.push(m);
      state.messagesByRoom.set(m.room_id, arr);
      state.pendingByTempId.delete(key);
      hideEmptyIfNeeded();
      maybeMarkRead(m);
      return;
    }
  }
  const arr = state.messagesByRoom.get(m.room_id) || [];
  const held = arr.find((x) => x.id === m.id);
  if (held) {
    // Held-message mutation row (reconnect backfill) → converge via the same
    // guarded funnel; identical redeliveries are dropped.
    if (m.deleted_at || m.recalled_at || m.edited_at) applyChange(m);
    return;
  }
  arr.push(m);
  state.messagesByRoom.set(m.room_id, arr);
  const isCurrent = m.room_id === state.currentRoomId;
  const tabFocused = !document.hidden;
  if (isCurrent && tabFocused) {
    maybeMarkRead(m);
    // Show smart reply suggestions for other people's messages
    if (!isMine && els.msgList.lastElementChild) {
      onNewMessage(m.room_id, m.id, els.msgList.lastElementChild);
    }
  } else {
    bumpUnread(m.room_id);
    if (!isMine) notify(m);
  }
  if (isCurrent) {
    appendMessageEl(renderMsgWithReactions(m), { scroll: true });
    hideEmptyIfNeeded();
  }
}

function bumpUnread(roomId) {
  const cur = state.unreadByRoom.get(roomId) || 0;
  state.unreadByRoom.set(roomId, cur + 1);
  refreshRoomList();
  updateTitleBadge();
}

function clearUnread(roomId) {
  if (state.unreadByRoom.get(roomId)) {
    state.unreadByRoom.delete(roomId);
    refreshRoomList();
    updateTitleBadge();
  }
}

function updateTitleBadge() {
  let total = 0;
  for (const n of state.unreadByRoom.values()) total += n;
  document.title = (total > 0 ? `(${total}) ` : '') + 'Aero IM';
}

function notify(m) {
  if (!('Notification' in window) || Notification.permission !== 'granted') return;
  const sender = state.participants.get(m.sender_id);
  const title = (sender?.display_name || 'New message') + ' · Aero IM';
  const body = (m.blocks || []).map((b) => b?.content || '').join(' ').slice(0, 80) || '[attachment]';
  const n = new Notification(title, { body, tag: m.id });
  n.onclick = () => {
    window.focus();
    if (m.room_id !== state.currentRoomId) switchRoom(m.room_id);
    n.close();
  };
}

// Wave 1: someone @-mentioned or replied to me (targeted realtime frame).
function handleNotify(f) {
  if (!f || f.mentioned !== state.me?.id) return;
  const sender = state.participants.get(f.by);
  const who = sender?.display_name || '有人';
  const verb = f.notify_kind === 'reply' ? '回复了你' : '提到了你';
  const room = state.rooms.get(f.room_id);
  const where = room?.name ? `「${room.name}」` : '';
  toast(`${who} 在${where}${verb}`, 'info');
  // A new durable inbox entry just landed → bump the bell badge and re-pull if open.
  bumpNotifBadge();
  if (els.drawerNotif && !els.drawerNotif.hidden) loadNotifications();
  if ('Notification' in window && Notification.permission === 'granted') {
    const n = new Notification(`${who} ${verb} · Aero IM`, { tag: f.message_id });
    n.onclick = () => {
      window.focus();
      if (f.room_id !== state.currentRoomId) switchRoom(f.room_id);
      n.close();
    };
  }
}

// Wave 1: a message was pinned/unpinned in a room (fans out to all members).
function handlePin(f) {
  if (!f || f.room_id !== state.currentRoomId) return;
  toast(f.op === 'pin' ? '一条消息被置顶' : '取消了一条置顶', 'info');
}

function handleEdited(m) { applyMessageMutation(m, m.edited_at); }
function handleRecalled(m) { applyMessageMutation(m, m.recalled_at); }

/// Out-of-order + resurrect-guarded replacement for Edited/Recalled events
/// (mutation timestamp = order key; stale-after-Delete never resurrects).
function applyMessageMutation(m, stamp) {
  if (!m?.id || !m?.room_id) return;
  const ts = Date.parse(stamp || '') || 0;
  const prev = state.lastEditAt.get(m.id) || 0;
  if (ts && prev && ts < prev) return;
  if (ts) state.lastEditAt.set(m.id, ts);
  const arr = state.messagesByRoom.get(m.room_id) || [];
  const idx = arr.findIndex((x) => x.id === m.id);
  if (idx >= 0) {
    if (arr[idx].deleted_at) return; // resurrect guard: never un-tombstone
    arr[idx] = m;
  }
  state.messagesByRoom.set(m.room_id, arr);
  if (m.room_id === state.currentRoomId) replaceNodeForMsg(m.id, m);
}

/// Change-replay: tombstone → remove, recall → placeholder, else edit.
function applyChange(m) {
  if (!m?.id || !m?.room_id) return;
  if (m.deleted_at) handleDeleted({ room_id: m.room_id, message_id: m.id });
  else if (m.recalled_at) handleRecalled(m);
  else handleEdited(m);
}

/// On reconnect, replay edits/deletes/recalls that landed while we were
/// offline (backfill only returns NEW messages). Best-effort.
async function replayChanges(roomId) {
  const since = state.lastChangeSync.get(roomId);
  if (!since) return;
  // Advance the cursor first so a slow reply can't widen the next window.
  state.lastChangeSync.set(roomId, new Date().toISOString());
  try {
    const changes = await api.roomChanges(roomId, since, { limit: 200 });
    for (const m of changes || []) applyChange(m);
  } catch { /* a failed replay just leaves the view as-is until next reconnect */ }
}

function handleDeleted(f) {
  const { room_id, message_id } = f;
  const arr = state.messagesByRoom.get(room_id) || [];
  const idx = arr.findIndex((x) => x.id === message_id);
  if (idx >= 0) arr[idx] = { ...arr[idx], deleted_at: new Date().toISOString(), blocks: [] };
  state.messagesByRoom.set(room_id, arr);
  if (room_id === state.currentRoomId) {
    const node = els.msgList.querySelector(`[data-msg-id="${cssEscape(message_id)}"]`);
    if (node && idx >= 0) {
      const fresh = renderMsgWithReactions(arr[idx]);
      node.parentNode.replaceChild(fresh, node);
    }
  }
}

function handleReaction(f) {
  const { message_id, participant, emoji, op } = f;
  const list = state.reactionsByMsg.get(message_id) || [];
  let entry = list.find((s) => s.emoji === emoji);
  if (op === 'add') {
    if (!entry) {
      entry = { emoji, count: 0, participants: [] };
      list.push(entry);
    }
    if (!entry.participants.includes(participant)) {
      entry.participants.push(participant);
      entry.count++;
    }
  } else {
    if (entry) {
      entry.participants = entry.participants.filter((p) => p !== participant);
      entry.count = Math.max(0, entry.count - 1);
      if (entry.count === 0) {
        const i = list.indexOf(entry);
        if (i >= 0) list.splice(i, 1);
      }
    }
  }
  state.reactionsByMsg.set(message_id, list);
  refreshReactionsFor(message_id);
}

function handleReadReceipt(f) {
  const { room_id, participant, last_message_id, at } = f;
  const map = state.receiptsByRoom.get(room_id) || new Map();
  map.set(participant, { last_read_message_id: last_message_id, updated_at: at });
  state.receiptsByRoom.set(room_id, map);
  if (room_id === state.currentRoomId) refreshReadStrips();
  // Multi-device read convergence (ROADMAP v3 方向一): when *my own* read
  // receipt arrives (sent from another device — or this one), the room is read
  // up to `last_message_id`, so recompute its unread badge here too. Without
  // this, reading a room on device A leaves device B's badge stuck until reload.
  if (participant === state.me?.id) recomputeUnread(room_id, last_message_id);
}

// Recompute a room's unread badge from the read cursor: only cached messages
// strictly newer than `lastReadId` (and not my own) still count. ULIDs sort
// lexicographically, so string compare is chronological.
function recomputeUnread(roomId, lastReadId) {
  const arr = state.messagesByRoom.get(roomId) || [];
  const n = arr.filter((m) => m?.id && m.id > lastReadId && m.sender_id !== state.me?.id).length;
  if (n > 0) state.unreadByRoom.set(roomId, n);
  else state.unreadByRoom.delete(roomId);
  refreshRoomList();
  updateTitleBadge();
}

// Render a tiny avatar strip under each of my messages showing who's read up to it.
function refreshReadStrips() {
  const roomId = state.currentRoomId;
  if (!roomId || !state.me) return;
  const myPid = state.me.id;
  const recmap = state.receiptsByRoom.get(roomId) || new Map();
  // For each message of mine, compute set of other participants whose
  // last_read_message_id >= this message id.
  const arr = state.messagesByRoom.get(roomId) || [];
  for (const m of arr) {
    if (m.sender_id !== myPid) continue;
    const node = els.msgList.querySelector('[data-msg-id="' + cssEscape(m.id) + '"]');
    if (!node) continue;
    let strip = node.querySelector('.read-strip');
    if (!strip) {
      strip = document.createElement('div');
      strip.className = 'read-strip';
      node.querySelector('.msg-body').appendChild(strip);
    }
    strip.replaceChildren();
    const readers = [];
    for (const [pid, r] of recmap.entries()) {
      if (pid === myPid) continue;
      if (r.last_read_message_id >= m.id) readers.push(pid);
    }
    for (const pid of readers.slice(0, 5)) {
      const p = state.participants.get(pid);
      const a = document.createElement('span');
      a.className = 'read-avatar';
      a.setAttribute('style', avatarStyleFromId(pid));
      a.textContent = (p?.display_name || '?')[0];
      a.title = p?.display_name || pid.slice(0, 6);
      strip.appendChild(a);
    }
  }
}

function handleTyping(f) {
  const { room_id, participant, on } = f;
  if (participant === state.me?.id) return;
  const map = state.typing.get(room_id) || new Map();
  if (on) map.set(participant, Date.now()); else map.delete(participant);
  state.typing.set(room_id, map);
  if (room_id === state.currentRoomId) renderTypingBar();
  // Auto-expire stale typing flags after 6s.
  setTimeout(() => {
    const cur = state.typing.get(room_id);
    if (!cur) return;
    for (const [pid, ts] of cur.entries()) if (Date.now() - ts > 6000) cur.delete(pid);
    state.typing.set(room_id, cur);
    if (room_id === state.currentRoomId) renderTypingBar();
  }, 6500);
}

function renderTypingBar() {
  const map = state.typing.get(state.currentRoomId);
  const names = [];
  if (map) {
    for (const pid of map.keys()) {
      const p = state.participants.get(pid);
      names.push(p?.display_name || pid.slice(0, 6));
    }
  }
  renderTypingInto(els.typingBar, names);
}

function handlePresence(frame) {
  if (frame.room_id !== state.currentRoomId) return;
  const ids = Array.isArray(frame.online) ? frame.online : [];
  els.onlineList.replaceChildren();
  for (const pid of ids) els.onlineList.appendChild(renderOnlineItem(pid, state.participants.get(pid)));
  els.onlineCount.textContent = String(ids.length);
}

// Member joined/left the current channel (`RoomEvent::Membership`, fanned to all
// members). Presence isn't re-broadcast on a membership change, so re-join to pull a
// fresh Presence frame (idempotent; the WS join never re-fires Membership → no loop).
function handleMembership(frame) {
  if (frame.room_id !== state.currentRoomId) return;
  ws.joinRoom(state.currentRoomId);
}

// ---------- room list / switch ----------
function refreshRoomList() {
  els.roomList.replaceChildren();
  const list = Array.from(state.rooms.values());
  list.sort((a, b) => (a.name || a.id).localeCompare(b.name || b.id));
  if (!list.length) {
    const hint = document.createElement('div');
    hint.className = 'muted';
    hint.style.cssText = 'padding:12px 14px;font-size:12px;';
    hint.textContent = '还没有房间。点 "+ 新建" 创建一个。';
    els.roomList.appendChild(hint);
    return;
  }
  for (const r of list) {
    const unread = state.unreadByRoom.get(r.id) || 0;
    const node = renderRoomItem(r, { active: r.id === state.currentRoomId, unread });
    node.addEventListener('click', () => switchRoom(r.id));
    els.roomList.appendChild(node);
  }
}

function setActiveRoomVisual() {
  for (const el of els.roomList.querySelectorAll('.room-item'))
    el.classList.toggle('active', el.dataset.roomId === state.currentRoomId);
}

async function switchRoom(roomId) {
  if (state.currentRoomId === roomId) return;
  clearLiveCards(); // unwatch streams from the room we're leaving
  state.currentRoomId = roomId;
  clearUnread(roomId);
  // Seed the change-replay cursor: on the next reconnect we replay edits/deletes
  // that landed after this moment (ROADMAP 方向一).
  state.lastChangeSync.set(roomId, new Date().toISOString());
  setActiveRoomVisual();
  const room = state.rooms.get(roomId);
  els.roomName.textContent = room?.name || `Room ${roomId.slice(0, 6)}…`;
  els.roomMeta.textContent = `${room?.kind || 'room'} · ${roomId}`;
  els.btnAddMember.hidden = false;
  els.composer.hidden = false;
  els.composerSend.disabled = !els.composerInput.value.trim();
  ws.joinRoom(roomId);
  els.onlineList.replaceChildren();
  els.onlineCount.textContent = '0';
  els.msgList.replaceChildren();
  els.msgEmpty.hidden = true;
  renderTypingBar();
  if (!state.messagesByRoom.has(roomId)) await loadHistory(roomId, { initial: true });
  else rerenderCurrentRoom();
  // Fetch latest receipts (read state for others) lazily.
  api.listReceipts(roomId).then((rs) => {
    const map = new Map();
    for (const r of rs || []) map.set(r.participant_id, r);
    state.receiptsByRoom.set(roomId, map);
    refreshReadStrips();
  }).catch(() => {});
  // Restore AI conversation history from sessionStorage for this room.
  restoreAiHistory(roomId); draftRoomSwitched(roomId);
}

async function loadHistory(roomId, { initial = false } = {}) {
  if (state.loadingHistory) return;
  if (state.reachedTop.has(roomId) && !initial) return;
  state.loadingHistory = true;
  try {
    const current = state.messagesByRoom.get(roomId) || [];
    const before = !initial && current.length ? current[0].id : undefined;
    const list = await api.listMessages(roomId, { before, limit: 100 });
    const arr = Array.isArray(list) ? list.slice() : [];
    if (arr.length >= 2) {
      const t0 = Date.parse(arr[0].created_at || '') || 0;
      const t1 = Date.parse(arr[arr.length - 1].created_at || '') || 0;
      if (t0 > t1) arr.reverse();
    }
    if (arr.length < 100) state.reachedTop.add(roomId);

    if (initial) {
      state.messagesByRoom.set(roomId, arr);
      if (roomId === state.currentRoomId) { rerenderCurrentRoom(); scrollToBottom(); }
    } else {
      const merged = arr.concat(current);
      const seen = new Set(); const dedup = [];
      for (const m of merged) { if (!m?.id || seen.has(m.id)) continue; seen.add(m.id); dedup.push(m); }
      state.messagesByRoom.set(roomId, dedup);
      if (roomId === state.currentRoomId) {
        const ph = els.msgScroll.scrollHeight; const pt = els.msgScroll.scrollTop;
        rerenderCurrentRoom();
        const nh = els.msgScroll.scrollHeight;
        els.msgScroll.scrollTop = pt + (nh - ph);
      }
    }
    // Backfill reactions for any newly visible messages.
    const ids = (state.messagesByRoom.get(roomId) || []).map((m) => m.id);
    if (ids.length) {
      api.reactionsBatch(ids.slice(-100)).then((map) => {
        for (const [mid, summaries] of Object.entries(map || {})) {
          state.reactionsByMsg.set(mid, summaries);
          refreshReactionsFor(mid);
        }
      }).catch(() => {});
    }
    // Mark latest as read.
    const latest = (state.messagesByRoom.get(roomId) || []).at(-1);
    if (latest) maybeMarkRead(latest);
  } catch (err) {
    if (err instanceof ApiError && err.status === 401) forceReauth();
    else toast(`拉取历史失败:${err.message}`, 'error');
  } finally { state.loadingHistory = false; }
}

function rerenderCurrentRoom() {
  const roomId = state.currentRoomId;
  els.msgList.replaceChildren();
  const arr = state.messagesByRoom.get(roomId) || [];
  for (const m of arr) els.msgList.appendChild(renderMsgWithReactions(m));
  for (const [, p] of state.pendingByTempId)
    if (p.room_id === roomId) els.msgList.appendChild(renderMsgWithReactions(p, { pending: true }));
  els.msgEmpty.hidden = arr.length > 0 || hasPendingForRoom(roomId);
  for (const m of arr) refreshReactionsFor(m.id);
  refreshReadStrips();
}

function renderMsgWithReactions(m, opts = {}) {
  // Hydrate reply_to with the parent message snippet if available.
  if (m.reply_to && !opts.replyTarget) {
    const arr = state.messagesByRoom.get(m.room_id) || [];
    const parent = arr.find((x) => x.id === m.reply_to);
    if (parent) opts = { ...opts, replyTarget: parent };
  }
  const node = renderMessage(m, state.me?.id, state.participants, {
    ...opts,
    live: liveHooks(),
    onInteract: submitBlockInteraction,
  });
  wireMsgActions(node, m);
  // Click on the reply chip jumps to the parent.
  const chip = node.querySelector('.reply-chip');
  if (chip) {
    chip.addEventListener('click', () => {
      const tid = chip.dataset.targetId;
      if (tid) scrollToMessage(tid);
    });
  }
  return node;
}

function refreshReactionsFor(mid) {
  const node = els.msgList.querySelector(`.msg-reactions[data-msg-id="${cssEscape(mid)}"]`);
  if (!node) return;
  const summaries = state.reactionsByMsg.get(mid) || [];
  renderReactionsInto(node, summaries, state.me?.id, (emoji) => ws.react(mid, emoji));
}

// ---------- live interactivity (P11 弹幕 + 礼物) ----------
// Live-card plumbing extracted to livecards.js; message-DOM helpers stay here.

function wireMsgActions(node, m) {
  const actions = node.querySelector('.msg-actions');
  if (!actions) return;
  // Reflect any already-known thread-mute state; hydrate lazily on first hover.
  const muteBtn = actions.querySelector('.msg-act-mute');
  if (muteBtn) {
    paintThreadMuteBtn(muteBtn, state.threadMuted.get(m.id));
    actions.addEventListener('mouseenter', () => hydrateThreadMuted(m.id, muteBtn), { once: true });
  }
  actions.addEventListener('click', (e) => {
    const btn = e.target.closest('button.msg-act');
    if (!btn) return;
    const act = btn.dataset.action;
    if (act === 'react') openEmojiPicker(btn, (emoji) => ws.react(m.id, emoji));
    if (act === 'edit') beginEditMessage(m);
    if (act === 'recall') {
      // Window-expired 409 must reach the user; already-recalled/deleted 409s
      // converge silently; 429 backs off without auto-retry (recall_errors.js);
      // dead-session 401 follows the app's reauth convention (app.js:600).
      if (confirm('撤回这条消息?')) api.recallMessage(m.id).catch((err) => {
        if (err?.status === 401) return forceReauth();
        const t = recallErrorToast(err);
        if (t) toast(t.text, t.type);
      });
    }
    if (act === 'delete' && confirm('删除这条消息?')) ws.deleteMessage(m.id);
    if (act === 'reply') beginReply(m);
    if (act === 'mute-thread') toggleThreadMute(m.id, btn);
  });
}

// Paint the bell glyph + tooltip on a thread-mute button from a known state
// (`muted === undefined` = unknown — leave the default not-muted bell).
function paintThreadMuteBtn(btn, muted) {
  if (!btn) return;
  btn.textContent = muted ? '🔕' : '🔔';
  btn.setAttribute('title', muted ? '取消静音线程' : '静音线程');
  btn.classList.toggle('muted', Boolean(muted));
}

// Lazily fetch this thread's muters and repaint the bell; best-effort (a
// failed fetch leaves the default glyph — the toggle still works regardless).
async function hydrateThreadMuted(rootId, btn) {
  if (state.threadMuted.has(rootId)) return; // already known
  try {
    const res = await api.threadMuters(rootId);
    const muters = Array.isArray(res?.muters) ? res.muters : [];
    const mine = Boolean(state.me?.id && muters.includes(state.me.id));
    state.threadMuted.set(rootId, mine);
    paintThreadMuteBtn(btn, mine);
  } catch {
    // ignore — keep default glyph, toggle still available
  }
}

// Toggle the caller's mute on the thread rooted at `rootId`; optimistic flip
// + server reconciliation (rolls back on error).
async function toggleThreadMute(rootId, btn) {
  const currentlyMuted = Boolean(state.threadMuted.get(rootId));
  const next = !currentlyMuted;
  state.threadMuted.set(rootId, next);
  paintThreadMuteBtn(btn, next);
  try {
    const res = next ? await api.muteThread(rootId) : await api.unmuteThread(rootId);
    const confirmed = Boolean(res?.muted);
    state.threadMuted.set(rootId, confirmed);
    paintThreadMuteBtn(btn, confirmed);
    toast(confirmed ? '已静音该线程的回复通知' : '已取消该线程静音', 'info');
  } catch (err) {
    // roll back the optimistic flip
    state.threadMuted.set(rootId, currentlyMuted);
    paintThreadMuteBtn(btn, currentlyMuted);
    toast(`操作失败:${err.message}`, 'error');
  }
}

function beginReply(m) {
  state.replyTo = { id: m.id, sender_id: m.sender_id, blocks: m.blocks };
  renderReplyChip();
  els.composerInput.focus();
}

function clearReply() {
  state.replyTo = null;
  renderReplyChip();
}

function renderReplyChip() {
  let row = document.getElementById('composer-reply');
  if (!state.replyTo) {
    if (row) row.remove();
    return;
  }
  if (!row) {
    row = document.createElement('div');
    row.id = 'composer-reply';
    row.className = 'composer-reply';
    els.composer.insertBefore(row, els.composer.firstChild);
  }
  row.replaceChildren();
  const sname = state.participants.get(state.replyTo.sender_id)?.display_name || state.replyTo.sender_id.slice(0, 6);
  const txt = (state.replyTo.blocks || []).map((b) => b?.content || '').join(' ').slice(0, 60);
  const lbl = document.createElement('span');
  lbl.className = 'composer-reply-label';
  lbl.textContent = '↩ 回复 ' + sname + ': ';
  const body = document.createElement('span');
  body.className = 'composer-reply-text';
  body.textContent = txt;
  const x = document.createElement('button');
  x.type = 'button';
  x.className = 'composer-reply-x';
  x.textContent = '×';
  x.addEventListener('click', clearReply);
  row.appendChild(lbl);
  row.appendChild(body);
  row.appendChild(x);
}

function beginEditMessage(m) {
  const text = (m.blocks || []).filter((b) => b.type === 'text').map((b) => b.content).join('\n');
  const next = prompt('编辑消息:', text);
  if (next == null || next.trim() === '' || next === text) return;
  ws.editMessage(m.id, [{ type: 'text', content: next }]);
}

function hideEmptyIfNeeded() {
  const arr = state.messagesByRoom.get(state.currentRoomId) || [];
  els.msgEmpty.hidden = arr.length > 0 || hasPendingForRoom(state.currentRoomId);
}

function appendMessageEl(node, { scroll = true } = {}) {
  els.msgList.appendChild(node);
  if (scroll) scrollToBottom();
}
function scrollToBottom() {
  requestAnimationFrame(() => {
    els.msgScroll.scrollTop = els.msgScroll.scrollHeight;
    requestAnimationFrame(() => { els.msgScroll.scrollTop = els.msgScroll.scrollHeight; });
  });
}

function replaceNodeForMsg(id, m) {
  const node = els.msgList.querySelector(`[data-msg-id="${cssEscape(id)}"]`);
  const fresh = renderMsgWithReactions(m);
  if (node?.parentNode) node.parentNode.replaceChild(fresh, node);
  else els.msgList.appendChild(fresh);
}

// ---------- scroll for older history + mark read ----------
els.msgScroll.addEventListener('scroll', () => {
  if (!state.currentRoomId) return;
  if (els.msgScroll.scrollTop <= 40 && !state.loadingHistory) {
    const rid = state.currentRoomId;
    if (!state.reachedTop.has(rid) && (state.messagesByRoom.get(rid)?.length || 0) > 0) loadHistory(rid);
  }
  const distance = els.msgScroll.scrollHeight - els.msgScroll.scrollTop - els.msgScroll.clientHeight;
  if (distance < 80) {
    const arr = state.messagesByRoom.get(state.currentRoomId) || [];
    const latest = arr.at(-1);
    if (latest) maybeMarkRead(latest);
  }
});

function maybeMarkRead(m) {
  if (!m?.id || !state.currentRoomId || m.room_id !== state.currentRoomId) return;
  const map = state.receiptsByRoom.get(m.room_id) || new Map();
  const cur = map.get(state.me?.id);
  if (cur?.last_read_message_id && cur.last_read_message_id >= m.id) return;
  ws.markRead(m.room_id, m.id);
}

// ---------- composer ----------
els.composerInput.addEventListener('input', () => {
  els.composerSend.disabled = !els.composerInput.value.trim();
  autoGrow(els.composerInput);
  sendTypingThrottled(true);
  maybeShowMentionMenu();
});
els.composerInput.addEventListener('blur', () => { sendTypingThrottled(false); setTimeout(closeMentionMenu, 120); });
els.composerInput.addEventListener('keydown', (e) => {
  if (isMentionMenuOpen()) {
    if (e.key === 'ArrowDown') { e.preventDefault(); moveMention(1); return; }
    if (e.key === 'ArrowUp')   { e.preventDefault(); moveMention(-1); return; }
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      pickMention();
      return;
    }
    if (e.key === 'Escape')    { closeMentionMenu(); return; }
  }
  if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
    e.preventDefault();
    submitComposer();
  }
});
els.composer.addEventListener('submit', (e) => { e.preventDefault(); submitComposer(); });

function sendTypingThrottled(on) {
  if (!state.currentRoomId) return;
  const now = Date.now();
  if (on) {
    if (now - state.lastTypingSentAt > 3500) {
      ws.typing(state.currentRoomId, true);
      state.lastTypingSentAt = now;
    }
  } else {
    ws.typing(state.currentRoomId, false);
    state.lastTypingSentAt = 0;
  }
}

function autoGrow(ta) {
  ta.style.height = 'auto';
  ta.style.height = Math.min(ta.scrollHeight, 180) + 'px';
}

function submitComposer() {
  if (!state.currentRoomId) return;
  const raw = els.composerInput.value.replace(/\s+$/g, '');
  if (!raw.trim()) return;
  const roomId = state.currentRoomId;
  const replyTo = state.replyTo ? state.replyTo.id : null;
  // Markdown send path: opt in via the "MD" toggle or a leading `/md ` prefix.
  // The raw text goes to the server's `SendMarkdown` frame; the edge parses it
  // into rich Text blocks (bold/italic/code/strike/link spans) and broadcasts
  // the result, which replaces the optimistic echo below. This is purely
  // additive — the normal `send_message` path is unchanged.
  const mdPrefixed = raw.startsWith('/md ');
  const asMarkdown = (els.mdToggle && els.mdToggle.checked) || mdPrefixed;
  if (asMarkdown) {
    const md = mdPrefixed ? raw.slice(4) : raw;
    if (md.trim()) {
      // Optimistic echo as plain text; the real message frame swaps in spans.
      const blocks = [{ type: 'text', content: md }];
      if (!sendOptimistically(
        (id) => ws.sendMarkdown(roomId, md, replyTo, null, id),
        (delivery) => optimisticAdd(roomId, blocks, replyTo, delivery),
        { kind: 'markdown', roomId, markdown: md, replyTo, expiresAfterSecs: null },
      )) return;
      ws.typing(roomId, false);
      state.lastTypingSentAt = 0;
      els.composerInput.value = '';
      els.composerSend.disabled = true;
      autoGrow(els.composerInput);
      closeMentionMenu();
      clearReply();
      draftComposerCleared(roomId); // send accepted → the draft is obsolete
    }
    return;
  }
  const blocks = composeBlocksFromInput(raw);
  if (!sendOptimistically(
    (id) => ws.sendMessage(roomId, blocks, replyTo, id),
    (delivery) => optimisticAdd(roomId, blocks, replyTo, delivery),
    { kind: 'blocks', roomId, blocks, replyTo },
  )) return;
  ws.typing(roomId, false);
  state.lastTypingSentAt = 0;
  els.composerInput.value = '';
  els.composerSend.disabled = true;
  autoGrow(els.composerInput);
  closeMentionMenu();
  clearReply();
  draftComposerCleared(roomId); // send accepted → the draft is obsolete
}

function composeBlocksFromInput(text) {
  const blocks = [];
  const re = /@([0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{25,26})\b/g;
  let last = 0;
  let m;
  while ((m = re.exec(text))) {
    if (m.index > last) {
      const seg = text.slice(last, m.index);
      if (seg) blocks.push({ type: 'text', content: seg });
    }
    blocks.push({ type: 'mention', participant: m[1] });
    last = m.index + m[0].length;
  }
  if (last < text.length) {
    const seg = text.slice(last);
    if (seg) blocks.push({ type: 'text', content: seg });
  }
  if (!blocks.length) blocks.push({ type: 'text', content: text });
  return blocks;
}

function optimisticAdd(roomId, blocks, replyTo = null, delivery = {}) {
  const tempId = pendingTempId(delivery.client_message_id || crypto.randomUUID());
  const pending = {
    id: tempId, room_id: roomId, sender_id: state.me?.id, blocks,
    reply_to: replyTo, metadata: {}, created_at: new Date().toISOString(),
    edited_at: null, deleted_at: null, ...delivery,
  };
  state.pendingByTempId.set(tempId, pending);
  appendMessageEl(renderMsgWithReactions(pending, { pending: true }), { scroll: true });
  hideEmptyIfNeeded();
  return pending;
}

function restorePendingDelivery(pending) {
  const restored = { ...pending, id: pendingTempId(pending.client_message_id) };
  state.pendingByTempId.set(restored.id, restored);
  if (restored.room_id === state.currentRoomId)
    appendMessageEl(renderMsgWithReactions(restored, { pending: true }), { scroll: true });
  hideEmptyIfNeeded();
  return restored;
}
// ---------- extracted domain wiring ----------
// Search drawer + AI assistant, notification inbox, live-streams, and drafts
// attach their listeners here at module-load time. `switchRoom` is hoisted.
initSearchAi();
initNotifications({ switchRoom });
initLive();
initSmartReplies({ optimisticAdd });
initMedia({ optimisticAdd, clearReply, forceReauth });
initDrafts({ state, els, forceReauth, clearReply, renderReplyChip });
// Modal-backed forms + auth/logout wiring (`enterChat`/`showAuth`/callbacks hoisted).
initModalForms({ forceReauth, refreshRoomList, switchRoom });
initAuthUi({ enterChat, showAuth, onLogout: resetDrafts });
initChrome();
// ---------- calls ----------
// 1:1 + captions + group-mesh call logic lives in calls.js; attach its
// controls here (module-load ordering, DOM already present from index.html).
wireCallControls();
// ---------- utilities ----------
function forceReauth() {
  resetDrafts(); // session teardown: drop in-memory draft state BEFORE auth.clear() (mirrors the active room under the OLD pid)
  toast('会话过期,请重新登录', 'error');
  auth.clear();
  state.me = null;
  ws.close();
  showAuth();
}
// ---------- bootstrap ----------
installUnhandledRejectionReporting(toast);

(async function bootstrap() {
  const t = auth.getToken();
  if (!t) { showAuth(); return; }
  try {
    const me = await api.me();
    if (!me?.id) throw new Error('invalid /me response');
    state.me = me;
    state.participants.set(me.id, me);
    enterChat();
  } catch (err) {
    console.warn('session check failed', err);
    auth.clear();
    showAuth();
  }
})();
