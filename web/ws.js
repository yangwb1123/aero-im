// ws.js — WebSocket client with exponential backoff reconnect and a tiny event bus.

const BACKOFF_MS = [1000, 2000, 4000, 8000, 16000, 30000];

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

export class WsClient {
  constructor() {
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
  }

  on(event, fn) {
    if (!this.handlers.has(event)) this.handlers.set(event, new Set());
    this.handlers.get(event).add(fn);
    return () => this.handlers.get(event)?.delete(fn);
  }
  _emit(event, ...args) {
    const set = this.handlers.get(event);
    if (!set) return;
    for (const fn of set) {
      try { fn(...args); } catch (e) { console.error('[ws handler]', event, e); }
    }
  }

  connect(token) {
    this.token = token;
    this.closedByUser = false;
    this._lastSeen = null; // fresh session → no backfill cursor yet
    this._seqGate.reset(); // fresh session → forget seen seqs too
    this._open();
  }

  _open() {
    if (this._reconnectTimer) { clearTimeout(this._reconnectTimer); this._reconnectTimer = null; }
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    let url = `${proto}//${location.host}/ws?token=${encodeURIComponent(this.token)}`;
    // On reconnect, ask the server to replay everything created after the last
    // message we saw, so a network blip never silently drops messages.
    if (this._lastSeen) url += `&since=${encodeURIComponent(this._lastSeen)}`;
    this._emit('status', 'connecting');
    let ws;
    try {
      ws = new WebSocket(url);
    } catch (e) {
      this._emit('status', 'down');
      this._scheduleReconnect();
      return;
    }
    this.ws = ws;

    ws.addEventListener('open', () => {
      this.attempts = 0;
      this._emit('status', 'up');
      this._emit('open');
      this._startPing();
    });

    ws.addEventListener('message', (ev) => {
      let msg;
      try { msg = JSON.parse(ev.data); }
      catch { this._emit('error', { code: 'PARSE', msg: 'invalid frame' }); return; }
      // Seq dedup (ROADMAP v3 方向一): drop a frame whose per-room/per-stream
      // seq was already applied (at-least-once redelivery). Frames without a
      // seq pass through unchanged.
      if (msg && msg.seq != null && !this._seqGate.accept(seqScope(msg), msg.seq)) return;
      this._emit('message', msg);
      // Track the newest message id for the reconnect `?since=` cursor. ULIDs sort
      // lexicographically, so a string compare yields the latest; only `message`
      // frames carry a fresh id (an edit/delete carries an older one, which the
      // `>` guard ignores).
      const mid = msg && msg.message && msg.message.id;
      if (mid && (!this._lastSeen || mid > this._lastSeen)) this._lastSeen = mid;
      if (msg && typeof msg.type === 'string') {
        this._emit(`msg:${msg.type}`, msg);
      }
    });

    ws.addEventListener('close', (ev) => {
      this._stopPing();
      this.ws = null;
      this._emit('status', 'down');
      this._emit('close', ev);
      if (!this.closedByUser) this._scheduleReconnect();
    });

    ws.addEventListener('error', () => {
      // Browsers don't expose much; rely on close for reconnect logic.
      this._emit('status', 'down');
    });
  }

  _scheduleReconnect() {
    if (this.closedByUser) return;
    const idx = Math.min(this.attempts, BACKOFF_MS.length - 1);
    const wait = BACKOFF_MS[idx];
    this.attempts += 1;
    this._emit('status', 'wait');
    this._reconnectTimer = setTimeout(() => this._open(), wait);
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

  joinRoom(roomId) { return this.send({ type: 'join_room', room_id: roomId }); }
  sendMessage(roomId, blocks, replyTo = null) {
    return this.send({ type: 'send_message', room_id: roomId, blocks, reply_to: replyTo });
  }
  editMessage(id, blocks) {
    return this.send({ type: 'edit_message', id, blocks });
  }
  deleteMessage(id) {
    return this.send({ type: 'delete_message', id });
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
    if (this._reconnectTimer) { clearTimeout(this._reconnectTimer); this._reconnectTimer = null; }
    this._stopPing();
    if (this.ws) {
      try { this.ws.close(1000, 'bye'); } catch {}
      this.ws = null;
    }
  }
}
