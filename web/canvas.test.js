import test from 'node:test';
import assert from 'node:assert/strict';

import { api } from './api.js';
import {
  bindCanvasRealtime,
  CanvasReducer,
  normalizeCanvasOp,
  reduceCanvasBlocks,
} from './canvas_model.js';

function row(seq, op, id = `op-${seq}`) {
  return {
    id,
    canvas_id: 'canvas-1',
    seq,
    author_id: 'participant-1',
    op,
  };
}

test('normalization prefers durable op_seq and duplicate WS echo is idempotent', () => {
  const reducer = new CanvasReducer({
    id: 'canvas-1',
    title: 'Plan',
    version: 2,
    blocks: [{ type: 'text', content: 'before' }],
  });
  assert.equal(reducer.ingest(row(1, { type: 'set_text', text: 'after' })).status, 'applied');

  const echo = {
    type: 'canvas_op',
    seq: 9876,
    op_seq: 1,
    room_id: 'room-1',
    canvas_id: 'canvas-1',
    op_id: 'op-1',
    author_id: 'participant-1',
    op: { type: 'set_text', text: 'after' },
  };
  assert.equal(normalizeCanvasOp(echo).seq, 1);
  assert.equal(reducer.ingest(echo).status, 'duplicate');
  assert.equal(reducer.cursor, 1);
  assert.deepEqual(reducer.blocks, [{ type: 'text', content: 'after' }]);
});

test('a missing live op_seq queues without using the room bus seq as the document cursor', () => {
  const reducer = new CanvasReducer({ id: 'canvas-1', blocks: [] });
  const liveFrame = {
    type: 'canvas_op',
    room_id: 'room-1',
    canvas_id: 'canvas-1',
    op_id: 'op-1',
    seq: 9876,
    op: { type: 'set_text', text: 'durable' },
  };
  assert.equal(normalizeCanvasOp(liveFrame), null);
  assert.deepEqual(reducer.ingest(liveFrame), { status: 'queued', applied: [], gap: true });
  assert.equal(reducer.cursor, 0);
  assert.equal(reducer.snapshot().pending_unsequenced, 1);

  const restored = reducer.ingest(row(1, { type: 'set_text', text: 'durable' }, 'op-1'));
  assert.equal(restored.status, 'applied');
  assert.equal(reducer.cursor, 1);
  assert.equal(reducer.snapshot().pending_unsequenced, 0);
  assert.equal(reducer.ingest(liveFrame).status, 'duplicate');
  assert.equal(reducer.snapshot().pending_unsequenced, 0);
  assert.deepEqual(reducer.blocks, [{ type: 'text', content: 'durable' }]);
});

test('out-of-order operations queue at a gap and drain strictly by op_seq', () => {
  const reducer = new CanvasReducer({ id: 'canvas-1', version: 0, blocks: [] });

  assert.deepEqual(
    reducer.ingest(row(3, { type: 'add_note', note_id: 'n3', text: 'third' })),
    { status: 'queued', applied: [], gap: true },
  );
  assert.equal(reducer.ingest(row(1, {
    type: 'add_note',
    note_id: 'n1',
    text: 'first',
  })).status, 'applied');
  const filled = reducer.ingest(row(2, {
    type: 'add_note',
    note_id: 'n2',
    text: 'second',
  }));

  assert.equal(filled.status, 'applied');
  assert.deepEqual(filled.applied.map((op) => op.seq), [2, 3]);
  assert.equal(filled.gap, false);
  assert.equal(reducer.cursor, 3);
  assert.deepEqual(reducer.blocks.map((block) => block.content), ['first', 'second', 'third']);
});

test('unsupported operations still advance the durable cursor and remain visible', () => {
  const reducer = new CanvasReducer({ id: 'canvas-1', blocks: [] });
  assert.equal(reducer.ingest(row(1, { type: 'future_operation', value: 1 })).status, 'applied');
  assert.equal(reducer.cursor, 1);
  assert.equal(reducer.snapshot().unknown_ops, 1);
  assert.equal(reducer.ingest(row(2, { type: 'set_text', text: 'continued' })).status, 'applied');
  assert.equal(reducer.cursor, 2);
  assert.equal(reducer.blocks[0].content, 'continued');
});

test('supported block operations replay deterministically without duplicate notes', () => {
  let blocks = [{ type: 'text', content: 'old' }];
  blocks = reduceCanvasBlocks(blocks, { type: 'set_text', text: 'new' });
  blocks = reduceCanvasBlocks(blocks, { type: 'add_note', note_id: 'n1', text: 'draft' });
  blocks = reduceCanvasBlocks(blocks, { type: 'add_note', note_id: 'n1', text: 'final' });
  blocks = reduceCanvasBlocks(blocks, {
    type: 'upsert_block',
    block: { id: 'status', type: 'status', value: 'doing' },
  });
  blocks = reduceCanvasBlocks(blocks, {
    type: 'upsert_block',
    block: { id: 'status', type: 'status', value: 'done' },
  });

  assert.equal(blocks.filter((block) => block.type === 'note').length, 1);
  assert.equal(blocks.find((block) => block.type === 'note').content, 'final');
  assert.deepEqual(blocks.find((block) => block.id === 'status'), {
    id: 'status',
    type: 'status',
    value: 'done',
  });

  blocks = reduceCanvasBlocks(blocks, { type: 'delete_note', note_id: 'n1' });
  blocks = reduceCanvasBlocks(blocks, { type: 'delete_block', block_id: 'status' });
  assert.deepEqual(blocks, [{ type: 'text', content: 'new' }]);
  assert.deepEqual(
    reduceCanvasBlocks(blocks, {
      type: 'set_blocks',
      blocks: [{ id: 'fresh', type: 'status', value: 'ready' }],
    }),
    [{ id: 'fresh', type: 'status', value: 'ready' }],
  );
});

test('snapshot baseline skips materialized ops and replays only the durable tail', () => {
  const old = new CanvasReducer({
    id: 'canvas-1',
    title: 'Old',
    version: 1,
    blocks: [{ type: 'text', content: 'old base' }],
  });
  old.ingestMany([
    row(1, { type: 'set_text', text: 'old op' }),
    row(3, { type: 'add_note', note_id: 'stale', text: 'queued' }),
  ]);
  assert.equal(old.pending.size, 1);

  old.reset({
    id: 'canvas-1',
    title: 'New',
    version: 2,
    blocks: [
      { type: 'text', content: 'canonical' },
      { type: 'note', note_id: 'n1', content: 'kept' },
    ],
    snapshot_op_seq: 2,
  });
  const log = [
    row(1, { type: 'set_text', text: 'canonical' }),
    row(2, { type: 'add_note', note_id: 'n1', text: 'kept' }),
    row(3, { type: 'add_note', note_id: 'n2', text: 'tail' }),
  ];
  const replay = old.ingestMany(log);
  assert.deepEqual(
    replay.results.map((result) => result.status),
    ['duplicate', 'duplicate', 'applied'],
  );

  const fresh = new CanvasReducer({
    id: 'canvas-1',
    title: 'New',
    version: 2,
    blocks: [
      { type: 'text', content: 'canonical' },
      { type: 'note', note_id: 'n1', content: 'kept' },
    ],
    snapshot_op_seq: 2,
  });
  fresh.ingestMany(log);
  assert.deepEqual(old.snapshot(), fresh.snapshot());
  assert.equal(old.snapshot().snapshot_op_seq, 2);
  assert.equal(old.cursor, 3);
  assert.equal(old.blocks.filter((block) => block.note_id === 'n1').length, 1);
  assert.equal(old.blocks.find((block) => block.note_id === 'n2').content, 'tail');
  assert.equal(old.pending.size, 0);
});

test('Canvas WS binding dispatches live ops and reconnect catch-up hooks', () => {
  class FakeWs {
    constructor() { this.handlers = new Map(); }
    on(event, fn) {
      if (!this.handlers.has(event)) this.handlers.set(event, new Set());
      this.handlers.get(event).add(fn);
      return () => this.handlers.get(event).delete(fn);
    }
    emit(event, value) {
      for (const fn of this.handlers.get(event) || []) fn(value);
    }
  }
  const ws = new FakeWs();
  const observed = [];
  const unbind = bindCanvasRealtime(ws, {
    onCanvasOp: (frame) => observed.push(['op', frame.op_seq]),
    onReconnect: () => observed.push(['open']),
  });

  ws.emit('msg:canvas_op', { op_seq: 4 });
  ws.emit('open');
  assert.deepEqual(observed, [['op', 4], ['open']]);
  unbind();
  ws.emit('open');
  assert.deepEqual(observed, [['op', 4], ['open']]);
});

test('Canvas API wrappers preserve scope, encoding, cursors, and JSON bodies', async () => {
  const previousFetch = globalThis.fetch;
  const previousStorage = Object.getOwnPropertyDescriptor(globalThis, 'localStorage');
  const requests = [];
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    value: { getItem: () => 'access.jwt' },
  });
  globalThis.fetch = async (url, init) => {
    requests.push({ url, init });
    return {
      status: 200,
      ok: true,
      headers: { get: () => 'application/json' },
      json: async () => ({}),
    };
  };

  try {
    await api.listCanvases('room /1');
    await api.createCanvas('room /1', {
      title: 'Launch',
      blocks: [{ type: 'text', content: '' }],
    });
    await api.getCanvas('room /1', 'canvas /1');
    await api.updateCanvas('room /1', 'canvas /1', {
      blocks: [{ type: 'text', content: 'snapshot' }],
      expectedVersion: 3,
      snapshotOpSeq: 7,
    });
    await api.deleteCanvas('room /1', 'canvas /2');
    await api.listCanvasOps('room /1', 'canvas /1', { since: 7, limit: 500 });
    await api.appendCanvasOp(
      'room /1',
      'canvas /1',
      { type: 'set_text', text: 'ready' },
      '8c9ecf0b-ab71-4b10-96cc-c5dcf88ce705',
    );
  } finally {
    globalThis.fetch = previousFetch;
    if (previousStorage === undefined) delete globalThis.localStorage;
    else Object.defineProperty(globalThis, 'localStorage', previousStorage);
  }

  assert.deepEqual(requests.map((request) => [request.init.method, request.url]), [
    ['GET', '/api/rooms/room%20%2F1/canvases'],
    ['POST', '/api/rooms/room%20%2F1/canvases'],
    ['GET', '/api/rooms/room%20%2F1/canvases/canvas%20%2F1'],
    ['PUT', '/api/rooms/room%20%2F1/canvases/canvas%20%2F1'],
    ['DELETE', '/api/rooms/room%20%2F1/canvases/canvas%20%2F2'],
    ['GET', '/api/rooms/room%20%2F1/canvases/canvas%20%2F1/ops?since=7&limit=500'],
    ['POST', '/api/rooms/room%20%2F1/canvases/canvas%20%2F1/ops'],
  ]);
  assert.deepEqual(JSON.parse(requests[1].init.body), {
    title: 'Launch',
    blocks: [{ type: 'text', content: '' }],
  });
  assert.deepEqual(JSON.parse(requests[3].init.body), {
    blocks: [{ type: 'text', content: 'snapshot' }],
    expected_version: 3,
    snapshot_op_seq: 7,
  });
  assert.deepEqual(JSON.parse(requests[6].init.body), {
    client_op_id: '8c9ecf0b-ab71-4b10-96cc-c5dcf88ce705',
    op: { type: 'set_text', text: 'ready' },
  });
});
