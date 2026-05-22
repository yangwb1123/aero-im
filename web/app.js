// app.js — Aero IM debug client entry point.
// Wires up auth flow, room list, message stream, composer, and WS events.

import { api, auth, ApiError } from './api.js';
import { WsClient } from './ws.js';
import {
  renderMessage,
  renderRoomItem,
  renderOnlineItem,
  initialOf,
  formatHM,
  toast,
} from './render.js';

// ---------- state ----------
const state = {
  me: null,                 // { id, display_name, email, ... }
  rooms: new Map(),         // room_id -> room
  participants: new Map(),  // participant_id -> { id, display_name, ... }
  currentRoomId: null,
  messagesByRoom: new Map(),// room_id -> Array<message>
  loadingHistory: false,
  reachedTop: new Set(),    // room_ids where we've already exhausted history
  pendingByTempId: new Map(),// tempId -> message-shaped object
};

const ws = new WsClient();

// ---------- DOM ----------
const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => Array.from(root.querySelectorAll(sel));

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

  msgScroll: $('#msg-scroll'),
  msgList: $('#msg-list'),
  msgEmpty: $('#msg-empty'),

  composer: $('#composer'),
  composerInput: $('#composer-input'),
  composerSend: $('#composer-send'),

  onlineList: $('#online-list'),
  onlineCount: $('#online-count'),

  modalNewRoom: $('#modal-new-room'),
  formNewRoom: $('#form-new-room'),
  modalAddMember: $('#modal-add-member'),
  formAddMember: $('#form-add-member'),
};

// ---------- view switching ----------
function showAuth() {
  els.viewAuth.hidden = false;
  els.viewChat.hidden = true;
}
function showChat() {
  els.viewAuth.hidden = true;
  els.viewChat.hidden = false;
}

// ---------- auth tabs ----------
for (const t of els.tabs) {
  t.addEventListener('click', () => {
    const name = t.dataset.tab;
    for (const x of els.tabs) x.classList.toggle('active', x === t);
    for (const p of els.tabPanels) p.hidden = (p.dataset.tabPanel !== name);
  });
}

// ---------- form busy helper ----------
function setBusy(form, busy) {
  for (const ctrl of form.querySelectorAll('input, button, select, textarea')) {
    ctrl.disabled = !!busy;
  }
}

// ---------- auth submit ----------
els.formLogin.addEventListener('submit', async (e) => {
  e.preventDefault();
  const fd = new FormData(els.formLogin);
  const email = String(fd.get('email') || '').trim();
  const password = String(fd.get('password') || '');
  if (!email || !password) return;
  setBusy(els.formLogin, true);
  try {
    const res = await api.login({ email, password });
    onAuthSuccess(res);
  } catch (err) {
    toast(err.message || '登录失败', 'error');
  } finally {
    setBusy(els.formLogin, false);
  }
});

els.formRegister.addEventListener('submit', async (e) => {
  e.preventDefault();
  const fd = new FormData(els.formRegister);
  const email = String(fd.get('email') || '').trim();
  const password = String(fd.get('password') || '');
  const display_name = String(fd.get('display_name') || '').trim();
  if (!email || !password || !display_name) return;
  setBusy(els.formRegister, true);
  try {
    const res = await api.register({ email, password, display_name });
    onAuthSuccess(res);
  } catch (err) {
    toast(err.message || '注册失败', 'error');
  } finally {
    setBusy(els.formRegister, false);
  }
});

function onAuthSuccess(res) {
  if (!res || !res.access_token || !res.participant) {
    toast('服务端返回不完整', 'error');
    return;
  }
  auth.setSession(res.access_token, res.refresh_token, res.participant.id);
  state.me = res.participant;
  state.participants.set(res.participant.id, res.participant);
  enterChat();
}

// ---------- logout ----------
els.btnLogout.addEventListener('click', () => {
  ws.close();
  auth.clear();
  state.me = null;
  state.currentRoomId = null;
  state.rooms.clear();
  state.messagesByRoom.clear();
  state.participants.clear();
  state.reachedTop.clear();
  els.roomList.replaceChildren();
  els.msgList.replaceChildren();
  els.onlineList.replaceChildren();
  showAuth();
});

// ---------- enter chat ----------
function enterChat() {
  showChat();
  const me = state.me;
  els.meName.textContent = me.display_name || '—';
  els.meEmail.textContent = me.email || me.id || '';
  els.meAvatar.textContent = initialOf(me.display_name || me.email);
  els.meAvatar.setAttribute('style', avatarStyleFromId(me.id));

  // connect ws
  ws.connect(auth.getToken());
  hookWs();
}

function avatarStyleFromId(id) {
  let h = 0;
  if (id) for (let i = 0; i < id.length; i++) h = (h * 31 + id.charCodeAt(i)) >>> 0;
  const hue = h % 360, hue2 = (hue + 40) % 360;
  return `background: linear-gradient(135deg, hsl(${hue} 70% 55%), hsl(${hue2} 70% 50%));`;
}

// ---------- ws wiring ----------
function hookWs() {
  ws.on('status', (s) => {
    els.wsDot.classList.remove('ws-up', 'ws-down', 'ws-wait');
    if (s === 'up') els.wsDot.classList.add('ws-up');
    else if (s === 'wait' || s === 'connecting') els.wsDot.classList.add('ws-wait');
    else els.wsDot.classList.add('ws-down');
    els.wsDot.title = `WS: ${s}`;
  });
  ws.on('open', () => {
    if (state.currentRoomId) ws.joinRoom(state.currentRoomId);
  });
  ws.on('msg:message', (frame) => handleIncomingMessage(frame.message));
  ws.on('msg:presence', (frame) => handlePresence(frame));
  ws.on('msg:error', (frame) => {
    toast(`服务端:${frame.msg || frame.code || 'error'}`, 'error');
  });
  ws.on('msg:pong', () => { /* keepalive */ });
}

function handleIncomingMessage(m) {
  if (!m || !m.id || !m.room_id) return;
  // try to replace a pending optimistic message: same sender + same blocks text + close-by time
  const isMine = m.sender_id === state.me?.id;
  if (isMine) {
    const matchKey = findPendingMatch(m);
    if (matchKey) {
      const tempNode = els.msgList.querySelector(`[data-msg-id="${cssEscape(matchKey)}"]`);
      const newNode = renderMessage(m, state.me?.id, state.participants);
      if (tempNode && tempNode.parentNode) tempNode.parentNode.replaceChild(newNode, tempNode);
      state.pendingByTempId.delete(matchKey);
      // replace in cache
      const arr = state.messagesByRoom.get(m.room_id) || [];
      const idx = arr.findIndex((x) => x.id === matchKey);
      if (idx >= 0) arr[idx] = m;
      else { arr.push(m); state.messagesByRoom.set(m.room_id, arr); }
      hideEmptyIfNeeded();
      return;
    }
  }
  // append fresh
  const arr = state.messagesByRoom.get(m.room_id) || [];
  if (arr.some((x) => x.id === m.id)) return; // dedupe
  arr.push(m);
  state.messagesByRoom.set(m.room_id, arr);
  if (m.room_id === state.currentRoomId) {
    appendMessageEl(renderMessage(m, state.me?.id, state.participants), { scroll: true });
    hideEmptyIfNeeded();
  }
}

function findPendingMatch(serverMsg) {
  // crude match: same sender, identical top-level text content within ±10s
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
  return blocks.map((b) => (b?.type === 'text' ? (b.content ?? '') : '')).join('');
}

function handlePresence(frame) {
  if (frame.room_id !== state.currentRoomId) return;
  const ids = Array.isArray(frame.online) ? frame.online : [];
  els.onlineList.replaceChildren();
  for (const pid of ids) {
    els.onlineList.appendChild(renderOnlineItem(pid, state.participants.get(pid)));
  }
  els.onlineCount.textContent = String(ids.length);
}

// ---------- room list ----------
function refreshRoomList() {
  els.roomList.replaceChildren();
  const list = Array.from(state.rooms.values());
  list.sort((a, b) => (a.name || a.id).localeCompare(b.name || b.id));
  if (!list.length) {
    const hint = document.createElement('div');
    hint.className = 'muted';
    hint.style.padding = '12px 14px';
    hint.style.fontSize = '12px';
    hint.textContent = '还没有房间。点 "+ 新建" 创建一个。';
    els.roomList.appendChild(hint);
    return;
  }
  for (const r of list) {
    const node = renderRoomItem(r, { active: r.id === state.currentRoomId });
    node.addEventListener('click', () => switchRoom(r.id));
    els.roomList.appendChild(node);
  }
}

function setActiveRoomVisual() {
  for (const el of els.roomList.querySelectorAll('.room-item')) {
    el.classList.toggle('active', el.dataset.roomId === state.currentRoomId);
  }
}

// ---------- switch / load room ----------
async function switchRoom(roomId) {
  if (state.currentRoomId === roomId) return;
  state.currentRoomId = roomId;
  setActiveRoomVisual();
  const room = state.rooms.get(roomId);
  els.roomName.textContent = room?.name || `Room ${roomId.slice(0, 6)}…`;
  els.roomMeta.textContent = `${room?.kind || 'room'} · ${roomId}`;
  els.btnAddMember.hidden = false;
  els.composer.hidden = false;
  els.composerSend.disabled = !els.composerInput.value.trim();

  // tell ws
  ws.joinRoom(roomId);
  els.onlineList.replaceChildren();
  els.onlineCount.textContent = '0';

  // load history
  els.msgList.replaceChildren();
  els.msgEmpty.hidden = true;
  if (!state.messagesByRoom.has(roomId)) {
    await loadHistory(roomId, { initial: true });
  } else {
    rerenderCurrentRoom();
  }
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
    // server returns most-recent first per spec; we want chronological asc.
    // Heuristic: if items are not already ascending by created_at, reverse.
    if (arr.length >= 2) {
      const t0 = Date.parse(arr[0].created_at || '') || 0;
      const t1 = Date.parse(arr[arr.length - 1].created_at || '') || 0;
      if (t0 > t1) arr.reverse();
    }

    if (arr.length < 100) state.reachedTop.add(roomId);

    if (initial) {
      state.messagesByRoom.set(roomId, arr);
      if (roomId === state.currentRoomId) {
        rerenderCurrentRoom();
        scrollToBottom();
      }
    } else {
      // prepend older history; preserve scroll position
      const merged = arr.concat(current);
      // dedupe by id
      const seen = new Set();
      const dedup = [];
      for (const m of merged) {
        if (!m || !m.id || seen.has(m.id)) continue;
        seen.add(m.id);
        dedup.push(m);
      }
      state.messagesByRoom.set(roomId, dedup);
      if (roomId === state.currentRoomId) {
        const prevHeight = els.msgScroll.scrollHeight;
        const prevTop = els.msgScroll.scrollTop;
        rerenderCurrentRoom();
        const newHeight = els.msgScroll.scrollHeight;
        els.msgScroll.scrollTop = prevTop + (newHeight - prevHeight);
      }
    }
  } catch (err) {
    if (err instanceof ApiError && err.status === 401) {
      forceReauth();
    } else {
      toast(`拉取历史失败:${err.message}`, 'error');
    }
  } finally {
    state.loadingHistory = false;
  }
}

function rerenderCurrentRoom() {
  const roomId = state.currentRoomId;
  els.msgList.replaceChildren();
  const arr = state.messagesByRoom.get(roomId) || [];
  for (const m of arr) {
    els.msgList.appendChild(renderMessage(m, state.me?.id, state.participants));
  }
  // append local pending for this room at the end
  for (const [, p] of state.pendingByTempId) {
    if (p.room_id === roomId) {
      els.msgList.appendChild(renderMessage(p, state.me?.id, state.participants, { pending: true }));
    }
  }
  els.msgEmpty.hidden = arr.length > 0 || state.pendingByTempId.size > 0;
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
  // double rAF to give layout a beat
  requestAnimationFrame(() => {
    els.msgScroll.scrollTop = els.msgScroll.scrollHeight;
    requestAnimationFrame(() => { els.msgScroll.scrollTop = els.msgScroll.scrollHeight; });
  });
}

// ---------- scroll for older history ----------
els.msgScroll.addEventListener('scroll', () => {
  if (!state.currentRoomId) return;
  if (els.msgScroll.scrollTop <= 40 && !state.loadingHistory) {
    const roomId = state.currentRoomId;
    if (!state.reachedTop.has(roomId) && (state.messagesByRoom.get(roomId)?.length || 0) > 0) {
      loadHistory(roomId);
    }
  }
});

// ---------- composer ----------
els.composerInput.addEventListener('input', () => {
  els.composerSend.disabled = !els.composerInput.value.trim();
  autoGrow(els.composerInput);
});
els.composerInput.addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
    e.preventDefault();
    submitComposer();
  }
});
els.composer.addEventListener('submit', (e) => {
  e.preventDefault();
  submitComposer();
});

function autoGrow(ta) {
  ta.style.height = 'auto';
  ta.style.height = Math.min(ta.scrollHeight, 180) + 'px';
}

function submitComposer() {
  if (!state.currentRoomId) return;
  const text = els.composerInput.value.replace(/\s+$/g, '');
  if (!text.trim()) return;

  const roomId = state.currentRoomId;
  const blocks = [{ type: 'text', content: text }];

  // optimistic
  const tempId = '_pending_' + crypto.randomUUID();
  const pending = {
    id: tempId,
    room_id: roomId,
    sender_id: state.me?.id,
    blocks,
    reply_to: null,
    metadata: {},
    created_at: new Date().toISOString(),
    edited_at: null,
    deleted_at: null,
  };
  state.pendingByTempId.set(tempId, pending);
  appendMessageEl(renderMessage(pending, state.me?.id, state.participants, { pending: true }), { scroll: true });
  hideEmptyIfNeeded();

  const ok = ws.sendMessage(roomId, blocks, null);
  if (!ok) {
    toast('WebSocket 未就绪,消息排队中…', 'info');
  }

  els.composerInput.value = '';
  els.composerSend.disabled = true;
  autoGrow(els.composerInput);
}

// ---------- new room modal ----------
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
  } finally {
    setBusy(els.formNewRoom, false);
  }
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
  } finally {
    setBusy(els.formAddMember, false);
  }
});

// ---------- modal helpers ----------
function openModal(m) { m.hidden = false; }
function closeModal(m) { m.hidden = true; }
for (const m of [els.modalNewRoom, els.modalAddMember]) {
  m.addEventListener('click', (e) => {
    if (e.target === m) closeModal(m);
    if (e.target instanceof HTMLElement && e.target.hasAttribute('data-close')) closeModal(m);
  });
}
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') {
    for (const m of [els.modalNewRoom, els.modalAddMember]) if (!m.hidden) closeModal(m);
  }
});

// ---------- utility ----------
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

// ---------- bootstrap ----------
(async function bootstrap() {
  const t = auth.getToken();
  if (!t) {
    showAuth();
    return;
  }
  try {
    const me = await api.me();
    if (!me || !me.id) throw new Error('invalid /me response');
    state.me = me;
    state.participants.set(me.id, me);
    enterChat();
  } catch (err) {
    console.warn('session check failed', err);
    auth.clear();
    showAuth();
  }
})();
