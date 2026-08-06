// recall_errors.test.js — recall failure → toast mapping (window-expired must
// reach the user; convergent 409s stay silent; everything else errors).
import test from 'node:test';
import assert from 'node:assert/strict';

import { recallErrorToast } from './recall_errors.js';

test('window-expired 409 maps to an info toast (prefix-tolerant)', () => {
  // Real wire envelope: thiserror Display prefix included (see error.rs).
  const t = recallErrorToast({ status: 409, message: 'conflict: recall window expired' });
  assert.equal(t.type, 'info');
  assert.match(t.text, /撤回时间窗已过/);
  assert.doesNotMatch(t.text, /\d/, 'toast must not hardcode the operator-tunable window');
});

test('reworded or unknown 409 variants surface, never silently swallow (whitelist)', () => {
  // A server reword of the window detail must not fail closed into silence.
  const reworded = recallErrorToast({ status: 409, message: 'conflict: recall window elapsed' });
  assert.notEqual(reworded, null);
  assert.equal(reworded.type, 'error');
  // Any unknown future 409 variant reaches the user.
  const unknown = recallErrorToast({ status: 409, message: 'conflict: something else' });
  assert.notEqual(unknown, null);
  assert.equal(unknown.type, 'error');
});

test('convergent 409s (already recalled / deleted) map to null', () => {
  assert.equal(
    recallErrorToast({ status: 409, message: 'conflict: message is already recalled' }),
    null,
  );
  assert.equal(
    recallErrorToast({ status: 409, message: 'conflict: message is deleted' }),
    null,
  );
});

test('429 maps to an error toast (rate gate, no auto-retry)', () => {
  const t = recallErrorToast({ status: 429, message: 'rate limited' });
  assert.equal(t.type, 'error');
});

test('non-409 failures map to an error toast', () => {
  assert.equal(recallErrorToast({ status: 500, message: 'internal: boom' }).type, 'error');
  assert.equal(recallErrorToast({ status: 0, message: '网络错误:timeout' }).type, 'error');
  assert.equal(
    recallErrorToast({ status: 403, message: 'forbidden: only author or room admin may recall' }).type,
    'error',
  );
  assert.equal(recallErrorToast({ status: 404, message: 'not found: msg' }).type, 'error');
});

test('missing error object degrades to an error toast, never a crash', () => {
  assert.equal(recallErrorToast(undefined).type, 'error');
  assert.equal(recallErrorToast(null).type, 'error');
});
