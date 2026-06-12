// api.js — thin HTTP wrapper around the Rust backend
// All endpoints are relative; expect the static page to be served from same origin
// (or use a reverse proxy that passes /api/* to the backend).

const TOKEN_KEY = 'aero_token';
const REFRESH_KEY = 'aero_refresh';
const PID_KEY = 'aero_pid';

export const auth = {
  getToken() { return localStorage.getItem(TOKEN_KEY); },
  getRefresh() { return localStorage.getItem(REFRESH_KEY); },
  getPid() { return localStorage.getItem(PID_KEY); },
  setSession(access, refresh, pid) {
    if (access) localStorage.setItem(TOKEN_KEY, access);
    if (refresh) localStorage.setItem(REFRESH_KEY, refresh);
    if (pid) localStorage.setItem(PID_KEY, pid);
  },
  clear() {
    localStorage.removeItem(TOKEN_KEY);
    localStorage.removeItem(REFRESH_KEY);
    localStorage.removeItem(PID_KEY);
  },
};

export class ApiError extends Error {
  constructor(status, body, message) {
    super(message || `HTTP ${status}`);
    this.status = status;
    this.body = body;
  }
}

async function request(method, path, { body, query, withAuth = true, raw = false } = {}) {
  const headers = { 'Accept': 'application/json' };
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  if (withAuth) {
    const t = auth.getToken();
    if (t) headers['Authorization'] = `Bearer ${t}`;
  }
  let url = path;
  if (query && Object.keys(query).length) {
    const usp = new URLSearchParams();
    for (const [k, v] of Object.entries(query)) {
      if (v === undefined || v === null || v === '') continue;
      usp.set(k, String(v));
    }
    const qs = usp.toString();
    if (qs) url += (path.includes('?') ? '&' : '?') + qs;
  }
  let resp;
  try {
    if (raw && body !== undefined) {
      delete headers['Content-Type']; // let browser set multipart boundary
    }
    resp = await fetch(url, {
      method,
      headers,
      body: raw ? body : body !== undefined ? JSON.stringify(body) : undefined,
    });
  } catch (e) {
    throw new ApiError(0, null, `网络错误:${e.message}`);
  }
  if (resp.status === 204) return null;
  const ct = resp.headers.get('content-type') || '';
  const data = ct.includes('application/json') ? await resp.json().catch(() => null) : await resp.text();
  if (!resp.ok) {
    const msg = (data && typeof data === 'object' && (data.message || data.error || data.msg)) || `HTTP ${resp.status}`;
    throw new ApiError(resp.status, data, msg);
  }
  return data;
}

export const api = {
  register({ email, password, display_name }) {
    return request('POST', '/api/auth/register', {
      body: { email, password, display_name },
      withAuth: false,
    });
  },
  login({ email, password }) {
    return request('POST', '/api/auth/login', {
      body: { email, password },
      withAuth: false,
    });
  },
  me() {
    return request('GET', '/api/me');
  },
  updateMe({ display_name, avatar_url } = {}) {
    const body = {};
    if (display_name !== undefined) body.display_name = display_name;
    if (avatar_url !== undefined) body.avatar_url = avatar_url;
    return request('PATCH', '/api/me', { body });
  },
  createRoom({ kind, name }) {
    const body = { kind };
    if (name && name.trim()) body.name = name.trim();
    return request('POST', '/api/rooms', { body });
  },
  listRooms() {
    return request('GET', '/api/rooms');
  },
  addMember(roomId, participantId) {
    return request('POST', `/api/rooms/${encodeURIComponent(roomId)}/members`, {
      body: { participant_id: participantId },
    });
  },
  // `before` pages backward (history); `since` pages forward (catch-up after a
  // truncated WS backfill / resync — ROADMAP v3 方向一). Mutually exclusive.
  listMessages(roomId, { before, since, limit = 100 } = {}) {
    return request('GET', `/api/rooms/${encodeURIComponent(roomId)}/messages`, {
      query: { before, since, limit },
    });
  },
  editMessage(id, blocks) {
    return request('PATCH', `/api/messages/${encodeURIComponent(id)}`, { body: { blocks } });
  },
  deleteMessage(id) {
    return request('DELETE', `/api/messages/${encodeURIComponent(id)}`);
  },
  toggleReaction(messageId, emoji) {
    return request('POST', `/api/messages/${encodeURIComponent(messageId)}/reactions`, {
      body: { emoji },
    });
  },
  reactionsBatch(messageIds) {
    return request('POST', '/api/messages/reactions', { body: { message_ids: messageIds } });
  },
  markRead(roomId, lastMessageId) {
    return request('POST', `/api/rooms/${encodeURIComponent(roomId)}/read`, {
      body: { last_message_id: lastMessageId },
    });
  },
  listReceipts(roomId) {
    return request('GET', `/api/rooms/${encodeURIComponent(roomId)}/receipts`);
  },
  // Messages edited or deleted since the RFC3339 `since` instant — the
  // change-replay companion to listMessages's NEW-message backfill, so a client
  // that was offline during an edit/delete converges on it (ROADMAP 方向一).
  roomChanges(roomId, since, { limit = 200 } = {}) {
    return request('GET', `/api/rooms/${encodeURIComponent(roomId)}/changes`, {
      query: { since, limit },
    });
  },
  search(roomId, { query, limit = 20, mode = 'auto' } = {}) {
    return request('POST', `/api/rooms/${encodeURIComponent(roomId)}/search`, {
      body: { query, limit, mode },
    });
  },
  uploadBlob(file) {
    const fd = new FormData();
    fd.append('file', file, file.name || 'file');
    return request('POST', '/api/blobs', { body: fd, raw: true });
  },
  blobUrl(id) {
    return `/api/blobs/${encodeURIComponent(id)}`;
  },
  aiSummarize(roomId, lastN = 50) {
    return request('POST', '/api/ai/summarize', { body: { room_id: roomId, last_n: lastN } });
  },
  aiAsk(roomId, question, k = 8) {
    return request('POST', '/api/ai/ask', { body: { room_id: roomId, question, k } });
  },
  createStream({ title, room_id, protocol = 'rtmp' }) {
    return request('POST', '/api/streams', { body: { title, room_id, protocol } });
  },
  listStreams() {
    return request('GET', '/api/streams');
  },
  getStream(id) {
    return request('GET', `/api/streams/${encodeURIComponent(id)}`);
  },
  endStream(id) {
    return request('POST', `/api/streams/${encodeURIComponent(id)}/end`);
  },
  // ----- live interactivity (P4 弹幕 + 礼物) -----
  liveGifts() {
    return request('GET', '/api/live/gifts');
  },
  // `since` is the forward catch-up cursor (last chat-line id rendered), used to
  // continue a truncated WS danmaku replay (ROADMAP v3 方向一).
  streamChatList(id, limit = 50, since = null) {
    return request('GET', `/api/streams/${encodeURIComponent(id)}/chat`, { query: { limit, since } });
  },
  postStreamChat(id, body) {
    return request('POST', `/api/streams/${encodeURIComponent(id)}/chat`, { body: { body } });
  },
  streamGiftList(id, limit = 30) {
    return request('GET', `/api/streams/${encodeURIComponent(id)}/gifts`, { query: { limit } });
  },
  sendGift(id, giftId, qty = 1) {
    return request('POST', `/api/streams/${encodeURIComponent(id)}/gifts`, {
      body: { gift_id: giftId, qty },
    });
  },
  streamLeaderboard(id, limit = 10) {
    return request('GET', `/api/streams/${encodeURIComponent(id)}/leaderboard`, { query: { limit } });
  },
  rtcConfig() {
    return request('GET', '/api/rtc/config');
  },
  getParticipant(id) {
    return request('GET', `/api/participants/${encodeURIComponent(id)}`);
  },
  searchParticipants(q, limit = 20) {
    return request('GET', '/api/participants', { query: { q, limit } });
  },
  listRoomMembers(roomId) {
    return request('GET', `/api/rooms/${encodeURIComponent(roomId)}/members/list`);
  },
  createAgent({ display_name, kind = 'bot', avatar_url } = {}) {
    return request('POST', '/api/agents', { body: { display_name, kind, avatar_url } });
  },

  // ----- threads (Wave 1) -----
  thread(rootMessageId, { after, limit = 50 } = {}) {
    return request('GET', `/api/messages/${encodeURIComponent(rootMessageId)}/thread`, {
      query: { after, limit },
    });
  },

  // ----- notifications + unread (Wave 1) -----
  listNotifications({ unread = false, before, limit = 50 } = {}) {
    return request('GET', '/api/notifications', { query: { unread, before, limit } });
  },
  notificationCount() {
    return request('GET', '/api/notifications/count');
  },
  markNotificationsRead({ ids, all = false, room_id } = {}) {
    return request('POST', '/api/notifications/read', { body: { ids: ids || [], all, room_id } });
  },
  unread() {
    return request('GET', '/api/unread');
  },

  // ----- pinned messages (Wave 1) -----
  listPins(roomId) {
    return request('GET', `/api/rooms/${encodeURIComponent(roomId)}/pins`);
  },
  pinMessage(roomId, messageId) {
    return request('POST', `/api/rooms/${encodeURIComponent(roomId)}/pins`, {
      body: { message_id: messageId },
    });
  },
  unpinMessage(roomId, messageId) {
    return request('DELETE', `/api/rooms/${encodeURIComponent(roomId)}/pins/${encodeURIComponent(messageId)}`);
  },

  // ----- channel management (public/private, join/leave, archive, meta) -----
  listChannels(workspaceId) {
    return request('GET', `/api/workspaces/${encodeURIComponent(workspaceId)}/channels`);
  },
  joinRoom(roomId) {
    return request('POST', `/api/rooms/${encodeURIComponent(roomId)}/join`);
  },
  leaveRoom(roomId) {
    return request('POST', `/api/rooms/${encodeURIComponent(roomId)}/leave`);
  },
  archiveRoom(roomId, archived = true) {
    return request('POST', `/api/rooms/${encodeURIComponent(roomId)}/archive`, {
      body: { archived },
    });
  },
  updateChannel(roomId, { topic, description, is_private } = {}) {
    const body = {};
    if (topic !== undefined) body.topic = topic;
    if (description !== undefined) body.description = description;
    if (is_private !== undefined) body.is_private = is_private;
    return request('PATCH', `/api/rooms/${encodeURIComponent(roomId)}/channel`, { body });
  },
};
