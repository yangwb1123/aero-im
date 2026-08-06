import test from 'node:test';
import assert from 'node:assert/strict';

import {
  mirrorClear,
  mirrorRead,
  mirrorWrite,
  pickRestoreAction,
  resolveReplyTarget,
  shouldDiscardOnSend,
} from './drafts_store.js';
import { withStorage } from './drafts_test_helpers.js';

const serverDraft = (text, updatedAt, replyTo = null) => ({
  blocks: [{ type: 'text', content: text }], reply_to: replyTo, updated_at: updatedAt,
});

// ---------- mirror (localStorage fallback) ----------

test('mirror write/read/clear round-trips through localStorage', () => {
  withStorage({ aero_pid: 'pid-1' }, () => {
    const snap = { text: 'unsaved text', replyTo: 'm9' };
    mirrorWrite('room-1', snap);
    const got = mirrorRead('room-1');
    assert.equal(got.text, 'unsaved text');
    assert.equal(got.reply_to, 'm9');
    mirrorClear('room-1');
    assert.equal(mirrorRead('room-1'), null);
  });
});

test('mirror is per-room and per-participant', () => {
  withStorage({ aero_pid: 'pid-1' }, () => {
    mirrorWrite('room-a', { text: 'a text', replyTo: null });
    mirrorWrite('room-b', { text: 'b text', replyTo: null });
    assert.equal(mirrorRead('room-a').text, 'a text');
    assert.equal(mirrorRead('room-b').text, 'b text');
    mirrorClear('room-a');
    assert.equal(mirrorRead('room-a'), null);
    assert.equal(mirrorRead('room-b').text, 'b text');
  });
});

test('mirror ignores malformed payloads', () => {
  withStorage({ aero_pid: 'pid-1', 'aero_draft_v1:pid-1:room-1': 'not json' }, () => {
    assert.equal(mirrorRead('room-1'), null);
  });
});

// ---------- restore source precedence (pickRestoreAction) ----------

test('restore precedence: local unsaved > server > mirror > empty', () => {
  const base = { roomChanged: false, inputRevChanged: false };
  const server = serverDraft('server text', '2026-08-06T12:00:00.000Z', 'm1');
  const mirror = { text: 'mirror text', reply_to: null, saved_at: '2026-08-06T11:00:00.000Z' };
  assert.deepEqual(
    pickRestoreAction({ ...base, local: { text: 'local text', replyTo: null }, serverDraft: server, mirror }),
    { apply: true, source: 'local', text: 'local text', replyTo: null, clean: false, clearMirror: false },
  );
  assert.equal(
    pickRestoreAction({ ...base, local: null, serverDraft: server, mirror: null }).source,
    'server',
  );
  assert.equal(
    pickRestoreAction({ ...base, local: null, serverDraft: null, mirror }).source,
    'mirror',
  );
  assert.deepEqual(
    pickRestoreAction({ ...base, local: null, serverDraft: null, mirror: null }),
    { apply: true, source: 'empty', text: '', replyTo: null, clean: false, clearMirror: false },
  );
});

test('mirror newer than the server draft wins; server wins when newer', () => {
  const base = { local: null, roomChanged: false, inputRevChanged: false };
  // The last successful autosave predates the failed one whose text is in the
  // mirror: the server copy is OLDER, so the mirror's local intent wins.
  const mirror = { text: 'newer local text', reply_to: null, saved_at: '2026-08-06T12:30:00.000Z' };
  const staleServer = serverDraft('older server text', '2026-08-06T12:00:00.000Z');
  const action = pickRestoreAction({ ...base, serverDraft: staleServer, mirror });
  assert.equal(action.source, 'mirror', 'newer local intent beats a stale server copy');
  assert.equal(action.clearMirror, false, 'the mirror stays until the server confirms');
  // Genuinely newer server draft: server wins and the mirror may be cleared.
  const freshServer = serverDraft('fresher server text', '2026-08-06T13:00:00.000Z');
  const fresh = pickRestoreAction({ ...base, serverDraft: freshServer, mirror });
  assert.equal(fresh.source, 'server');
  assert.equal(fresh.clean, true);
  assert.equal(fresh.clearMirror, true);
  assert.equal(fresh.text, 'fresher server text');
});

test('restore guards: room switch or newer input skips the restore', () => {
  const server = serverDraft('server text', '2026-08-06T12:00:00.000Z');
  const skipped = { apply: false, source: null, text: '', replyTo: null, clean: false, clearMirror: false };
  assert.deepEqual(
    pickRestoreAction({ local: null, serverDraft: server, mirror: null, roomChanged: true, inputRevChanged: false }),
    skipped,
  );
  assert.deepEqual(
    pickRestoreAction({ local: null, serverDraft: server, mirror: null, roomChanged: false, inputRevChanged: true }),
    skipped,
  );
});

// ---------- reply chip hydration ----------

test('resolveReplyTarget keeps live parents and drops stale ones', () => {
  const messages = [
    { id: 'm1', sender_id: 'u1', blocks: [{ type: 'text', content: 'parent' }] },
    { id: 'm2', sender_id: 'u2', blocks: [] },
    { id: 'm3', sender_id: 'u3', blocks: [], deleted_at: '2026-08-06T12:00:00.000Z' },
  ];
  assert.deepEqual(resolveReplyTarget(messages, 'm1'), { id: 'm1', sender_id: 'u1', blocks: messages[0].blocks });
  assert.equal(resolveReplyTarget(messages, 'gone'), null);
  assert.equal(resolveReplyTarget(messages, 'm3'), null, 'soft-deleted parents are not live reply targets');
  assert.equal(resolveReplyTarget(messages, null), null);
  assert.equal(resolveReplyTarget(null, 'm1'), null);
});

// ---------- clear-on-send guard (Enter + button funnel) ----------

test('shouldDiscardOnSend: empty composer in the send room only', () => {
  assert.equal(shouldDiscardOnSend({ value: '', roomId: 'r1', activeRoom: 'r1' }), true);
  assert.equal(shouldDiscardOnSend({ value: '   ', roomId: 'r1', activeRoom: 'r1' }), true);
  assert.equal(shouldDiscardOnSend({ value: 'not sent', roomId: 'r1', activeRoom: 'r1' }), false, 'rejected send keeps the draft');
  assert.equal(shouldDiscardOnSend({ value: '', roomId: 'r1', activeRoom: 'r2' }), false, 'room mismatch keeps the draft');
});
