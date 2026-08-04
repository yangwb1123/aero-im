import test from 'node:test';
import assert from 'node:assert/strict';

import { api } from './api.js';
import {
  buildInvitationPayload,
  complianceAccess,
  deleteConfirmationMatches,
  objectList,
  oneTimeCredential,
  parseRetentionDays,
  parseWebhookEvents,
  retentionSummary,
  roleForParticipant,
  workspaceExportFilename,
} from './compliance_admin_utils.js';

test('compliance permissions and tenant data normalization fail closed', () => {
  const members = [
    { participant_id: 'p1', role: 'owner' },
    { participant_id: 'p2', role: 'member' },
  ];
  assert.equal(roleForParticipant(members, 'p1'), 'owner');
  assert.equal(roleForParticipant(members, 'missing'), null);
  assert.equal(roleForParticipant({ members }, 'p1'), null);
  assert.deepEqual(complianceAccess('Owner'), { admin: true, owner: true });
  assert.deepEqual(complianceAccess('admin'), { admin: true, owner: false });
  assert.deepEqual(complianceAccess('member'), { admin: false, owner: false });
  assert.deepEqual(complianceAccess('unexpected'), { admin: false, owner: false });
  assert.deepEqual(objectList({ holds: [null, { id: 'h1' }] }, 'holds'), [{ id: 'h1' }]);
});

test('retention and invitation validators reject unsafe values', () => {
  assert.equal(parseRetentionDays(''), null);
  assert.equal(parseRetentionDays('30'), 30);
  assert.throws(() => parseRetentionDays('0'), /1–3650/);
  assert.throws(() => parseRetentionDays('1.5'), /1–3650/);
  assert.throws(() => parseRetentionDays('3651'), /1–3650/);

  assert.deepEqual(buildInvitationPayload({
    email: ' admin@example.com ',
    role: 'admin',
    max_uses: '2',
    expires_days: '7',
  }), {
    email: 'admin@example.com',
    role: 'admin',
    max_uses: 2,
    expires_in_secs: 604800,
  });
  assert.deepEqual(buildInvitationPayload({ role: 'member' }), { role: 'member' });
  assert.throws(() => buildInvitationPayload({ role: 'root' }), /角色无效/);
  assert.throws(() => buildInvitationPayload({ max_uses: '0' }), /正整数/);
  assert.throws(() => buildInvitationPayload({ expires_days: '366' }), /1–365/);
});

test('destructive confirmation, webhook filters, and one-time credentials are exact', () => {
  const workspace = { id: 'w1', name: 'Aero Team', slug: 'aero-team' };
  assert.equal(deleteConfirmationMatches(workspace, 'Aero Team'), true);
  assert.equal(deleteConfirmationMatches(workspace, ' aero-team '), true);
  assert.equal(deleteConfirmationMatches(workspace, 'AERO-TEAM'), false);
  assert.equal(deleteConfirmationMatches(workspace, ''), false);
  assert.deepEqual(parseWebhookEvents('message, edited message  deleted'), [
    'message', 'edited', 'deleted',
  ]);
  assert.deepEqual(oneTimeCredential({
    token: 'plain-token',
    invite_url: 'https://aero.test/invite/plain-token',
  }, 'invitation'), {
    label: '邀请链接（只显示一次）',
    value: 'https://aero.test/invite/plain-token',
  });
  assert.deepEqual(oneTimeCredential({ secret: 'hmac-secret' }, 'outgoing'), {
    label: 'Webhook 签名 Secret（只显示一次）',
    value: 'hmac-secret',
  });
  assert.equal(oneTimeCredential({ id: 'listed-row' }, 'invitation'), null);
  assert.equal(retentionSummary({ room: null, workspace: 30, effective: 30 }), '继承工作区 · 30 天');
  assert.equal(retentionSummary({ room: 7, workspace: 30, effective: 7 }), '房间覆盖 · 7 天');
  assert.equal(
    workspaceExportFilename(workspace, new Date('2026-07-28T00:00:00Z')),
    'aero-workspace-aero-team-2026-07-28.json',
  );
});

test('compliance API wrappers encode every workspace, room, and resource id', async () => {
  const previousFetch = globalThis.fetch;
  const previousStorage = Object.getOwnPropertyDescriptor(globalThis, 'localStorage');
  const requests = [];
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    value: { getItem: () => 'access.jwt' },
  });
  globalThis.fetch = async (url, init) => {
    requests.push({ url, init });
    const noContent = init.method === 'DELETE' || init.method === 'PUT';
    return {
      status: noContent ? 204 : 200,
      ok: true,
      headers: { get: () => noContent ? '' : 'application/json' },
      json: async () => ({}),
      text: async () => '',
    };
  };

  try {
    await api.listRoomsForWorkspace('ws /1');
    await api.listLegalHolds('ws /1');
    await api.createLegalHold('ws /1', { room_id: 'room /1', reason: 'Case 7' });
    await api.releaseLegalHold('hold /1');
    await api.roomRetention('room /1');
    await api.setRoomRetention('room /1', null);
    await api.setWorkspaceRetention('ws /1', 365);
    await api.listInformationBarriers('ws /1');
    await api.createInformationBarrier('ws /1', 'group /a', 'group /b');
    await api.deleteInformationBarrier('barrier /1');
    await api.listDeactivatedMembers('ws /1');
    await api.deactivateMember('ws /1', 'person /1');
    await api.reactivateMember('ws /1', 'person /1');
    await api.listInvitations('ws /1');
    await api.createInvitation('ws /1', { role: 'member', max_uses: 1 });
    await api.revokeInvitation('invite /1');
    await api.listRoomWebhooks('room /1');
    await api.createIncomingWebhook('room /1', { label: 'CI' });
    await api.createOutgoingWebhook('room /1', {
      url: 'https://hooks.example.com/aero',
      events: ['message'],
    });
    await api.revokeIncomingWebhook('hook /in');
    await api.revokeOutgoingWebhook('hook /out');
    await api.listWebhookDeliveries('hook /out', 25);
    await api.listWebhookDeadLetters('hook /out', 20);
    await api.requeueWebhookDelivery('delivery /1');
    await api.exportWorkspace('ws /1');
    await api.deleteWorkspace('ws /1');
  } finally {
    globalThis.fetch = previousFetch;
    if (previousStorage === undefined) delete globalThis.localStorage;
    else Object.defineProperty(globalThis, 'localStorage', previousStorage);
  }

  assert.deepEqual(requests.map(({ init, url }) => [init.method, url]), [
    ['GET', '/api/rooms?workspace_id=ws+%2F1'],
    ['GET', '/api/workspaces/ws%20%2F1/legal-holds'],
    ['POST', '/api/workspaces/ws%20%2F1/legal-holds'],
    ['DELETE', '/api/legal-holds/hold%20%2F1'],
    ['GET', '/api/rooms/room%20%2F1/retention'],
    ['PUT', '/api/rooms/room%20%2F1/retention'],
    ['PUT', '/api/workspaces/ws%20%2F1/retention'],
    ['GET', '/api/workspaces/ws%20%2F1/barriers'],
    ['POST', '/api/workspaces/ws%20%2F1/barriers'],
    ['DELETE', '/api/barriers/barrier%20%2F1'],
    ['GET', '/api/workspaces/ws%20%2F1/deactivated'],
    ['POST', '/api/workspaces/ws%20%2F1/members/person%20%2F1/deactivate'],
    ['POST', '/api/workspaces/ws%20%2F1/members/person%20%2F1/reactivate'],
    ['GET', '/api/workspaces/ws%20%2F1/invitations'],
    ['POST', '/api/workspaces/ws%20%2F1/invitations'],
    ['DELETE', '/api/invitations/invite%20%2F1'],
    ['GET', '/api/rooms/room%20%2F1/webhooks'],
    ['POST', '/api/rooms/room%20%2F1/webhooks/incoming'],
    ['POST', '/api/rooms/room%20%2F1/webhooks/outgoing'],
    ['DELETE', '/api/webhooks/incoming/hook%20%2Fin'],
    ['DELETE', '/api/webhooks/outgoing/hook%20%2Fout'],
    ['GET', '/api/webhooks/hook%20%2Fout/deliveries?limit=25'],
    ['GET', '/api/webhooks/hook%20%2Fout/deliveries/dead?limit=20'],
    ['POST', '/api/webhook-deliveries/delivery%20%2F1/requeue'],
    ['GET', '/api/workspaces/ws%20%2F1/export'],
    ['DELETE', '/api/workspaces/ws%20%2F1'],
  ]);
  assert.deepEqual(JSON.parse(requests[2].init.body), {
    room_id: 'room /1',
    reason: 'Case 7',
  });
  assert.deepEqual(JSON.parse(requests[5].init.body), { days: null });
  assert.deepEqual(JSON.parse(requests[6].init.body), { days: 365 });
  assert.deepEqual(JSON.parse(requests[8].init.body), {
    group_a: 'group /a',
    group_b: 'group /b',
  });
  assert.deepEqual(JSON.parse(requests[14].init.body), { role: 'member', max_uses: 1 });
  assert.deepEqual(JSON.parse(requests[18].init.body), {
    url: 'https://hooks.example.com/aero',
    events: ['message'],
  });
  assert.equal(requests.every(({ init }) => init.headers.Authorization === 'Bearer access.jwt'), true);
});
