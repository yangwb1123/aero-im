// ws.js — WebSocket client with exponential backoff reconnect and a tiny event bus.

const BACKOFF_MS = [1000, 2000, 4000, 8000, 16000, 30000];

export class WsClient {
  constructor() {
    this.ws = null;
    this.token = null;
    this.attempts = 0;
    this.closedByUser = false;
    this.handlers = new Map(); // event -> Set<fn>
    this.pingTimer = null;
    this._reconnectTimer = null;
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
    this._open();
  }

  _open() {
    if (this._reconnectTimer) { clearTimeout(this._reconnectTimer); this._reconnectTimer = null; }
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = `${proto}//${location.host}/ws?token=${encodeURIComponent(this.token)}`;
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
      this._emit('message', msg);
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
