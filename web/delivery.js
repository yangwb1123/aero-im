// delivery.js — correlated optimistic sends and a persistent reconnect outbox.

import { auth } from './api.js';
import { toast } from './render.js';

export const MESSAGE_ACK_CAPABILITY = 'message_ack_v1';
const ACK_TIMEOUT_MS = 8000;
const MAX_SEND_ATTEMPTS = 3;
const RETRY_BACKOFF_MS = [1000, 2000];
const STORAGE_PREFIX = 'aero_pending_delivery_v1:';
const MAX_PERSISTED = 50;
const MAX_PERSISTED_AGE_MS = 24 * 60 * 60 * 1000;

let runtime = null;
const restoredParticipants = new Set();
let pagehideTarget = null;

function currentParticipantId() {
  try { return auth.getPid(); } catch { return null; }
}

function currentConnectionMarker() {
  return runtime ? `${runtime.instanceId}:${runtime.ws.connectionId}` : 0;
}

export function pendingTempId(clientMessageId) {
  return `_pending_${clientMessageId}`;
}

export function retryPendingMessage(clientMessageId) {
  const pending = currentPending(clientMessageId);
  if (!pending || pending.delivery_status !== 'failed') return false;
  pending.attempts = 0;
  pending.failure_message = null;
  pending.retryable = true;
  pending.delivery_status = 'waiting';
  pendingChanged();
  retryPending(pending);
  return true;
}

export function clearPendingDelivery(pending) {
  if (pending?._ackTimer) {
    clearTimeout(pending._ackTimer);
    pending._ackTimer = null;
  }
  // Callers synchronously remove the item immediately after clearing it. Persist
  // in a microtask so the snapshot observes that deletion.
  if (pending?.client_message_id) queueMicrotask(persistPending);
}

// Install once after the app has registered its ordinary message handlers.
// `getPendingMap` is a getter because logout replaces the whole state Map.
export function initReliableDelivery({
  ws,
  getPendingMap,
  onCanonical,
  onRestore = () => null,
  onPendingChanged = () => {},
  onFailure = (msg) => toast(msg, 'error'),
  onNotice = (msg, kind = 'info') => toast(msg, kind),
}) {
  if (runtime?.ws === ws) {
    const participantId = currentParticipantId();
    if (runtime.participantId !== participantId) {
      restoredParticipants.delete(runtime.participantId);
      runtime.persistedClientMessageIds.clear();
      runtime.legacyLoaded = false;
    }
    Object.assign(runtime, {
      getPendingMap, onCanonical, onRestore, onPendingChanged, onFailure, onNotice,
      participantId,
      persistenceDiscarded: false,
    });
    return runtime.dispose;
  }
  runtime?.dispose?.();
  const current = {
    ws, getPendingMap, onCanonical, onRestore, onPendingChanged, onFailure, onNotice,
    participantId: currentParticipantId(),
    instanceId: crypto.randomUUID(),
    persistedClientMessageIds: new Set(),
    legacyLoaded: false,
    persistenceDiscarded: false,
    disposers: [],
    dispose: null,
  };
  runtime = current;
  for (const [event, handler] of [
    ['msg:message_ack', handleAck],
    ['msg:message_nack', handleNack],
    ['msg:welcome', handleWelcome],
    ['close', handleClose],
    ['status', handleConnectionStatus],
  ]) {
    const dispose = ws.on(event, handler);
    if (typeof dispose === 'function') current.disposers.push(dispose);
  }
  if (typeof window !== 'undefined' && window.addEventListener) {
    pagehideTarget = window;
    pagehideTarget.addEventListener('pagehide', persistPending);
  }
  current.dispose = () => {
    if (runtime !== current) return;
    for (const pending of current.getPendingMap().values()) {
      if (pending?._ackTimer) clearTimeout(pending._ackTimer);
      if (pending) pending._ackTimer = null;
      if (pending && pending.delivery_status !== 'failed') pending.delivery_status = 'waiting';
    }
    persistPending();
    for (const dispose of current.disposers) dispose();
    pagehideTarget?.removeEventListener?.('pagehide', persistPending);
    pagehideTarget = null;
    restoredParticipants.delete(current.participantId);
    runtime = null;
  };
  return current.dispose;
}

export function sendOptimistically(sendFrame, addPending, outbound = null) {
  const clientMessageId = crypto.randomUUID();
  const canQueue = Boolean(outbound && runtime);
  const delivery = {
    client_message_id: clientMessageId,
    outbound: cloneOutbound(outbound),
    attempts: 0,
    last_connection_id: 0,
    delivery_status: 'waiting',
    retryable: true,
  };
  // Persist the logical send before touching the socket when storage is available.
  // A crash after transport acceptance can then only cause an idempotent retry.
  let pending = null;
  if (canQueue) {
    pending = addPending(delivery);
    if (!pending) return false;
    persistPending();
  }

  let accepted = false;
  try {
    accepted = Boolean(sendFrame(clientMessageId));
  } catch {
    // An ambiguous transport exception must be retried with the same id.
  }
  if (!accepted && (!outbound || !runtime)) {
    (runtime?.onNotice || toast)('连接中断，消息未发送，请重试', 'error');
    return false;
  }

  if (!pending) {
    pending = addPending({
      ...delivery,
      attempts: accepted ? 1 : 0,
      last_connection_id: accepted ? currentConnectionMarker() : 0,
      delivery_status: accepted ? 'sending' : 'waiting',
    });
  }
  if (!pending) return accepted;

  if (accepted) {
    pending.attempts = 1;
    pending.last_connection_id = currentConnectionMarker();
    pending.delivery_status = 'sending';
    pending.failure_message = null;
    if (runtime) {
      pendingChanged();
      armRetry(pending, ACK_TIMEOUT_MS);
    } else {
      persistPending();
    }
  } else {
    pending.attempts = 0;
    pending.last_connection_id = 0;
    pending.delivery_status = 'waiting';
    if (runtime) {
      pendingChanged();
      runtime.onNotice('当前离线，消息已加入待发送队列', 'info');
    }
  }
  return true;
}

/// Remove the current participant's persistent outbox on an explicit logout.
/// Network disconnects never call this: they must preserve and retry the queue.
export function discardPersistedDeliveries(participantId = currentParticipantId()) {
  if (!participantId) return;
  if (runtime?.participantId === participantId) {
    runtime.persistenceDiscarded = true;
    runtime.persistedClientMessageIds.clear();
  }
  restoredParticipants.delete(participantId);
  const storage = browserStorage();
  if (!storage) return;
  try {
    storage.removeItem(storageKey(participantId));
    for (const key of storageKeysWithPrefix(storage, storageEntryPrefix(participantId))) {
      storage.removeItem(key);
    }
  } catch {
    // Private browsing / disabled storage: the in-memory queue still works.
  }
}

export function findPendingMatch(serverMsg, pendingMap, myPid) {
  if (serverMsg.sender_id !== myPid) return null;
  const serverBlocks = JSON.stringify(serverMsg.blocks || []);
  const serverTime = Date.parse(serverMsg.created_at || '') || Date.now();
  let match = null;
  for (const [tempId, pending] of pendingMap) {
    if (
      pending.sender_id !== myPid ||
      pending.room_id !== serverMsg.room_id ||
      (pending.reply_to || null) !== (serverMsg.reply_to || null) ||
      JSON.stringify(pending.blocks || []) !== serverBlocks
    ) continue;
    const pendingTime = Date.parse(pending.created_at || '') || Date.now();
    if (Math.abs(serverTime - pendingTime) > 15000) continue;
    // Without a server correlation id, matching identical sends is ambiguous.
    // Leave all candidates pending rather than falsely settling the wrong one.
    if (match !== null) return null;
    match = tempId;
  }
  return match;
}

function cloneOutbound(outbound) {
  return outbound == null ? null : JSON.parse(JSON.stringify(outbound));
}

function storageKey(participantId) {
  return `${STORAGE_PREFIX}${participantId}`;
}

function storageEntryPrefix(participantId) {
  return `${storageKey(participantId)}:item:`;
}

function storageEntryKey(participantId, clientMessageId) {
  return `${storageEntryPrefix(participantId)}${clientMessageId}`;
}

function storageKeysWithPrefix(storage, prefix) {
  const keys = [];
  try {
    for (let index = 0; index < storage.length; index += 1) {
      const key = storage.key(index);
      if (key?.startsWith(prefix)) keys.push(key);
    }
  } catch {
    // Unsupported Storage implementations simply cannot enumerate outbox keys.
  }
  return keys;
}

function browserStorage() {
  if (typeof window === 'undefined') return null;
  try {
    return window.localStorage || null;
  } catch {
    return null;
  }
}

function serializablePending(pending) {
  const {
    _ackTimer: ignoredTimer,
    persistence_warning: ignoredWarning,
    ...serializable
  } = pending;
  void ignoredTimer;
  void ignoredWarning;
  return serializable;
}

function persistPending() {
  const storage = browserStorage();
  if (!runtime || !storage || runtime.persistenceDiscarded) return;
  const participantId = runtime.participantId;
  if (!participantId) return;
  const items = Array.from(runtime.getPendingMap().values())
    .filter((pending) => (
      pending?.sender_id === participantId
      && pending.client_message_id
      && pending.outbound
    ))
    .sort((left, right) => Date.parse(left.created_at || '') - Date.parse(right.created_at || ''))
    .slice(-MAX_PERSISTED)
    .map(serializablePending);
  const retainedIds = new Set();
  for (const item of items) {
    const clientMessageId = item.client_message_id;
    retainedIds.add(clientMessageId);
    try {
      storage.setItem(storageEntryKey(participantId, clientMessageId), JSON.stringify({
        version: 1,
        saved_at: new Date().toISOString(),
        item,
      }));
      runtime.persistedClientMessageIds.add(clientMessageId);
    } catch {
      // Storage is an enhancement. The bounded in-memory queue remains active.
    }
  }
  for (const clientMessageId of runtime.persistedClientMessageIds) {
    if (retainedIds.has(clientMessageId)) continue;
    try {
      storage.removeItem(storageEntryKey(participantId, clientMessageId));
      runtime.persistedClientMessageIds.delete(clientMessageId);
    } catch {
      // Keep the id tracked so a later persistence pass can retry cleanup.
    }
  }
  if (runtime.legacyLoaded) {
    try { storage.removeItem(storageKey(participantId)); } catch { /* best effort */ }
  }
}

function restorePersisted() {
  const storage = browserStorage();
  if (!runtime || !storage) return;
  const participantId = runtime.participantId;
  if (!participantId || restoredParticipants.has(participantId)) return;
  restoredParticipants.add(participantId);
  const candidates = new Map();
  for (const storedKey of storageKeysWithPrefix(storage, storageEntryPrefix(participantId))) {
    let record;
    try {
      record = JSON.parse(storage.getItem(storedKey) || 'null');
    } catch {
      try { storage.removeItem(storedKey); } catch { /* best effort */ }
      continue;
    }
    const item = record?.version === 1 ? record.item : null;
    const id = item?.client_message_id;
    if (!item || typeof id !== 'string' || storedKey !== storageEntryKey(participantId, id)) {
      try { storage.removeItem(storedKey); } catch { /* best effort */ }
      continue;
    }
    candidates.set(id, { item, storedKey, legacy: false });
  }

  const legacyKey = storageKey(participantId);
  let legacyRecord;
  try {
    legacyRecord = JSON.parse(storage.getItem(legacyKey) || 'null');
  } catch {
    try { storage.removeItem(legacyKey); } catch { /* best effort */ }
  }
  runtime.legacyLoaded = true;
  if (legacyRecord?.version === 1 && Array.isArray(legacyRecord.items)) {
    for (const item of legacyRecord.items.slice(-MAX_PERSISTED)) {
      if (typeof item?.client_message_id !== 'string' || candidates.has(item.client_message_id)) continue;
      candidates.set(item.client_message_id, { item, storedKey: legacyKey, legacy: true });
    }
  } else if (legacyRecord != null) {
    try { storage.removeItem(legacyKey); } catch { /* best effort */ }
  }

  const now = Date.now();
  let restored = 0;
  const ordered = [...candidates.values()]
    .sort((left, right) => Date.parse(left.item.created_at || '') - Date.parse(right.item.created_at || ''))
    .slice(-MAX_PERSISTED);
  for (const candidate of ordered) {
    const { item, storedKey, legacy } = candidate;
    const created = Date.parse(item?.created_at || '');
    const attempts = Number(item?.attempts ?? 0);
    const failed = item?.delivery_status === 'failed' || attempts >= MAX_SEND_ATTEMPTS;
    if (
      item.sender_id !== participantId
      || !item.outbound
      || !Number.isFinite(created)
      || now - created > MAX_PERSISTED_AGE_MS
      || !Number.isFinite(attempts)
      || attempts < 0
    ) {
      if (!legacy) {
        try { storage.removeItem(storedKey); } catch { /* best effort */ }
      }
      continue;
    }
    const key = pendingTempId(item.client_message_id);
    if (runtime.getPendingMap().has(key)) {
      if (!legacy) runtime.persistedClientMessageIds.add(item.client_message_id);
      continue;
    }
    const restoredItem = runtime.onRestore({
      ...item,
      id: key,
      attempts: Math.min(Math.floor(attempts), MAX_SEND_ATTEMPTS),
      delivery_status: failed ? 'failed' : 'waiting',
      failure_message: failed
        ? (typeof item.failure_message === 'string'
          ? item.failure_message.slice(0, 500)
          : '发送未获确认，请检查连接后重新发送')
        : null,
      retryable: failed ? false : true,
      _ackTimer: null,
    });
    if (restoredItem) {
      restored += 1;
      if (!legacy) runtime.persistedClientMessageIds.add(item.client_message_id);
    } else if (!legacy) {
      try { storage.removeItem(storedKey); } catch { /* best effort */ }
    }
  }
  if (restored) runtime.onPendingChanged();
  persistPending();
}

function currentPending(clientMessageId) {
  return runtime?.getPendingMap()?.get(pendingTempId(clientMessageId)) || null;
}

function isCurrent(pending) {
  return currentPending(pending?.client_message_id) === pending;
}

function armRetry(pending, delayMs) {
  clearPendingDelivery(pending);
  if (
    !isCurrent(pending) ||
    !pending.outbound ||
    !runtime?.ws?.supports(MESSAGE_ACK_CAPABILITY)
  ) return;
  pending._ackTimer = setTimeout(() => retryPending(pending), delayMs);
}

function sendOutbound(pending) {
  const { outbound: frame, client_message_id: clientMessageId } = pending;
  if (!frame) return false;
  if (frame.kind === 'markdown') {
    return runtime.ws.sendMarkdown(
      frame.roomId,
      frame.markdown,
      frame.replyTo ?? null,
      frame.expiresAfterSecs ?? null,
      clientMessageId,
    );
  }
  if (frame.kind === 'blocks') {
    return runtime.ws.sendMessage(
      frame.roomId,
      frame.blocks,
      frame.replyTo ?? null,
      clientMessageId,
    );
  }
  return false;
}

function retryPending(pending) {
  clearPendingDelivery(pending);
  if (!isCurrent(pending)) return;
  if (pending.attempts >= MAX_SEND_ATTEMPTS) {
    failPending(pending, '发送未获确认，请检查连接后重新发送');
    return;
  }
  if (!runtime.ws.supports(MESSAGE_ACK_CAPABILITY)) {
    if (sendOutbound(pending)) {
      pending.attempts += 1;
      pending.last_connection_id = currentConnectionMarker();
      pending.delivery_status = 'sending';
      pending.failure_message = null;
    } else {
      pending.delivery_status = 'waiting';
    }
    pendingChanged();
    return;
  }
  if (!sendOutbound(pending)) {
    pending.delivery_status = 'waiting';
    pendingChanged();
    return;
  }
  pending.attempts += 1;
  pending.last_connection_id = currentConnectionMarker();
  pending.delivery_status = 'sending';
  pending.failure_message = null;
  pendingChanged();
  armRetry(pending, ACK_TIMEOUT_MS);
}

function handleAck(frame) {
  if (!frame?.client_message_id || !frame?.message) return;
  runtime.onCanonical(frame.message, frame.client_message_id);
  persistPending();
}

function handleNack(frame) {
  const pending = currentPending(frame?.client_message_id);
  if (!pending) return;
  clearPendingDelivery(pending);
  pending.failure_message = frame.msg || frame.code || '发送失败';
  pending.retryable = Boolean(frame.retryable);
  if (pending.retryable && pending.attempts < MAX_SEND_ATTEMPTS) {
    pending.delivery_status = 'retrying';
    pendingChanged();
    const idx = Math.min(Math.max(pending.attempts - 1, 0), RETRY_BACKOFF_MS.length - 1);
    armRetry(pending, RETRY_BACKOFF_MS[idx]);
    return;
  }
  failPending(pending, pending.failure_message);
}

function handleWelcome() {
  restorePersisted();
  const supportsAck = runtime.ws.supports(MESSAGE_ACK_CAPABILITY);
  const connection = currentConnectionMarker();
  for (const pending of runtime.getPendingMap().values()) {
    if (!pending.client_message_id || pending.delivery_status === 'failed') continue;
    if (pending.last_connection_id !== connection) {
      retryPending(pending);
    } else if (supportsAck && !pending._ackTimer) {
      armRetry(pending, ACK_TIMEOUT_MS);
    }
  }
}

function handleClose() {
  let changed = false;
  for (const pending of runtime.getPendingMap().values()) {
    clearPendingDelivery(pending);
    if (!pending.client_message_id || pending.delivery_status === 'failed') continue;
    pending.delivery_status = 'waiting';
    changed = true;
  }
  if (changed) pendingChanged();
}

function failPending(pending, message) {
  clearPendingDelivery(pending);
  pending.delivery_status = 'failed';
  pending.failure_message = message;
  pending.retryable = false;
  pendingChanged();
  runtime.onFailure(`消息发送失败：${message}`);
}

function pendingChanged() {
  persistPending();
  refreshPersistenceWarnings();
  runtime?.onPendingChanged();
}

function refreshPersistenceWarnings() {
  if (!runtime) return;
  const storage = browserStorage();
  const participantId = runtime.participantId;
  const storageAvailable = Boolean(storage && participantId) && !runtime.persistenceDiscarded;
  for (const pending of runtime.getPendingMap().values()) {
    if (!pending?.client_message_id || !pending.outbound) continue;
    let durable = false;
    if (storageAvailable && pending.sender_id === participantId) {
      try {
        const record = JSON.parse(storage.getItem(storageEntryKey(participantId, pending.client_message_id)) || 'null');
        durable = record?.version === 1
          && record.item?.client_message_id === pending.client_message_id
          && Boolean(record.item.outbound);
      } catch {
        durable = false;
      }
    }
    pending.persistence_warning = !durable;
  }
}

function handleConnectionStatus(status) {
  if (typeof document === 'undefined') return;
  const indicator = document.getElementById('composer-network');
  const composer = document.getElementById('composer');
  if (!indicator || !composer) return;
  const online = status === 'up';
  indicator.hidden = online;
  indicator.textContent = status === 'wait'
    ? '连接中断 · 消息将排队并自动重试'
    : '正在重新连接 · 消息将排队';
  composer.classList.toggle('offline', !online);
}
