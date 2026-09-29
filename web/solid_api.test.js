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

function response(body = {}, { ok = true, status = 200 } = {}) {
  return {
    ok,
    status,
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
    const clientCreateId = 'd5c9d733-76a3-79c0-bdb7-2497bc4fe779';
    const op = { type: 'set_text', text: 'saved' };
    await api.listCanvases(roomId);
    await api.createCanvas(roomId, { title: 'Launch', blocks, clientCreateId });
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
    assert.deepEqual(JSON.parse(requests[1].init.body), {
      title: 'Launch',
      blocks,
      client_create_id: clientCreateId,
    });
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

test('Solid message recall, deletion, and refresh APIs encode ids and preserve REST methods', async () => {
  const previousWindow = Object.getOwnPropertyDescriptor(globalThis, 'window');
  const previousFetch = globalThis.fetch;
  const requests = [];
  Object.defineProperty(globalThis, 'window', {
    configurable: true,
    value: { localStorage: { getItem: (key) => key === 'aero_token' ? 'solid-access-token' : null } },
  });
  globalThis.fetch = async (url, init) => {
    requests.push({ url: String(url), init });
    return response({ id: 'message /#1', room_id: 'room-1', recalled_at: null });
  };

  try {
    const { api } = await loadSolidApi();
    await api.getMessage('message /#1');
    await api.recallMessage('message /#1');
    await api.deleteMessage('message /#1');

    assert.deepEqual(requests.map(({ url, init }) => [init.method, url]), [
      ['GET', '/api/messages/message%20%2F%231'],
      ['POST', '/api/messages/message%20%2F%231/recall'],
      ['DELETE', '/api/messages/message%20%2F%231'],
    ]);
    assert.equal(requests[0].init.body, undefined);
    assert.ok(requests.every(({ init }) => init.body === undefined));
    assert.ok(requests.every(({ init }) => init.headers.Authorization === 'Bearer solid-access-token'));
  } finally {
    globalThis.fetch = previousFetch;
    if (previousWindow === undefined) delete globalThis.window;
    else Object.defineProperty(globalThis, 'window', previousWindow);
  }
});

test('Solid message edits send the optimistic version only when available', async () => {
  const previousWindow = Object.getOwnPropertyDescriptor(globalThis, 'window');
  const previousFetch = globalThis.fetch;
  const requests = [];
  Object.defineProperty(globalThis, 'window', {
    configurable: true,
    value: { localStorage: { getItem: () => 'solid-access-token' } },
  });
  globalThis.fetch = async (url, init) => {
    requests.push({ url: String(url), init });
    return response({ id: 'message-1', room_id: 'room-a', version: 2 });
  };

  try {
    const { api } = await loadSolidApi();
    const blocks = [{ type: 'text', content: 'edited text' }];
    await api.editMessage('message /1', blocks, 7);
    await api.editMessage('message-2', blocks);
    assert.deepEqual(requests.map(({ url, init }) => [init.method, url]), [
      ['PATCH', '/api/messages/message%20%2F1'],
      ['PATCH', '/api/messages/message-2'],
    ]);
    assert.deepEqual(JSON.parse(requests[0].init.body), { blocks, expected_version: 7 });
    assert.deepEqual(JSON.parse(requests[1].init.body), { blocks });
  } finally {
    globalThis.fetch = previousFetch;
    if (previousWindow === undefined) delete globalThis.window;
    else Object.defineProperty(globalThis, 'window', previousWindow);
  }
});

test('Solid reaction APIs preserve the message scope and batch body', async () => {
  const previousWindow = Object.getOwnPropertyDescriptor(globalThis, 'window');
  const previousFetch = globalThis.fetch;
  const requests = [];
  Object.defineProperty(globalThis, 'window', {
    configurable: true,
    value: { localStorage: { getItem: () => 'solid-access-token' } },
  });
  globalThis.fetch = async (url, init) => {
    requests.push({ url: String(url), init });
    return response({});
  };

  try {
    const { api } = await loadSolidApi();
    await api.toggleReaction('message /#1', '👍');
    await api.reactionsBatch(['message-1', 'message-2']);
    assert.deepEqual(requests.map(({ url, init }) => [init.method, url]), [
      ['POST', '/api/messages/message%20%2F%231/reactions'],
      ['POST', '/api/messages/reactions'],
    ]);
    assert.deepEqual(JSON.parse(requests[0].init.body), { emoji: '👍' });
    assert.deepEqual(JSON.parse(requests[1].init.body), { message_ids: ['message-1', 'message-2'] });
    assert.ok(requests.every(({ init }) => init.headers.Authorization === 'Bearer solid-access-token'));
  } finally {
    globalThis.fetch = previousFetch;
    if (previousWindow === undefined) delete globalThis.window;
    else Object.defineProperty(globalThis, 'window', previousWindow);
  }
});

test('Solid session APIs use access-token auth and send refresh only in the confirmed-request body', async () => {
  const previousWindow = Object.getOwnPropertyDescriptor(globalThis, 'window');
  const previousFetch = globalThis.fetch;
  const storage = new Map([
    ['aero_token', 'solid-access-token'],
    ['aero_refresh', 'current-refresh-secret'],
  ]);
  const requests = [];
  Object.defineProperty(globalThis, 'window', {
    configurable: true,
    value: { localStorage: { getItem: (key) => storage.get(key) ?? null } },
  });
  globalThis.fetch = async (url, init) => {
    requests.push({ url: String(url), init });
    return response(requests.length === 1 ? [] : { revoked_count: 2 });
  };

  try {
    const { api } = await loadSolidApi();
    await api.listSessions();
    const revokeResult = await api.revokeOtherSessions('current-refresh-secret');

    assert.deepEqual(revokeResult, { revoked_count: 2 });
    assert.deepEqual(requests.map(({ url, init }) => [init.method, url]), [
      ['GET', '/api/auth/sessions'],
      ['POST', '/api/auth/sessions/revoke-others'],
    ]);
    assert.equal(requests[0].init.body, undefined, 'GET must not send a body');
    assert.equal(requests[0].init.headers.Authorization, 'Bearer solid-access-token');
    assert.equal(requests[1].init.headers.Authorization, 'Bearer solid-access-token');
    assert.deepEqual(JSON.parse(requests[1].init.body), {
      current_refresh_token: 'current-refresh-secret',
    });
    assert.ok(requests.every(({ url, init }) => (
      !url.includes('current-refresh-secret')
      && !String(init.headers.Authorization ?? '').includes('current-refresh-secret')
    )));
  } finally {
    globalThis.fetch = previousFetch;
    if (previousWindow === undefined) delete globalThis.window;
    else Object.defineProperty(globalThis, 'window', previousWindow);
  }
});

test('Solid session API preserves ApiError details for an unauthorized response', async () => {
  const previousWindow = Object.getOwnPropertyDescriptor(globalThis, 'window');
  const previousFetch = globalThis.fetch;
  Object.defineProperty(globalThis, 'window', {
    configurable: true,
    value: { localStorage: { getItem: () => 'solid-access-token' } },
  });
  globalThis.fetch = async () => response({ msg: 'not authorized' }, { ok: false, status: 401 });

  try {
    const { ApiError, api } = await loadSolidApi();
    await assert.rejects(api.listSessions(), (error) => (
      error instanceof ApiError && error.status === 401 && error.message === 'not authorized'
    ));
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
