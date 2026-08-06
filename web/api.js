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

/// Fetch timeout: if the server doesn't respond within 30s the promise rejects
/// with a network-like error so the caller sees a clear timeout message instead
/// of hanging indefinitely.
const REQUEST_TIMEOUT = 30_000;

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
    // AbortController-based timeout: if fetch hangs past REQUEST_TIMEOUT ms,
    // the signal fires and fetch rejects with an AbortError.
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), REQUEST_TIMEOUT);
    try {
      resp = await fetch(url, {
        method,
        headers,
        body: raw ? body : body !== undefined ? JSON.stringify(body) : undefined,
        signal: controller.signal,
      });
    } finally {
      clearTimeout(timer);
    }
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
  authConfig() {
    return request('GET', '/api/auth/config', { withAuth: false });
  },
  oidcLogin(id_token) {
    return request('POST', '/api/auth/oidc', {
      body: { id_token },
      withAuth: false,
    });
  },
  register({ email, password, display_name }) {
    return request('POST', '/api/auth/register', {
      body: { email, password, display_name },
      withAuth: false,
    });
  },
  login({ email, password, totp, recovery_code }) {
    const body = { email, password };
    if (totp && totp.trim()) body.totp = totp.trim();
    if (recovery_code && recovery_code.trim()) body.recovery_code = recovery_code.trim();
    return request('POST', '/api/auth/login', {
      body,
      withAuth: false,
    });
  },
  refresh(refresh_token) {
    return request('POST', '/api/auth/refresh', {
      body: { refresh_token },
      withAuth: false,
    });
  },
  logout(refresh_token) {
    return request('POST', '/api/auth/logout', {
      body: { refresh_token },
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
  twoFactorStatus() {
    return request('GET', '/api/me/2fa');
  },
  twoFactorEnroll() {
    return request('POST', '/api/me/2fa/enroll');
  },
  twoFactorVerify(code) {
    return request('POST', '/api/me/2fa/verify', { body: { code } });
  },
  twoFactorRecoveryCodes(code) {
    return request('POST', '/api/me/2fa/recovery-codes', { body: { code } });
  },
  twoFactorDisable(code = '') {
    return request('DELETE', '/api/me/2fa', { body: { code } });
  },

  // ----- enterprise security administration -----
  listWorkspaces() {
    return request('GET', '/api/workspaces');
  },
  listWorkspaceMembers(workspaceId) {
    return request('GET', `/api/workspaces/${encodeURIComponent(workspaceId)}/members`);
  },
  workspaceSecurity(workspaceId) {
    return request('GET', `/api/workspaces/${encodeURIComponent(workspaceId)}/security`);
  },
  setWorkspaceSecurity(workspaceId, require_2fa) {
    return request('PUT', `/api/workspaces/${encodeURIComponent(workspaceId)}/security`, {
      body: { require_2fa: Boolean(require_2fa) },
    });
  },
  workspaceStorageRegion(workspaceId) {
    return request(
      'GET',
      `/api/workspaces/${encodeURIComponent(workspaceId)}/storage-region`,
    );
  },
  setWorkspaceStorageRegion(workspaceId, storage_region) {
    return request(
      'PUT',
      `/api/workspaces/${encodeURIComponent(workspaceId)}/storage-region`,
      { body: { storage_region } },
    );
  },
  ipAllowlist(workspaceId) {
    return request('GET', `/api/workspaces/${encodeURIComponent(workspaceId)}/ip-allowlist`);
  },
  addIpAllowlist(workspaceId, { cidr, note } = {}) {
    const body = { cidr };
    if (note !== undefined) body.note = note;
    return request('POST', `/api/workspaces/${encodeURIComponent(workspaceId)}/ip-allowlist`, {
      body,
    });
  },
  removeIpAllowlist(workspaceId, cidr) {
    return request('DELETE', `/api/workspaces/${encodeURIComponent(workspaceId)}/ip-allowlist`, {
      body: { cidr },
    });
  },
  mintScimToken(workspaceId, label) {
    const body = {};
    if (label !== undefined) body.label = label;
    return request('POST', `/api/workspaces/${encodeURIComponent(workspaceId)}/scim/token`, {
      body,
    });
  },
  listScimTokens(workspaceId) {
    return request('GET', `/api/workspaces/${encodeURIComponent(workspaceId)}/scim/tokens`);
  },
  revokeScimToken(tokenId) {
    return request('DELETE', `/api/scim/tokens/${encodeURIComponent(tokenId)}`);
  },
  listAutoModRules(workspaceId) {
    return request('GET', `/api/workspaces/${encodeURIComponent(workspaceId)}/auto-mod-rules`);
  },
  createAutoModRule(workspaceId, { pattern, match_type = 'contains', action = 'block' }) {
    return request(
      'POST',
      `/api/workspaces/${encodeURIComponent(workspaceId)}/auto-mod-rules`,
      { body: { pattern, match_type, action } },
    );
  },
  deleteAutoModRule(workspaceId, ruleId) {
    const workspace = encodeURIComponent(workspaceId);
    const rule = encodeURIComponent(ruleId);
    return request('DELETE', `/api/workspaces/${workspace}/auto-mod-rules/${rule}`);
  },
  listSessions() {
    return request('GET', '/api/auth/sessions');
  },
  revokeSession(sessionId) {
    return request('DELETE', `/api/auth/sessions/${encodeURIComponent(sessionId)}`);
  },
  revokeOtherSessions(currentRefreshToken) {
    return request('POST', '/api/auth/sessions/revoke-others', {
      body: { current_refresh_token: currentRefreshToken },
    });
  },
  revokeMemberSessions(workspaceId, participantId) {
    const ws = encodeURIComponent(workspaceId);
    const participant = encodeURIComponent(participantId);
    return request('POST', `/api/workspaces/${ws}/members/${participant}/revoke-sessions`);
  },
  samlMetadata() {
    return request('GET', '/saml/metadata', { withAuth: false });
  },

  // ----- governance: audit trail + bot platform -----
  workspaceAudit(workspaceId, filters = {}) {
    return request('GET', `/api/workspaces/${encodeURIComponent(workspaceId)}/audit`, {
      query: filters,
    });
  },
  workspaceAuditCsv(workspaceId, filters = {}) {
    return request('GET', `/api/workspaces/${encodeURIComponent(workspaceId)}/audit/export`, {
      query: filters,
    });
  },
  listBots() {
    return request('GET', '/api/bots');
  },
  createBot({ name, icon_url, workspace_id } = {}) {
    const body = { name };
    if (icon_url) body.icon_url = icon_url;
    if (workspace_id) body.workspace_id = workspace_id;
    return request('POST', '/api/bots', { body });
  },
  rotateBotToken(botId) {
    return request('POST', `/api/bots/${encodeURIComponent(botId)}/token`);
  },
  listBotSubscriptions(botId) {
    return request('GET', `/api/bots/${encodeURIComponent(botId)}/subscriptions`);
  },
  createBotSubscription(botId, {
    event_type,
    filters = {},
    webhook_url,
  } = {}) {
    const body = { event_type, filters };
    if (webhook_url) body.webhook_url = webhook_url;
    return request('POST', `/api/bots/${encodeURIComponent(botId)}/subscriptions`, { body });
  },
  deleteBotSubscription(botId, subscriptionId) {
    const bot = encodeURIComponent(botId);
    const subscription = encodeURIComponent(subscriptionId);
    return request('DELETE', `/api/bots/${bot}/subscriptions/${subscription}`);
  },
  rotateBotSubscriptionSecret(botId, subscriptionId) {
    const bot = encodeURIComponent(botId);
    const subscription = encodeURIComponent(subscriptionId);
    return request('POST', `/api/bots/${bot}/subscriptions/${subscription}/secret`);
  },
  listBotDeliveries(botId, limit = 100) {
    return request('GET', `/api/bots/${encodeURIComponent(botId)}/deliveries`, {
      query: { limit },
    });
  },
  requeueBotDelivery(botId, deliveryId) {
    const bot = encodeURIComponent(botId);
    const delivery = encodeURIComponent(deliveryId);
    return request('POST', `/api/bots/${bot}/deliveries/${delivery}/requeue`);
  },

  createRoom({ kind, name }) {
    const body = { kind };
    if (name && name.trim()) body.name = name.trim();
    return request('POST', '/api/rooms', { body });
  },
  listRooms() {
    return request('GET', '/api/rooms');
  },
  listCanvases(roomId) {
    return request('GET', `/api/rooms/${encodeURIComponent(roomId)}/canvases`);
  },
  createCanvas(roomId, { title, blocks = [] }) {
    return request('POST', `/api/rooms/${encodeURIComponent(roomId)}/canvases`, {
      body: { title, blocks },
    });
  },
  getCanvas(roomId, canvasId) {
    return request(
      'GET',
      `/api/rooms/${encodeURIComponent(roomId)}/canvases/${encodeURIComponent(canvasId)}`,
    );
  },
  updateCanvas(roomId, canvasId, {
    title,
    blocks,
    expectedVersion,
    snapshotOpSeq,
  } = {}) {
    const body = {};
    if (title !== undefined) body.title = title;
    if (blocks !== undefined) body.blocks = blocks;
    if (expectedVersion !== undefined) body.expected_version = expectedVersion;
    if (snapshotOpSeq !== undefined) body.snapshot_op_seq = snapshotOpSeq;
    return request(
      'PUT',
      `/api/rooms/${encodeURIComponent(roomId)}/canvases/${encodeURIComponent(canvasId)}`,
      { body },
    );
  },
  deleteCanvas(roomId, canvasId) {
    return request(
      'DELETE',
      `/api/rooms/${encodeURIComponent(roomId)}/canvases/${encodeURIComponent(canvasId)}`,
    );
  },
  listCanvasOps(roomId, canvasId, { since = 0, limit = 500 } = {}) {
    return request(
      'GET',
      `/api/rooms/${encodeURIComponent(roomId)}/canvases/${encodeURIComponent(canvasId)}/ops`,
      { query: { since, limit } },
    );
  },
  appendCanvasOp(roomId, canvasId, op, clientOpId) {
    return request(
      'POST',
      `/api/rooms/${encodeURIComponent(roomId)}/canvases/${encodeURIComponent(canvasId)}/ops`,
      { body: { client_op_id: clientOpId, op } },
    );
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
  // Recall (撤回): author or room admin replaces the content with the system
  // placeholder. Rejects with 409 when already recalled/deleted — callers may
  // treat that as success (the desired end state is achieved).
  recallMessage(id) {
    return request('POST', `/api/messages/${encodeURIComponent(id)}/recall`);
  },
  toggleReaction(messageId, emoji) {
    return request('POST', `/api/messages/${encodeURIComponent(messageId)}/reactions`, {
      body: { emoji },
    });
  },
  reactionsBatch(messageIds) {
    return request('POST', '/api/messages/reactions', { body: { message_ids: messageIds } });
  },
  // Record an interaction with an interactive message block (Button click /
  // Select choice). `value` is the chosen Select option value (omit for a
  // value-less button click). Backend verifies the message actually carries a
  // component with `action_id` (404 otherwise) and broadcasts RoomEvent::Interaction.
  interactBlock(messageId, actionId, value = null) {
    const body = { action_id: actionId };
    if (value != null && value !== '') body.value = value;
    return request('POST', `/api/messages/${encodeURIComponent(messageId)}/interact`, { body });
  },
  markMessageSeen(messageId) {
    return request('POST', `/api/messages/${encodeURIComponent(messageId)}/seen`);
  },
  listMessageSeen(messageId) {
    return request('GET', `/api/messages/${encodeURIComponent(messageId)}/seen`);
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
  uploadBlob(file, roomId = null) {
    const fd = new FormData();
    fd.append('file', file, file.name || 'file');
    const path = roomId
      ? `/api/rooms/${encodeURIComponent(roomId)}/blobs`
      : '/api/blobs';
    return request('POST', path, { body: fd, raw: true });
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
  createAgent(roomId, { display_name, kind = 'bot', avatar_url } = {}) {
    const body = { room_id: roomId, display_name, kind };
    if (avatar_url) body.avatar_url = avatar_url;
    return request('POST', '/api/agents', { body });
  },

  // ----- polls -----
  // List a room's polls (newest first). `open=true` → only those still open.
  listPolls(roomId, { open = false } = {}) {
    return request('GET', `/api/rooms/${encodeURIComponent(roomId)}/polls`, {
      query: { open: open ? 'true' : undefined },
    });
  },
  createPoll(roomId, { question, options, multi = false, anonymous = false }) {
    return request('POST', `/api/rooms/${encodeURIComponent(roomId)}/polls`, {
      body: { question, options, multi, anonymous },
    });
  },
  // The poll + live tally: { poll, counts, total, voted, anonymous }.
  getPoll(pollId) {
    return request('GET', `/api/polls/${encodeURIComponent(pollId)}`);
  },
  // Vote: pass `optionIdx` (single-choice) OR `optionIdxs` (multi-choice).
  votePoll(pollId, { optionIdx, optionIdxs }) {
    const body = {};
    if (optionIdxs !== undefined) body.option_idxs = optionIdxs;
    else if (optionIdx !== undefined) body.option_idx = optionIdx;
    return request('POST', `/api/polls/${encodeURIComponent(pollId)}/vote`, { body });
  },
  closePoll(pollId) {
    return request('POST', `/api/polls/${encodeURIComponent(pollId)}/close`);
  },

  // ----- threads (Wave 1) -----
  thread(rootMessageId, { after, limit = 50 } = {}) {
    return request('GET', `/api/messages/${encodeURIComponent(rootMessageId)}/thread`, {
      query: { after, limit },
    });
  },
  // Mute the thread rooted at this message for the caller (stops reply
  // notification fan-out). Idempotent. Returns `{ root_message_id, muted: true }`.
  muteThread(rootMessageId) {
    return request('POST', `/api/threads/${encodeURIComponent(rootMessageId)}/mute`);
  },
  // Unmute the thread for the caller. Idempotent. Returns `{ ..., muted: false }`.
  unmuteThread(rootMessageId) {
    return request('DELETE', `/api/threads/${encodeURIComponent(rootMessageId)}/mute`);
  },
  // Participants who have muted this thread. Returns `{ root_message_id, muters }`.
  // The backend exposes no per-user "threads I muted" query, so we derive our own
  // mute state by checking whether our pid is in `muters`.
  threadMuters(rootMessageId) {
    return request('GET', `/api/threads/${encodeURIComponent(rootMessageId)}/mutes`);
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
  suggestReplies(roomId, { k = 10 } = {}) {
    return request('POST', `/api/rooms/${encodeURIComponent(roomId)}/suggest-replies`, {
      body: { k },
    });
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

  // ----- productivity + people -----
  listRoomTasks(roomId, status) {
    return request('GET', `/api/rooms/${encodeURIComponent(roomId)}/tasks`, {
      query: { status },
    });
  },
  listMyTasks() { return request('GET', '/api/me/tasks'); },
  createTask(roomId, body) {
    return request('POST', `/api/rooms/${encodeURIComponent(roomId)}/tasks`, { body });
  },
  updateTask(taskId, body) {
    return request('PATCH', `/api/tasks/${encodeURIComponent(taskId)}`, { body });
  },
  deleteTask(taskId) {
    return request('DELETE', `/api/tasks/${encodeURIComponent(taskId)}`);
  },
  listApprovals(workspaceId, direction = 'incoming', status) {
    return request(
      'GET',
      `/api/workspaces/${encodeURIComponent(workspaceId)}/approvals/${direction}`,
      { query: { status } },
    );
  },
  createApproval(workspaceId, body) {
    return request('POST', `/api/workspaces/${encodeURIComponent(workspaceId)}/approvals`, { body });
  },
  decideApproval(approvalId, decision, note = null) {
    return request('POST', `/api/approvals/${encodeURIComponent(approvalId)}/${decision}`, {
      body: { note },
    });
  },
  listDirectory(workspaceId, { q, title, limit = 50, offset = 0 } = {}) {
    return request('GET', `/api/workspaces/${encodeURIComponent(workspaceId)}/directory`, {
      query: { q, title, limit, offset },
    });
  },
  getManager(workspaceId, participantId) {
    return request(
      'GET',
      `/api/workspaces/${encodeURIComponent(workspaceId)}/participants/${encodeURIComponent(participantId)}/manager`,
    );
  },
  setManager(workspaceId, participantId, managerId) {
    return request(
      'PUT',
      `/api/workspaces/${encodeURIComponent(workspaceId)}/participants/${encodeURIComponent(participantId)}/manager`,
      { body: { manager_id: managerId } },
    );
  },
  clearManager(workspaceId, participantId) {
    return request(
      'DELETE',
      `/api/workspaces/${encodeURIComponent(workspaceId)}/participants/${encodeURIComponent(participantId)}/manager`,
    );
  },
  getDirectReports(workspaceId, participantId) {
    return request(
      'GET',
      `/api/workspaces/${encodeURIComponent(workspaceId)}/participants/${encodeURIComponent(participantId)}/reports`,
    );
  },
  getReportingChain(workspaceId, participantId) {
    return request(
      'GET',
      `/api/workspaces/${encodeURIComponent(workspaceId)}/participants/${encodeURIComponent(participantId)}/chain`,
    );
  },

  // ----- scheduled work + saved items -----
  listScheduled(roomId) {
    return request('GET', `/api/rooms/${encodeURIComponent(roomId)}/scheduled`);
  },
  listAllScheduled() {
    return request('GET', '/api/scheduled');
  },
  createScheduled(roomId, body) {
    return request('POST', `/api/rooms/${encodeURIComponent(roomId)}/scheduled`, { body });
  },
  updateScheduled(roomId, id, body) {
    if (body === undefined) {
      return request('PATCH', `/api/scheduled/${encodeURIComponent(roomId)}`, { body: id });
    }
    return request(
      'PATCH',
      `/api/rooms/${encodeURIComponent(roomId)}/scheduled/${encodeURIComponent(id)}`,
      { body },
    );
  },
  cancelScheduled(roomId, id) {
    if (id === undefined) return request('DELETE', `/api/scheduled/${encodeURIComponent(roomId)}`);
    return request(
      'DELETE',
      `/api/rooms/${encodeURIComponent(roomId)}/scheduled/${encodeURIComponent(id)}`,
    );
  },
  retryScheduled(roomId, id) {
    if (id === undefined) return request('POST', `/api/scheduled/${encodeURIComponent(roomId)}`);
    return request(
      'POST',
      `/api/rooms/${encodeURIComponent(roomId)}/scheduled/${encodeURIComponent(id)}`,
    );
  },
  listRecurring(roomId) {
    return request('GET', `/api/rooms/${encodeURIComponent(roomId)}/recurring`);
  },
  createRecurring(roomId, body) {
    return request('POST', `/api/rooms/${encodeURIComponent(roomId)}/recurring`, { body });
  },
  cancelRecurring(roomId, id) {
    if (id === undefined) return request('DELETE', `/api/recurring/${encodeURIComponent(roomId)}`);
    return request(
      'DELETE',
      `/api/rooms/${encodeURIComponent(roomId)}/recurring/${encodeURIComponent(id)}`,
    );
  },
  listDigests() { return request('GET', '/api/digests'); },
  createDigest(body) { return request('POST', '/api/digests', { body }); },
  deleteDigest(id) { return request('DELETE', `/api/digests/${encodeURIComponent(id)}`); },
  remindMessage(messageId, when) {
    return request('POST', `/api/messages/${encodeURIComponent(messageId)}/remind`, {
      body: { when },
    });
  },
  listMessageReminders(messageId) {
    return request('GET', `/api/messages/${encodeURIComponent(messageId)}/reminders`);
  },
  saveMessage(messageId, note = null) {
    return request('POST', `/api/messages/${encodeURIComponent(messageId)}/save`, {
      body: { note },
    });
  },
  unsaveMessage(messageId) {
    return request('DELETE', `/api/messages/${encodeURIComponent(messageId)}/save`);
  },
  listSaved(limit = 100, collectionId = null) {
    return request('GET', '/api/saved', {
      query: { limit, collection_id: collectionId },
    });
  },

  // ----- workspace community operations -----
  listUserGroups(workspaceId) {
    return request('GET', `/api/workspaces/${encodeURIComponent(workspaceId)}/user-groups`);
  },
  createUserGroup(workspaceId, body) {
    return request('POST', `/api/workspaces/${encodeURIComponent(workspaceId)}/user-groups`, { body });
  },
  getUserGroup(groupId) {
    return request('GET', `/api/user-groups/${encodeURIComponent(groupId)}`);
  },
  deleteUserGroup(workspaceId, groupId) {
    return request(
      'DELETE',
      `/api/workspaces/${encodeURIComponent(workspaceId)}/user-groups/${encodeURIComponent(groupId)}`,
    );
  },
  setUserGroupMember(workspaceId, groupId, participantId, add = true) {
    return request(
      add ? 'PUT' : 'DELETE',
      `/api/workspaces/${encodeURIComponent(workspaceId)}/user-groups/${encodeURIComponent(groupId)}/members/${encodeURIComponent(participantId)}`,
    );
  },
  requestRoomJoin(roomId) {
    return request('POST', `/api/rooms/${encodeURIComponent(roomId)}/join-request`);
  },
  listJoinRequests(roomId) {
    return request('GET', `/api/rooms/${encodeURIComponent(roomId)}/join-requests`);
  },
  decideJoinRequest(requestId, decision) {
    return request('POST', `/api/join-requests/${encodeURIComponent(requestId)}/${decision}`);
  },
  listAnnouncements(workspaceId) {
    return request('GET', `/api/workspaces/${encodeURIComponent(workspaceId)}/announcements`);
  },
  createAnnouncement(workspaceId, body) {
    return request('POST', `/api/workspaces/${encodeURIComponent(workspaceId)}/announcements`, { body });
  },
  deleteAnnouncement(workspaceId, announcementId) {
    return request(
      'DELETE',
      `/api/workspaces/${encodeURIComponent(workspaceId)}/announcements/${encodeURIComponent(announcementId)}`,
    );
  },

  // ----- enterprise compliance administration -----
  listRoomsForWorkspace(workspaceId) {
    return request('GET', '/api/rooms', { query: { workspace_id: workspaceId } });
  },
  listLegalHolds(workspaceId) {
    return request('GET', `/api/workspaces/${encodeURIComponent(workspaceId)}/legal-holds`);
  },
  createLegalHold(workspaceId, { room_id, reason }) {
    const body = { reason };
    if (room_id) body.room_id = room_id;
    return request('POST', `/api/workspaces/${encodeURIComponent(workspaceId)}/legal-holds`, { body });
  },
  releaseLegalHold(holdId) {
    return request('DELETE', `/api/legal-holds/${encodeURIComponent(holdId)}`);
  },
  roomRetention(roomId) {
    return request('GET', `/api/rooms/${encodeURIComponent(roomId)}/retention`);
  },
  setRoomRetention(roomId, days) {
    return request('PUT', `/api/rooms/${encodeURIComponent(roomId)}/retention`, { body: { days } });
  },
  setWorkspaceRetention(workspaceId, days) {
    return request('PUT', `/api/workspaces/${encodeURIComponent(workspaceId)}/retention`, { body: { days } });
  },
  listInformationBarriers(workspaceId) {
    return request('GET', `/api/workspaces/${encodeURIComponent(workspaceId)}/barriers`);
  },
  createInformationBarrier(workspaceId, group_a, group_b) {
    return request('POST', `/api/workspaces/${encodeURIComponent(workspaceId)}/barriers`, { body: { group_a, group_b } });
  },
  deleteInformationBarrier(barrierId) {
    return request('DELETE', `/api/barriers/${encodeURIComponent(barrierId)}`);
  },
  listDeactivatedMembers(workspaceId) {
    return request('GET', `/api/workspaces/${encodeURIComponent(workspaceId)}/deactivated`);
  },
  deactivateMember(workspaceId, participantId) {
    const workspace = encodeURIComponent(workspaceId);
    const participant = encodeURIComponent(participantId);
    return request('POST', `/api/workspaces/${workspace}/members/${participant}/deactivate`);
  },
  reactivateMember(workspaceId, participantId) {
    const workspace = encodeURIComponent(workspaceId);
    const participant = encodeURIComponent(participantId);
    return request('POST', `/api/workspaces/${workspace}/members/${participant}/reactivate`);
  },
  listInvitations(workspaceId) {
    return request('GET', `/api/workspaces/${encodeURIComponent(workspaceId)}/invitations`);
  },
  createInvitation(workspaceId, body) {
    return request('POST', `/api/workspaces/${encodeURIComponent(workspaceId)}/invitations`, { body });
  },
  revokeInvitation(invitationId) {
    return request('DELETE', `/api/invitations/${encodeURIComponent(invitationId)}`);
  },
  listRoomWebhooks(roomId) {
    return request('GET', `/api/rooms/${encodeURIComponent(roomId)}/webhooks`);
  },
  createIncomingWebhook(roomId, body) {
    return request('POST', `/api/rooms/${encodeURIComponent(roomId)}/webhooks/incoming`, { body });
  },
  createOutgoingWebhook(roomId, body) {
    return request('POST', `/api/rooms/${encodeURIComponent(roomId)}/webhooks/outgoing`, { body });
  },
  revokeIncomingWebhook(webhookId) {
    return request('DELETE', `/api/webhooks/incoming/${encodeURIComponent(webhookId)}`);
  },
  revokeOutgoingWebhook(webhookId) {
    return request('DELETE', `/api/webhooks/outgoing/${encodeURIComponent(webhookId)}`);
  },
  listWebhookDeliveries(webhookId, limit = 100) {
    return request('GET', `/api/webhooks/${encodeURIComponent(webhookId)}/deliveries`, { query: { limit } });
  },
  listWebhookDeadLetters(webhookId, limit = 100) {
    return request('GET', `/api/webhooks/${encodeURIComponent(webhookId)}/deliveries/dead`, { query: { limit } });
  },
  requeueWebhookDelivery(deliveryId) {
    return request('POST', `/api/webhook-deliveries/${encodeURIComponent(deliveryId)}/requeue`);
  },
  exportWorkspace(workspaceId) {
    return request('GET', `/api/workspaces/${encodeURIComponent(workspaceId)}/export`);
  },
  deleteWorkspace(workspaceId) {
    return request('DELETE', `/api/workspaces/${encodeURIComponent(workspaceId)}`);
  },
};
