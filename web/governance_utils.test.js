import test from 'node:test';
import assert from 'node:assert/strict';

import {
  auditHasFilters,
  buildAuditQuery,
  csvFilename,
  deliverySummary,
  formatJson,
  normalizeObjectArray,
  oneTimeSecret,
  parseFilterObject,
  requireExternalSubscriptionScope,
  roleForParticipant,
  toRfc3339,
} from './governance_utils.js';

test('bot subscription filters accept only JSON objects', () => {
  assert.deepEqual(parseFilterObject(''), {});
  assert.deepEqual(parseFilterObject('{"room_id":"r1"}'), { room_id: 'r1' });
  assert.throws(() => parseFilterObject('[]'), /JSON 对象/);
  assert.throws(() => parseFilterObject('{broken'), SyntaxError);
});

test('external bot webhooks require an explicit tenant scope', () => {
  assert.doesNotThrow(() => requireExternalSubscriptionScope('', {}));
  assert.doesNotThrow(() => requireExternalSubscriptionScope('https://hooks.example/aero', {
    room_id: 'r1',
  }));
  assert.doesNotThrow(() => requireExternalSubscriptionScope('https://hooks.example/aero', {
    workspace_id: 'w1',
  }));
  assert.throws(
    () => requireExternalSubscriptionScope('https://hooks.example/aero', {}),
    /room_id 或 workspace_id/,
  );
  assert.throws(
    () => requireExternalSubscriptionScope('https://hooks.example/aero', { room_id: '   ' }),
    /room_id 或 workspace_id/,
  );
});

test('one-time webhook secrets preserve the exact API value', () => {
  assert.equal(oneTimeSecret({ secret: '  signed-secret  ' }), '  signed-secret  ');
  assert.equal(oneTimeSecret({ secret: '' }), null);
  assert.equal(oneTimeSecret({}), null);
  assert.equal(oneTimeSecret(null), null);
});

test('audit query trims values and converts local dates to RFC3339', () => {
  const query = buildAuditQuery({
    action: ' member.add ',
    actor: 'p1',
    target: '',
    after: '2026-07-28T10:30:00Z',
  });
  assert.equal(query.action, 'member.add');
  assert.equal(query.actor, 'p1');
  assert.equal(query.after, '2026-07-28T10:30:00.000Z');
  assert.equal(query.limit, 100);
  assert.equal(auditHasFilters(query), true);
  assert.equal(auditHasFilters({ limit: 100, before: 'cursor' }), false);
  assert.throws(() => toRfc3339('not-a-date'), /时间格式无效/);
});

test('workspace roles and response arrays are normalized defensively', () => {
  const rows = normalizeObjectArray([null, 1, { participant_id: 'p1', role: 'Admin' }]);
  assert.equal(rows.length, 1);
  assert.equal(roleForParticipant(rows, 'p1'), 'admin');
  assert.equal(roleForParticipant(rows, 'missing'), null);
});

test('governance labels are bounded and deterministic', () => {
  assert.equal(deliverySummary({ status: 'failed', http_status: 503, attempts: 2 }),
    'failed · HTTP 503 · 2 次尝试');
  assert.equal(deliverySummary({ status: 'dead', attempts: 6 }),
    'dead · 6 次尝试');
  assert.equal(formatJson({ token: 'abcdef' }, 10), '{"token":…');
  assert.equal(
    csvFilename('ws/unsafe', new Date('2026-07-28T12:00:00Z')),
    'aero-audit-ws_unsafe-2026-07-28.csv',
  );
});
