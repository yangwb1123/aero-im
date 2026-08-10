// drafts.js — composer draft wiring (browser only): per-room autosave
// (debounced, server-persisted), restore on room open, clear on send, visible
// status indicator, pagehide keepalive + localStorage mirror. Pure state lives
// in drafts_store.js (plain-node testable).
//
// Wiring (delivery.js DI pattern): app.js calls initDrafts({ state, els,
// forceReauth, clearReply, renderReplyChip }), draftRoomSwitched(roomId) at the
// end of switchRoom, draftComposerCleared(roomId) right after submitComposer
// clears the input (Enter and the send button share that funnel — the form
// submit event alone never fires on Enter), and resetDrafts via initAuthUi's
// onLogout AND forceReauth (401/auth_expired session teardown) so a different
// account can never inherit — or autosave over — the previous user's state.
import { api, auth } from './api.js';
import { toast } from './render.js';
import {
  createDraftStore, mirrorClear, mirrorRead, mirrorWrite,
  pickRestoreAction, resolveReplyTarget, shouldDiscardOnSend, textToDraftBlocks,
} from './drafts_store.js';

const RESTORED_HINT_MS = 2000;

let runtime = null;         // initDrafts deps ({ state, els, forceReauth, clearReply, renderReplyChip })
let store = null;
let storeFactory = null;    // rebuilds `store` on logout (resetDrafts)
let activeRoom = null;      // the room the composer currently belongs to
let statusEl = null;
let stateRef = null;
let elsRef = null;
let restoreFailed = false;
let restoredHintTimer = null;
let pagehideInstalled = false;

const STATUS_TEXT = { saving: '草稿保存中…', saved: '草稿已保存', forbidden: '无法保存草稿(无权限)', error: '草稿保存失败,点击重试' };
const STATUS_KIND = { saved: 'saved', forbidden: 'error', error: 'error' };

function hhmm(d) {
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
}
function setIndicator(text, kind = '') {
  if (!statusEl) return;
  statusEl.hidden = !text;
  statusEl.className = text ? `composer-draft-status ${kind}`.trim() : 'composer-draft-status';
  statusEl.textContent = text ?? '';
}

// Mirror lifecycle is owned by the store (written on save failure, cleared on
// confirmed saves); this hook only renders. A live save supersedes any stale
// restore error.
function onStoreStatus(roomId, status) {
  if (status === 'saved') restoreFailed = false;
  if (roomId !== activeRoom) return;
  const text = STATUS_TEXT[status];
  if (text == null) { setIndicator(''); return; }
  setIndicator(status === 'saved' ? `${text} ${hhmm(new Date())}` : text, STATUS_KIND[status] ?? '');
}

function onInput() {
  if (!activeRoom) return;
  const replyTo = stateRef.replyTo ? stateRef.replyTo.id : null;
  store.input(activeRoom, elsRef.composerInput.value, replyTo);
}

function onPageHide() {
  if (!activeRoom) return;
  const snap = store.snapshot(activeRoom);
  if (!snap) return;
  mirrorWrite(activeRoom, snap); // synchronous safety net
  keepaliveSave(activeRoom, snap);
}

function onVisibilityHidden() {
  if (document.hidden) onPageHide();
}

// Chained behind the room's in-flight ops: an older autosave PUT must never
// land after this newer keepalive. Best-effort — under hard teardown the chain
// may never run, but the synchronous mirror write above remains the backstop
// (pickRestoreAction prefers a newer mirror over the server draft).
function keepaliveSave(roomId, snap) {
  const token = auth.getToken();
  const body = { blocks: textToDraftBlocks(snap.text) };
  if (snap.replyTo) body.reply_to = snap.replyTo;
  const headers = { 'Content-Type': 'application/json' };
  if (token) headers.Authorization = `Bearer ${token}`;
  store.afterInflight(roomId, () => fetch(`/api/rooms/${encodeURIComponent(roomId)}/draft`, {
    method: 'PUT', headers, body: JSON.stringify(body), keepalive: true,
  }).catch(() => { /* best-effort: the mirror already holds the text */ }));
}

function autoGrowInput() {
  const ta = elsRef.composerInput;
  ta.style.height = 'auto';
  ta.style.height = Math.min(ta.scrollHeight, 180) + 'px';
}

function showRestoredHint() {
  setIndicator('已恢复草稿', 'saved');
  if (restoredHintTimer) clearTimeout(restoredHintTimer);
  restoredHintTimer = setTimeout(() => {
    restoredHintTimer = null;
    if (statusEl && statusEl.textContent === '已恢复草稿') setIndicator('');
  }, RESTORED_HINT_MS);
}

function fillComposer(roomId, text, replyTo, updatedAt, opts = {}) {
  const input = elsRef.composerInput;
  input.value = text;
  elsRef.composerSend.disabled = !text.trim();
  autoGrowInput();
  // Reply chip: hydrate from the loaded history when the parent is still live,
  // otherwise drop the stale reply context (the server validates reply_to).
  // The store is re-staged with the RESOLVED target so chip and saved draft
  // always agree — a deleted/out-of-history parent can never be re-sent into
  // a 400 livelock.
  const parent = resolveReplyTarget(stateRef.messagesByRoom.get(roomId) ?? [], replyTo);
  const resolved = parent ? parent.id : null;
  if (parent) stateRef.replyTo = { id: parent.id, sender_id: parent.sender_id, blocks: parent.blocks };
  else if (stateRef.replyTo) stateRef.replyTo = null;
  if (opts.clean) store.setClean(roomId, text, resolved, updatedAt);
  runtime.renderReplyChip();
  showRestoredHint();
  return resolved;
}

async function restoreRoom(roomId) {
  const inputRevAtStart = store.inputRevOf(roomId);
  restoreFailed = false;
  setIndicator('正在恢复草稿…');
  let res;
  try {
    res = await api.getDraft(roomId);
  } catch (err) {
    if (activeRoom !== roomId) return;
    if (err?.status === 401) { runtime.forceReauth(); return; }
    if (err?.status === 403) { store.markForbidden(roomId); setIndicator('无法加载草稿(无权限)', 'error'); return; }
    restoreFailed = true;
    setIndicator('草稿加载失败,点击重试', 'error');
    return;
  }
  // Race guards (stale responses never clobber newer state): the room changed
  // while fetching → the new room owns the indicator; the user typed meanwhile
  // → drop the stale "正在恢复草稿…" hint, the debounced autosave takes over.
  if (activeRoom !== roomId) return;
  if (store.inputRevOf(roomId) !== inputRevAtStart) { setIndicator(''); return; }
  // A successful GET proves access was re-granted after a save-403 (the read
  // path applies the same assert_room_access guard): clear the sticky
  // forbidden flag so autosave resumes even when the restore source is
  // local/mirror (sticky-403 fix).
  store.reauthorize(roomId);
  const action = pickRestoreAction({
    local: store.snapshot(roomId),
    serverDraft: res?.draft ?? null,
    mirror: mirrorRead(roomId),
    roomChanged: false,
    inputRevChanged: false,
  });
  if (!action.apply) { setIndicator(''); return; }
  if (action.source === 'empty') { setIndicator(''); return; }
  if (action.clearMirror) mirrorClear(roomId);
  if (action.source === 'server') {
    fillComposer(roomId, action.text, action.replyTo, res.draft.updated_at, { clean: true });
    return;
  }
  const resolved = fillComposer(roomId, action.text, action.replyTo, null, { clean: false });
  if (action.source === 'mirror') {
    // Mirror replay self-heals: re-stage the text and re-save it to the server
    // with the RESOLVED reply target (fillComposer already dropped a stale
    // parent from the chip), so the replay never re-sends a dead reply_to.
    store.input(roomId, action.text, resolved);
    store.flush(roomId).catch(() => {});
  } else if (action.source === 'local') {
    // Local unsaved text survived a failed flush — retry the save now.
    store.flush(roomId).catch(() => {});
  }
}

function onStatusClick() {
  if (!activeRoom || !store) return;
  if (restoreFailed) { restoreRoom(activeRoom); return; }
  store.retry(activeRoom).catch(() => {});
}

export function draftRoomSwitched(newRoomId) {
  if (!runtime) return;
  const prev = activeRoom;
  activeRoom = newRoomId;
  if (prev && prev !== newRoomId) {
    const snap = store.snapshot(prev);
    if (snap) {
      mirrorWrite(prev, snap); // synchronous safety net for a failed flush
      store.flush(prev).catch(() => toast('草稿保存失败,已保留在本地', 'error'));
    }
  }
  // Per-room isolation: composer text and reply context belong to the new room.
  elsRef.composerInput.value = '';
  elsRef.composerSend.disabled = true;
  autoGrowInput();
  runtime.clearReply();
  setIndicator('');
  restoreRoom(newRoomId);
}

// Clear-on-send (Enter AND button path): app.js calls this right after
// submitComposer cleared the input, so an empty composer in the send room means
// the message was accepted and the draft is obsolete. A non-empty input
// (rejected send) or a room mismatch keeps the draft.
export function draftComposerCleared(roomId) {
  if (!runtime || !store) return;
  if (!shouldDiscardOnSend({ value: elsRef.composerInput.value, roomId, activeRoom })) return;
  mirrorClear(roomId); // drop the local fallback immediately (DELETE confirms server-side)
  store.discard(roomId);
}

// Logout + session-teardown hook (wired by app.js into initAuthUi's onLogout
// and called at the top of forceReauth BEFORE auth.clear()): the in-memory
// store is session scratch, so a different account on the same SPA session must
// never inherit — or autosave over — the previous user's drafts. The active
// room's unsaved snapshot is mirrored first (the mirror key is pid-scoped) so
// the owner's next login on this device can still restore it.
export function resetDrafts() {
  if (!runtime || !store) return;
  if (activeRoom) {
    const snap = store.snapshot(activeRoom);
    if (snap) mirrorWrite(activeRoom, snap);
  }
  store.reset();
  activeRoom = null;
  restoreFailed = false;
  if (restoredHintTimer) { clearTimeout(restoredHintTimer); restoredHintTimer = null; }
  setIndicator('');
}

export function initDrafts(deps) {
  if (runtime) return;
  runtime = {
    state: deps.state,
    els: deps.els,
    forceReauth: deps.forceReauth ?? (() => {}),
    clearReply: deps.clearReply ?? (() => {}),
    renderReplyChip: deps.renderReplyChip ?? (() => {}),
  };
  stateRef = runtime.state;
  elsRef = runtime.els;
  statusEl = document.getElementById('composer-draft-status');
  storeFactory = () => createDraftStore({
    now: () => Date.now(), schedule: (fn, ms) => setTimeout(fn, ms), clear: (h) => clearTimeout(h),
    save: (roomId, blocks, replyTo) => api.saveDraft(roomId, { blocks, reply_to: replyTo }), del: (roomId) => api.deleteDraft(roomId),
    onStatus: onStoreStatus, onAuthError: () => runtime.forceReauth(),
  });
  store = storeFactory();
  elsRef.composerInput.addEventListener('input', onInput);
  if (statusEl) statusEl.addEventListener('click', onStatusClick);
  if (!pagehideInstalled && typeof window !== 'undefined') {
    pagehideInstalled = true;
    window.addEventListener('pagehide', onPageHide);
    document.addEventListener('visibilitychange', onVisibilityHidden);
  }
}
