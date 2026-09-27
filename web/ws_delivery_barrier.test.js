import test from 'node:test';
import assert from 'node:assert/strict';
import { WsClient } from './ws.js';

class MemoryStorage {
  constructor() { this.values = new Map(); }
  getItem(key) { return this.values.get(key) ?? null; }
  setItem(key, value) { this.values.set(key, String(value)); }
}

test('a failed delivery_ready handler cannot restore or acknowledge persisted cursors', () => {
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
    emit(event, payload = {}) { return this.handlers.get(event)?.(payload); }
    send(raw) { this.frames.push(JSON.parse(raw)); }
    close(code, reason) { this.closes.push({ code, reason }); }
  }
  globalThis.WebSocket = FakeSocket;
  globalThis.location = { protocol: 'https:', host: 'example.test' };
  console.error = () => {};
  try {
    const storage = new MemoryStorage();
    const key = 'aero_delivery_cursors_v1:participant-a';
    const seed = new WsClient({ cursorStorage: storage });
    seed.connect('token-a', 'participant-a');
    const seedSocket = sockets[0];
    seedSocket.emit('message', { data: JSON.stringify({
      type: 'welcome', participant: 'participant-a', capabilities: ['delivery_cursor_v2'],
    }) });
    seedSocket.emit('message', { data: JSON.stringify({
      type: 'delivery_ready', rooms: [{ room_id: 'room-a', delivery_ordinal: 0 }],
    }) });
    seedSocket.emit('message', { data: JSON.stringify({
      type: 'message', seq: 1, delivery_ordinal: 1,
      message: { id: 'message-1', room_id: 'room-a' },
    }) });
    seed._flushDeliveryAcks();
    const persisted = storage.values.get(key);
    assert.ok(persisted, 'seed a durable cursor for the next connection');
    seed.close();

    const client = new WsClient({ cursorStorage: storage });
    client.on('msg:delivery_ready', () => { throw new Error('barrier application failed'); });
    client.connect('token-a', 'participant-a');
    const socket = sockets[1];
    socket.emit('message', { data: JSON.stringify({
      type: 'welcome', participant: 'participant-a', capabilities: ['delivery_cursor_v2'],
    }) });
    socket.emit('message', { data: JSON.stringify({
      type: 'delivery_ready', rooms: [{ room_id: 'room-a', delivery_ordinal: 1 }],
    }) });
    client._flushDeliveryAcks();

    assert.deepEqual(socket.frames, [], 'failed barrier application must not ACK restored cursors');
    assert.equal(client._deliveryReady, false);
    assert.equal(client._deliveryAuthorizedRooms.size, 0);
    assert.equal(storage.values.get(key), persisted, 'failed barrier cannot change the durable ledger');
    assert.deepEqual(socket.closes, [{ code: 1011, reason: 'message application failed' }]);
    client.close();
  } finally {
    console.error = previousConsoleError;
    if (previousWebSocket === undefined) delete globalThis.WebSocket;
    else globalThis.WebSocket = previousWebSocket;
    if (previousLocation === undefined) delete globalThis.location;
    else globalThis.location = previousLocation;
  }
});
