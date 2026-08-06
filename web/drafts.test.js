import test from 'node:test';
import assert from 'node:assert/strict';

import {
  draftBlocksToText,
  mirrorRead,
  textToDraftBlocks,
} from './drafts_store.js';
import { FakeClock, FakeNet, flush, makeStore, withStorage } from './drafts_test_helpers.js';

const MENTION_A = '01ARZ3NDEKTSV4RRFFQ69G5FAV'; // 26-char ULID (Crockford alphabet)
const MENTION_B = 'abcdefghjkmnpqrstvwxyz12';   // 25-char ULID

test('blocks round-trip verbatim, mentions included', () => {
  const text = `hi @${MENTION_A} there\nsecond line @${MENTION_B}`;
  assert.equal(draftBlocksToText(textToDraftBlocks(text)), text);
  assert.deepEqual(textToDraftBlocks('plain text'), [{ type: 'text', content: 'plain text' }]);
});

test('mentions at start/mid/end, adjacent mentions, and short @words', () => {
  for (const text of [
    `@${MENTION_A} hello`,
    `hello @${MENTION_A}`,
    `@${MENTION_A} @${MENTION_B} both`,
    `a@${MENTION_A}b@${MENTION_B}c`,
    'foo@bar baz', // short @word stays plain text (ULID length required)
  ]) {
    assert.equal(draftBlocksToText(textToDraftBlocks(text)), text);
  }
});

test('mention regex state resets between calls', () => {
  textToDraftBlocks(`@${MENTION_A} first`);
  assert.deepEqual(textToDraftBlocks('no mention here'), [{ type: 'text', content: 'no mention here' }]);
});

// ---------- debounce + serialization ----------

test('debounced autosave coalesces keystrokes into one PUT', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store } = makeStore(net, clock);
  store.input('r1', 'a', null);
  clock.tick(400);
  store.input('r1', 'ab', null);
  clock.tick(400);
  assert.equal(net.saves.length, 0, 'still inside the 800ms window');
  clock.tick(400);
  await flush();
  assert.equal(net.saves.length, 1);
  assert.equal(net.saves[0].roomId, 'r1');
  assert.deepEqual(net.saves[0].blocks, [{ type: 'text', content: 'ab' }]);
});

test('flush cancels the debounce and saves immediately', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store } = makeStore(net, clock);
  store.input('r1', 'pending text', null);
  const p = store.flush('r1');
  await flush();
  assert.equal(net.saves.length, 1, 'flush skips the debounce');
  net.saves[0].resolve({ saved: true });
  await p;
  clock.tick(5000);
  assert.equal(net.saves.length, 1, 'no stray debounced duplicate');
  assert.equal(store.snapshot('r1'), null, 'clean after a successful flush');
});

test('saves serialize per room: a new PUT waits for the in-flight one', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store } = makeStore(net, clock);
  store.input('r1', 'v1', null);
  clock.tick(800);
  await flush();
  assert.equal(net.saves.length, 1);
  store.input('r1', 'v2', null);
  clock.tick(800);
  await flush();
  assert.equal(net.saves.length, 1, 'v2 is chained, not issued concurrently');
  net.saves[0].resolve({ saved: true });
  await flush();
  assert.equal(net.saves.length, 2);
  assert.deepEqual(net.saves[1].blocks, [{ type: 'text', content: 'v2' }]);
});

test('a slow save never clears dirty when newer input arrived', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store } = makeStore(net, clock);
  store.input('r1', 'v1', null);
  clock.tick(800);
  await flush();
  store.input('r1', 'v2', null);
  clock.tick(800);
  await flush();
  net.saves[0].resolve({ saved: true });
  await flush();
  assert.equal(net.saves.length, 2);
  assert.equal(store.snapshot('r1').text, 'v2', 'still dirty until the newest save lands');
  net.saves[1].resolve({ saved: true });
  await flush();
  assert.equal(store.snapshot('r1'), null);
});

// ---------- discard (clear on send) ----------

test('clear-on-send: discard deletes AFTER any in-flight PUT (Enter + button)', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store, statuses } = makeStore(net, clock);
  store.input('r1', 'about to send', null);
  clock.tick(800);
  await flush();
  assert.equal(net.saves.length, 1);
  store.discard('r1'); // what draftComposerCleared runs after a send
  await flush();
  assert.equal(net.dels.length, 0, 'delete waits for the in-flight PUT');
  clock.tick(5000);
  assert.equal(net.dels.length, 0, 'no timer race either');
  net.saves[0].resolve({ saved: true });
  await flush();
  assert.equal(net.dels.length, 1);
  assert.equal(net.dels[0].roomId, 'r1');
  net.dels[0].resolve({ deleted: true });
  await flush();
  assert.ok(statuses.some((s) => s.roomId === 'r1' && s.status === 'idle'));
});

test('a failed discard can be retried as a delete', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store, statuses } = makeStore(net, clock);
  store.input('r1', 'sent', null);
  clock.tick(800);
  await flush();
  net.saves[0].resolve({ saved: true });
  await flush();
  store.discard('r1');
  await flush();
  net.dels[0].reject({ status: 0 });
  await flush();
  assert.ok(statuses.some((s) => s.status === 'error'));
  store.retry('r1').catch(() => {});
  await flush();
  assert.equal(net.dels.length, 2, 'retry re-runs the delete');
  net.dels[1].resolve({ deleted: true });
  await flush();
});

// ---------- empty composer semantics ----------

test('clearing the composer deletes the server draft (never PUTs empty)', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store } = makeStore(net, clock);
  store.input('r1', 'draft text', null);
  clock.tick(800);
  await flush();
  net.saves[0].resolve({ saved: true });
  await flush();
  store.input('r1', '   ', null);
  clock.tick(800);
  await flush();
  assert.equal(net.saves.length, 1, 'empty text is never PUT');
  assert.equal(net.dels.length, 1);
  net.dels[0].resolve({ deleted: true });
  await flush();
});

// ---------- error mapping ----------

test('401 on save triggers onAuthError', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  let authed = 0;
  const { store } = makeStore(net, clock, { onAuthError: () => { authed += 1; } });
  store.input('r1', 'text', null);
  clock.tick(800);
  await flush();
  net.saves[0].reject({ status: 401 });
  await flush();
  assert.equal(authed, 1);
});

test('403 marks the room forbidden and stops autosave without losing text', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store, statuses } = makeStore(net, clock);
  store.input('r1', 'text', null);
  clock.tick(800);
  await flush();
  net.saves[0].reject({ status: 403 });
  await flush();
  assert.ok(statuses.some((s) => s.status === 'forbidden'));
  store.input('r1', 'more text', null);
  clock.tick(5000);
  assert.equal(net.saves.length, 1, 'no further attempts after 403');
  assert.equal(store.snapshot('r1').text, 'more text', 'text is still tracked locally');
  await store.retry('r1');
  assert.equal(net.saves.length, 1, 'retry is a no-op while forbidden');
});

test('403 on save writes the mirror too (reload can recover the text)', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store } = makeStore(net, clock);
  await withStorage({ aero_pid: 'pid-1' }, async () => {
    store.input('r1', 'text while forbidden', null);
    clock.tick(800);
    await flush();
    net.saves[0].reject({ status: 403 });
    await flush();
    assert.equal(mirrorRead('r1').text, 'text while forbidden', 'mirror holds the 403 text');
  });
});

test('reauthorize after a save-403 resumes autosave (sticky-403 fix)', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store, statuses } = makeStore(net, clock);
  store.input('r1', 'text', null);
  clock.tick(800);
  await flush();
  net.saves[0].reject({ status: 403 });
  await flush();
  assert.ok(statuses.some((s) => s.status === 'forbidden'));
  store.input('r1', 'newer text', null); // typed while forbidden
  clock.tick(5000);
  assert.equal(net.saves.length, 1, 'forbidden: no autosave');
  store.reauthorize('r1'); // what restoreRoom runs after a successful GET
  store.reauthorize('unknown-room'); // no-op on an unknown room
  const resumed = store.flush('r1');
  await flush(); // pump: the resumed save dispatches (never await a pending op before settling it)
  assert.equal(net.saves.length, 2, 'autosave resumes after reauthorize');
  assert.deepEqual(net.saves[1].blocks, [{ type: 'text', content: 'newer text' }]);
  net.saves[1].resolve({ saved: true });
  await resumed;
  await flush();
  assert.ok(statuses.some((s) => s.roomId === 'r1' && s.status === 'saved'));
  assert.equal(store.snapshot('r1'), null, 'the resumed save is clean');
});

test('permission re-grant: setClean clears forbidden so autosave resumes', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store, statuses } = makeStore(net, clock);
  store.input('r1', 'text', null);
  clock.tick(800);
  await flush();
  net.saves[0].reject({ status: 403 });
  await flush();
  assert.ok(statuses.some((s) => s.status === 'forbidden'));
  store.setClean('r1', 'restored after re-grant', null, 0);
  store.input('r1', 'new text', null);
  clock.tick(800);
  await flush();
  assert.equal(net.saves.length, 2, 'autosave fires again after a successful restore');
  assert.deepEqual(net.saves[1].blocks, [{ type: 'text', content: 'new text' }]);
  net.saves[1].resolve({ saved: true });
  await flush();
});

test('save failure keeps the text dirty; manual retry re-saves it', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store, statuses } = makeStore(net, clock);
  store.input('r1', 'keep me', null);
  clock.tick(800);
  await flush();
  net.saves[0].reject({ status: 500 });
  await flush();
  assert.ok(statuses.some((s) => s.status === 'error'));
  assert.equal(store.snapshot('r1').text, 'keep me', 'failure never clears the form');
  store.retry('r1').catch(() => {});
  await flush();
  assert.equal(net.saves.length, 2);
  assert.deepEqual(net.saves[1].blocks, [{ type: 'text', content: 'keep me' }]);
  net.saves[1].resolve({ saved: true });
  await flush();
  assert.ok(statuses.some((s) => s.status === 'saved'));
  assert.equal(store.snapshot('r1'), null);
});

test('400 with a stale reply_to drops the reply context and re-saves once', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store, statuses } = makeStore(net, clock);
  store.input('r1', 'text', 'm9'); // parent was deleted server-side
  clock.tick(800);
  await flush();
  net.saves[0].reject({ status: 400, message: 'reply_to must reference an existing message in the same room' });
  await flush();
  assert.equal(net.saves.length, 2, 'auto-retried once WITHOUT reply_to');
  assert.equal(net.saves[1].replyTo, null, 'the retry omits the stale reply_to');
  assert.deepEqual(net.saves[1].blocks, [{ type: 'text', content: 'text' }]);
  net.saves[1].resolve({ saved: true });
  await flush();
  assert.ok(statuses.some((s) => s.status === 'saved'));
  assert.equal(store.snapshot('r1'), null, 'the retried save is clean');
});

test('stale-reply auto-retry is bounded and never fires for other 4xx', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store, statuses } = makeStore(net, clock);
  // A generic 400 (no reply_to semantics) is NOT auto-retried.
  store.input('r1', 'text', null);
  clock.tick(800);
  await flush();
  net.saves[0].reject({ status: 400, message: 'blocks too large' });
  await flush();
  assert.equal(net.saves.length, 1, 'generic 400 keeps the draft for manual retry');
  assert.ok(statuses.some((s) => s.status === 'error'));
  // Even a repeated reply_to 400 cannot loop: the second failure has no
  // reply context left, so it lands on the generic error path.
  const net2 = new FakeNet();
  const s2 = makeStore(net2, clock).store;
  s2.input('r1', 'text', 'm9');
  clock.tick(800);
  await flush();
  net2.saves[0].reject({ status: 400, message: 'reply_to must reference an existing message in the same room' });
  await flush();
  assert.equal(net2.saves.length, 2);
  net2.saves[1].reject({ status: 400, message: 'reply_to must reference an existing message in the same room' });
  await flush();
  assert.equal(net2.saves.length, 2, 'no retry loop');
});

test('400 from the repo fence (deleted parent) also self-heals', async () => {
  // The handler preflight does not filter deleted_at, so a DELETED parent is
  // rejected by the tx fence with a different message: "reply target is not a
  // live message in this room". The heal must match that surface too, or the
  // deleted-parent case stays in a permanent 400 livelock.
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store, statuses } = makeStore(net, clock);
  store.input('r1', 'text', 'm9');
  clock.tick(800);
  await flush();
  net.saves[0].reject({ status: 400, message: 'reply target is not a live message in this room' });
  await flush();
  assert.equal(net.saves.length, 2, 'auto-retried once WITHOUT reply_to');
  assert.equal(net.saves[1].replyTo, null, 'the retry omits the stale reply_to');
  net.saves[1].resolve({ saved: true });
  await flush();
  assert.ok(statuses.some((s) => s.status === 'saved'));
  assert.equal(store.snapshot('r1'), null, 'the retried save is clean');
});

test('failed flush keeps the text dirty so the mirror fallback can replay it', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store } = makeStore(net, clock);
  store.input('r1', 'to be saved later', null);
  const p = store.flush('r1');
  await flush();
  net.saves[0].reject({ status: 0 });
  await p.catch(() => {});
  assert.equal(store.snapshot('r1').text, 'to be saved later');
});

// ---------- mirror lifecycle (written on failure, cleared on confirm) ----------

test('in-room autosave failure writes the mirror (reload safety net)', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store } = makeStore(net, clock);
  await withStorage({ aero_pid: 'pid-1' }, async () => {
    store.input('r1', 'unsaved on failure', null);
    clock.tick(800);
    await flush();
    net.saves[0].reject({ status: 500 });
    await flush();
    assert.equal(mirrorRead('r1').text, 'unsaved on failure', 'mirror holds the failed text');
    store.retry('r1').catch(() => {});
    await flush();
    net.saves[1].resolve({ saved: true });
    await flush();
    assert.equal(mirrorRead('r1'), null, 'confirmed save clears the mirror');
  });
});

test('a save of older text never clears a newer mirror (rev guard)', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store, statuses } = makeStore(net, clock);
  await withStorage({ aero_pid: 'pid-1' }, async () => {
    store.input('r1', 'v1', null);
    clock.tick(800);
    await flush();
    store.input('r1', 'v2 newer', null); // typed while the v1 PUT is in flight
    clock.tick(800);
    await flush();
    net.saves[0].resolve({ saved: true });
    await flush();
    const saved = statuses.filter((s) => s.status === 'saved').at(-1);
    assert.equal(saved.clean, false, 'the v1 save is not clean');
    net.saves[1].resolve({ saved: true });
    await flush();
    assert.ok(statuses.some((s) => s.status === 'saved' && s.clean), 'newest save is clean');
  });
});

// ---------- keepalive chaining (pagehide) ----------

test('afterInflight runs only after the room tail settles (no PUT race)', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store } = makeStore(net, clock);
  store.input('r1', 'v1', null);
  clock.tick(800);
  await flush();
  assert.equal(net.saves.length, 1);
  let fired = 0;
  const p = store.afterInflight('r1', () => { fired += 1; });
  await flush();
  assert.equal(fired, 0, 'keepalive waits for the in-flight PUT');
  net.saves[0].resolve({ saved: true });
  await p;
  assert.equal(fired, 1);
});

test('afterInflight on an idle room runs immediately', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store } = makeStore(net, clock);
  let fired = 0;
  await store.afterInflight('r1', () => { fired += 1; });
  assert.equal(fired, 1);
});

test('restore guard primitives: inputRev advances on typing and on setClean', () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store } = makeStore(net, clock);
  const rev0 = store.inputRevOf('r1');
  store.input('r1', 'typed', null);
  assert.notEqual(store.inputRevOf('r1'), rev0, 'typing invalidates a pending restore');
  store.setClean('r1', 'restored', null, 0);
  assert.notEqual(store.inputRevOf('r1'), rev0);
  assert.equal(store.snapshot('r1'), null, 'setClean leaves nothing unsaved');
  assert.equal(store.hasState('r1'), true);
});

// ---------- reset (logout: no cross-account inheritance) ----------

test('reset drops all rooms and cancels pending debounce timers', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store } = makeStore(net, clock);
  store.input('r1', 'old user text', null);
  store.input('r2', 'also old', null);
  clock.tick(400);
  store.reset();
  clock.tick(5000);
  await flush();
  assert.equal(net.saves.length, 0, 'pending timers are cancelled, nothing fires after reset');
  assert.equal(store.hasState('r1'), false);
  assert.equal(store.hasState('r2'), false);
  assert.equal(store.snapshot('r1'), null);
});

test('reset clears forbidden + dirty state; a later input starts a fresh save', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store, statuses } = makeStore(net, clock);
  store.input('r1', 'text', null);
  clock.tick(800);
  await flush();
  net.saves[0].reject({ status: 403 });
  await flush();
  assert.ok(statuses.some((s) => s.status === 'forbidden'));
  store.reset();
  store.input('r1', 'new user text', null);
  clock.tick(800);
  await flush();
  assert.equal(net.saves.length, 2, 'the fresh entry is not forbidden and saves normally');
  assert.deepEqual(net.saves[1].blocks, [{ type: 'text', content: 'new user text' }]);
  net.saves[1].resolve({ saved: true });
  await flush();
});

test('reset while a save is in flight does not corrupt the chain for later rooms', async () => {
  const clock = new FakeClock();
  const net = new FakeNet();
  const { store } = makeStore(net, clock);
  store.input('r1', 'in flight', null);
  clock.tick(800);
  await flush();
  assert.equal(net.saves.length, 1);
  store.reset();
  net.saves[0].resolve({ saved: true });
  await flush();
  store.input('r1', 'after reset', null);
  clock.tick(800);
  await flush();
  assert.equal(net.saves.length, 2, 'a fresh room entry chains its own ops');
  net.saves[1].resolve({ saved: true });
  await flush();
  assert.equal(store.snapshot('r1'), null);
});
