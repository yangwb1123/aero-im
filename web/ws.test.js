import test from 'node:test';
import assert from 'node:assert/strict';

import { accessTokenNeedsRefresh, reconnectDelay, WsClient } from './ws.js';
import { refreshWsAccessToken } from './ws_auth.js';

class MemoryStorage {
  constructor() { this.values = new Map(); }
  getItem(key) { return this.values.get(key) ?? null; }
  setItem(key, value) { this.values.set(key, String(value)); }
}

function jwtWithExpiry(exp) {
  const encode = (value) => globalThis.btoa(JSON.stringify(value))
    .replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/g, '');
  return `${encode({ alg: 'none' })}.${encode({ exp })}.signature`;
}

test('server-away close reconnects promptly while failures retain bounded backoff', () => {
  assert.equal(reconnectDelay(0, 1001), 100);
  assert.equal(reconnectDelay(0, 1006), 1000);
  assert.equal(reconnectDelay(3, 1006), 8000);
  assert.equal(reconnectDelay(99, 1006), 30000);
});

test('JWT expiry is used only as a refresh timing hint', () => {
  const now = 2_000_000_000_000;
  assert.equal(accessTokenNeedsRefresh(jwtWithExpiry((now - 1000) / 1000), now), true);
  assert.equal(accessTokenNeedsRefresh(jwtWithExpiry((now + 60_000) / 1000), now), false);
  assert.equal(accessTokenNeedsRefresh('opaque-access-token', now), false);
});

test('an expired token is refreshed before a WebSocket reconnect', async () => {
  const previousWebSocket = globalThis.WebSocket;
  const previousLocation = globalThis.location;
  const previousSetTimeout = globalThis.setTimeout;
  const previousClearTimeout = globalThis.clearTimeout;
  const sockets = [];
  const timers = new Map();
  let nextTimer = 1;
  class FakeSocket {
    static OPEN = 1;
    constructor(url) {
      this.url = url;
      this.readyState = 0;
      this.handlers = new Map();
      sockets.push(this);
    }
    addEventListener(event, fn) { this.handlers.set(event, fn); }
    emit(event, payload = {}) { return this.handlers.get(event)?.(payload); }
    send() {}
    close() {}
  }
  globalThis.WebSocket = FakeSocket;
  globalThis.location = { protocol: 'https:', host: 'example.test' };
  globalThis.setTimeout = (fn, delay) => {
    const id = nextTimer++;
    timers.set(id, { fn, delay });
    return id;
  };
  globalThis.clearTimeout = (id) => timers.delete(id);

  try {
    const nowSeconds = Math.floor(Date.now() / 1000);
    const initialToken = jwtWithExpiry(nowSeconds + 3600);
    const expiredToken = jwtWithExpiry(nowSeconds - 60);
    const refreshedToken = jwtWithExpiry(nowSeconds + 7200);
    let refreshes = 0;
    const client = new WsClient();
    client.connect(initialToken, 'participant-a', {
      refreshAccessToken: async () => {
        refreshes += 1;
        return refreshedToken;
      },
    });
    assert.equal(new URL(sockets[0].url).searchParams.get('token'), initialToken);

    // Model the server's expiry-driven close without waiting an hour.
    client.token = expiredToken;
    sockets[0].emit('close', { code: 1006 });
    assert.equal(timers.size, 1);
    const [[timerId, timer]] = timers;
    timers.delete(timerId);
    await timer.fn();

    assert.equal(refreshes, 1);
    assert.equal(sockets.length, 2);
    assert.equal(new URL(sockets[1].url).searchParams.get('token'), refreshedToken);
    assert.ok(!sockets[1].url.includes(encodeURIComponent(expiredToken)));
    client.close();
  } finally {
    if (previousWebSocket === undefined) delete globalThis.WebSocket;
    else globalThis.WebSocket = previousWebSocket;
    if (previousLocation === undefined) delete globalThis.location;
    else globalThis.location = previousLocation;
    globalThis.setTimeout = previousSetTimeout;
    globalThis.clearTimeout = previousClearTimeout;
  }
});

test('refresh failure stops expired-token reconnects', async () => {
  const previousWebSocket = globalThis.WebSocket;
  const previousLocation = globalThis.location;
  const sockets = [];
  class FakeSocket {
    static OPEN = 1;
    constructor(url) { this.url = url; sockets.push(this); }
    addEventListener() {}
    close() {}
  }
  globalThis.WebSocket = FakeSocket;
  globalThis.location = { protocol: 'https:', host: 'example.test' };
  try {
    const expiredToken = jwtWithExpiry(Math.floor(Date.now() / 1000) - 60);
    const client = new WsClient();
    let authExpired = 0;
    client.on('auth_expired', () => { authExpired += 1; });
    client.connect(expiredToken, 'participant-a', {
      refreshAccessToken: async () => { throw new Error('refresh rejected'); },
    });
    await Promise.resolve();
    await Promise.resolve();

    assert.equal(sockets.length, 0, 'expired bearer must never reach the WS URL');
    assert.equal(authExpired, 1);
    assert.equal(client.closedByUser, true);
    assert.equal(client._reconnectTimer, null);
  } finally {
    if (previousWebSocket === undefined) delete globalThis.WebSocket;
    else globalThis.WebSocket = previousWebSocket;
    if (previousLocation === undefined) delete globalThis.location;
    else globalThis.location = previousLocation;
  }
});

test('the SPA refresh adapter persists a rotated session', async () => {
  const saved = [];
  const auth = {
    getRefresh: () => 'refresh-old',
    setSession: (...args) => saved.push(args),
  };
  const api = {
    refresh: async (token) => {
      assert.equal(token, 'refresh-old');
      return {
        access_token: 'access-new',
        refresh_token: 'refresh-new',
        participant: { id: 'participant-a' },
      };
    },
  };

  const access = await refreshWsAccessToken(api, auth, { me: { id: 'participant-a' } });
  assert.equal(access, 'access-new');
  assert.deepEqual(saved, [['access-new', 'refresh-new', 'participant-a']]);
});

test('the SPA refresh adapter cannot overwrite a switched account', async () => {
  let currentRefresh = 'refresh-old';
  let writes = 0;
  const auth = {
    getRefresh: () => currentRefresh,
    setSession: () => { writes += 1; },
  };
  const state = { me: { id: 'participant-a' } };
  const api = {
    refresh: async () => {
      currentRefresh = 'refresh-account-b';
      state.me = { id: 'participant-b' };
      return {
        access_token: 'stale-access-a',
        refresh_token: 'stale-refresh-a',
        participant: { id: 'participant-a' },
      };
    },
  };

  await assert.rejects(
    refreshWsAccessToken(api, auth, state),
    /refresh session changed/,
  );
  assert.equal(writes, 0);
});

test('message sends carry the caller-provided correlation id', () => {
  const previousWebSocket = globalThis.WebSocket;
  globalThis.WebSocket = { OPEN: 1 };
  try {
    const frames = [];
    const client = new WsClient();
    client.ws = { readyState: 1, send: (raw) => frames.push(JSON.parse(raw)) };

    assert.equal(client.sendMessage(
      'room-1',
      [{ type: 'text', content: 'hello' }],
      null,
      '6f1c7051-dfb2-42ce-839a-38ff827a8abc',
    ), true);
    assert.equal(
      frames[0].client_message_id,
      '6f1c7051-dfb2-42ce-839a-38ff827a8abc',
    );
  } finally {
    if (previousWebSocket === undefined) delete globalThis.WebSocket;
    else globalThis.WebSocket = previousWebSocket;
  }
});

test('recallMessage sends a recall_message frame with the message id', () => {
  const previousWebSocket = globalThis.WebSocket;
  globalThis.WebSocket = { OPEN: 1 };
  try {
    const frames = [];
    const client = new WsClient();
    client.ws = { readyState: 1, send: (raw) => frames.push(JSON.parse(raw)) };

    assert.equal(client.recallMessage('msg-recall-1'), true);
    assert.deepEqual(frames, [{ type: 'recall_message', id: 'msg-recall-1' }]);
  } finally {
    if (previousWebSocket === undefined) delete globalThis.WebSocket;
    else globalThis.WebSocket = previousWebSocket;
  }
});

test('mutation frames never advance the legacy backfill cursor', () => {
  // Hygiene (async-reviewer #7): a `recalled` frame — even one the app applies
  // — must NOT advance the legacy `?since=` cursor. That cursor only gates the
  // NEW-message backfill; mutations (edited/deleted/recalled) converge via
  // `changes_since`, and advancing the cursor on them could skip a
  // not-yet-fetched create for the rest of the session.
  const previousWebSocket = globalThis.WebSocket;
  const previousLocation = globalThis.location;
  const sockets = [];
  class FakeSocket {
    static OPEN = 1;
    constructor() {
      this.readyState = 0;
      this.handlers = new Map();
      sockets.push(this);
    }
    addEventListener(event, fn) { this.handlers.set(event, fn); }
    emit(event, payload = {}) { this.handlers.get(event)?.(payload); }
    send() {}
    close() {}
  }
  globalThis.WebSocket = FakeSocket;
  globalThis.location = { protocol: 'https:', host: 'example.test' };
  let client = null;
  try {
    const applied = [];
    client = new WsClient();
    client.on('msg:recalled', (frame) => {
      applied.push(frame.message.id);
      return true;
    });
    client.connect('access-token', 'participant-a');
    const socket = sockets[0];
    socket.readyState = FakeSocket.OPEN;
    socket.emit('open');

    // Recalled frame for a message the client never held: applied by the
    // handler, but the cursor must not move.
    socket.emit('message', {
      data: JSON.stringify({
        type: 'recalled',
        message: { id: 'unknown-msg', room_id: 'room-a' },
      }),
    });
    assert.deepEqual(applied, ['unknown-msg']);
    assert.equal(client._lastSeen, null, 'recalled frame must not move the cursor');

    // An applied recall of a held message: still no cursor movement.
    socket.emit('message', {
      data: JSON.stringify({
        type: 'recalled',
        message: { id: 'held-msg-1', room_id: 'room-a' },
      }),
    });
    assert.deepEqual(applied, ['unknown-msg', 'held-msg-1']);
    assert.equal(client._lastSeen, null);

    // Ordinary room-message frames keep advancing it (legacy contract).
    socket.emit('message', {
      data: JSON.stringify({
        type: 'message',
        message: { id: 'msg-9', room_id: 'room-a' },
      }),
    });
    assert.equal(client._lastSeen, 'msg-9');
  } finally {
    client?.close();
    if (previousWebSocket === undefined) delete globalThis.WebSocket;
    else globalThis.WebSocket = previousWebSocket;
    if (previousLocation === undefined) delete globalThis.location;
    else globalThis.location = previousLocation;
  }
});

test('capabilities become visible only when welcome arrives', () => {
  const previousWebSocket = globalThis.WebSocket;
  const previousLocation = globalThis.location;
  const sockets = [];
  class FakeSocket {
    static OPEN = 1;
    constructor() {
      this.readyState = 0;
      this.handlers = new Map();
      sockets.push(this);
    }
    addEventListener(event, fn) { this.handlers.set(event, fn); }
    emit(event, payload = {}) { this.handlers.get(event)?.(payload); }
    send() {}
    close() {}
  }
  globalThis.WebSocket = FakeSocket;
  globalThis.location = { protocol: 'https:', host: 'example.test' };
  try {
    const client = new WsClient();
    const observed = [];
    client.on('msg:welcome', () => observed.push(client.supports('message_ack_v1')));
    client.connect('access-token', 'p1');
    sockets[0].readyState = FakeSocket.OPEN;
    sockets[0].emit('open');
    assert.equal(client.supports('message_ack_v1'), false);
    sockets[0].emit('message', {
      data: JSON.stringify({
        type: 'welcome',
        participant: 'p1',
        capabilities: ['message_ack_v1', 'delivery_cursor_v2'],
      }),
    });
    assert.equal(client.supports('message_ack_v1'), true);
    assert.equal(client.supports('delivery_cursor_v2'), true);
    assert.deepEqual(observed, [true]);
    client.close();
  } finally {
    if (previousWebSocket === undefined) delete globalThis.WebSocket;
    else globalThis.WebSocket = previousWebSocket;
    if (previousLocation === undefined) delete globalThis.location;
    else globalThis.location = previousLocation;
  }
});

test('cursor mode persists an applied message and sends a monotonic delivery ACK', () => {
  const previousWebSocket = globalThis.WebSocket;
  const previousLocation = globalThis.location;
  const sockets = [];
  class FakeSocket {
    static OPEN = 1;
    constructor(url) {
      this.url = url;
      this.readyState = 0;
      this.handlers = new Map();
      this.frames = [];
      sockets.push(this);
    }
    addEventListener(event, fn) { this.handlers.set(event, fn); }
    emit(event, payload = {}) { this.handlers.get(event)?.(payload); }
    send(raw) { this.frames.push(JSON.parse(raw)); }
    close() {}
  }
  globalThis.WebSocket = FakeSocket;
  globalThis.location = { protocol: 'https:', host: 'example.test' };
  try {
    const storage = new MemoryStorage();
    const client = new WsClient({ cursorStorage: storage });
    const applied = [];
    client.on('msg:message', (frame) => applied.push(frame.message.id));
    client.connect('access-token', 'participant-a');
    const socket = sockets[0];
    assert.match(socket.url, /[?&]cursors=1(?:&|$)/);
    socket.readyState = FakeSocket.OPEN;
    socket.emit('open');
    socket.emit('message', {
      data: JSON.stringify({
        type: 'welcome',
        participant: 'participant-a',
        capabilities: ['delivery_cursor_v2'],
      }),
    });
    socket.emit('message', {
      data: JSON.stringify({
        type: 'message',
        seq: 7,
        delivery_ordinal: 7,
        message: { id: 'message-7', room_id: 'room-a' },
      }),
    });
    assert.deepEqual(applied, ['message-7']);
    assert.deepEqual(socket.frames, []);
    assert.equal(
      storage.values.size,
      0,
      'an interleaved live frame cannot advance durably before backfill completes',
    );
    socket.emit('message', {
      data: JSON.stringify({
        type: 'delivery_ready',
        rooms: [{ room_id: 'room-a', delivery_ordinal: 6 }],
      }),
    });
    client._flushDeliveryAcks();
    assert.deepEqual(socket.frames, [{
      type: 'delivery_ack',
      room_id: 'room-a',
      message_id: 'message-7',
      delivery_ordinal: 7,
      seq: 7,
    }]);

    // A stale redelivery neither rolls local state back nor creates another ACK.
    socket.emit('message', {
      data: JSON.stringify({
        type: 'message',
        seq: 6,
        delivery_ordinal: 6,
        message: { id: 'message-6', room_id: 'room-a' },
      }),
    });
    client._flushDeliveryAcks();
    assert.equal(socket.frames.length, 1);
    client.close();
  } finally {
    if (previousWebSocket === undefined) delete globalThis.WebSocket;
    else globalThis.WebSocket = previousWebSocket;
    if (previousLocation === undefined) delete globalThis.location;
    else globalThis.location = previousLocation;
  }
});

test('cursor mode acknowledges seq-less backfill after the ready barrier', () => {
  const previousWebSocket = globalThis.WebSocket;
  const previousLocation = globalThis.location;
  const sockets = [];
  class FakeSocket {
    static OPEN = 1;
    constructor(url) {
      this.url = url;
      this.readyState = FakeSocket.OPEN;
      this.handlers = new Map();
      this.frames = [];
      sockets.push(this);
    }
    addEventListener(event, fn) { this.handlers.set(event, fn); }
    emit(event, payload = {}) { this.handlers.get(event)?.(payload); }
    send(raw) { this.frames.push(JSON.parse(raw)); }
    close() {}
  }
  globalThis.WebSocket = FakeSocket;
  globalThis.location = { protocol: 'https:', host: 'example.test' };
  try {
    const storage = new MemoryStorage();
    const client = new WsClient({ cursorStorage: storage });
    client.connect('access-token', 'participant-a');
    const socket = sockets[0];
    socket.emit('message', {
      data: JSON.stringify({
        type: 'welcome',
        participant: 'participant-a',
        capabilities: ['delivery_cursor_v2'],
      }),
    });
    for (const ordinal of [4, 5]) {
      socket.emit('message', {
        data: JSON.stringify({
          type: 'message',
          delivery_ordinal: ordinal,
          message: { id: `message-${ordinal}`, room_id: 'room-a' },
        }),
      });
    }
    socket.emit('message', {
      data: JSON.stringify({
        type: 'message',
        delivery_ordinal: 8,
        message: { id: 'unlisted-message', room_id: 'room-b' },
      }),
    });
    assert.deepEqual(socket.frames, [], 'backfill cannot ACK before its barrier');
    socket.emit('message', {
      data: JSON.stringify({
        type: 'delivery_ready',
        rooms: [{ room_id: 'room-a', delivery_ordinal: 5 }],
      }),
    });
    client._flushDeliveryAcks();
    assert.deepEqual(socket.frames, [{
      type: 'delivery_ack',
      room_id: 'room-a',
      message_id: 'message-5',
      delivery_ordinal: 5,
      seq: 0,
    }]);
    assert.equal(storage.values.get('aero_delivery_cursors_v1:participant-a')
      ?.includes('room-b'), false, 'pre-ready messages outside the barrier are not persisted');
    client.close();
  } finally {
    if (previousWebSocket === undefined) delete globalThis.WebSocket;
    else globalThis.WebSocket = previousWebSocket;
    if (previousLocation === undefined) delete globalThis.location;
    else globalThis.location = previousLocation;
  }
});

test('a reloaded account restores only its own cursor and keeps legacy since fallback', () => {
  const previousWebSocket = globalThis.WebSocket;
  const previousLocation = globalThis.location;
  const sockets = [];
  class FakeSocket {
    static OPEN = 1;
    constructor(url) {
      this.url = url;
      this.readyState = FakeSocket.OPEN;
      this.handlers = new Map();
      this.frames = [];
      sockets.push(this);
    }
    addEventListener(event, fn) { this.handlers.set(event, fn); }
    emit(event, payload = {}) { this.handlers.get(event)?.(payload); }
    send(raw) { this.frames.push(JSON.parse(raw)); }
    close() {}
  }
  globalThis.WebSocket = FakeSocket;
  globalThis.location = { protocol: 'http:', host: 'example.test' };
  try {
    const storage = new MemoryStorage();
    const first = new WsClient({ cursorStorage: storage });
    first.connect('token-a', 'participant-a');
    sockets[0].emit('message', {
      data: JSON.stringify({
        type: 'welcome',
        participant: 'participant-a',
        capabilities: ['delivery_cursor_v2'],
      }),
    });
    sockets[0].emit('message', {
      data: JSON.stringify({
        type: 'delivery_ready',
        rooms: [{ room_id: 'room-a', delivery_ordinal: 10 }],
      }),
    });
    sockets[0].emit('message', {
      data: JSON.stringify({
        type: 'message',
        seq: 11,
        delivery_ordinal: 11,
        message: { id: 'message-11', room_id: 'room-a' },
      }),
    });
    first.close();

    const reloaded = new WsClient({ cursorStorage: storage });
    reloaded.connect('token-a', 'participant-a');
    const ownSocket = sockets[1];
    ownSocket.emit('message', {
      data: JSON.stringify({
        type: 'welcome',
        participant: 'participant-a',
        capabilities: ['delivery_cursor_v2'],
      }),
    });
    ownSocket.emit('message', {
      data: JSON.stringify({
        type: 'delivery_ready',
        rooms: [{ room_id: 'room-a', delivery_ordinal: 11 }],
      }),
    });
    assert.deepEqual(ownSocket.frames, [{
      type: 'delivery_ack',
      room_id: 'room-a',
      message_id: 'message-11',
      delivery_ordinal: 11,
      seq: 11,
    }]);

    const other = new WsClient({ cursorStorage: storage });
    other.connect('token-b', 'participant-b');
    const otherSocket = sockets[2];
    otherSocket.emit('message', {
      data: JSON.stringify({
        type: 'welcome',
        participant: 'participant-b',
        capabilities: ['delivery_cursor_v2'],
      }),
    });
    otherSocket.emit('message', {
      data: JSON.stringify({ type: 'delivery_ready', rooms: [] }),
    });
    assert.deepEqual(otherSocket.frames, []);

    // `since` remains additive for old servers while modern servers prefer
    // cursors=1. This protects a rolling upgrade/downgrade boundary.
    other._lastSeen = 'message-global';
    other._open();
    assert.match(sockets[3].url, /[?&]cursors=1(?:&|$)/);
    assert.match(sockets[3].url, /[?&]since=message-global(?:&|$)/);

    const unauthorized = new WsClient({ cursorStorage: storage });
    unauthorized.connect('token-a', 'participant-a');
    const unauthorizedSocket = sockets[4];
    unauthorizedSocket.emit('message', {
      data: JSON.stringify({
        type: 'welcome', participant: 'participant-a',
        capabilities: ['delivery_cursor_v2'],
      }),
    });
    unauthorizedSocket.emit('message', {
      data: JSON.stringify({ type: 'delivery_ready', rooms: [] }),
    });
    assert.deepEqual(unauthorizedSocket.frames, [], 'saved cursors outside the barrier are not restored');

    const malformed = new WsClient({ cursorStorage: storage });
    malformed.connect('token-a', 'participant-a');
    const malformedSocket = sockets[5];
    malformedSocket.emit('message', {
      data: JSON.stringify({
        type: 'welcome', participant: 'participant-a',
        capabilities: ['delivery_cursor_v2'],
      }),
    });
    malformedSocket.emit('message', {
      data: JSON.stringify({ type: 'delivery_ready', rooms: {} }),
    });
    assert.deepEqual(malformedSocket.frames, [], 'a malformed barrier fails closed');
    reloaded.close();
    other.close();
    unauthorized.close();
    malformed.close();
  } finally {
    if (previousWebSocket === undefined) delete globalThis.WebSocket;
    else globalThis.WebSocket = previousWebSocket;
    if (previousLocation === undefined) delete globalThis.location;
    else globalThis.location = previousLocation;
  }
});

test('a throwing message handler neither advances nor ACKs and forces replay', () => {
  const previousWebSocket = globalThis.WebSocket;
  const previousLocation = globalThis.location;
  const previousConsoleError = console.error;
  const sockets = [];
  class FakeSocket {
    static OPEN = 1;
    constructor() {
      this.readyState = FakeSocket.OPEN;
      this.handlers = new Map();
      this.frames = [];
      this.closes = [];
      sockets.push(this);
    }
    addEventListener(event, fn) { this.handlers.set(event, fn); }
    emit(event, payload = {}) { this.handlers.get(event)?.(payload); }
    send(raw) { this.frames.push(JSON.parse(raw)); }
    close(code, reason) { this.closes.push({ code, reason }); }
  }
  globalThis.WebSocket = FakeSocket;
  globalThis.location = { protocol: 'https:', host: 'example.test' };
  console.error = () => {};
  try {
    const storage = new MemoryStorage();
    const client = new WsClient({ cursorStorage: storage });
    client.on('msg:message', () => { throw new Error('render failed'); });
    client.connect('token-a', 'participant-a');
    const socket = sockets[0];
    socket.emit('message', {
      data: JSON.stringify({
        type: 'welcome',
        participant: 'participant-a',
        capabilities: ['delivery_cursor_v2'],
      }),
    });
    socket.emit('message', {
      data: JSON.stringify({
        type: 'delivery_ready',
        rooms: [{ room_id: 'room-a', delivery_ordinal: 0 }],
      }),
    });
    client._pendingDeliveryAcks.set('room-a', {
      room_id: 'room-a', message_id: 'stale', delivery_ordinal: 1, seq: 1,
    });
    client._preReadyDeliveryAcks.set('room-a', {
      room_id: 'room-a', message_id: 'stale', delivery_ordinal: 1, seq: 1,
    });
    client._pausedDeliveryAcks.set('room-a', {
      room_id: 'room-a', message_id: 'stale', delivery_ordinal: 1, seq: 1,
    });
    socket.emit('message', {
      data: JSON.stringify({
        type: 'message',
        seq: 0,
        delivery_ordinal: 1,
        room_id: 'room-a',
        message: { id: 'message-1' },
      }),
    });
    client._flushDeliveryAcks();

    assert.deepEqual(socket.frames, []);
    assert.equal(client._deliveryFailedRooms.has('room-a'), true);
    assert.equal(client._pendingDeliveryAcks.has('room-a'), false);
    assert.equal(client._preReadyDeliveryAcks.has('room-a'), false);
    assert.equal(client._pausedDeliveryAcks.has('room-a'), false);
    assert.equal(client._seqGate.scopes.has('r:room-a'), false);
    assert.equal(storage.values.size, 0);
    assert.equal(client._lastSeen, null, 'legacy fallback must not skip rejected content');
    assert.deepEqual(socket.closes, [{
      code: 1011,
      reason: 'message application failed',
    }]);
    client.close();
  } finally {
    console.error = previousConsoleError;
    if (previousWebSocket === undefined) delete globalThis.WebSocket;
    else globalThis.WebSocket = previousWebSocket;
    if (previousLocation === undefined) delete globalThis.location;
    else globalThis.location = previousLocation;
  }
});

test('application catch-up pauses durable ACKs and commits only on success', () => {
  const previousWebSocket = globalThis.WebSocket;
  const previousLocation = globalThis.location;
  const sockets = [];
  class FakeSocket {
    static OPEN = 1;
    constructor() {
      this.readyState = FakeSocket.OPEN;
      this.handlers = new Map();
      this.frames = [];
      this.closes = [];
      sockets.push(this);
    }
    addEventListener(event, fn) { this.handlers.set(event, fn); }
    emit(event, payload = {}) { this.handlers.get(event)?.(payload); }
    send(raw) { this.frames.push(JSON.parse(raw)); }
    close(code, reason) { this.closes.push({ code, reason }); }
  }
  globalThis.WebSocket = FakeSocket;
  globalThis.location = { protocol: 'https:', host: 'example.test' };
  try {
    const storage = new MemoryStorage();
    const client = new WsClient({ cursorStorage: storage });
    client.connect('token-a', 'participant-a');
    const socket = sockets[0];
    socket.emit('message', {
      data: JSON.stringify({
        type: 'welcome',
        participant: 'participant-a',
        capabilities: ['delivery_cursor_v2'],
      }),
    });
    socket.emit('message', {
      data: JSON.stringify({
        type: 'delivery_ready',
        rooms: [{ room_id: 'room-a', delivery_ordinal: 0 }],
      }),
    });

    const complete = client.pauseDeliveryAcks();
    socket.emit('message', {
      data: JSON.stringify({
        type: 'message',
        seq: 5,
        delivery_ordinal: 5,
        message: { id: 'message-5', room_id: 'room-a' },
      }),
    });
    assert.equal(storage.values.size, 0);
    assert.deepEqual(socket.frames, []);
    complete(true);
    client._flushDeliveryAcks();
    assert.deepEqual(socket.frames, [{
      type: 'delivery_ack',
      room_id: 'room-a',
      message_id: 'message-5',
      delivery_ordinal: 5,
      seq: 5,
    }]);

    const fail = client.pauseDeliveryAcks();
    socket.emit('message', {
      data: JSON.stringify({
        type: 'message',
        seq: 6,
        delivery_ordinal: 6,
        message: { id: 'message-6', room_id: 'room-a' },
      }),
    });
    fail(false);
    client._flushDeliveryAcks();
    assert.equal(socket.frames.length, 1, 'failed catch-up cannot ACK its held frame');
    const persisted = JSON.parse(Array.from(storage.values.values())[0]);
    assert.equal(persisted.rooms['room-a'].delivery_ordinal, 5);
    assert.ok(socket.closes.some(({ code }) => code === 1011));
    client.close();
  } finally {
    if (previousWebSocket === undefined) delete globalThis.WebSocket;
    else globalThis.WebSocket = previousWebSocket;
    if (previousLocation === undefined) delete globalThis.location;
    else globalThis.location = previousLocation;
  }
});

test('stale account-A socket callbacks cannot contaminate account B', () => {
  const previousWebSocket = globalThis.WebSocket;
  const previousLocation = globalThis.location;
  const sockets = [];
  class FakeSocket {
    static OPEN = 1;
    constructor() {
      this.readyState = FakeSocket.OPEN;
      this.handlers = new Map();
      this.frames = [];
      sockets.push(this);
    }
    addEventListener(event, fn) { this.handlers.set(event, fn); }
    emit(event, payload = {}) { this.handlers.get(event)?.(payload); }
    send(raw) { this.frames.push(JSON.parse(raw)); }
    close() {}
  }
  globalThis.WebSocket = FakeSocket;
  globalThis.location = { protocol: 'https:', host: 'example.test' };
  try {
    const storage = new MemoryStorage();
    const applied = [];
    const client = new WsClient({ cursorStorage: storage });
    client.on('msg:message', (frame) => applied.push(frame.message.id));

    client.connect('token-a', 'participant-a');
    const old = sockets[0];
    old.emit('message', {
      data: JSON.stringify({
        type: 'welcome',
        participant: 'participant-a',
        capabilities: ['delivery_cursor_v2'],
      }),
    });
    old.emit('message', {
      data: JSON.stringify({ type: 'delivery_ready', rooms: [] }),
    });

    client.connect('token-b', 'participant-b');
    const current = sockets[1];
    current.emit('message', {
      data: JSON.stringify({
        type: 'welcome',
        participant: 'participant-b',
        capabilities: ['delivery_cursor_v2'],
      }),
    });
    current.emit('message', {
      data: JSON.stringify({
        type: 'delivery_ready',
        rooms: [{ room_id: 'room-b', delivery_ordinal: 0 }],
      }),
    });

    old.emit('message', {
      data: JSON.stringify({
        type: 'message',
        seq: 99,
        delivery_ordinal: 99,
        message: { id: 'account-a-message', room_id: 'room-a' },
      }),
    });
    old.emit('close', { code: 1006 });
    old.emit('error');
    assert.equal(client.ws, current);
    assert.deepEqual(applied, []);
    assert.equal(sockets.length, 2, 'stale close must not schedule account-A reconnect');

    current.emit('message', {
      data: JSON.stringify({
        type: 'message',
        seq: 1,
        delivery_ordinal: 1,
        message: { id: 'account-b-message', room_id: 'room-b' },
      }),
    });
    client._flushDeliveryAcks();
    assert.deepEqual(applied, ['account-b-message']);
    assert.deepEqual(current.frames, [{
      type: 'delivery_ack',
      room_id: 'room-b',
      message_id: 'account-b-message',
      delivery_ordinal: 1,
      seq: 1,
    }]);
    assert.deepEqual(
      Array.from(storage.values.keys()).map((key) => decodeURIComponent(key)),
      ['aero_delivery_cursors_v1:participant-b'],
    );
    client.close();
  } finally {
    if (previousWebSocket === undefined) delete globalThis.WebSocket;
    else globalThis.WebSocket = previousWebSocket;
    if (previousLocation === undefined) delete globalThis.location;
    else globalThis.location = previousLocation;
  }
});
