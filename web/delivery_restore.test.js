import test from 'node:test';
import assert from 'node:assert/strict';

import {
  MESSAGE_ACK_CAPABILITY,
  initReliableDelivery,
  pendingTempId,
  retryPendingMessage,
} from './delivery.js';

class MemoryStorage {
  constructor(entries = {}) { this.values = new Map(Object.entries(entries)); }
  get length() { return this.values.size; }
  key(index) { return [...this.values.keys()][index] ?? null; }
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

test('a reloaded runtime retries even when its socket counter matches the persisted one', () => {
  const previousStorage = Object.getOwnPropertyDescriptor(globalThis, 'localStorage');
  const previousWindow = globalThis.window;
  const createdAt = new Date().toISOString();
  const clientMessageId = '33333333-4444-4555-8666-777777777777';
  const storage = new MemoryStorage({
    aero_pid: 'p-reload-marker',
    [`aero_pending_delivery_v1:p-reload-marker:item:${clientMessageId}`]: JSON.stringify({
      version: 1,
      saved_at: createdAt,
      item: {
        id: pendingTempId(clientMessageId),
        room_id: 'r1',
        sender_id: 'p-reload-marker',
        blocks: [{ type: 'text', content: 'retry after reload' }],
        created_at: createdAt,
        client_message_id: clientMessageId,
        outbound: {
          kind: 'blocks', roomId: 'r1',
          blocks: [{ type: 'text', content: 'retry after reload' }],
        },
        attempts: 1,
        last_connection_id: 1,
        delivery_status: 'sending',
        retryable: true,
      },
    }),
  });
  Object.defineProperty(globalThis, 'localStorage', { configurable: true, value: storage });
  globalThis.window = { localStorage: storage, addEventListener: () => {} };
  const ws = new FakeWs();
  ws.connectionId = 1;
  const pending = new Map();
  const dispose = initReliableDelivery({
    ws,
    getPendingMap: () => pending,
    onCanonical: () => {},
    onRestore: (item) => { pending.set(item.id, item); return item; },
    onFailure: () => {},
  });
  try {
    ws.emit('msg:welcome', { capabilities: [MESSAGE_ACK_CAPABILITY] });
    assert.equal(ws.sent.length, 1);
    assert.equal(ws.sent[0].clientMessageId, clientMessageId);
    assert.notEqual(pending.get(pendingTempId(clientMessageId)).last_connection_id, 1);
  } finally {
    dispose();
    if (previousWindow === undefined) delete globalThis.window;
    else globalThis.window = previousWindow;
    if (previousStorage === undefined) delete globalThis.localStorage;
    else Object.defineProperty(globalThis, 'localStorage', previousStorage);
  }
});

test('an exhausted in-flight item restores as failed and remains manually retryable', () => {
  const previousStorage = Object.getOwnPropertyDescriptor(globalThis, 'localStorage');
  const previousWindow = globalThis.window;
  const participantId = 'p-exhausted';
  const clientMessageId = '44444444-5555-4666-8777-888888888888';
  const createdAt = new Date().toISOString();
  const entryKey = `aero_pending_delivery_v1:${participantId}:item:${clientMessageId}`;
  const storage = new MemoryStorage({
    aero_pid: participantId,
    [entryKey]: JSON.stringify({
      version: 1,
      item: {
        id: pendingTempId(clientMessageId),
        room_id: 'r1',
        sender_id: participantId,
        blocks: [{ type: 'text', content: 'last attempt' }],
        created_at: createdAt,
        client_message_id: clientMessageId,
        outbound: {
          kind: 'blocks', roomId: 'r1',
          blocks: [{ type: 'text', content: 'last attempt' }],
        },
        attempts: 3,
        last_connection_id: 'old-runtime:1',
        delivery_status: 'sending',
        retryable: true,
      },
    }),
  });
  Object.defineProperty(globalThis, 'localStorage', { configurable: true, value: storage });
  globalThis.window = { localStorage: storage, addEventListener: () => {} };
  const ws = new FakeWs();
  const pending = new Map();
  const dispose = initReliableDelivery({
    ws,
    getPendingMap: () => pending,
    onCanonical: () => {},
    onRestore: (item) => { pending.set(item.id, item); return item; },
    onFailure: () => {},
  });
  try {
    ws.emit('msg:welcome', { capabilities: [MESSAGE_ACK_CAPABILITY] });
    const restored = pending.get(pendingTempId(clientMessageId));
    assert.ok(restored);
    assert.equal(restored.delivery_status, 'failed');
    assert.equal(ws.sent.length, 0, 'exhausted items are never auto-retried');
    assert.equal(JSON.parse(storage.getItem(entryKey)).item.delivery_status, 'failed');
    assert.equal(retryPendingMessage(clientMessageId), true);
    assert.equal(ws.sent.length, 1);
    assert.equal(ws.sent[0].clientMessageId, clientMessageId);
  } finally {
    dispose();
    if (previousWindow === undefined) delete globalThis.window;
    else globalThis.window = previousWindow;
    if (previousStorage === undefined) delete globalThis.localStorage;
    else Object.defineProperty(globalThis, 'localStorage', previousStorage);
  }
});
