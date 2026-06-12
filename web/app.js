// app.js — Aero IM debug client entry point (P2 features wired).
// Auth, rooms, messages, optimistic rendering, typing, read, reactions,
// attachments, search drawer, AI drawer, 1:1 calls, live streaming.

import { api, auth, ApiError } from './api.js';
import { WsClient } from './ws.js';
import {
  renderMessage,
  renderRoomItem,
  renderOnlineItem,
  renderReactionsInto,
  renderTypingInto,
  initialOf,
  toast,
} from './render.js';

// ---------- state ----------
const state = {
  me: null,
  rooms: new Map(),
  participants: new Map(),
  currentRoomId: null,
  messagesByRoom: new Map(),
  loadingHistory: false,
  reachedTop: new Set(),
  pendingByTempId: new Map(),
  // P2
  typing: new Map(),
  reactionsByMsg: new Map(),
  receiptsByRoom: new Map(),
  lastTypingSentAt: 0,
  // P9.5
  unreadByRoom: new Map(),    // room_id -> count
  replyTo: null,              // { id, sender_id, blocks } when composing a reply
  // ROADMAP v3 方向一: last-applied edit timestamp per message id, so an older
  // Edited event redelivered out of order never clobbers a newer edit.
  lastEditAt: new Map(),      // message_id -> epoch ms of the applied edit
  lastChangeSync: new Map(),  // room_id -> RFC3339 of the last edit/delete replay sync
  resyncInFlight: false,      // collapse bursts of server `resync` frames
  // Call
  call: null,
  gcall: null,               // group (mesh) call: { id, roomId, kind, localStream, peers:Map }
  rtcConfig: null,
  // P11 live interactivity
  giftCatalog: [],
  liveCards: new Map(),       // stream_id -> card controller (from render.js)
  watchedStreams: new Set(),  // stream_ids we've sent watch_stream for
};

const ws = new WsClient();

// ---------- DOM ----------
const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => Array.from(r.querySelectorAll(s));
const els = {
  viewAuth: $('#view-auth'),
  viewChat: $('#view-chat'),
  formLogin: $('#form-login'),
  formRegister: $('#form-register'),
  tabs: $$('.tab'),
  tabPanels: $$('[data-tab-panel]'),

  roomList: $('#room-list'),
  meAvatar: $('#me-avatar'),
  meName: $('#me-name'),
  meEmail: $('#me-email'),
  btnLogout: $('#btn-logout'),
  btnNewRoom: $('#btn-new-room'),

  wsDot: $('#ws-dot'),

  roomName: $('#room-name'),
  roomMeta: $('#room-meta'),
  btnAddMember: $('#btn-add-member'),
  btnSearch: $('#btn-search'),
  btnAi: $('#btn-ai'),
  btnCallAudio: $('#btn-call-audio'),
  btnCallVideo: $('#btn-call-video'),
  btnCallGroup: $('#btn-call-group'),
  btnGoLive: $('#btn-go-live'),
  btnLivePage: $('#btn-live-page'),

  msgScroll: $('#msg-scroll'),
  msgList: $('#msg-list'),
  msgEmpty: $('#msg-empty'),
  typingBar: $('#typing-bar'),

  composer: $('#composer'),
  composerInput: $('#composer-input'),
  composerSend: $('#composer-send'),
  btnAttach: $('#btn-attach'),
  fileInput: $('#file-input'),

  onlineList: $('#online-list'),
  onlineCount: $('#online-count'),

  modalNewRoom: $('#modal-new-room'),
  formNewRoom: $('#form-new-room'),
  modalAddMember: $('#modal-add-member'),
  formAddMember: $('#form-add-member'),
  modalProfile: $('#modal-profile'),
  formProfile: $('#form-profile'),

  drawerSearch: $('#drawer-search'),
  searchInput: $('#search-input'),
  searchResults: $('#search-results'),
  drawerAi: $('#drawer-ai'),
  aiInput: $('#ai-input'),
  aiResults: $('#ai-results'),
  btnAiSummarize: $('#btn-ai-summarize'),

  drawerLive: $('#drawer-live'),
  liveList: $('#live-list'),
  modalGoLive: $('#modal-go-live'),
  formGoLive: $('#form-go-live'),
  modalStreamInfo: $('#modal-stream-info'),
  streamIngest: $('#stream-ingest'),
  streamHls: $('#stream-hls'),

  callOverlay: $('#call-overlay'),
  callLocal: $('#call-local'),
  callRemote: $('#call-remote'),
  callMute: $('#call-mute'),
  callCam: $('#call-cam'),
  callEnd: $('#call-end'),
  callCaptions: $('#call-captions'),
  callCc: $('#call-cc'),
  callCcLang: $('#call-cc-lang'),
  gcallOverlay: $('#gcall-overlay'),
  gcallGrid: $('#gcall-grid'),
  gcallMute: $('#gcall-mute'),
  gcallCam: $('#gcall-cam'),
  gcallLeave: $('#gcall-leave'),
  gcallCount: $('#gcall-count'),
};

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

function setBusy(form, busy) {
  for (const c of form.querySelectorAll('input, button, select, textarea')) c.disabled = !!busy;
}

// ---------- auth ----------
els.formLogin.addEventListener('submit', async (e) => {
  e.preventDefault();
  const fd = new FormData(els.formLogin);
  const email = String(fd.get('email') || '').trim();
  const password = String(fd.get('password') || '');
  if (!email || !password) return;
  setBusy(els.formLogin, true);
  try { onAuthSuccess(await api.login({ email, password })); }
  catch (err) { toast(err.message || '登录失败', 'error'); }
  finally { setBusy(els.formLogin, false); }
});
els.formRegister.addEventListener('submit', async (e) => {
  e.preventDefault();
  const fd = new FormData(els.formRegister);
  const email = String(fd.get('email') || '').trim();
  const password = String(fd.get('password') || '');
  const display_name = String(fd.get('display_name') || '').trim();
  if (!email || !password || !display_name) return;
  setBusy(els.formRegister, true);
  try { onAuthSuccess(await api.register({ email, password, display_name })); }
  catch (err) { toast(err.message || '注册失败', 'error'); }
  finally { setBusy(els.formRegister, false); }
});

function onAuthSuccess(res) {
  if (!res?.access_token || !res?.participant) { toast('服务端返回不完整', 'error'); return; }
  auth.setSession(res.access_token, res.refresh_token, res.participant.id);
  state.me = res.participant;
  state.participants.set(res.participant.id, res.participant);
  enterChat();
}

els.btnLogout.addEventListener('click', () => {
  ws.close();
  auth.clear();
  Object.assign(state, {
    me: null, currentRoomId: null,
    rooms: new Map(), participants: new Map(), messagesByRoom: new Map(),
    reachedTop: new Set(), pendingByTempId: new Map(),
    typing: new Map(), reactionsByMsg: new Map(), receiptsByRoom: new Map(),
  });
  els.roomList.replaceChildren();
  els.msgList.replaceChildren();
  els.onlineList.replaceChildren();
  showAuth();
});

function enterChat() {
  showChat();
  const me = state.me;
  els.meName.textContent = me.display_name || '—';
  els.meEmail.textContent = me.email || me.id || '';
  els.meAvatar.textContent = initialOf(me.display_name || me.email);
  els.meAvatar.setAttribute('style', avatarStyleFromId(me.id));
  ws.connect(auth.getToken());
  hookWs();
  refreshRoomsFromServer();
  api.rtcConfig().then((c) => { state.rtcConfig = c; }).catch(() => {});
  // Load the gift catalog once so stream cards can render the gift bar; if a
  // room was already open, re-render so existing cards pick it up.
  api.liveGifts().then((r) => {
    state.giftCatalog = r?.gifts || [];
    if (state.currentRoomId) rerenderCurrentRoom();
  }).catch(() => {});
  // Ask for notification permission once (silent if denied).
  if ('Notification' in window && Notification.permission === 'default') {
    Notification.requestPermission().catch(() => {});
  }
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden && state.currentRoomId) clearUnread(state.currentRoomId);
  });
}

function avatarStyleFromId(id) {
  let h = 0;
  if (id) for (let i = 0; i < id.length; i++) h = (h * 31 + id.charCodeAt(i)) >>> 0;
  const hue = h % 360, hue2 = (hue + 40) % 360;
  return `background: linear-gradient(135deg, hsl(${hue} 70% 55%), hsl(${hue2} 70% 50%));`;
}

async function refreshRoomsFromServer() {
  try {
    const rooms = await api.listRooms();
    if (Array.isArray(rooms)) {
      for (const r of rooms) state.rooms.set(r.id, r);
      refreshRoomList();
    }
  } catch (e) { /* ignore */ }
}

// ---------- ws ----------
function hookWs() {
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
  ws.on('msg:message', (f) => handleIncomingMessage(f.message));
  ws.on('msg:edited', (f) => handleEdited(f.message));
  ws.on('msg:deleted', (f) => handleDeleted(f));
  ws.on('msg:reaction', (f) => handleReaction(f));
  ws.on('msg:read', (f) => handleReadReceipt(f));
  ws.on('msg:typing', (f) => handleTyping(f));
  ws.on('msg:notify', (f) => handleNotify(f));
  ws.on('msg:pin', (f) => handlePin(f));
  ws.on('msg:presence', (f) => handlePresence(f));
  ws.on('msg:call', (f) => handleCall(f.event));
  ws.on('msg:stream_event', (f) => handleStreamEvent(f.event));
  // ROADMAP v3 方向一: the WS reconnect backfill replay hit its per-room (or
  // per-stream) cap — continue fetching the remainder over REST `?since=`.
  ws.on('msg:backfill', (f) => handleBackfillTruncated(f));
  // ROADMAP v3 方向一: the server dropped frames for us while our queue was
  // full (slow consumer, drop-only mode) — re-pull what we missed over REST.
  ws.on('msg:resync', () => handleResync());
  ws.on('msg:error', (f) => toast(`服务端:${f.msg || f.code || 'error'}`, 'error'));
  ws.on('msg:pong', () => {});
}

// Bound on REST catch-up pagination after a truncated backfill, so a client
// that has been away for a very long time converges in bounded work (anything
// older is reachable through normal history scroll-back).
const CATCHUP_MAX_PAGES = 10;
const CATCHUP_PAGE_SIZE = 100;

// Continue a server-truncated replay over REST. Room frames carry `room_id`
// (continue via GET /api/rooms/:id/messages?since=); stream danmaku frames
// carry `stream_id` (continue via GET /api/streams/:id/chat?since=).
async function handleBackfillTruncated(f) {
  if (!f || !f.truncated || !f.next_since) return;
  if (f.room_id) await pullRoomSince(f.room_id, f.next_since, CATCHUP_MAX_PAGES);
  else if (f.stream_id) await pullStreamChatSince(f.stream_id, f.next_since, CATCHUP_MAX_PAGES);
}

// Forward-page a room's messages from `since` (exclusive), feeding each one
// through the normal incoming-message path (id-dedup, unread badges, render).
// Stops after `maxPages` pages or the first short page (caught up).
async function pullRoomSince(roomId, since, maxPages) {
  let cursor = since;
  for (let page = 0; page < maxPages && cursor; page++) {
    let list;
    try { list = await api.listMessages(roomId, { since: cursor, limit: CATCHUP_PAGE_SIZE }); }
    catch { return; } // best-effort: a failed catch-up page is retried on the next resync
    const arr = Array.isArray(list) ? list : [];
    for (const m of arr) handleIncomingMessage(m);
    if (arr.length < CATCHUP_PAGE_SIZE) return; // short page → fully caught up
    // ULIDs sort lexicographically: advance to the newest id of this page.
    cursor = arr.reduce((mx, m) => (m?.id && m.id > mx ? m.id : mx), cursor);
  }
}

// Forward-page a stream's danmaku from `since` into its live card.
async function pullStreamChatSince(streamId, since, maxPages) {
  const ctrl = state.liveCards.get(streamId);
  if (!ctrl) return; // card unmounted (room switched) — nothing to render into
  let cursor = since;
  const pageSize = 200; // server clamps chat pages to 200
  for (let page = 0; page < maxPages && cursor; page++) {
    let r;
    try { r = await api.streamChatList(streamId, pageSize, cursor); }
    catch { return; }
    const lines = Array.isArray(r?.chat) ? r.chat : [];
    for (const line of lines) ctrl.addChat(line);
    if (lines.length < pageSize) return;
    cursor = lines.reduce((mx, l) => (l?.id && l.id > mx ? l.id : mx), cursor);
  }
}

// Slow-consumer resync: the server dropped an unknown set of frames for this
// connection, so re-pull every room we hold local state for from its newest
// known message forward. Messages are the durable record (edits/reactions are
// re-fetched lazily per room view); a short page bound keeps this cheap.
async function handleResync() {
  if (state.resyncInFlight) return;
  state.resyncInFlight = true;
  try {
    for (const [roomId, arr] of state.messagesByRoom) {
      const last = arr.length ? arr[arr.length - 1]?.id : null;
      if (last) await pullRoomSince(roomId, last, 3);
    }
  } finally {
    state.resyncInFlight = false;
  }
}

function handleIncomingMessage(m) {
  if (!m?.id || !m?.room_id) return;
  const isMine = m.sender_id === state.me?.id;
  if (isMine) {
    const key = findPendingMatch(m);
    if (key) {
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
  if (arr.some((x) => x.id === m.id)) return;
  arr.push(m);
  state.messagesByRoom.set(m.room_id, arr);
  const isCurrent = m.room_id === state.currentRoomId;
  const tabFocused = !document.hidden;
  if (isCurrent && tabFocused) {
    appendMessageEl(renderMsgWithReactions(m), { scroll: true });
    hideEmptyIfNeeded();
    maybeMarkRead(m);
  } else {
    bumpUnread(m.room_id);
    if (!isMine) notify(m);
    if (isCurrent) {
      // Tab is hidden but room is active — still append, just don't mark read.
      appendMessageEl(renderMsgWithReactions(m), { scroll: true });
      hideEmptyIfNeeded();
    }
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
  if (!('Notification' in window)) return;
  if (Notification.permission !== 'granted') return;
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

function handleEdited(m) {
  if (!m?.id || !m?.room_id) return;
  // ROADMAP v3 方向一: ignore an edit older than one we already applied (an
  // at-least-once redelivery or cross-instance replay can arrive out of order;
  // edits carry no id of their own, so the edit timestamp is the order key).
  const ts = Date.parse(m.edited_at || '') || 0;
  const prev = state.lastEditAt.get(m.id) || 0;
  if (ts && prev && ts < prev) return;
  if (ts) state.lastEditAt.set(m.id, ts);
  const arr = state.messagesByRoom.get(m.room_id) || [];
  const idx = arr.findIndex((x) => x.id === m.id);
  if (idx >= 0) {
    // Resurrect guard (ROADMAP 方向一): a stale Edited arriving AFTER a Delete
    // (out-of-order redelivery, or a change-replay that interleaves with the
    // live stream) must not bring a deleted message back onto the screen.
    if (arr[idx].deleted_at) return;
    arr[idx] = m;
  }
  state.messagesByRoom.set(m.room_id, arr);
  if (m.room_id === state.currentRoomId) replaceNodeForMsg(m.id, m);
}

/// Apply one change-replay row: a tombstone (populated `deleted_at`) removes the
/// message, otherwise it is treated as an edit. Reuses the live handlers so the
/// resurrect guard and out-of-order edit guard apply identically (ROADMAP 方向一).
function applyChange(m) {
  if (!m?.id || !m?.room_id) return;
  if (m.deleted_at) handleDeleted({ room_id: m.room_id, message_id: m.id });
  else handleEdited(m);
}

/// On reconnect, replay edits/deletes that landed while we were offline: the
/// message backfill (`?since=<id>`) only returns NEW messages, never mutations
/// to messages the client already holds (ROADMAP 方向一). Best-effort.
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

function findPendingMatch(serverMsg) {
  const myPid = state.me?.id;
  if (serverMsg.sender_id !== myPid) return null;
  const sText = textOf(serverMsg.blocks);
  const sT = Date.parse(serverMsg.created_at || '') || Date.now();
  for (const [tempId, pending] of state.pendingByTempId) {
    if (pending.sender_id !== myPid) continue;
    if (textOf(pending.blocks) !== sText) continue;
    const pT = Date.parse(pending.created_at || '') || Date.now();
    if (Math.abs(sT - pT) <= 15000) return tempId;
  }
  return null;
}
function textOf(blocks) {
  if (!Array.isArray(blocks)) return '';
  return blocks.map((b) => (b?.type === 'text' ? (b.content ?? '') : '')).join('');
}

function handlePresence(frame) {
  if (frame.room_id !== state.currentRoomId) return;
  const ids = Array.isArray(frame.online) ? frame.online : [];
  els.onlineList.replaceChildren();
  for (const pid of ids) els.onlineList.appendChild(renderOnlineItem(pid, state.participants.get(pid)));
  els.onlineCount.textContent = String(ids.length);
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
  restoreAiHistory(roomId);
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
  els.msgEmpty.hidden = arr.length > 0 || state.pendingByTempId.size > 0;
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
  const node = renderMessage(m, state.me?.id, state.participants, { ...opts, live: liveHooks() });
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

// Hooks handed to render.js's stream cards. Built fresh per render so the gift
// catalog + my id are always current.
function liveHooks() {
  return {
    gifts: state.giftCatalog,
    meId: state.me?.id || null,
    register: (streamId, controller) => registerLiveCard(streamId, controller),
    chat: (streamId, body) => ws.streamChat(streamId, body),
    gift: (streamId, giftId, qty) => ws.streamGift(streamId, giftId, qty),
    end: (streamId) => endStream(streamId),
  };
}

// A stream card just mounted: remember its controller, start watching, and
// hydrate a little recent-gift backlog (chat backlog is pushed by the server).
function registerLiveCard(streamId, controller) {
  if (!streamId) return;
  state.liveCards.set(streamId, controller);
  ws.watchStream(streamId);
  state.watchedStreams.add(streamId);
  api.streamGiftList(streamId, 5)
    .then((r) => { for (const g of (r?.gifts || [])) controller.addGift(g); })
    .catch(() => {});
}

// Stop watching every stream and forget controllers (called on room switch,
// before the message list is rebuilt with the new room's cards).
function clearLiveCards() {
  for (const sid of state.watchedStreams) ws.unwatchStream(sid);
  state.watchedStreams.clear();
  state.liveCards.clear();
}

function handleStreamEvent(ev) {
  if (!ev || !ev.kind || !ev.stream_id) return;
  const ctrl = state.liveCards.get(ev.stream_id);
  if (!ctrl) return;
  switch (ev.kind) {
    case 'chat': ctrl.addChat(ev); break;
    case 'gift': ctrl.addGift(ev); break;
    case 'viewers': ctrl.setViewers(ev.count); break;
    case 'status': ctrl.setStatus(ev.status); break;
    default: break;
  }
}

async function endStream(streamId) {
  if (!streamId) return;
  if (!window.confirm('确定结束这场直播?')) return;
  try {
    await api.endStream(streamId);
    toast('直播已结束', 'info');
  } catch (err) {
    toast(`结束失败:${err.message}`, 'error');
  }
}

function wireMsgActions(node, m) {
  const actions = node.querySelector('.msg-actions');
  if (!actions) return;
  actions.addEventListener('click', (e) => {
    const btn = e.target.closest('button.msg-act');
    if (!btn) return;
    const act = btn.dataset.action;
    if (act === 'react') openEmojiPicker(btn, (emoji) => ws.react(m.id, emoji));
    if (act === 'edit') beginEditMessage(m);
    if (act === 'delete') {
      if (confirm('删除这条消息?')) ws.deleteMessage(m.id);
    }
    if (act === 'reply') beginReply(m);
  });
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
  els.msgEmpty.hidden = arr.length > 0 || state.pendingByTempId.size > 0;
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
  if (mentionState.open) {
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
  const blocks = composeBlocksFromInput(raw);
  const replyTo = state.replyTo ? state.replyTo.id : null;
  optimisticAdd(roomId, blocks, replyTo);
  ws.sendMessage(roomId, blocks, replyTo);
  ws.typing(roomId, false);
  state.lastTypingSentAt = 0;
  els.composerInput.value = '';
  els.composerSend.disabled = true;
  autoGrow(els.composerInput);
  closeMentionMenu();
  clearReply();
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

const mentionState = { open: false, items: [], index: 0, anchor: 0, pop: null };

async function maybeShowMentionMenu() {
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
function closeMentionMenu() {
  if (mentionState.pop && mentionState.pop.parentNode) mentionState.pop.parentNode.removeChild(mentionState.pop);
  mentionState.pop = null;
  mentionState.open = false;
}
function moveMention(delta) {
  const n = mentionState.items.length;
  if (!n) return;
  mentionState.index = (mentionState.index + delta + n) % n;
  openMentionMenu();
}
function pickMention() {
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

function optimisticAdd(roomId, blocks, replyTo = null) {
  const tempId = '_pending_' + crypto.randomUUID();
  const pending = {
    id: tempId, room_id: roomId, sender_id: state.me?.id, blocks,
    reply_to: replyTo, metadata: {}, created_at: new Date().toISOString(),
    edited_at: null, deleted_at: null,
  };
  state.pendingByTempId.set(tempId, pending);
  appendMessageEl(renderMsgWithReactions(pending, { pending: true }), { scroll: true });
  hideEmptyIfNeeded();
}

// ---------- attachments ----------
els.btnAttach.addEventListener('click', () => els.fileInput.click());
els.fileInput.addEventListener('change', async () => {
  const f = els.fileInput.files?.[0];
  if (!f) return;
  els.fileInput.value = '';
  await uploadAndSend(f);
});

async function uploadAndSend(file) {
  if (!state.currentRoomId) { toast('请先选择房间', 'error'); return; }
  toast(`上传 ${file.name}…`, 'info');
  try {
    const blob = await api.uploadBlob(file);
    const block = {
      type: 'file',
      blob_id: blob.id,
      kind: blob.kind,
      name: blob.name,
      size: blob.size,
    };
    optimisticAdd(state.currentRoomId, [block]);
    ws.sendMessage(state.currentRoomId, [block], null);
  } catch (err) {
    if (err instanceof ApiError && err.status === 401) forceReauth();
    else toast(`上传失败:${err.message}`, 'error');
  }
}

// ---------- voice recording ----------
const btnVoice = document.getElementById('btn-voice');
const voiceState = { rec: null, chunks: [], started: 0, stream: null };
if (btnVoice) {
  btnVoice.addEventListener('click', toggleRecording);
}

async function toggleRecording() {
  if (voiceState.rec) {
    stopRecording();
    return;
  }
  if (!state.currentRoomId) { toast('请先选择房间', 'error'); return; }
  if (!navigator.mediaDevices || !window.MediaRecorder) {
    toast('浏览器不支持录音', 'error'); return;
  }
  try {
    voiceState.stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    const mime = MediaRecorder.isTypeSupported('audio/webm;codecs=opus') ? 'audio/webm;codecs=opus' : '';
    voiceState.rec = mime ? new MediaRecorder(voiceState.stream, { mimeType: mime }) : new MediaRecorder(voiceState.stream);
    voiceState.chunks = [];
    voiceState.started = Date.now();
    voiceState.rec.addEventListener('dataavailable', (e) => {
      if (e.data && e.data.size > 0) voiceState.chunks.push(e.data);
    });
    voiceState.rec.addEventListener('stop', onVoiceStop);
    voiceState.rec.start(250);
    btnVoice.classList.add('recording');
    btnVoice.textContent = '⏹';
    btnVoice.title = '点击停止';
    toast('录音中…', 'info');
  } catch (err) {
    toast(`录音失败:${err.message}`, 'error');
    cleanupVoice();
  }
}

function stopRecording() {
  if (voiceState.rec && voiceState.rec.state !== 'inactive') {
    voiceState.rec.stop();
  }
}

async function onVoiceStop() {
  const durationMs = Date.now() - voiceState.started;
  const blob = new Blob(voiceState.chunks, { type: voiceState.rec?.mimeType || 'audio/webm' });
  cleanupVoice();
  if (blob.size < 200) {
    toast('录音太短', 'error');
    return;
  }
  if (!state.currentRoomId) return;
  toast('上传录音…', 'info');
  try {
    const file = new File([blob], `voice-${Date.now()}.webm`, { type: blob.type });
    const meta = await api.uploadBlob(file);
    const voiceBlock = {
      type: 'voice',
      blob_id: meta.id,
      duration_ms: durationMs,
    };
    optimisticAdd(state.currentRoomId, [voiceBlock]);
    ws.sendMessage(state.currentRoomId, [voiceBlock], state.replyTo ? state.replyTo.id : null);
    clearReply();
  } catch (err) {
    if (err instanceof ApiError && err.status === 401) forceReauth();
    else toast(`上传失败:${err.message}`, 'error');
  }
}

function cleanupVoice() {
  if (voiceState.stream) {
    for (const t of voiceState.stream.getTracks()) t.stop();
  }
  voiceState.rec = null;
  voiceState.chunks = [];
  voiceState.stream = null;
  if (btnVoice) {
    btnVoice.classList.remove('recording');
    btnVoice.textContent = '🎙';
    btnVoice.title = '按住录音 / 点击开始';
  }
}

// Drag-and-drop file upload into the message area.
['dragenter', 'dragover'].forEach((evt) => {
  els.msgScroll.addEventListener(evt, (e) => {
    if (!e.dataTransfer || !Array.from(e.dataTransfer.types || []).includes('Files')) return;
    e.preventDefault();
    els.msgScroll.classList.add('drop-active');
  });
});
['dragleave', 'dragend'].forEach((evt) => {
  els.msgScroll.addEventListener(evt, () => els.msgScroll.classList.remove('drop-active'));
});
els.msgScroll.addEventListener('drop', async (e) => {
  if (!e.dataTransfer) return;
  e.preventDefault();
  els.msgScroll.classList.remove('drop-active');
  const files = Array.from(e.dataTransfer.files || []);
  for (const f of files) {
    // serial to keep order
    // eslint-disable-next-line no-await-in-loop
    await uploadAndSend(f);
  }
});

// ---------- profile editing ----------
els.meAvatar.addEventListener('click', () => {
  if (!state.me) return;
  els.formProfile.querySelector('[name="display_name"]').value = state.me.display_name || '';
  els.formProfile.querySelector('[name="avatar_url"]').value = state.me.avatar_url || '';
  openModal(els.modalProfile);
});
els.formProfile.addEventListener('submit', async (e) => {
  e.preventDefault();
  const fd = new FormData(els.formProfile);
  const display_name = String(fd.get('display_name') || '').trim();
  const avatar_url_raw = String(fd.get('avatar_url') || '').trim();
  if (!display_name) { toast('显示名不能空', 'error'); return; }
  setBusy(els.formProfile, true);
  try {
    const me = await api.updateMe({ display_name, avatar_url: avatar_url_raw || null });
    state.me = me;
    state.participants.set(me.id, me);
    els.meName.textContent = me.display_name || '—';
    els.meEmail.textContent = me.email || me.id || '';
    els.meAvatar.textContent = initialOf(me.display_name || me.email);
    closeModal(els.modalProfile);
    toast('已更新', 'ok');
  } catch (err) {
    if (err instanceof ApiError && err.status === 401) forceReauth();
    else toast(`更新失败:${err.message}`, 'error');
  } finally { setBusy(els.formProfile, false); }
});

// ---------- new room ----------
els.btnNewRoom.addEventListener('click', () => openModal(els.modalNewRoom));
els.formNewRoom.addEventListener('submit', async (e) => {
  e.preventDefault();
  const fd = new FormData(els.formNewRoom);
  const kind = String(fd.get('kind') || 'group');
  const name = String(fd.get('name') || '').trim();
  setBusy(els.formNewRoom, true);
  try {
    const room = await api.createRoom({ kind, name });
    state.rooms.set(room.id, room);
    refreshRoomList();
    closeModal(els.modalNewRoom);
    els.formNewRoom.reset();
    toast(`已创建 ${room.kind} · ${room.id.slice(0, 8)}…`, 'ok');
    switchRoom(room.id);
  } catch (err) {
    if (err instanceof ApiError && err.status === 401) forceReauth();
    else toast(`创建失败:${err.message}`, 'error');
  } finally { setBusy(els.formNewRoom, false); }
});

// ---------- add member ----------
els.btnAddMember.addEventListener('click', () => openModal(els.modalAddMember));
els.formAddMember.addEventListener('submit', async (e) => {
  e.preventDefault();
  if (!state.currentRoomId) return;
  const fd = new FormData(els.formAddMember);
  const pid = String(fd.get('participant_id') || '').trim();
  if (!pid) return;
  setBusy(els.formAddMember, true);
  try {
    await api.addMember(state.currentRoomId, pid);
    closeModal(els.modalAddMember);
    els.formAddMember.reset();
    toast('已添加成员', 'ok');
  } catch (err) {
    if (err instanceof ApiError && err.status === 401) forceReauth();
    else toast(`添加失败:${err.message}`, 'error');
  } finally { setBusy(els.formAddMember, false); }
});

// ---------- modal helpers ----------
function openModal(m) { m.hidden = false; }
function closeModal(m) { m.hidden = true; }
for (const m of [els.modalNewRoom, els.modalAddMember, els.modalGoLive, els.modalStreamInfo, els.modalProfile]) {
  if (!m) continue;
  m.addEventListener('click', (e) => {
    if (e.target === m) closeModal(m);
    if (e.target instanceof HTMLElement && e.target.hasAttribute('data-close')) closeModal(m);
  });
}
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') {
    for (const m of [els.modalNewRoom, els.modalAddMember, els.modalGoLive, els.modalStreamInfo, els.modalProfile])
      if (m && !m.hidden) closeModal(m);
    for (const d of [els.drawerSearch, els.drawerAi, els.drawerLive])
      if (d && !d.hidden) d.hidden = true;
    closeEmojiPicker();
  }
});

// ---------- drawer helpers ----------
document.querySelectorAll('[data-close-drawer]').forEach((b) => {
  b.addEventListener('click', () => {
    const k = b.dataset.closeDrawer;
    if (k === 'search') els.drawerSearch.hidden = true;
    if (k === 'ai') els.drawerAi.hidden = true;
    if (k === 'live') els.drawerLive.hidden = true;
  });
});

// ---------- search ----------
els.btnSearch.addEventListener('click', () => {
  if (!state.currentRoomId) { toast('请先选择房间', 'error'); return; }
  els.drawerSearch.hidden = false;
  els.searchInput.focus();
});
els.searchInput.addEventListener('keydown', async (e) => {
  if (e.key !== 'Enter') return;
  const q = els.searchInput.value.trim();
  if (!q) return;
  els.searchResults.replaceChildren(loadingDiv('搜索中…'));
  try {
    const res = await api.search(state.currentRoomId, { query: q, limit: 30, mode: 'auto' });
    const hits = res?.results || [];
    els.searchResults.replaceChildren();
    if (!hits.length) { els.searchResults.appendChild(mutedDiv('无结果')); return; }
    for (const h of hits) {
      const m = h.message;
      const wrap = document.createElement('div'); wrap.className = 'search-hit';
      const meta = document.createElement('div'); meta.className = 'search-hit-meta';
      meta.textContent = `${state.participants.get(m.sender_id)?.display_name || m.sender_id?.slice(0,6)} · ${formatTime(m.created_at)} · score ${h.score.toFixed(2)}`;
      const text = document.createElement('div'); text.className = 'search-hit-text';
      text.textContent = (m.blocks || []).map((b) => b.content || '').join(' ').slice(0, 240);
      wrap.appendChild(meta); wrap.appendChild(text);
      wrap.addEventListener('click', () => {
        els.drawerSearch.hidden = true;
        scrollToMessage(m.id);
      });
      els.searchResults.appendChild(wrap);
    }
  } catch (err) {
    els.searchResults.replaceChildren(mutedDiv(`搜索失败:${err.message}`));
  }
});

// ---------- AI ----------
els.btnAi.addEventListener('click', () => {
  if (!state.currentRoomId) { toast('请先选择房间', 'error'); return; }
  els.drawerAi.hidden = false;
  els.aiInput.focus();
});
els.btnAiSummarize.addEventListener('click', async () => {
  if (!state.currentRoomId) return;
  appendAiMessage('生成摘要中…', '');
  try {
    const res = await api.aiSummarize(state.currentRoomId, 80);
    appendAiMessage('房间摘要', res.summary);
  } catch (err) {
    appendAiMessage('摘要失败', err.message);
  }
});
els.aiInput.addEventListener('keydown', async (e) => {
  if (e.key !== 'Enter') return;
  const q = els.aiInput.value.trim();
  if (!q) return;
  els.aiInput.value = '';
  appendAiMessage(q, '思考中…');
  try {
    const res = await api.aiAsk(state.currentRoomId, q, 8);
    appendAiMessage(q, res.answer, res.citations || []);
  } catch (err) {
    appendAiMessage(q, `失败:${err.message}`);
  }
});
function appendAiMessage(q, a, citations = []) {
  const wrap = document.createElement('div'); wrap.className = 'ai-message';
  const qEl = document.createElement('div'); qEl.className = 'ai-q'; qEl.textContent = q;
  const aEl = document.createElement('div'); aEl.className = 'ai-a'; aEl.textContent = a;
  wrap.appendChild(qEl); wrap.appendChild(aEl);
  if (citations.length) {
    const row = document.createElement('div');
    for (const c of citations) {
      const chip = document.createElement('span'); chip.className = 'ai-cite';
      chip.textContent = '↗ ' + String(c).slice(0, 6);
      chip.addEventListener('click', () => { els.drawerAi.hidden = true; scrollToMessage(c); });
      row.appendChild(chip);
    }
    wrap.appendChild(row);
  }
  els.aiResults.appendChild(wrap);
  els.aiResults.scrollTop = els.aiResults.scrollHeight;
  saveAiHistory(state.currentRoomId, { q, a, citations });
}

function aiStoreKey(roomId) { return 'aero_ai_history:' + roomId; }
function saveAiHistory(roomId, entry) {
  if (!roomId) return;
  try {
    const k = aiStoreKey(roomId);
    const raw = sessionStorage.getItem(k);
    const arr = raw ? JSON.parse(raw) : [];
    arr.push({ ...entry, ts: Date.now() });
    if (arr.length > 50) arr.shift();
    sessionStorage.setItem(k, JSON.stringify(arr));
  } catch (e) { /* quota; ignore */ }
}
function restoreAiHistory(roomId) {
  els.aiResults.replaceChildren();
  if (!roomId) return;
  try {
    const k = aiStoreKey(roomId);
    const raw = sessionStorage.getItem(k);
    if (!raw) return;
    const arr = JSON.parse(raw);
    for (const e of arr) {
      const wrap = document.createElement('div'); wrap.className = 'ai-message';
      const qEl = document.createElement('div'); qEl.className = 'ai-q'; qEl.textContent = e.q;
      const aEl = document.createElement('div'); aEl.className = 'ai-a'; aEl.textContent = e.a;
      wrap.appendChild(qEl); wrap.appendChild(aEl);
      if (Array.isArray(e.citations) && e.citations.length) {
        const row = document.createElement('div');
        for (const c of e.citations) {
          const chip = document.createElement('span'); chip.className = 'ai-cite';
          chip.textContent = '↗ ' + String(c).slice(0, 6);
          chip.addEventListener('click', () => { els.drawerAi.hidden = true; scrollToMessage(c); });
          row.appendChild(chip);
        }
        wrap.appendChild(row);
      }
      els.aiResults.appendChild(wrap);
    }
  } catch (err) { /* ignore */ }
}

function scrollToMessage(id) {
  const node = els.msgList.querySelector(`[data-msg-id="${cssEscape(id)}"]`);
  if (!node) return;
  node.scrollIntoView({ block: 'center', behavior: 'smooth' });
  node.classList.add('flash');
  setTimeout(() => node.classList.remove('flash'), 1200);
}

// ---------- live streams ----------
els.btnLivePage.addEventListener('click', async () => {
  els.drawerLive.hidden = false;
  els.liveList.replaceChildren(loadingDiv('加载中…'));
  try {
    const list = await api.listStreams();
    els.liveList.replaceChildren();
    if (!list?.length) { els.liveList.appendChild(mutedDiv('当前无直播。')); return; }
    for (const s of list) {
      const card = document.createElement('div'); card.className = 'live-card';
      const video = document.createElement('video');
      video.controls = true; video.playsInline = true; video.muted = true;
      const src = `/hls/${s.id}/index.m3u8`;
      if (window.MediaSource && video.canPlayType('application/vnd.apple.mpegurl')) {
        video.src = src;
      } else {
        // Fallback: just show a placeholder div.
        video.style.display = 'none';
        const ph = document.createElement('div'); ph.className = 'live-thumb';
        ph.style.cssText = 'display:grid;place-items:center;color:#888;';
        ph.textContent = '当前浏览器不支持原生 HLS';
        card.appendChild(ph);
      }
      card.appendChild(video);
      const body = document.createElement('div'); body.className = 'live-card-body';
      const title = document.createElement('div'); title.className = 'live-title';
      title.textContent = s.title || '(no title)';
      const status = document.createElement('span'); status.className = 'live-status'; status.textContent = 'LIVE';
      title.appendChild(status);
      const sub = document.createElement('div'); sub.className = 'live-sub';
      sub.textContent = `${s.protocol} · ${s.id.slice(0, 6)}`;
      body.appendChild(title); body.appendChild(sub);
      card.appendChild(body);
      els.liveList.appendChild(card);
    }
  } catch (err) {
    els.liveList.replaceChildren(mutedDiv(`加载失败:${err.message}`));
  }
});

els.btnGoLive.addEventListener('click', () => openModal(els.modalGoLive));
els.formGoLive.addEventListener('submit', async (e) => {
  e.preventDefault();
  const fd = new FormData(els.formGoLive);
  const title = String(fd.get('title') || '').trim();
  const protocol = String(fd.get('protocol') || 'rtmp');
  setBusy(els.formGoLive, true);
  try {
    const res = await api.createStream({ title, protocol, room_id: state.currentRoomId });
    closeModal(els.modalGoLive);
    els.streamIngest.value = res.ingest_url;
    els.streamHls.value = res.hls_url;
    openModal(els.modalStreamInfo);
    els.formGoLive.reset();
  } catch (err) {
    toast(`创建失败:${err.message}`, 'error');
  } finally { setBusy(els.formGoLive, false); }
});

// ---------- emoji picker ----------
const EMOJIS = ['👍','❤️','😂','🎉','🚀','🔥','👀','🤔','✅','❌','💯','🙏','👏','😎','😢','😡','💪','🧠','🤖','✨'];
let emojiPop = null;
function openEmojiPicker(anchor, onPick) {
  closeEmojiPicker();
  const pop = document.createElement('div'); pop.className = 'emoji-pop';
  for (const e of EMOJIS) {
    const b = document.createElement('button'); b.textContent = e;
    b.addEventListener('click', () => { onPick(e); closeEmojiPicker(); });
    pop.appendChild(b);
  }
  document.body.appendChild(pop);
  const r = anchor.getBoundingClientRect();
  pop.style.top = `${r.bottom + 6}px`;
  pop.style.left = `${Math.min(window.innerWidth - 300, r.left)}px`;
  emojiPop = pop;
  setTimeout(() => document.addEventListener('click', onDocClick, { once: true }), 0);
}
function onDocClick(e) {
  if (emojiPop && !emojiPop.contains(e.target)) closeEmojiPicker();
}
function closeEmojiPicker() { if (emojiPop?.parentNode) emojiPop.parentNode.removeChild(emojiPop); emojiPop = null; }

// ---------- 1:1 call (WebRTC P2P) ----------
els.btnCallAudio.addEventListener('click', () => startCall('audio'));
els.btnCallVideo.addEventListener('click', () => startCall('video'));
els.btnCallGroup.addEventListener('click', () => startGroupCall('video'));
els.gcallLeave.addEventListener('click', () => leaveGroupCall());
els.gcallMute.addEventListener('click', () => gcallToggleTrack('audio'));
els.gcallCam.addEventListener('click', () => gcallToggleTrack('video'));
els.callEnd.addEventListener('click', () => endCall('hangup'));
els.callMute.addEventListener('click', () => toggleTrack('audio'));
els.callCam.addEventListener('click', () => toggleTrack('video'));
els.callCc.addEventListener('click', () => toggleCaptions());
els.callCcLang.addEventListener('change', () => { if (state.call) state.call.targetLang = els.callCcLang.value || null; });

function toggleTrack(kind) {
  const s = state.call?.localStream;
  if (!s) return;
  for (const t of s.getTracks()) if (t.kind === kind) t.enabled = !t.enabled;
}

async function startCall(kind) {
  if (!state.currentRoomId) { toast('请选择房间', 'error'); return; }
  if (state.call) { toast('已在通话中', 'error'); return; }
  try {
    const localStream = await navigator.mediaDevices.getUserMedia({
      audio: true, video: kind === 'video',
    });
    state.call = {
      id: null, roomId: state.currentRoomId, kind,
      pc: null, localStream, remoteStream: null,
    };
    els.callLocal.srcObject = localStream;
    els.callOverlay.hidden = false;
    const pc = makePeer();
    state.call.pc = pc;
    for (const t of localStream.getTracks()) pc.addTrack(t, localStream);
    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    // server creates the call session + fans out invite via NATS.
    ws.callInvite(state.currentRoomId, kind, offer.sdp);
  } catch (err) {
    toast(`通话失败:${err.message}`, 'error');
    endCall('error');
  }
}

function makePeer() {
  const cfg = state.rtcConfig
    ? { iceServers: state.rtcConfig.ice_servers || state.rtcConfig.iceServers }
    : { iceServers: [{ urls: 'stun:stun.l.google.com:19302' }] };
  const pc = new RTCPeerConnection(cfg);
  pc.addEventListener('icecandidate', (e) => {
    if (!e.candidate || !state.call) return;
    ws.callIce(state.call.id, state.call.roomId, state.call.peer, e.candidate.toJSON());
  });
  pc.addEventListener('track', (e) => {
    if (!state.call) return;
    state.call.remoteStream = e.streams[0];
    els.callRemote.srcObject = state.call.remoteStream;
  });
  pc.addEventListener('connectionstatechange', () => {
    if (pc.connectionState === 'failed' || pc.connectionState === 'disconnected') endCall(pc.connectionState);
  });
  return pc;
}

async function handleCall(event) {
  const op = event?.op;
  // ---- group-call (P6 mesh) routing ----
  if (op === 'roster') return gcallOnRoster(event);
  if (op === 'join') return gcallOnJoin(event);
  if (op === 'leave') return gcallOnLeave(event);
  if (op === 'offer') return gcallOnOffer(event);
  // answer/ice belonging to the active group call route to the mesh handlers
  if (state.gcall && event.call_id === state.gcall.id && (op === 'answer' || op === 'ice')) {
    return op === 'answer' ? gcallOnAnswer(event) : gcallOnIce(event);
  }
  if (op === 'invite') {
    if (event.from === state.me?.id) {
      state.call = state.call || { id: event.call_id, roomId: event.room_id };
      state.call.id = event.call_id;
      state.call.peer = (event.to || []).find((p) => p !== state.me?.id) || event.to?.[0];
      return;
    }
    if (state.call) return; // already in another call
    const callKind = event.call_kind || event.kind; // server renamed kind→call_kind on the wire
    if (!confirm(`收到 ${callKind} 通话邀请,接听?`)) {
      ws.callEnd(event.call_id, event.room_id, 'declined');
      return;
    }
    try {
      const localStream = await navigator.mediaDevices.getUserMedia({ audio: true, video: callKind === 'video' });
      state.call = {
        id: event.call_id, roomId: event.room_id, kind: callKind,
        pc: null, localStream, remoteStream: null, peer: event.from,
      };
      els.callLocal.srcObject = localStream;
      els.callOverlay.hidden = false;
      const pc = makePeer();
      state.call.pc = pc;
      for (const t of localStream.getTracks()) pc.addTrack(t, localStream);
      await pc.setRemoteDescription({ type: 'offer', sdp: event.sdp });
      const ans = await pc.createAnswer();
      await pc.setLocalDescription(ans);
      ws.callAnswer(event.call_id, event.room_id, event.from, ans.sdp);
    } catch (err) {
      toast(`接听失败:${err.message}`, 'error');
      ws.callEnd(event.call_id, event.room_id, 'gum_failed');
      endCall('error');
    }
  } else if (op === 'answer') {
    if (!state.call || state.call.id !== event.call_id) return;
    state.call.peer = event.from;
    try { await state.call.pc.setRemoteDescription({ type: 'answer', sdp: event.sdp }); }
    catch (err) { toast(`SDP 失败:${err.message}`, 'error'); }
  } else if (op === 'ice') {
    if (!state.call || state.call.id !== event.call_id) return;
    try { await state.call.pc.addIceCandidate(event.candidate); }
    catch (err) { console.warn('addIceCandidate', err); }
  } else if (op === 'end') {
    if (state.call && state.call.id === event.call_id) endCall('remote_end');
  } else if (op === 'caption') {
    if (!state.call || state.call.id !== event.call_id) return;
    if (event.from === state.me?.id) return; // our own captions render locally
    const name = state.participants.get(event.from)?.display_name || '对方';
    renderCaption(event.from, name, event.text, event.translated, event.is_final);
  }
}

function endCall(reason) {
  const c = state.call;
  if (!c) { els.callOverlay.hidden = true; return; }
  stopRecognition();
  try { c.pc?.close(); } catch {}
  try { c.localStream?.getTracks().forEach((t) => t.stop()); } catch {}
  if (c.id && c.roomId) {
    try { ws.callEnd(c.id, c.roomId, reason || 'hangup'); } catch {}
  }
  els.callLocal.srcObject = null;
  els.callRemote.srcObject = null;
  els.callCaptions.replaceChildren();
  els.callCaptions.hidden = true;
  els.callCc.classList.remove('active');
  els.callOverlay.hidden = true;
  state.call = null;
}

// ---------- live captions (P3 实时字幕翻译) ----------

function toggleCaptions() {
  if (!state.call) return;
  if (state.call.recog) { stopRecognition(); return; }
  const SR = window.SpeechRecognition || window.webkitSpeechRecognition;
  if (!SR) { toast('当前浏览器不支持语音识别(建议 Chrome/Edge)', 'error'); return; }
  const srcLang = navigator.language || 'zh-CN';
  state.call.srcLang = srcLang;
  state.call.targetLang = els.callCcLang.value || null;
  let recog;
  try { recog = new SR(); } catch (e) { toast('字幕启动失败', 'error'); return; }
  recog.lang = srcLang;
  recog.continuous = true;
  recog.interimResults = true;
  recog.addEventListener('result', (e) => onSpeech(e));
  recog.addEventListener('error', (e) => { if (e.error !== 'no-speech') console.warn('[speech]', e.error); });
  recog.addEventListener('end', () => {
    // SpeechRecognition stops itself periodically; restart while captions are on.
    if (state.call && state.call.recog === recog) {
      try { recog.start(); } catch {}
    }
  });
  try { recog.start(); } catch (e) { toast('字幕启动失败', 'error'); return; }
  state.call.recog = recog;
  els.callCc.classList.add('active');
  els.callCaptions.hidden = false;
}

function stopRecognition() {
  const c = state.call;
  if (c?.recog) {
    const r = c.recog;
    c.recog = null; // prevent the 'end' handler from restarting
    try { r.stop(); } catch {}
  }
  els.callCc?.classList.remove('active');
}

function onSpeech(e) {
  if (!state.call) return;
  for (let i = e.resultIndex; i < e.results.length; i++) {
    const res = e.results[i];
    const text = (res[0]?.transcript || '').trim();
    if (!text) continue;
    const isFinal = res.isFinal;
    // Render my own caption locally; broadcast (server translates final lines).
    renderCaption('me', '我', text, null, isFinal);
    ws.callCaption(state.call.id, state.call.roomId, text, state.call.srcLang, isFinal, state.call.targetLang);
  }
}

// Maintain a rolling subtitle list; one in-progress (interim) line per speaker.
function renderCaption(key, name, text, translated, isFinal) {
  const box = els.callCaptions;
  if (!box) return;
  box.hidden = false;
  state.call._capLines = state.call._capLines || new Map();
  let line = state.call._capLines.get(key);
  if (!line) {
    line = document.createElement('div');
    line.className = 'cap-line';
    const who = document.createElement('span');
    who.className = 'cap-who';
    who.textContent = `${name}: `;
    const orig = document.createElement('span');
    orig.className = 'cap-text';
    const tr = document.createElement('div');
    tr.className = 'cap-tr';
    line.appendChild(who);
    line.appendChild(orig);
    line.appendChild(tr);
    box.appendChild(line);
    line._orig = orig;
    line._tr = tr;
    state.call._capLines.set(key, line);
  }
  line._orig.textContent = text;
  if (translated) line._tr.textContent = translated;
  line.classList.toggle('interim', !isFinal);
  if (isFinal) state.call._capLines.delete(key); // next utterance starts a fresh line
  while (box.childElementCount > 5) box.removeChild(box.firstElementChild);
  box.scrollTop = box.scrollHeight;
}

// ---------- group call (P6 mesh) ----------
// Full-mesh: each participant holds one RTCPeerConnection per other participant.
// Glare-free pairing — for any pair, the peer with the smaller participant id
// creates the offer. Media is browser-native P2P; the server only relays
// signaling + tracks the roster.

function rtcIceConfig() {
  return state.rtcConfig
    ? { iceServers: state.rtcConfig.ice_servers || state.rtcConfig.iceServers }
    : { iceServers: [{ urls: 'stun:stun.l.google.com:19302' }] };
}

const gcallPrompted = new Set(); // call_ids we've already offered to join

// Start a new group call in the current room (callId === null → server creates).
function startGroupCall(kind) { return enterGroupCall(state.currentRoomId, kind, null); }

// Join an existing group call we were invited to.
function joinGroupCall(roomId, kind, callId) { return enterGroupCall(roomId, kind, callId); }

async function enterGroupCall(roomId, kind, callId) {
  if (!roomId) { toast('请选择房间', 'error'); return; }
  if (state.gcall) { toast('已在群通话中', 'error'); return; }
  if (state.call) { toast('请先结束 1:1 通话', 'error'); return; }
  try {
    const localStream = await navigator.mediaDevices.getUserMedia({ audio: true, video: kind === 'video' });
    state.gcall = { id: callId, roomId, kind, localStream, peers: new Map() };
    els.gcallGrid.replaceChildren();
    addGcallTile('me', '我', localStream, true);
    els.gcallOverlay.hidden = false;
    updateGcallCount();
    ws.callJoin(roomId, kind, callId); // null → create; else join existing → server sends roster
  } catch (err) {
    toast(`群通话失败:${err.message}`, 'error');
    leaveGroupCall();
  }
}

function promptJoinGroupCall(ev) {
  if (gcallPrompted.has(ev.call_id)) return;
  gcallPrompted.add(ev.call_id);
  const name = state.participants.get(ev.from)?.display_name || '有人';
  const kind = ev.call_kind || 'video';
  if (confirm(`${name} 发起了群通话,加入?`)) {
    joinGroupCall(ev.room_id, kind, ev.call_id);
  }
}

function gcallPeer(peerId) {
  if (!state.gcall) return null;
  let entry = state.gcall.peers.get(peerId);
  if (entry) return entry;
  const pc = new RTCPeerConnection(rtcIceConfig());
  pc.addEventListener('icecandidate', (e) => {
    if (e.candidate && state.gcall) ws.callIce(state.gcall.id, state.gcall.roomId, peerId, e.candidate.toJSON());
  });
  pc.addEventListener('track', (e) => {
    const name = state.participants.get(peerId)?.display_name || peerId.slice(0, 6);
    addGcallTile(peerId, name, e.streams[0], false);
  });
  pc.addEventListener('connectionstatechange', () => {
    if (pc.connectionState === 'failed' || pc.connectionState === 'closed') removeGcallPeer(peerId);
  });
  for (const t of state.gcall.localStream.getTracks()) pc.addTrack(t, state.gcall.localStream);
  entry = { pc };
  state.gcall.peers.set(peerId, entry);
  updateGcallCount();
  return entry;
}

async function gcallOfferTo(peerId) {
  const entry = gcallPeer(peerId);
  if (!entry) return;
  try {
    const offer = await entry.pc.createOffer();
    await entry.pc.setLocalDescription(offer);
    ws.callOffer(state.gcall.id, state.gcall.roomId, peerId, offer.sdp);
  } catch (err) { console.warn('[gcall offer]', err); }
}

async function gcallOnRoster(ev) {
  if (!state.gcall) return;
  state.gcall.id = ev.call_id;
  for (const m of (ev.members || [])) {
    if (m === state.me?.id) continue;
    // lower id offers; otherwise wait for their offer
    if (String(state.me?.id) < String(m)) await gcallOfferTo(m);
  }
}

async function gcallOnJoin(ev) {
  if (ev.from === state.me?.id) return;
  if (!state.gcall) {
    // Invited to a group call we're not in yet — offer to join.
    promptJoinGroupCall(ev);
    return;
  }
  if (ev.call_id !== state.gcall.id) return;
  if (String(state.me?.id) < String(ev.from)) await gcallOfferTo(ev.from);
}

async function gcallOnOffer(ev) {
  if (!state.gcall || ev.call_id !== state.gcall.id) return;
  if (ev.to !== state.me?.id) return;
  const entry = gcallPeer(ev.from);
  if (!entry) return;
  try {
    await entry.pc.setRemoteDescription({ type: 'offer', sdp: ev.sdp });
    const ans = await entry.pc.createAnswer();
    await entry.pc.setLocalDescription(ans);
    ws.callAnswer(state.gcall.id, state.gcall.roomId, ev.from, ans.sdp);
  } catch (err) { console.warn('[gcall offer-in]', err); }
}

async function gcallOnAnswer(ev) {
  const entry = state.gcall?.peers.get(ev.from);
  if (!entry) return;
  try { await entry.pc.setRemoteDescription({ type: 'answer', sdp: ev.sdp }); }
  catch (err) { console.warn('[gcall answer]', err); }
}

async function gcallOnIce(ev) {
  const entry = state.gcall?.peers.get(ev.from);
  if (!entry) return;
  try { await entry.pc.addIceCandidate(ev.candidate); }
  catch (err) { console.warn('[gcall ice]', err); }
}

function gcallOnLeave(ev) {
  if (!state.gcall || ev.call_id !== state.gcall.id) return;
  removeGcallPeer(ev.from);
}

function removeGcallPeer(peerId) {
  const entry = state.gcall?.peers.get(peerId);
  if (!entry) return;
  try { entry.pc.close(); } catch {}
  state.gcall.peers.delete(peerId);
  const tile = els.gcallGrid.querySelector(`[data-peer="${cssEscape(peerId)}"]`);
  if (tile) tile.remove();
  updateGcallCount();
}

function gcallToggleTrack(kind) {
  const s = state.gcall?.localStream;
  if (!s) return;
  for (const t of s.getTracks()) if (t.kind === kind) t.enabled = !t.enabled;
}

function leaveGroupCall() {
  const g = state.gcall;
  if (!g) { els.gcallOverlay.hidden = true; return; }
  if (g.id) { try { ws.callLeave(g.id, g.roomId); } catch {} }
  for (const [, entry] of g.peers) { try { entry.pc.close(); } catch {} }
  try { g.localStream?.getTracks().forEach((t) => t.stop()); } catch {}
  els.gcallGrid.replaceChildren();
  els.gcallOverlay.hidden = true;
  state.gcall = null;
}

function addGcallTile(peerId, label, stream, muted) {
  let tile = els.gcallGrid.querySelector(`[data-peer="${cssEscape(peerId)}"]`);
  if (!tile) {
    tile = document.createElement('div');
    tile.className = 'call-tile';
    tile.dataset.peer = peerId;
    const v = document.createElement('video');
    v.autoplay = true; v.playsInline = true; v.muted = !!muted;
    const lab = document.createElement('span');
    lab.className = 'label';
    lab.textContent = label;
    tile.appendChild(v); tile.appendChild(lab);
    els.gcallGrid.appendChild(tile);
  }
  tile.querySelector('video').srcObject = stream;
  updateGcallCount();
}

function updateGcallCount() {
  if (!state.gcall) return;
  els.gcallCount.textContent = `${state.gcall.peers.size + 1} 人`;
}

// ---------- utilities ----------
function cssEscape(s) {
  if (window.CSS && CSS.escape) return CSS.escape(s);
  return String(s).replace(/[^a-zA-Z0-9_-]/g, '\\$&');
}
function forceReauth() {
  toast('会话过期,请重新登录', 'error');
  auth.clear();
  state.me = null;
  ws.close();
  showAuth();
}
function loadingDiv(text) { const d = document.createElement('div'); d.className = 'muted'; d.style.cssText='padding:12px;text-align:center;'; d.textContent = text; return d; }
function mutedDiv(text) { const d = document.createElement('div'); d.className = 'muted'; d.style.cssText='padding:12px;text-align:center;font-size:12.5px;'; d.textContent = text; return d; }
function formatTime(iso) {
  if (!iso) return '';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '';
  const yy = d.getFullYear(), mm = String(d.getMonth()+1).padStart(2,'0'), dd = String(d.getDate()).padStart(2,'0');
  const hh = String(d.getHours()).padStart(2,'0'), mi = String(d.getMinutes()).padStart(2,'0');
  return `${yy}-${mm}-${dd} ${hh}:${mi}`;
}

// ---------- bootstrap ----------
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
