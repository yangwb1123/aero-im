// ws.js — WebSocket client with exponential backoff reconnect and a tiny event bus.

import { DeliveryCursorLedger } from './delivery_cursor.js';

const BACKOFF_MS = [1000, 2000, 4000, 8000, 16000, 30000];
const SERVER_AWAY_RECONNECT_MS = 100;
const DELIVERY_ACK_FLUSH_MS = 250;
const ACCESS_TOKEN_REFRESH_SKEW_MS = 5000;

// JWT payloads are inspected only to decide when to ask the server for a new
// access token. This is not authentication: signature and session validation
// remain entirely server-side.
export function accessTokenNeedsRefresh(token, now = Date.now()) {
  if (typeof token !== 'string') return false;
  const parts = token.split('.');
  if (parts.length !== 3 || !parts[1]) return false;
  try {
    const encoded = parts[1].replace(/-/g, '+').replace(/_/g, '/');
    const padded = encoded.padEnd(encoded.length + ((4 - (encoded.length % 4)) % 4), '=');
    const payload = JSON.parse(atob(padded));
    return typeof payload.exp === 'number'
      && Number.isFinite(payload.exp)
      && payload.exp * 1000 <= now + ACCESS_TOKEN_REFRESH_SKEW_MS;
  } catch {
    // Opaque/legacy tokens retain the existing reconnect behaviour. The server
    // remains authoritative for whether they are valid.
    return false;
  }
}

/** Compute reconnect delay. RFC 6455 code 1001 means the server is intentionally
 * going away (for example a rolling deploy), so migrate promptly instead of
 * applying the ordinary failure backoff. */
export function reconnectDelay(attempts, closeCode = null) {
  if (closeCode === 1001) return SERVER_AWAY_RECONNECT_MS;
  const idx = Math.min(Math.max(0, attempts), BACKOFF_MS.length - 1);
  return BACKOFF_MS[idx];
}

// How many recently-seen seqs to remember per room/stream scope. Duplicates
// from NATS at-least-once redelivery arrive close together, so a small window
// is plenty; the cap bounds memory for very chatty rooms.
const SEQ_RECENT_CAP = 256;

// Per-scope event-seq dedup (ROADMAP v3 方向一). The server stamps every
// bus-delivered frame with a per-room/per-stream monotonic `seq`, minted at
// publish time — so an at-least-once redelivery (or a multi-instance replay)
// carries the SAME seq. Dropping a seq we've already applied dedups events
// that have no id of their own (edits, deletes, reactions, typing…).
// Gaps are legal (only dedup + relative order matter), and frames without a
// seq (legacy servers, locally-generated frames, REST backfill) always pass.
export class SeqGate {
  constructor() {
    this.scopes = new Map(); // scope -> { recent:Set<number>, order:number[], high:number }
  }
  /** True when the frame should be applied; false for an already-seen seq. */
  accept(scope, seq) {
    if (!scope || typeof seq !== 'number' || !Number.isFinite(seq)) return true;
    let s = this.scopes.get(scope);
    if (!s) { s = { recent: new Set(), order: [], high: 0 }; this.scopes.set(scope, s); }
    if (s.recent.has(seq)) return false; // duplicate delivery — drop
    s.recent.add(seq);
    s.order.push(seq);
    if (s.order.length > SEQ_RECENT_CAP) s.recent.delete(s.order.shift());
    if (seq > s.high) s.high = seq; // highest-applied, for relative ordering
    return true;
  }
  /** Highest seq applied for a scope (0 when none) — lets callers ignore
   *  events older than something newer they already applied. */
  high(scope) { return this.scopes.get(scope)?.high || 0; }
  /** Undo a just-recorded seq when application handlers rejected its frame. */
  forget(scope, seq) {
    const s = this.scopes.get(scope);
    if (!s || !s.recent.delete(seq)) return;
    s.order = s.order.filter((value) => value !== seq);
    s.high = s.order.length ? Math.max(...s.order) : 0;
    if (!s.order.length) this.scopes.delete(scope);
  }
  reset() { this.scopes.clear(); }
}

// Dedup scope of a server frame: per stream for stream events, per room for
// room events, a shared bucket for anything else that carries a seq.
function seqScope(msg) {
  if (msg.type === 'stream_event') {
    const sid = msg.event && msg.event.stream_id;
    return sid ? `s:${sid}` : null;
  }
  const rid = msg.room_id || msg.message?.room_id || msg.event?.room_id;
  return rid ? `r:${rid}` : 'g';
}

// The delivery barrier is also the server-authorized room set for restoring
// client cursors. Any malformed shape fails closed rather than widening scope.
function deliveryBarrierRooms(rooms) {
  if (!Array.isArray(rooms)) return null;
  const authorized = new Set();
  for (const room of rooms) {
    if (!room || typeof room !== 'object' || Array.isArray(room)
      || typeof room.room_id !== 'string' || !room.room_id
      || !Number.isSafeInteger(room.delivery_ordinal) || room.delivery_ordinal < 0) {
      return null;
    }
    authorized.add(room.room_id);
  }
  return authorized;
}

export class WsClient {
  constructor({ cursorStorage } = {}) {
    this.ws = null;
    this.token = null;
    this.attempts = 0;
    this.closedByUser = false;
    this.handlers = new Map(); // event -> Set<fn>
    this.pingTimer = null;
    this._reconnectTimer = null;
    // Newest message id this client has received — the `?since=` reconnect cursor
    // so a dropped/replaced socket replays messages missed while offline (the
    // server's backfill protocol, see crates/aero-server/src/ws.rs). In memory
    // only: a full page reload re-fetches state, so it resets per session.
    this._lastSeen = null;
    // Event-seq dedup across the whole connection lifetime (survives reconnects
    // on purpose: the post-reconnect live stream may redeliver stamped events).
    this._seqGate = new SeqGate();
    this.capabilities = new Set();
    this.connectionId = 0;
    this._socketGeneration = 0;
    this._deliveryCursorSupported = false;
    this._deliveryReady = false;
    this._deliveryAuthorizedRooms = new Set();
    this._deliveryLedger = new DeliveryCursorLedger(cursorStorage);
    this._pendingDeliveryAcks = new Map();
    this._preReadyDeliveryAcks = new Map();
    this._deliveryFailedRooms = new Set();
    this._deliveryPauseDepth = 0;
    this._pausedDeliveryAcks = new Map();
    this._deliveryAckTimer = null;
    this._refreshAccessToken = null;
  }

  on(event, fn) {
    if (!this.handlers.has(event)) this.handlers.set(event, new Set());
    this.handlers.get(event).add(fn);
    return () => this.handlers.get(event)?.delete(fn);
  }
  _emit(event, ...args) {
    const set = this.handlers.get(event);
    if (!set) return true;
    let applied = true;
    for (const fn of set) {
      try { fn(...args); } catch (e) {
        applied = false;
        console.error('[ws handler]', event, e);
      }
    }
    return applied;
  }

  connect(token, participantId = null, { refreshAccessToken = null } = {}) {
    // Invalidate callbacks from a previous account/socket before opening the
    // replacement. WebSocket.close() is asynchronous and already-queued events
    // from account A must never mutate account B's ledger or UI.
    const previous = this.ws;
    this._socketGeneration += 1;
    this.ws = null;
    if (previous) {
      try { previous.close(1000, 'replaced'); } catch { /* already closed */ }
    }
    this._stopPing();
    if (this._deliveryAckTimer) {
      clearTimeout(this._deliveryAckTimer);
      this._deliveryAckTimer = null;
    }
    this.token = token;
    this._refreshAccessToken = typeof refreshAccessToken === 'function'
      ? refreshAccessToken : null;
    this.closedByUser = false;
    this._lastSeen = null; // fresh session → no backfill cursor yet
    this._seqGate.reset(); // fresh session → forget seen seqs too
    this.capabilities.clear();
    this._deliveryCursorSupported = false;
    this._deliveryReady = false;
    this._deliveryAuthorizedRooms.clear();
    this._pendingDeliveryAcks.clear();
    this._preReadyDeliveryAcks.clear();
    this._deliveryFailedRooms.clear();
    this._deliveryPauseDepth = 0;
    this._pausedDeliveryAcks.clear();
    this._deliveryLedger.open(participantId);
    const generation = this._socketGeneration;
    if (accessTokenNeedsRefresh(this.token)) {
      this._emit('status', 'wait');
      void this._openWithFreshAccessToken(generation);
    } else {
      this._open();
    }
  }

  async _openWithFreshAccessToken(expectedGeneration) {
    if (this.closedByUser || expectedGeneration !== this._socketGeneration) return;
    if (accessTokenNeedsRefresh(this.token)) {
      if (!this._refreshAccessToken) {
        this._stopExpiredTokenReconnect();
        return;
      }
      let freshToken;
      try {
        freshToken = await this._refreshAccessToken();
      } catch {
        if (!this.closedByUser && expectedGeneration === this._socketGeneration) {
          this._stopExpiredTokenReconnect();
        }
        return;
      }
      if (this.closedByUser || expectedGeneration !== this._socketGeneration) return;
      if (typeof freshToken !== 'string'
        || !freshToken.trim()
        || accessTokenNeedsRefresh(freshToken)) {
        this._stopExpiredTokenReconnect();
        return;
      }
      this.token = freshToken;
    }
    if (!this.closedByUser && expectedGeneration === this._socketGeneration) this._open();
  }

  _stopExpiredTokenReconnect() {
    this.closedByUser = true;
    this.token = null;
    if (this._reconnectTimer) {
      clearTimeout(this._reconnectTimer);
      this._reconnectTimer = null;
    }
    this._emit('status', 'auth');
    this._emit('auth_expired');
  }

  _open() {
    if (this._reconnectTimer) { clearTimeout(this._reconnectTimer); this._reconnectTimer = null; }
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    let url = `${proto}//${location.host}/ws?token=${encodeURIComponent(this.token)}`;
    // Modern servers use per-room delivery cursors; legacy servers ignore this
    // additive query parameter and continue using `since` below.
    url += '&cursors=1';
    // On reconnect, ask the server to replay everything created after the last
    // message we saw. A cursor-capable server prefers `cursors=1`; an older
    // server ignores it and retains this legacy global fallback.
    if (this._lastSeen) url += `&since=${encodeURIComponent(this._lastSeen)}`;
    this._emit('status', 'connecting');
    let ws;
    const generation = ++this._socketGeneration;
    try {
      ws = new WebSocket(url);
    } catch {
      this._emit('status', 'down');
      this._scheduleReconnect();
      return;
    }
    this.ws = ws;
    const isCurrent = () => this.ws === ws && this._socketGeneration === generation;

    ws.addEventListener('open', () => {
      if (!isCurrent()) return;
      this.attempts = 0;
      this.connectionId += 1;
      this.capabilities.clear();
      this._deliveryCursorSupported = false;
      this._deliveryReady = false;
      this._deliveryAuthorizedRooms.clear();
      this._preReadyDeliveryAcks.clear();
      this._deliveryFailedRooms.clear();
      this._deliveryPauseDepth = 0;
      this._pausedDeliveryAcks.clear();
      this._emit('status', 'up');
      this._emit('open');
      this._startPing();
    });

    ws.addEventListener('message', (ev) => {
      if (!isCurrent()) return;
      let msg;
      try { msg = JSON.parse(ev.data); }
      catch { this._emit('error', { code: 'PARSE', msg: 'invalid frame' }); return; }
      if (msg?.type === 'welcome') {
        const participant = typeof msg.participant === 'string' ? msg.participant : null;
        if (participant && participant !== this._deliveryLedger.participantId) {
          // The authenticated Welcome is authoritative. Switching the ledger
          // here prevents a stale UI/account hint from crossing identities.
          this._pendingDeliveryAcks.clear();
          this._preReadyDeliveryAcks.clear();
          this._pausedDeliveryAcks.clear();
          this._deliveryFailedRooms.clear();
          this._deliveryAuthorizedRooms.clear();
          this._deliveryPauseDepth = 0;
          this._lastSeen = null;
          this._seqGate.reset();
          this._deliveryLedger.open(participant);
        }
        this.capabilities = new Set(
          Array.isArray(msg.capabilities) ? msg.capabilities.map(String) : [],
        );
        this._deliveryCursorSupported = this.capabilities.has('delivery_cursor_v2');
      }
      if (msg?.type === 'delivery_ready' && this._deliveryCursorSupported) {
        this._deliveryAuthorizedRooms = deliveryBarrierRooms(msg.rooms) || new Set();
        this._deliveryReady = true;
        for (const cursor of this._preReadyDeliveryAcks.values()) {
          if (!this._deliveryAuthorizedRooms.has(cursor.room_id)) continue;
          this._commitDeliveryCursor(
            cursor.room_id,
            cursor.message_id,
            cursor.delivery_ordinal,
            cursor.seq,
          );
        }
        this._preReadyDeliveryAcks.clear();
        this._restoreDeliveryAcks();
        this._flushDeliveryAcks();
      }
      // Seq dedup (ROADMAP v3 方向一): drop a frame whose per-room/per-stream
      // seq was already applied (at-least-once redelivery). Frames without a
      // seq pass through unchanged.
      if (msg && msg.seq != null && !this._seqGate.accept(seqScope(msg), msg.seq)) return;
      const genericApplied = this._emit('message', msg);
      const typedApplied = !msg || typeof msg.type !== 'string'
        ? true : this._emit(`msg:${msg.type}`, msg);
      const applied = genericApplied && typedApplied;
      // A handler failure is not delivery: undo seq de-duplication and do not
      // move even the legacy `?since=` fallback past the rejected frame.
      if (!applied && msg?.seq != null) this._seqGate.forget(seqScope(msg), msg.seq);
      if (!applied) this._fenceFailedApplication(msg);
      if (applied) {
        // Only ordinary room-message frames advance the legacy `?since=`
        // backfill cursor. Mutations (edited/deleted/recalled) converge via
        // the `changes_since` replay instead — advancing the cursor on them
        // could skip a not-yet-fetched create in the legacy backfill.
        if (msg?.type === 'message') {
          const mid = msg?.message?.id;
          if (mid && (!this._lastSeen || mid > this._lastSeen)) this._lastSeen = mid;
        }
      }
      // Persist ordinary room messages after applying them. Live frames carry a
      // publish-time room seq; database backfill frames use the zero sentinel.
      this._queueDeliveryAck(msg, applied);
    });

    ws.addEventListener('close', (ev) => {
      if (!isCurrent()) return;
      this._stopPing();
      this.ws = null;
      this._emit('status', 'down');
      this._emit('close', ev);
      if (!this.closedByUser) this._scheduleReconnect(ev.code);
    });

    ws.addEventListener('error', () => {
      if (!isCurrent()) return;
      // Browsers don't expose much; rely on close for reconnect logic.
      this._emit('status', 'down');
    });
  }

  _scheduleReconnect(closeCode = null) {
    if (this.closedByUser || this._reconnectTimer) return;
    const wait = reconnectDelay(this.attempts, closeCode);
    this.attempts += 1;
    this._emit('status', 'wait');
    const generation = this._socketGeneration;
    this._reconnectTimer = setTimeout(async () => {
      this._reconnectTimer = null;
      await this._openWithFreshAccessToken(generation);
    }, wait);
  }

  _startPing() {
    this._stopPing();
    this.pingTimer = setInterval(() => {
      this.send({ type: 'ping' });
    }, 25000);
  }
  _stopPing() {
    if (this.pingTimer) { clearInterval(this.pingTimer); this.pingTimer = null; }
  }

  _restoreDeliveryAcks() {
    for (const cursor of this._deliveryLedger.entries()) {
      if (!this._deliveryAuthorizedRooms.has(cursor.room_id)
        || this._deliveryFailedRooms.has(cursor.room_id)) continue;
      const current = this._pendingDeliveryAcks.get(cursor.room_id);
      if (!current || cursor.delivery_ordinal > current.delivery_ordinal
        || (cursor.delivery_ordinal === current.delivery_ordinal
          && cursor.seq > current.seq)) {
        this._pendingDeliveryAcks.set(cursor.room_id, cursor);
      }
    }
  }

  _fenceFailedApplication(msg) {
    const roomId = msg?.room_id || msg?.message?.room_id || msg?.event?.room_id;
    if (typeof roomId === 'string' && roomId) {
      this._deliveryFailedRooms.add(roomId);
      this._preReadyDeliveryAcks.delete(roomId);
      this._pendingDeliveryAcks.delete(roomId);
      this._pausedDeliveryAcks.delete(roomId);
    }
    // Fence before ACK validation can return for an invalid seq or missing
    // room. If the frame has no room identity, closing is still required so
    // the failed application is retried instead of silently consumed.
    try { this.ws?.close(1011, 'message application failed'); } catch { /* close event retries */ }
  }

  _queueDeliveryAck(msg, applied) {
    if (!applied) return;
    if (msg?.type !== 'message') return;
    const roomId = msg.room_id || msg.message?.room_id;
    const messageId = msg.message?.id;
    const deliveryOrdinal = msg.delivery_ordinal;
    // Backfill messages are database rows and therefore have no NATS seq. ACK
    // them with the explicit zero sentinel so a second reconnect does not replay
    // the same offline batch forever. A present seq must still be a valid
    // positive bus position.
    if (msg.seq != null && (!Number.isSafeInteger(msg.seq) || msg.seq <= 0)) return;
    const seq = msg.seq == null ? 0 : msg.seq;
    if (typeof roomId !== 'string' || typeof messageId !== 'string'
      || !Number.isSafeInteger(deliveryOrdinal) || deliveryOrdinal <= 0) return;
    if (this._deliveryFailedRooms.has(roomId)) return;
    if (this._deliveryReady && !this._deliveryAuthorizedRooms.has(roomId)) return;
    if (!this._deliveryReady) {
      const current = this._preReadyDeliveryAcks.get(roomId);
      if (!current || deliveryOrdinal > current.delivery_ordinal
        || (deliveryOrdinal === current.delivery_ordinal && seq > current.seq)) {
        this._preReadyDeliveryAcks.set(roomId, {
          room_id: roomId,
          message_id: deliveryOrdinal >= (current?.delivery_ordinal ?? 0)
            ? messageId : current.message_id,
          delivery_ordinal: Math.max(current?.delivery_ordinal ?? 0, deliveryOrdinal),
          seq: current ? Math.max(current.seq, seq) : seq,
        });
      }
      return;
    }
    this._commitDeliveryCursor(roomId, messageId, deliveryOrdinal, seq);
  }

  _commitDeliveryCursor(roomId, messageId, deliveryOrdinal, seq) {
    if (this._deliveryFailedRooms.has(roomId)
      || (this._deliveryReady && !this._deliveryAuthorizedRooms.has(roomId))) return;
    if (this._deliveryPauseDepth > 0) {
      const current = this._pausedDeliveryAcks.get(roomId);
      if (!current || deliveryOrdinal > current.delivery_ordinal
        || (deliveryOrdinal === current.delivery_ordinal && seq > current.seq)) {
        this._pausedDeliveryAcks.set(roomId, {
          room_id: roomId,
          message_id: deliveryOrdinal >= (current?.delivery_ordinal ?? 0)
            ? messageId : current.message_id,
          delivery_ordinal: Math.max(current?.delivery_ordinal ?? 0, deliveryOrdinal),
          seq: current ? Math.max(current.seq, seq) : seq,
        });
      }
      return;
    }
    const cursor = this._deliveryLedger.advance(
      roomId, messageId, deliveryOrdinal, seq,
    );
    if (!cursor) return;
    this._pendingDeliveryAcks.set(roomId, cursor);
    if (!this._deliveryCursorSupported || this._deliveryAckTimer) return;
    this._deliveryAckTimer = setTimeout(() => {
      this._deliveryAckTimer = null;
      this._flushDeliveryAcks();
    }, DELIVERY_ACK_FLUSH_MS);
  }

  _flushDeliveryAcks() {
    if (!this._deliveryCursorSupported || !this._deliveryReady
      || this._deliveryPauseDepth > 0) return;
    for (const [roomId, cursor] of this._pendingDeliveryAcks) {
      if (!this._deliveryAuthorizedRooms.has(roomId)
        || this._deliveryFailedRooms.has(roomId)) {
        this._pendingDeliveryAcks.delete(roomId);
        continue;
      }
      if (this.deliveryAck(
        roomId, cursor.message_id, cursor.delivery_ordinal, cursor.seq,
      )) {
        this._pendingDeliveryAcks.delete(roomId);
      }
    }
  }

  /** Freeze durable cursor advancement while an application-level REST resync
   * closes a known delivery gap. The returned callback must be invoked with
   * `true` only after every page was successfully applied. */
  pauseDeliveryAcks() {
    const generation = this._socketGeneration;
    this._deliveryPauseDepth += 1;
    let finished = false;
    return (success) => {
      if (finished || generation !== this._socketGeneration) return;
      finished = true;
      if (!success) {
        this._deliveryPauseDepth = 0;
        this._pausedDeliveryAcks.clear();
        // Reconnect from the unchanged server cursor. Do not mark this as a
        // user close: the ordinary close handler owns the retry.
        try { this.ws?.close(1011, 'delivery catch-up failed'); } catch { /* retry via close */ }
        return;
      }
      this._deliveryPauseDepth = Math.max(0, this._deliveryPauseDepth - 1);
      if (this._deliveryPauseDepth > 0) return;
      const pending = Array.from(this._pausedDeliveryAcks.values());
      this._pausedDeliveryAcks.clear();
      for (const cursor of pending) {
        this._commitDeliveryCursor(
          cursor.room_id,
          cursor.message_id,
          cursor.delivery_ordinal,
          cursor.seq,
        );
      }
      this._flushDeliveryAcks();
    };
  }

  send(obj) {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return false;
    try {
      this.ws.send(JSON.stringify(obj));
      return true;
    } catch (e) {
      console.warn('[ws send]', e);
      return false;
    }
  }

  supports(capability) { return this.capabilities.has(capability); }

  joinRoom(roomId) { return this.send({ type: 'join_room', room_id: roomId }); }
  deliveryAck(roomId, messageId, deliveryOrdinal, seq) {
    return this.send({
      type: 'delivery_ack',
      room_id: roomId,
      message_id: messageId,
      delivery_ordinal: deliveryOrdinal,
      seq,
    });
  }
  sendMessage(roomId, blocks, replyTo = null, clientMessageId = null) {
    const frame = { type: 'send_message', room_id: roomId, blocks, reply_to: replyTo };
    if (clientMessageId) frame.client_message_id = clientMessageId;
    return this.send(frame);
  }
  // Send raw Markdown text; the server parses it into rich Text blocks (with
  // spans for **bold** / *italic* / `code` / ~~strike~~ / [link](url)) and
  // broadcasts the resulting message. See the `SendMarkdown` WS frame.
  sendMarkdown(
    roomId,
    markdown,
    replyTo = null,
    expiresAfterSecs = null,
    clientMessageId = null,
  ) {
    const frame = { type: 'send_markdown', room_id: roomId, markdown, reply_to: replyTo };
    if (expiresAfterSecs != null) frame.expires_after_secs = expiresAfterSecs;
    if (clientMessageId) frame.client_message_id = clientMessageId;
    return this.send(frame);
  }
  editMessage(id, blocks) {
    return this.send({ type: 'edit_message', id, blocks });
  }
  deleteMessage(id) {
    return this.send({ type: 'delete_message', id });
  }
  recallMessage(id) {
    return this.send({ type: 'recall_message', id });
  }
  react(messageId, emoji) {
    return this.send({ type: 'react', message_id: messageId, emoji });
  }
  markRead(roomId, lastMessageId) {
    return this.send({ type: 'mark_read', room_id: roomId, last_message_id: lastMessageId });
  }
  typing(roomId, on) {
    return this.send({ type: 'typing', room_id: roomId, on });
  }
  callInvite(roomId, kind, sdp, mode = 'p2p') {
    return this.send({ type: 'call_invite', room_id: roomId, kind, mode, sdp });
  }
  callAnswer(callId, roomId, to, sdp) {
    return this.send({ type: 'call_answer', call_id: callId, room_id: roomId, to, sdp });
  }
  callIce(callId, roomId, to, candidate) {
    return this.send({ type: 'call_ice', call_id: callId, room_id: roomId, to, candidate });
  }
  callEnd(callId, roomId, reason = 'hangup') {
    return this.send({ type: 'call_end', call_id: callId, room_id: roomId, reason });
  }
  callCaption(callId, roomId, text, lang, isFinal, targetLang) {
    return this.send({
      type: 'call_caption', call_id: callId, room_id: roomId,
      text, lang: lang || null, target_lang: targetLang || null, is_final: !!isFinal,
    });
  }
  // ----- group call (P6 mesh) -----
  callJoin(roomId, kind, callId = null) {
    return this.send({ type: 'call_join', room_id: roomId, kind, call_id: callId });
  }
  callLeave(callId, roomId) {
    return this.send({ type: 'call_leave', call_id: callId, room_id: roomId });
  }
  callOffer(callId, roomId, to, sdp) {
    return this.send({ type: 'call_offer', call_id: callId, room_id: roomId, to, sdp });
  }
  callSfuOffer(callId, roomId, sdp) {
    return this.send({ type: 'call_sfu_offer', call_id: callId, room_id: roomId, sdp });
  }
  callSfuIce(callId, roomId, sessionGeneration, candidate) {
    return this.send({
      type: 'call_sfu_ice',
      call_id: callId,
      room_id: roomId,
      session_generation: sessionGeneration,
      candidate,
    });
  }
  callSfuSubscribe(callId, roomId, sessionGeneration, revision, tracks) {
    return this.send({
      type: 'call_sfu_subscribe',
      call_id: callId,
      room_id: roomId,
      session_generation: sessionGeneration,
      revision,
      tracks,
    });
  }

  // ----- live interactivity (P4 弹幕 + 礼物) -----
  // `since` (optional): last chat-line id already rendered — the server then
  // replays only what was missed instead of the default recent tail.
  watchStream(streamId, since = null) {
    const frame = { type: 'watch_stream', stream_id: streamId };
    if (since) frame.since = since;
    return this.send(frame);
  }
  unwatchStream(streamId) { return this.send({ type: 'unwatch_stream', stream_id: streamId }); }
  streamChat(streamId, body) {
    return this.send({ type: 'stream_chat', stream_id: streamId, body });
  }
  streamGift(streamId, giftId, qty = 1) {
    return this.send({ type: 'stream_gift', stream_id: streamId, gift_id: giftId, qty });
  }

  close() {
    this.closedByUser = true;
    this._socketGeneration += 1;
    if (this._reconnectTimer) { clearTimeout(this._reconnectTimer); this._reconnectTimer = null; }
    this._stopPing();
    if (this._deliveryAckTimer) {
      clearTimeout(this._deliveryAckTimer);
      this._deliveryAckTimer = null;
    }
    const ws = this.ws;
    this.ws = null;
    if (ws) {
      try { ws.close(1000, 'bye'); } catch { /* already closed */ }
    }
  }
}
