import test from 'node:test';
import assert from 'node:assert/strict';

import {
  MESSAGE_ACK_CAPABILITY,
  clearPendingDelivery,
  discardPersistedDeliveries,
  findPendingMatch,
  initReliableDelivery,
  pendingTempId,
  sendOptimistically,
} from './delivery.js';

class MemoryStorage {
  constructor(entries = {}) { this.values = new Map(Object.entries(entries)); }
  getItem(key) { return this.values.get(key) ?? null; }
  setItem(key, value) { this.values.set(key, String(value)); }
  removeItem(key) { this.values.delete(key); }
}

class FakeWs {
  constructor() {
    this.handlers = new Map();
    this.capabilities = new Set([MESSAGE_ACK_CAPABILITY]);
    this.connectionId = 1;
    this.sent = [];
  }
  on(event, fn) { this.handlers.set(event, fn); }
  emit(event, frame) { this.handlers.get(event)?.(frame); }
  supports(capability) { return this.capabilities.has(capability); }
  sendMessage(roomId, blocks, replyTo, clientMessageId) {
    this.sent.push({ roomId, blocks, replyTo, clientMessageId });
    return true;
  }
}

test('optimistic pending is added only after the socket accepts the frame', () => {
  const calls = [];
  let sentId;
  let pendingDelivery;
  const accepted = sendOptimistically(
    (id) => { sentId = id; calls.push('send'); return true; },
    (delivery) => { pendingDelivery = delivery; calls.push('pending'); },
    { kind: 'blocks', roomId: 'r1', blocks: [{ type: 'text', content: 'hi' }] },
  );

  assert.equal(accepted, true);
  assert.deepEqual(calls, ['send', 'pending']);
  assert.match(sentId, /^[0-9a-f-]{36}$/);
  assert.equal(pendingDelivery.client_message_id, sentId);
  assert.equal(pendingDelivery.attempts, 1);
  assert.equal(pendingDelivery.delivery_status, 'sending');
});

test('failed socket send never creates a pending message', () => {
  const previousDocument = globalThis.document;
  const previousLog = console.log;
  globalThis.document = { getElementById: () => null };
  console.log = () => {};
  try {
    let pendingAdded = false;
    const accepted = sendOptimistically(
      () => false,
      () => { pendingAdded = true; },
    );
    assert.equal(accepted, false);
    assert.equal(pendingAdded, false);
  } finally {
    console.log = previousLog;
    if (previousDocument === undefined) delete globalThis.document;
    else globalThis.document = previousDocument;
  }
});

test('ACK settles the exact client id and reconnect retries that same id', () => {
  const ws = new FakeWs();
  const pending = new Map();
  const canonical = [];
  initReliableDelivery({
    ws,
    getPendingMap: () => pending,
    onCanonical: (message, id) => {
      canonical.push({ message, id });
      const item = pending.get(pendingTempId(id));
      clearPendingDelivery(item);
      pending.delete(pendingTempId(id));
    },
    onFailure: () => {},
  });

  let firstId;
  sendOptimistically(
    (id) => { firstId = id; return true; },
    (delivery) => {
      const item = { id: pendingTempId(delivery.client_message_id), ...delivery };
      pending.set(item.id, item);
      return item;
    },
    {
      kind: 'blocks', roomId: 'r1',
      blocks: [{ type: 'text', content: 'same request' }], replyTo: null,
    },
  );

  ws.connectionId = 2;
  ws.emit('msg:welcome', { capabilities: [MESSAGE_ACK_CAPABILITY] });
  assert.equal(ws.sent.length, 1);
  assert.equal(ws.sent[0].clientMessageId, firstId);

  const message = { id: 'm1', room_id: 'r1' };
  ws.emit('msg:message_ack', { client_message_id: firstId, message });
  assert.deepEqual(canonical, [{ message, id: firstId }]);
  assert.equal(pending.size, 0);
});

test('legacy echo fallback never crosses rooms, replies, or attachment payloads', () => {
  const pending = new Map([
    ['room-a', {
      sender_id: 'me', room_id: 'a', reply_to: null,
      blocks: [{ type: 'file', blob_id: 'blob-a' }],
      created_at: '2026-01-01T00:00:00Z',
    }],
    ['room-b', {
      sender_id: 'me', room_id: 'b', reply_to: 'parent-b',
      blocks: [{ type: 'file', blob_id: 'blob-b' }],
      created_at: '2026-01-01T00:00:00Z',
    }],
  ]);
  assert.equal(findPendingMatch({
    sender_id: 'me', room_id: 'b', reply_to: 'parent-b',
    blocks: [{ type: 'file', blob_id: 'blob-b' }],
    created_at: '2026-01-01T00:00:05Z',
  }, pending, 'me'), 'room-b');
  assert.equal(findPendingMatch({
    sender_id: 'me', room_id: 'b', reply_to: null,
    blocks: [{ type: 'file', blob_id: 'blob-b' }],
    created_at: '2026-01-01T00:00:05Z',
  }, pending, 'me'), null);
});

test('a non-retryable NACK leaves one visible failed pending item', () => {
  const ws = new FakeWs();
  const pending = new Map();
  initReliableDelivery({
    ws,
    getPendingMap: () => pending,
    onCanonical: () => {},
    onFailure: () => {},
  });
  let clientMessageId;
  sendOptimistically(
    (id) => { clientMessageId = id; return true; },
    (delivery) => {
      const item = { id: pendingTempId(delivery.client_message_id), ...delivery };
      pending.set(item.id, item);
      return item;
    },
    { kind: 'blocks', roomId: 'r1', blocks: [{ type: 'text', content: 'bad' }] },
  );
  ws.emit('msg:message_nack', {
    client_message_id: clientMessageId,
    code: 'invalid',
    msg: 'invalid block',
    retryable: false,
  });
  const item = pending.get(pendingTempId(clientMessageId));
  assert.equal(pending.size, 1);
  assert.equal(item.delivery_status, 'failed');
  assert.equal(item.failure_message, 'invalid block');
  assert.equal(item._ackTimer, null);
});

test('an offline send becomes a visible, participant-scoped persistent outbox item', () => {
  const previousStorage = Object.getOwnPropertyDescriptor(globalThis, 'localStorage');
  const previousDocument = globalThis.document;
  const previousWindow = globalThis.window;
  const previousLog = console.log;
  const storage = new MemoryStorage({ aero_pid: 'p-offline' });
  Object.defineProperty(globalThis, 'localStorage', { configurable: true, value: storage });
  globalThis.window = { localStorage: storage, addEventListener: () => {} };
  globalThis.document = { getElementById: () => null };
  console.log = () => {};
  const ws = new FakeWs();
  const pending = new Map();
  initReliableDelivery({
    ws,
    getPendingMap: () => pending,
    onCanonical: () => {},
    onFailure: () => {},
  });
  try {
    const accepted = sendOptimistically(
      () => false,
      (delivery) => {
        const item = {
          id: pendingTempId(delivery.client_message_id),
          room_id: 'r1',
          sender_id: 'p-offline',
          blocks: [{ type: 'text', content: 'queued' }],
          created_at: new Date().toISOString(),
          ...delivery,
        };
        pending.set(item.id, item);
        return item;
      },
      { kind: 'blocks', roomId: 'r1', blocks: [{ type: 'text', content: 'queued' }] },
    );
    assert.equal(accepted, true);
    assert.equal(pending.size, 1);
    assert.equal(Array.from(pending.values())[0].delivery_status, 'waiting');
    const saved = JSON.parse(storage.getItem('aero_pending_delivery_v1:p-offline'));
    assert.equal(saved.version, 1);
    assert.equal(saved.items.length, 1);
    assert.equal(saved.items[0].sender_id, 'p-offline');
    assert.equal(Object.hasOwn(saved.items[0], '_ackTimer'), false);
    discardPersistedDeliveries('p-offline');
    assert.equal(storage.getItem('aero_pending_delivery_v1:p-offline'), null);
  } finally {
    console.log = previousLog;
    if (previousDocument === undefined) delete globalThis.document;
    else globalThis.document = previousDocument;
    if (previousWindow === undefined) delete globalThis.window;
    else globalThis.window = previousWindow;
    if (previousStorage === undefined) delete globalThis.localStorage;
    else Object.defineProperty(globalThis, 'localStorage', previousStorage);
  }
});

test('welcome restores a persisted item and retries the same client id', () => {
  const previousStorage = Object.getOwnPropertyDescriptor(globalThis, 'localStorage');
  const previousWindow = globalThis.window;
  const createdAt = new Date().toISOString();
  const clientMessageId = '11111111-2222-4333-8444-555555555555';
  const storage = new MemoryStorage({
    aero_pid: 'p-restored',
    'aero_pending_delivery_v1:p-restored': JSON.stringify({
      version: 1,
      saved_at: createdAt,
      items: [{
        id: pendingTempId(clientMessageId),
        room_id: 'r2',
        sender_id: 'p-restored',
        blocks: [{ type: 'text', content: 'survives reload' }],
        reply_to: null,
        created_at: createdAt,
        client_message_id: clientMessageId,
        outbound: {
          kind: 'blocks',
          roomId: 'r2',
          blocks: [{ type: 'text', content: 'survives reload' }],
          replyTo: null,
        },
        attempts: 0,
        delivery_status: 'waiting',
        retryable: true,
      }],
    }),
  });
  Object.defineProperty(globalThis, 'localStorage', { configurable: true, value: storage });
  globalThis.window = { localStorage: storage, addEventListener: () => {} };
  const ws = new FakeWs();
  ws.connectionId = 9;
  const pending = new Map();
  initReliableDelivery({
    ws,
    getPendingMap: () => pending,
    onCanonical: () => {},
    onRestore: (item) => {
      pending.set(item.id, item);
      return item;
    },
    onFailure: () => {},
  });
  try {
    ws.emit('msg:welcome', { capabilities: [MESSAGE_ACK_CAPABILITY] });
    const restored = pending.get(pendingTempId(clientMessageId));
    assert.ok(restored);
    assert.equal(restored.delivery_status, 'sending');
    assert.equal(restored.attempts, 1);
    assert.equal(ws.sent.length, 1);
    assert.equal(ws.sent[0].clientMessageId, clientMessageId);
    clearPendingDelivery(restored);
    pending.clear();
  } finally {
    if (previousWindow === undefined) delete globalThis.window;
    else globalThis.window = previousWindow;
    if (previousStorage === undefined) delete globalThis.localStorage;
    else Object.defineProperty(globalThis, 'localStorage', previousStorage);
  }
});
