// drafts_store.js — pure draft state for the composer (no DOM, plain-node
// testable): per-room autosave store, block conversion, restore-source
// decision, localStorage mirror. Browser glue that drives this lives in
// drafts.js; app.js injects `state`/`els` there via initDrafts (delivery.js
// DI pattern).
//
// Concurrency (async-data spec): 800ms-debounced PUTs, strict per-room
// serialization (every op chains on the room's inflight tail — a stale PUT can
// never land after a DELETE), and an inputRev counter so stale responses never
// clobber newer state. Mirror lifecycle lives here too: written when a save
// fails (reload safety net), cleared only when the server confirms it holds
// exactly the current text.
import { auth } from './api.js';

const DEBOUNCE_MS = 800;
const MIRROR_PREFIX = 'aero_draft_v1:';

// Mention ULID regex mirrors app.js composeBlocksFromInput (blocks round-trip verbatim).
const MENTION_RE = /@([0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{25,26})\b/g;

export function textToDraftBlocks(text) {
  const blocks = [];
  let last = 0;
  MENTION_RE.lastIndex = 0;
  let m;
  while ((m = MENTION_RE.exec(text))) {
    if (m.index > last) blocks.push({ type: 'text', content: text.slice(last, m.index) });
    blocks.push({ type: 'mention', participant: m[1] });
    last = m.index + m[0].length;
  }
  if (last < text.length) blocks.push({ type: 'text', content: text.slice(last) });
  if (!blocks.length) blocks.push({ type: 'text', content: text });
  return blocks;
}

export function draftBlocksToText(blocks) {
  return (blocks ?? []).map((b) => (
    b.type === 'mention' ? `@${b.participant}` : (b.content ?? '')
  )).join('');
}

// Restore decision (async-data spec: race guards + source precedence). A room
// switch or newer input invalidates a pending restore — stale responses never
// clobber newer state. Precedence: local unsaved text > mirror newer than the
// server draft > server draft > mirror > empty. `clearMirror` is true only
// when the server draft is genuinely newer: a stale in-flight PUT committing
// after a pagehide keepalive can never erase a newer local mirror.
export function pickRestoreAction({ local, serverDraft, mirror, roomChanged, inputRevChanged }) {
  if (roomChanged || inputRevChanged) {
    return { apply: false, source: null, text: '', replyTo: null, clean: false, clearMirror: false };
  }
  if (local) {
    return { apply: true, source: 'local', text: local.text, replyTo: local.replyTo ?? null, clean: false, clearMirror: false };
  }
  const server = serverDraft ? {
    text: draftBlocksToText(serverDraft.blocks ?? []),
    replyTo: serverDraft.reply_to ?? null,
    updatedAt: Date.parse(serverDraft.updated_at ?? '') || 0,
  } : null;
  const mirrorParsed = mirror && typeof mirror.text === 'string' ? {
    text: mirror.text,
    replyTo: mirror.reply_to ?? null,
    savedAt: Date.parse(mirror.saved_at ?? '') || 0,
  } : null;
  if (server && mirrorParsed) {
    // Numeric comparison (ISO strings with different fractional precision do
    // not sort lexicographically). Mirror wins when it is newer local intent.
    if (mirrorParsed.savedAt > server.updatedAt) {
      return { apply: true, source: 'mirror', text: mirrorParsed.text, replyTo: mirrorParsed.replyTo, clean: false, clearMirror: false };
    }
    return { apply: true, source: 'server', text: server.text, replyTo: server.replyTo, clean: true, clearMirror: true };
  }
  if (server) {
    return { apply: true, source: 'server', text: server.text, replyTo: server.replyTo, clean: true, clearMirror: true };
  }
  if (mirrorParsed) {
    return { apply: true, source: 'mirror', text: mirrorParsed.text, replyTo: mirrorParsed.replyTo, clean: false, clearMirror: false };
  }
  return { apply: true, source: 'empty', text: '', replyTo: null, clean: false, clearMirror: false };
}

// Reply chip hydration: only keep the reply context when the parent message is
// still a LIVE message in the loaded history — matching the server's
// `deleted_at IS NULL` fence (a soft-deleted parent 400s on save); the server
// validates reply_to anyway.
export function resolveReplyTarget(messages, replyTo) {
  if (!replyTo) return null;
  const parent = (messages ?? []).find((m) => m.id === replyTo && !m.deleted_at);
  return parent ? { id: parent.id, sender_id: parent.sender_id, blocks: parent.blocks } : null;
}

// Send-accepted guard for clear-on-send: app.js clears the composer only after
// an optimistic send was queued (Enter and button share that funnel), so an
// empty input in the send room means the draft is obsolete. A non-empty input
// (rejected send) or a room mismatch keeps the draft.
export function shouldDiscardOnSend({ value, roomId, activeRoom }) {
  return roomId === activeRoom && value.trim() === '';
}

// ---------- per-room save store ----------
// `save`/`del` are injected network impls rejecting with `{ status }`;
// `schedule`/`clear` inject a timer; `onStatus` gets (roomId, status, clean)
// transitions (`clean` = the saved payload matches the current inputRev).
export function createDraftStore({ now, schedule, clear, save, del, onStatus, onAuthError }) {
  const rooms = new Map();

  function roomOf(roomId) {
    let r = rooms.get(roomId);
    if (!r) {
      r = { id: roomId, text: '', replyTo: null, dirty: false, inputRev: 0, timer: null,
        inflight: null, status: 'idle', lastSavedAt: null, forbidden: false, pendingDelete: false };
      rooms.set(roomId, r);
    }
    return r;
  }

  function setStatus(room, status, clean = false) { room.status = status; onStatus(room.id, status, clean); }

  function cancelTimer(room) { if (room.timer) { clear(room.timer); room.timer = null; } }

  // Serialize every mutation per room: the tail never rejects, so a failed op
  // does not break the chain for the next one.
  function enqueue(room, op) {
    const tail = (room.inflight || Promise.resolve()).catch(() => {}).then(op);
    room.inflight = tail.catch(() => {});
    return tail;
  }

  function handleSaveError(room, err) {
    const status = err?.status;
    if (status === 401) { onAuthError(); setStatus(room, 'error'); return; }
    if (status === 403) {
      room.forbidden = true;
      // Mirror parity with the 5xx/network branch: the text is user input and
      // a reload must be able to recover it even while the server refuses it.
      if (room.dirty && room.text.trim()) mirrorWrite(room.id, { text: room.text, replyTo: room.replyTo });
      setStatus(room, 'forbidden');
      return;
    }
    if (status === 400 && room.replyTo && /reply_to|reply target/i.test(String(err?.message ?? ''))) {
      // Stale reply target: the parent was deleted server-side (repo fence:
      // "reply target is not a live message in this room") or never existed
      // (handler preflight: "reply_to must reference an existing message in
      // the same room"). Drop the reply context, mirror the text, and re-save
      // once WITHOUT reply_to (error-recovery spec: fix the field, never
      // blind-retry a 4xx). The retry's own failure goes down the generic
      // path — bounded to one retry.
      room.replyTo = null;
      if (room.text.trim()) mirrorWrite(room.id, { text: room.text, replyTo: null });
      return saveNow(room.id).catch(() => {});
    }
    // 409 / other 4xx / 5xx / network: keep dirty + manual retry (mutations
    // are never auto-retried). Mirror the unsaved text so a reload can
    // restore it even when the server never received it.
    if (room.dirty && room.text.trim()) mirrorWrite(room.id, { text: room.text, replyTo: room.replyTo });
    setStatus(room, 'error');
  }

  function saveNow(roomId) {
    const room = rooms.get(roomId);
    if (!room || room.forbidden) return Promise.resolve();
    cancelTimer(room);
    const revAtSave = room.inputRev;
    if (!room.text.trim()) {
      // Empty composer → clear the server draft (never PUT empty blocks).
      room.pendingDelete = false;
      return enqueue(room, () => del(roomId)).then(() => {
        if (room.inputRev === revAtSave) room.dirty = false;
        room.lastSavedAt = null;
        mirrorClear(roomId);
        setStatus(room, 'idle');
      }).catch((err) => { room.pendingDelete = true; handleSaveError(room, err); throw err; });
    }
    setStatus(room, 'saving');
    return enqueue(room, () => save(roomId, textToDraftBlocks(room.text), room.replyTo ?? null)).then(() => {
      const clean = room.inputRev === revAtSave;
      if (clean) room.dirty = false;
      room.lastSavedAt = now();
      // Only a confirmed save of exactly the current text makes the mirror
      // obsolete — a newer keystroke keeps it until its own save lands.
      if (clean) mirrorClear(roomId);
      setStatus(room, 'saved', clean);
    }).catch((err) => { handleSaveError(room, err); throw err; });
  }

  function input(roomId, text, replyTo) {
    const room = roomOf(roomId);
    room.inputRev += 1;
    room.text = text;
    room.replyTo = replyTo;
    room.dirty = true;
    room.pendingDelete = false;
    if (room.forbidden) return; // access revoked: track text, stop autosaving
    cancelTimer(room);
    room.timer = schedule(() => { room.timer = null; saveNow(roomId).catch(() => {}); }, DEBOUNCE_MS);
  }

  function flush(roomId) {
    const room = rooms.get(roomId);
    if (room?.forbidden || !room?.dirty) return Promise.resolve();
    return saveNow(roomId);
  }

  function discard(roomId) {
    const room = rooms.get(roomId);
    if (!room) return Promise.resolve();
    cancelTimer(room);
    room.inputRev += 1;
    room.dirty = false;
    room.text = '';
    room.replyTo = null;
    room.pendingDelete = false;
    return enqueue(room, () => del(roomId)).then(() => {
      room.lastSavedAt = null;
      mirrorClear(roomId);
      setStatus(room, 'idle');
    }).catch((err) => { room.pendingDelete = true; handleSaveError(room, err); });
  }

  function retry(roomId) {
    const room = rooms.get(roomId);
    if (!room || room.forbidden) return Promise.resolve();
    if (room.pendingDelete) return saveNow(roomId);
    if (room.dirty) return saveNow(roomId);
    return Promise.resolve();
  }

  function snapshot(roomId) {
    const room = rooms.get(roomId);
    return room?.dirty ? { text: room.text, replyTo: room.replyTo } : null;
  }

  // Chain a best-effort op (pagehide keepalive PUT) behind the room's
  // in-flight tail so an older autosave can never land after it.
  function afterInflight(roomId, fn) {
    const room = rooms.get(roomId);
    const tail = room?.inflight || Promise.resolve();
    return tail.catch(() => {}).then(fn);
  }

  // Drop every per-room entry (pending timers included). Used on logout so a
  // different account on the same SPA session can never inherit — or autosave
  // over — the previous user's draft state.
  function reset() {
    for (const room of rooms.values()) cancelTimer(room);
    rooms.clear();
  }

  function hasState(roomId) { return rooms.has(roomId); }
  function inputRevOf(roomId) { return rooms.get(roomId)?.inputRev ?? 0; }

  // Mark a room's draft as confirmed-saved (server restore). Clears forbidden:
  // a successful restore proves access was re-granted in-session, so autosave
  // must not stay silently dead (sticky-403 fix).
  function setClean(roomId, text, replyTo, updatedAt) {
    const room = roomOf(roomId);
    cancelTimer(room);
    room.inputRev += 1;
    room.text = text;
    room.replyTo = replyTo;
    room.dirty = false;
    room.pendingDelete = false;
    room.forbidden = false;
    room.lastSavedAt = updatedAt ?? now();
    mirrorClear(roomId);
    setStatus(room, 'idle');
  }

  function markForbidden(roomId) {
    const room = roomOf(roomId);
    room.forbidden = true;
    setStatus(room, 'forbidden');
  }

  // A successful draft GET proves room access (the server applies the same
  // assert_room_access guard on read and write): clear a sticky save-403 so
  // autosave resumes even when the restore source is local/mirror (sticky-403
  // fix). Status stays untouched — the next input/save drives the indicator.
  function reauthorize(roomId) {
    const room = rooms.get(roomId);
    if (room) room.forbidden = false;
  }

  return { input, flush, discard, retry, snapshot, afterInflight, hasState, inputRevOf, setClean, markForbidden, reauthorize, reset };
}

// ---------- localStorage mirror (reload / flush-failure fallback) ----------
function mirrorKey(roomId) {
  return `${MIRROR_PREFIX}${auth.getPid() ?? 'anon'}:${roomId}`;
}

function browserStorage() {
  try {
    if (typeof window !== 'undefined' && window.localStorage) return window.localStorage;
    if (globalThis.localStorage) return globalThis.localStorage;
  } catch { /* private browsing */ }
  return null;
}

export function mirrorWrite(roomId, snap) {
  const storage = browserStorage();
  if (!storage) return;
  try {
    storage.setItem(mirrorKey(roomId), JSON.stringify({
      text: snap.text, reply_to: snap.replyTo ?? null, saved_at: new Date().toISOString(),
    }));
  } catch { /* storage is an enhancement */ }
}

export function mirrorRead(roomId) {
  const storage = browserStorage();
  if (!storage) return null;
  try {
    const parsed = JSON.parse(storage.getItem(mirrorKey(roomId)) ?? 'null');
    return parsed && typeof parsed.text === 'string' ? parsed : null;
  } catch { return null; }
}

export function mirrorClear(roomId) {
  const storage = browserStorage();
  if (!storage) return;
  try { storage.removeItem(mirrorKey(roomId)); } catch { /* ignore */ }
}
