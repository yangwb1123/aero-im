// drafts_test_helpers.js — shared plain-node test scaffolding for the drafts
// feature (FakeClock / FakeNet / storage stub). No assertions live here; both
// drafts.test.js and drafts_restore.test.js import from this module.
import { createDraftStore } from './drafts_store.js';

export function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}

// Fake timer: fire due callbacks when tick() advances the clock.
export class FakeClock {
  constructor() { this.t = 0; this.nextId = 1; this.timers = new Map(); }
  now() { return this.t; }
  schedule(fn, ms) {
    const h = this.nextId++;
    this.timers.set(h, { fn, at: this.t + ms });
    return h;
  }
  clear(h) { this.timers.delete(h); }
  tick(ms) {
    this.t += ms;
    const due = [...this.timers.entries()].filter(([, x]) => x.at <= this.t).sort((a, b) => a[1].at - b[1].at);
    for (const [h, x] of due) { this.timers.delete(h); x.fn(); }
  }
}

// Manual network: every save/del stays pending until the test settles it.
// Ops are dispatched by the store via promise chains, so tests always drain
// microtasks (`await flush()`) after tick/discard/retry before asserting.
export class FakeNet {
  constructor() { this.saves = []; this.dels = []; }
  save(roomId, blocks, replyTo) {
    const d = deferred();
    this.saves.push({ roomId, blocks, replyTo, ...d });
    return d.promise;
  }
  del(roomId) {
    const d = deferred();
    this.dels.push({ roomId, ...d });
    return d.promise;
  }
}

export function makeStore(net, clock, { onAuthError } = {}) {
  const statuses = [];
  const store = createDraftStore({
    now: () => clock.now(),
    schedule: (fn, ms) => clock.schedule(fn, ms),
    clear: (h) => clock.clear(h),
    save: (roomId, blocks, replyTo) => net.save(roomId, blocks, replyTo),
    del: (roomId) => net.del(roomId),
    onStatus: (roomId, status, clean) => statuses.push({ roomId, status, clean }),
    onAuthError: onAuthError || (() => {}),
  });
  return { store, statuses };
}

// Drains all pending microtasks (setImmediate runs after them).
export const flush = () => new Promise((r) => globalThis.setImmediate(r));

// Stub localStorage for the mirror tests (node has no storage by default).
// Async-aware: the stub stays installed until the test body settles.
export async function withStorage(entries, fn) {
  const mem = new Map(Object.entries(entries || {}));
  globalThis.localStorage = {
    getItem: (k) => mem.get(k) ?? null,
    setItem: (k, v) => mem.set(k, String(v)),
    removeItem: (k) => mem.delete(k),
  };
  try {
    await fn();
  } finally {
    delete globalThis.localStorage;
  }
}
