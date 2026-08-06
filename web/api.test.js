import test from 'node:test';
import assert from 'node:assert/strict';

import { api } from './api.js';

test('login sends only supplied, trimmed second factors', async () => {
  const previousFetch = globalThis.fetch;
  const requests = [];
  globalThis.fetch = async (url, init) => {
    requests.push({ url, init });
    return {
      status: 200,
      ok: true,
      headers: { get: () => 'application/json' },
      json: async () => ({ ok: true }),
    };
  };

  try {
    await api.login({ email: 'a@example.com', password: 'secret', totp: '' });
    await api.login({ email: 'a@example.com', password: 'secret', totp: ' 123456 ' });
    await api.login({
      email: 'a@example.com',
      password: 'secret',
      recovery_code: ' a1b2c3d4e5f60708 ',
    });
  } finally {
    globalThis.fetch = previousFetch;
  }

  assert.deepEqual(JSON.parse(requests[0].init.body), {
    email: 'a@example.com',
    password: 'secret',
  });
  assert.deepEqual(JSON.parse(requests[1].init.body), {
    email: 'a@example.com',
    password: 'secret',
    totp: '123456',
  });
  assert.deepEqual(JSON.parse(requests[2].init.body), {
    email: 'a@example.com',
    password: 'secret',
    recovery_code: 'a1b2c3d4e5f60708',
  });
});

test('logout authenticates with the refresh token body', async () => {
  const previousFetch = globalThis.fetch;
  let captured;
  globalThis.fetch = async (url, init) => {
    captured = { url, init };
    return {
      status: 204,
      ok: true,
      headers: { get: () => '' },
    };
  };

  try {
    await api.logout('refresh.jwt');
  } finally {
    globalThis.fetch = previousFetch;
  }

  assert.equal(captured.url, '/api/auth/logout');
  assert.equal(captured.init.headers.Authorization, undefined);
  assert.deepEqual(JSON.parse(captured.init.body), { refresh_token: 'refresh.jwt' });
});

test('2FA management uses caller-scoped routes and sends the disable code', async () => {
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
      json: async () => ({ activated: true }),
    };
  };
  try {
    await api.twoFactorStatus();
    await api.twoFactorVerify('123456');
    await api.twoFactorRecoveryCodes('112233');
    await api.twoFactorDisable('654321');
  } finally {
    globalThis.fetch = previousFetch;
    if (previousStorage === undefined) delete globalThis.localStorage;
    else Object.defineProperty(globalThis, 'localStorage', previousStorage);
  }

  assert.deepEqual(requests.map((r) => [r.init.method, r.url]), [
    ['GET', '/api/me/2fa'],
    ['POST', '/api/me/2fa/verify'],
    ['POST', '/api/me/2fa/recovery-codes'],
    ['DELETE', '/api/me/2fa'],
  ]);
  assert.deepEqual(JSON.parse(requests[1].init.body), { code: '123456' });
  assert.deepEqual(JSON.parse(requests[2].init.body), { code: '112233' });
  assert.deepEqual(JSON.parse(requests[3].init.body), { code: '654321' });
});

test('recallMessage posts to the encoded message recall endpoint', async () => {
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
      json: async () => ({
        id: 'msg/1',
        recalled_at: '2026-08-06T00:00:00Z',
        recalled_by: 'participant-a',
      }),
    };
  };
  try {
    const recalled = await api.recallMessage('msg/1');
    assert.equal(recalled.recalled_at, '2026-08-06T00:00:00Z');
  } finally {
    globalThis.fetch = previousFetch;
    if (previousStorage === undefined) delete globalThis.localStorage;
    else Object.defineProperty(globalThis, 'localStorage', previousStorage);
  }

  assert.equal(requests.length, 1);
  assert.equal(requests[0].init.method, 'POST');
  assert.equal(requests[0].url, '/api/messages/msg%2F1/recall', 'id is path-encoded');
  assert.equal(requests[0].init.body, undefined, 'recall takes no body');
});

test('recallMessage surfaces a 409 already-recalled as ApiError for callers to map to success', async () => {
  const previousFetch = globalThis.fetch;
  const previousStorage = Object.getOwnPropertyDescriptor(globalThis, 'localStorage');
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    value: { getItem: () => 'access.jwt' },
  });
  globalThis.fetch = async () => ({
    status: 409,
    ok: false,
    headers: { get: () => 'application/json' },
    json: async () => ({ code: 'conflict', msg: 'message is already recalled' }),
  });
  try {
    await assert.rejects(
      api.recallMessage('msg-1'),
      (error) => error.status === 409
        && error.body.code === 'conflict'
        && /already recalled/.test(error.message),
    );
  } finally {
    globalThis.fetch = previousFetch;
    if (previousStorage === undefined) delete globalThis.localStorage;
    else Object.defineProperty(globalThis, 'localStorage', previousStorage);
  }
});

test('recallMessage surfaces a window-expired 409 with the real wire envelope', async () => {
  const previousFetch = globalThis.fetch;
  const previousStorage = Object.getOwnPropertyDescriptor(globalThis, 'localStorage');
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    value: { getItem: () => 'access.jwt' },
  });
  // Real envelope: Error::Conflict Display renders "conflict: <detail>" (see
  // crates/aero-server/src/error.rs contract test) — an unprefixed mock would
  // mask the production mismatch and let the 409-swallow regression return.
  globalThis.fetch = async () => ({
    status: 409,
    ok: false,
    headers: { get: () => 'application/json' },
    json: async () => ({ code: 'conflict', msg: 'conflict: recall window expired' }),
  });
  try {
    await assert.rejects(
      api.recallMessage('msg-1'),
      (error) => error.status === 409
        && error.body.code === 'conflict'
        && error.message === 'conflict: recall window expired',
    );
  } finally {
    globalThis.fetch = previousFetch;
    if (previousStorage === undefined) delete globalThis.localStorage;
    else Object.defineProperty(globalThis, 'localStorage', previousStorage);
  }
});

test('governance wrappers preserve tenant scope, filters, and bot ownership paths', async () => {
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
    await api.workspaceAudit('ws /1', { action: 'member.add', limit: 25 });
    await api.createBot({
      name: 'Release Bot',
      workspace_id: 'ws1',
      icon_url: 'https://example.com/bot.png',
    });
    await api.createBotSubscription('bot /1', {
      event_type: 'message',
      filters: { room_id: 'r1' },
      webhook_url: 'https://hooks.example.com/aero',
    });
    await api.deleteBotSubscription('bot /1', 'sub /1');
    await api.rotateBotSubscriptionSecret('bot /1', 'sub /1');
    await api.listBotDeliveries('bot /1', 30);
    await api.requeueBotDelivery('bot /1', 'delivery /1');
  } finally {
    globalThis.fetch = previousFetch;
    if (previousStorage === undefined) delete globalThis.localStorage;
    else Object.defineProperty(globalThis, 'localStorage', previousStorage);
  }

  assert.deepEqual(requests.map((request) => [request.init.method, request.url]), [
    ['GET', '/api/workspaces/ws%20%2F1/audit?action=member.add&limit=25'],
    ['POST', '/api/bots'],
    ['POST', '/api/bots/bot%20%2F1/subscriptions'],
    ['DELETE', '/api/bots/bot%20%2F1/subscriptions/sub%20%2F1'],
    ['POST', '/api/bots/bot%20%2F1/subscriptions/sub%20%2F1/secret'],
    ['GET', '/api/bots/bot%20%2F1/deliveries?limit=30'],
    ['POST', '/api/bots/bot%20%2F1/deliveries/delivery%20%2F1/requeue'],
  ]);
  assert.deepEqual(JSON.parse(requests[1].init.body), {
    name: 'Release Bot',
    icon_url: 'https://example.com/bot.png',
    workspace_id: 'ws1',
  });
  assert.deepEqual(JSON.parse(requests[2].init.body), {
    event_type: 'message',
    filters: { room_id: 'r1' },
    webhook_url: 'https://hooks.example.com/aero',
  });
});

test('agent creation carries the canonical room scope', async () => {
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
    await api.createAgent('room /1', {
      display_name: 'Release Agent',
      kind: 'agent',
      avatar_url: 'https://example.test/agent.png',
    });
  } finally {
    globalThis.fetch = previousFetch;
    if (previousStorage === undefined) delete globalThis.localStorage;
    else Object.defineProperty(globalThis, 'localStorage', previousStorage);
  }

  assert.equal(requests[0].url, '/api/agents');
  assert.deepEqual(JSON.parse(requests[0].init.body), {
    room_id: 'room /1',
    display_name: 'Release Agent',
    kind: 'agent',
    avatar_url: 'https://example.test/agent.png',
  });
});

test('productivity wrappers preserve room and workspace scope', async () => {
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
    await api.createTask('room /1', { title: 'Ship it' });
    await api.listApprovals('ws /1', 'incoming', 'pending');
    await api.setManager('ws /1', 'person /1', 'manager /1');
    await api.createScheduled('room /1', {
      blocks: [{ type: 'text', content: 'later' }],
      scheduled_at: '2027-01-01T00:00:00Z',
    });
    await api.retryScheduled('room /1', 'scheduled /1');
    await api.listAllScheduled();
    await api.createRecurring('room /1', {
      blocks: [{ type: 'text', content: 'again' }],
      cadence: 'daily',
    });
    await api.createDigest({ room_id: 'room /1', frequency: 'weekly' });
    await api.remindMessage('message /1', '2h');
    await api.saveMessage('message /1', 'important');
    await api.createUserGroup('ws /1', { handle: 'ops', name: 'Ops' });
    await api.requestRoomJoin('room /2');
    await api.createAnnouncement('ws /1', { body: 'Maintenance' });
  } finally {
    globalThis.fetch = previousFetch;
    if (previousStorage === undefined) delete globalThis.localStorage;
    else Object.defineProperty(globalThis, 'localStorage', previousStorage);
  }

  assert.deepEqual(requests.map((request) => [request.init.method, request.url]), [
    ['POST', '/api/rooms/room%20%2F1/tasks'],
    ['GET', '/api/workspaces/ws%20%2F1/approvals/incoming?status=pending'],
    ['PUT', '/api/workspaces/ws%20%2F1/participants/person%20%2F1/manager'],
    ['POST', '/api/rooms/room%20%2F1/scheduled'],
    ['POST', '/api/rooms/room%20%2F1/scheduled/scheduled%20%2F1'],
    ['GET', '/api/scheduled'],
    ['POST', '/api/rooms/room%20%2F1/recurring'],
    ['POST', '/api/digests'],
    ['POST', '/api/messages/message%20%2F1/remind'],
    ['POST', '/api/messages/message%20%2F1/save'],
    ['POST', '/api/workspaces/ws%20%2F1/user-groups'],
    ['POST', '/api/rooms/room%20%2F2/join-request'],
    ['POST', '/api/workspaces/ws%20%2F1/announcements'],
  ]);
  assert.deepEqual(JSON.parse(requests[2].init.body), { manager_id: 'manager /1' });
  assert.deepEqual(JSON.parse(requests[8].init.body), { when: '2h' });
});

// ---------- composer drafts (per-room, private to the author) ----------

async function withDraftFetch(responder, fn) {
  const previousFetch = globalThis.fetch;
  const previousStorage = Object.getOwnPropertyDescriptor(globalThis, 'localStorage');
  const requests = [];
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    value: { getItem: () => 'access.jwt' },
  });
  globalThis.fetch = async (url, init) => {
    requests.push({ url, init });
    return typeof responder === 'function' ? responder(url, init) : responder;
  };
  try {
    await fn(requests);
  } finally {
    globalThis.fetch = previousFetch;
    if (previousStorage === undefined) delete globalThis.localStorage;
    else Object.defineProperty(globalThis, 'localStorage', previousStorage);
  }
}

const okJson = (data) => ({ status: 200, ok: true, headers: { get: () => 'application/json' }, json: async () => data });
const errJson = (status, msg) => ({ status, ok: false, headers: { get: () => 'application/json' }, json: async () => ({ code: `err-${status}`, msg }) });

test('draft endpoints use the per-room routes with encoded ids', async () => {
  await withDraftFetch(okJson({ saved: true }), async (requests) => {
    await api.getDraft('room /1');
    await api.saveDraft('room /1', { blocks: [{ type: 'text', content: 'hi' }], reply_to: 'm/1' });
    await api.deleteDraft('room /1');
    assert.deepEqual(requests.map((r) => [r.init.method, r.url]), [
      ['GET', '/api/rooms/room%20%2F1/draft'],
      ['PUT', '/api/rooms/room%20%2F1/draft'],
      ['DELETE', '/api/rooms/room%20%2F1/draft'],
    ]);
    assert.equal(requests[0].init.body, undefined, 'GET takes no body');
    assert.deepEqual(JSON.parse(requests[1].init.body), {
      blocks: [{ type: 'text', content: 'hi' }], reply_to: 'm/1',
    });
  });
});

test('saveDraft omits reply_to when absent and passes blocks through', async () => {
  await withDraftFetch(okJson({ saved: true }), async (requests) => {
    await api.saveDraft('room-1', { blocks: [{ type: 'text', content: 'plain' }] });
    assert.deepEqual(JSON.parse(requests[0].init.body), { blocks: [{ type: 'text', content: 'plain' }] });
  });
});

test('draft errors surface as ApiError with status (401/403/409)', async () => {
  const checks = await Promise.all([401, 403, 409].map((status) => (
    withDraftFetch(errJson(status, `draft ${status}`), async () => {
      await assert.rejects(
        api.getDraft('room-1'),
        (error) => error.status === status && /draft/.test(error.message),
      );
    })
  )));
  assert.equal(checks.length, 3);
});
