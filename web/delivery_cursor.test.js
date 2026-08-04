import test from 'node:test';
import assert from 'node:assert/strict';

import {
  DELIVERY_CURSOR_STORAGE_PREFIX,
  DeliveryCursorLedger,
} from './delivery_cursor.js';

class MemoryStorage {
  constructor() { this.values = new Map(); }
  getItem(key) { return this.values.get(key) ?? null; }
  setItem(key, value) { this.values.set(key, String(value)); }
}

test('delivery cursors persist monotonically and survive a reload', () => {
  const storage = new MemoryStorage();
  const first = new DeliveryCursorLedger(storage);
  first.open('participant-a');

  assert.deepEqual(first.advance('room-a', 'message-5', 5, 5), {
    room_id: 'room-a', message_id: 'message-5', delivery_ordinal: 5, seq: 5,
  });
  assert.equal(first.advance('room-a', 'message-4', 4, 4), null);

  const reloaded = new DeliveryCursorLedger(storage);
  reloaded.open('participant-a');
  assert.deepEqual(reloaded.entries(), [{
    room_id: 'room-a', message_id: 'message-5', delivery_ordinal: 5, seq: 5,
  }]);
});

test('the room ordinal alone governs the content prefix', () => {
  const storage = new MemoryStorage();
  const ledger = new DeliveryCursorLedger(storage);
  ledger.open('participant-a');

  ledger.advance('room-a', 'message-5', 5, 5);
  assert.deepEqual(ledger.advance('room-a', 'message-1', 9, 3), {
    room_id: 'room-a', message_id: 'message-1', delivery_ordinal: 9, seq: 5,
  });
  assert.equal(ledger.advance('room-a', 'message-99', 7, 10), null);
  assert.deepEqual(ledger.advance('room-a', 'message-1', 9, 10), {
    room_id: 'room-a', message_id: 'message-1', delivery_ordinal: 9, seq: 10,
  });
  assert.equal(ledger.advance('room-a', 'message-99', 7, 8), null);
});

test('a seq-less backfill advances and survives with the zero sentinel', () => {
  const storage = new MemoryStorage();
  const ledger = new DeliveryCursorLedger(storage);
  ledger.open('participant-a');
  assert.deepEqual(ledger.advance('room-a', 'message-9', 9, 0), {
    room_id: 'room-a', message_id: 'message-9', delivery_ordinal: 9, seq: 0,
  });

  const reloaded = new DeliveryCursorLedger(storage);
  reloaded.open('participant-a');
  assert.deepEqual(reloaded.entries(), [{
    room_id: 'room-a', message_id: 'message-9', delivery_ordinal: 9, seq: 0,
  }]);
});

test('delivery cursor storage is isolated by authenticated participant', () => {
  const storage = new MemoryStorage();
  const ledger = new DeliveryCursorLedger(storage);
  ledger.open('participant-a');
  ledger.advance('room-a', 'message-a', 4, 7);

  ledger.open('participant-b');
  assert.deepEqual(ledger.entries(), []);
  ledger.advance('room-b', 'message-b', 6, 9);

  ledger.open('participant-a');
  assert.deepEqual(ledger.entries(), [{
    room_id: 'room-a', message_id: 'message-a', delivery_ordinal: 4, seq: 7,
  }]);
  assert.ok(storage.getItem(`${DELIVERY_CURSOR_STORAGE_PREFIX}participant-a`));
  assert.ok(storage.getItem(`${DELIVERY_CURSOR_STORAGE_PREFIX}participant-b`));
});

test('a failed localStorage write never produces an acknowledgeable cursor', () => {
  const storage = {
    getItem() { return null; },
    setItem() { throw new Error('quota'); },
  };
  const ledger = new DeliveryCursorLedger(storage);
  ledger.open('participant-a');
  assert.equal(ledger.advance('room-a', 'message-a', 1, 1), null);
  assert.deepEqual(ledger.entries(), []);
});
