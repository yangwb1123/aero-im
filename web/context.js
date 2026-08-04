// context.js — shared front-end spine.
//
// 背景：app.js 曾是单文件巨石（>2000 行，超 JS HARD 线）。模块化时最大的难点
// 是大量函数共享同一份「活」状态：state（运行时数据）、ws（WsClient 单例）、
// els（缓存的 DOM 引用）、$/$$（查询助手）。在无打包器的原生 ESM SPA 里，
// 把这份共享脊柱放进一个独立模块、由各域模块 import，是让 app.js 安全瘦身、
// 又不复制状态的唯一干净办法（ESM 单例语义保证所有 import 拿到的是同一对象）。
//
// 本模块只放「无业务逻辑、无前向依赖」的叶子：共享单例 + DOM 查询助手 +
// 纯工具函数。任何依赖 auth/showAuth/render 等上层符号的逻辑都留在 app.js。

import { WsClient } from './ws.js';

// ---------- DOM helpers ----------
export const $ = (s, r = document) => r.querySelector(s);
export const $$ = (s, r = document) => Array.from(r.querySelectorAll(s));

// ---------- shared state ----------
export const state = {
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
  // Thread mute: per root-message-id boolean of whether the current user has
  // muted that thread. Lazily hydrated when the mute button is first hovered/
  // clicked, then kept in sync on each toggle so the bell glyph stays correct.
  threadMuted: new Map(),     // root_message_id -> bool (true = muted)
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

// ---------- ws singleton ----------
export const ws = new WsClient();

export function hasPendingForRoom(roomId) {
  for (const pending of state.pendingByTempId.values()) {
    if (pending.room_id === roomId) return true;
  }
  return false;
}

// ---------- cached DOM refs ----------
export const els = {
  viewAuth: $('#view-auth'),
  viewChat: $('#view-chat'),
  formLogin: $('#form-login'),
  formRegister: $('#form-register'),
  authTabs: $('.tabs'),
  authSsoOption: $('#auth-sso-option'),
  btnSso: $('#btn-sso'),
  authModeHint: $('#auth-mode-hint'),
  loginSecondFactor: $('#form-login input[name="second_factor"]'),
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
  btnNotif: $('#btn-notif'),
  notifBadge: $('#notif-badge'),

  msgScroll: $('#msg-scroll'),
  msgList: $('#msg-list'),
  msgEmpty: $('#msg-empty'),
  typingBar: $('#typing-bar'),

  composer: $('#composer'),
  composerInput: $('#composer-input'),
  composerSend: $('#composer-send'),
  mdToggle: $('#md-toggle'),
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

  drawerNotif: $('#drawer-notif'),
  notifList: $('#notif-list'),
  btnNotifReadAll: $('#btn-notif-read-all'),

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
  callShare: $('#call-share'),
  callEnd: $('#call-end'),
  callCaptions: $('#call-captions'),
  callCc: $('#call-cc'),
  callCcLang: $('#call-cc-lang'),
  gcallOverlay: $('#gcall-overlay'),
  gcallGrid: $('#gcall-grid'),
  gcallMute: $('#gcall-mute'),
  gcallCam: $('#gcall-cam'),
  gcallShare: $('#gcall-share'),
  gcallLeave: $('#gcall-leave'),
  gcallCount: $('#gcall-count'),
};

// ---------- pure leaf utilities ----------
export function cssEscape(s) {
  if (window.CSS && CSS.escape) return CSS.escape(s);
  return String(s).replace(/[^a-zA-Z0-9_-]/g, '\\$&');
}

export function loadingDiv(text) {
  const d = document.createElement('div');
  d.className = 'muted';
  d.style.cssText = 'padding:12px;text-align:center;';
  d.textContent = text;
  return d;
}

export function mutedDiv(text) {
  const d = document.createElement('div');
  d.className = 'muted';
  d.style.cssText = 'padding:12px;text-align:center;font-size:12.5px;';
  d.textContent = text;
  return d;
}

export function formatTime(iso) {
  if (!iso) return '';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '';
  const yy = d.getFullYear(), mm = String(d.getMonth() + 1).padStart(2, '0'), dd = String(d.getDate()).padStart(2, '0');
  const hh = String(d.getHours()).padStart(2, '0'), mi = String(d.getMinutes()).padStart(2, '0');
  return `${yy}-${mm}-${dd} ${hh}:${mi}`;
}

// Disable/enable every interactive control inside a form (submit guards).
export function setBusy(form, busy) {
  for (const c of form.querySelectorAll('input, button, select, textarea')) c.disabled = !!busy;
}

// Deterministic avatar gradient derived from a participant/room id.
export function avatarStyleFromId(id) {
  let h = 0;
  if (id) for (let i = 0; i < id.length; i++) h = (h * 31 + id.charCodeAt(i)) >>> 0;
  const hue = h % 360, hue2 = (hue + 40) % 360;
  return `background: linear-gradient(135deg, hsl(${hue} 70% 55%), hsl(${hue2} 70% 50%));`;
}

// ---------- modal helpers ----------
export function openModal(m) { m.hidden = false; }
export function closeModal(m) { m.hidden = true; }

// ---------- message navigation ----------
// Scroll a message into view and flash it. Shared by search / AI citations /
// notification inbox / thread jumps, so it lives on the shared spine to avoid
// circular imports between those domain modules.
export function scrollToMessage(id) {
  const node = els.msgList.querySelector(`[data-msg-id="${cssEscape(id)}"]`);
  if (!node) return;
  node.scrollIntoView({ block: 'center', behavior: 'smooth' });
  node.classList.add('flash');
  setTimeout(() => node.classList.remove('flash'), 1200);
}
