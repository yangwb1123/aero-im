import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { transform } from 'esbuild';
import { appendCanvasOperationWithRetry } from './src/canvas_submission.js';

async function loadSolidApi() {
  const source = await readFile(new URL('./src/api.ts', import.meta.url), 'utf8');
  const compiled = await transform(source, { loader: 'ts', format: 'esm' });
  const url = `data:text/javascript;base64,${globalThis.Buffer.from(compiled.code).toString('base64')}`;
  return import(url);
}

function response(body = {}) {
  return {
    ok: true,
    status: 200,
    headers: { get: () => 'application/json' },
    json: async () => body,
  };
}

test('Solid Canvas API adapter encodes room-scoped requests and bodies', async () => {
  const previousWindow = Object.getOwnPropertyDescriptor(globalThis, 'window');
  const previousFetch = globalThis.fetch;
  const storage = new Map([['aero_token', 'solid-access-token']]);
  const requests = [];
  Object.defineProperty(globalThis, 'window', {
    configurable: true,
    value: { localStorage: { getItem: (key) => storage.get(key) ?? null } },
  });
  globalThis.fetch = async (url, init) => {
    requests.push({ url: String(url), init });
    return response({ canvas_id: 'canvas /#1', since: 17, ops: [] });
  };

  try {
    const { api } = await loadSolidApi();
    const roomId = 'room / #1';
    const canvasId = 'canvas /#1';
    const blocks = [{ type: 'text', content: 'start' }];
    const op = { type: 'set_text', text: 'saved' };
    await api.listCanvases(roomId);
    await api.createCanvas(roomId, { title: 'Launch', blocks });
    await api.getCanvas(roomId, canvasId);
    await api.listCanvasOps(roomId, canvasId, { since: 17, limit: 23 });
    await api.appendCanvasOp(roomId, canvasId, op, 'client-op-id-1');

    assert.deepEqual(requests.map(({ url, init }) => [init.method, url]), [
      ['GET', '/api/rooms/room%20%2F%20%231/canvases'],
      ['POST', '/api/rooms/room%20%2F%20%231/canvases'],
      ['GET', '/api/rooms/room%20%2F%20%231/canvases/canvas%20%2F%231'],
      ['GET', '/api/rooms/room%20%2F%20%231/canvases/canvas%20%2F%231/ops?since=17&limit=23'],
      ['POST', '/api/rooms/room%20%2F%20%231/canvases/canvas%20%2F%231/ops'],
    ]);
    assert.deepEqual(JSON.parse(requests[1].init.body), { title: 'Launch', blocks });
    assert.deepEqual(JSON.parse(requests[4].init.body), {
      client_op_id: 'client-op-id-1',
      op,
    });
    assert.equal(requests[4].init.headers.Authorization, 'Bearer solid-access-token');
    assert.ok(requests.every(({ url }) => url.includes('/rooms/')));
  } finally {
    globalThis.fetch = previousFetch;
    if (previousWindow === undefined) delete globalThis.window;
    else Object.defineProperty(globalThis, 'window', previousWindow);
  }
});

test('uncertain Solid Canvas append retries the same logical operation id', async () => {
  const previousWindow = Object.getOwnPropertyDescriptor(globalThis, 'window');
  const previousFetch = globalThis.fetch;
  Object.defineProperty(globalThis, 'window', {
    configurable: true,
    value: { localStorage: { getItem: () => 'token' } },
  });
  const attempts = [];
  globalThis.fetch = async (_url, init) => {
    attempts.push(JSON.parse(init.body));
    if (attempts.length === 1) throw new Error('response lost after commit');
    return response({
      id: 'persisted-op',
      canvas_id: 'canvas-1',
      seq: 1,
      author_id: 'participant-1',
      op: { type: 'set_text', text: 'same operation' },
    });
  };

  try {
    const { api } = await loadSolidApi();
    const op = { type: 'set_text', text: 'same operation' };
    const submitted = await appendCanvasOperationWithRetry(api, 'room-1', 'canvas-1', op);
    assert.equal(attempts.length, 2);
    assert.equal(attempts[0].client_op_id, attempts[1].client_op_id);
    assert.equal(attempts[0].client_op_id, submitted.clientOpId);
    assert.deepEqual(attempts[0].op, op);
    assert.deepEqual(attempts[1].op, op);
  } finally {
    globalThis.fetch = previousFetch;
    if (previousWindow === undefined) delete globalThis.window;
    else Object.defineProperty(globalThis, 'window', previousWindow);
  }
});
