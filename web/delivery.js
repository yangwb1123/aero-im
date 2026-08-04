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
let pagehideInstalled = false;

export function pendingTempId(clientMessageId) {
  return `_pending_${clientMessageId}`;
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
}) {
  if (runtime?.ws === ws) {
    runtime = {
      ws, getPendingMap, onCanonical, onRestore, onPendingChanged, onFailure,
    };
    return;
  }
  runtime = {
    ws, getPendingMap, onCanonical, onRestore, onPendingChanged, onFailure,
  };
  ws.on('msg:message_ack', handleAck);
  ws.on('msg:message_nack', handleNack);
  ws.on('msg:welcome', handleWelcome);
  ws.on('close', handleClose);
  ws.on('status', handleConnectionStatus);
  if (!pagehideInstalled && typeof window !== 'undefined' && window.addEventListener) {
    pagehideInstalled = true;
    window.addEventListener('pagehide', persistPending);
  }
}

export function sendOptimistically(sendFrame, addPending, outbound = null) {
  const clientMessageId = crypto.randomUUID();
  const accepted = sendFrame(clientMessageId);
  if (!accepted && (!outbound || !runtime)) {
    toast('连接中断，消息未发送，请重试', 'error');
    return false;
  }
  const delivery = {
    client_message_id: clientMessageId,
    outbound: cloneOutbound(outbound),
    attempts: accepted ? 1 : 0,
    last_connection_id: accepted ? runtime?.ws?.connectionId || 0 : 0,
    delivery_status: accepted ? 'sending' : 'waiting',
    retryable: true,
  };
  // WebSocket message events cannot interleave with this JavaScript task, so the
  // pending entry exists before the canonical server echo can be processed.
  const pending = addPending(delivery);
  if (pending) {
    persistPending();
    if (accepted) {
      armRetry(pending, ACK_TIMEOUT_MS);
    } else {
      runtime.onPendingChanged();
      toast('当前离线，消息已加入待发送队列', 'info');
    }
  }
  return true;
}

/// Remove the current participant's persistent outbox on an explicit logout.
/// Network disconnects never call this: they must preserve and retry the queue.
export function discardPersistedDeliveries(participantId = auth.getPid()) {
  const storage = browserStorage();
  if (!participantId || !storage) return;
  try {
    storage.removeItem(storageKey(participantId));
  } catch {
    // Private browsing / disabled storage: the in-memory queue still works.
  }
  restoredParticipants.delete(participantId);
}

export function findPendingMatch(serverMsg, pendingMap, myPid) {
  if (serverMsg.sender_id !== myPid) return null;
  const serverBlocks = JSON.stringify(serverMsg.blocks || []);
  const serverTime = Date.parse(serverMsg.created_at || '') || Date.now();
  for (const [tempId, pending] of pendingMap) {
    if (
      pending.sender_id !== myPid ||
      pending.room_id !== serverMsg.room_id ||
      (pending.reply_to || null) !== (serverMsg.reply_to || null) ||
      JSON.stringify(pending.blocks || []) !== serverBlocks
    ) continue;
    const pendingTime = Date.parse(pending.created_at || '') || Date.now();
    if (Math.abs(serverTime - pendingTime) <= 15000) return tempId;
  }
  return null;
}

function cloneOutbound(outbound) {
  return outbound == null ? null : JSON.parse(JSON.stringify(outbound));
}

function storageKey(participantId) {
  return `${STORAGE_PREFIX}${participantId}`;
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
    ...serializable
  } = pending;
  void ignoredTimer;
  return serializable;
}

function persistPending() {
  const storage = browserStorage();
  if (!runtime || !storage) return;
  const participantId = auth.getPid();
  if (!participantId) return;
  const items = Array.from(runtime.getPendingMap().values())
    .filter((pending) => (
      pending?.sender_id === participantId
      && pending.client_message_id
      && pending.outbound
      && pending.delivery_status !== 'failed'
    ))
    .sort((left, right) => Date.parse(left.created_at || '') - Date.parse(right.created_at || ''))
    .slice(-MAX_PERSISTED)
    .map(serializablePending);
  try {
    if (!items.length) {
      storage.removeItem(storageKey(participantId));
      return;
    }
    storage.setItem(storageKey(participantId), JSON.stringify({
      version: 1,
      saved_at: new Date().toISOString(),
      items,
    }));
  } catch {
    // Storage is an enhancement. The bounded in-memory queue remains active.
  }
}

function restorePersisted() {
  const storage = browserStorage();
  if (!runtime || !storage) return;
  const participantId = auth.getPid();
  if (!participantId || restoredParticipants.has(participantId)) return;
  restoredParticipants.add(participantId);
  let parsed;
  try {
    parsed = JSON.parse(storage.getItem(storageKey(participantId)) || 'null');
  } catch {
    discardPersistedDeliveries(participantId);
    return;
  }
  if (parsed?.version !== 1 || !Array.isArray(parsed.items)) return;
  const now = Date.now();
  let restored = 0;
  for (const item of parsed.items.slice(-MAX_PERSISTED)) {
    const created = Date.parse(item?.created_at || '');
    if (
      !item
      || item.sender_id !== participantId
      || typeof item.client_message_id !== 'string'
      || !item.outbound
      || !Number.isFinite(created)
      || now - created > MAX_PERSISTED_AGE_MS
      || Number(item.attempts || 0) >= MAX_SEND_ATTEMPTS
    ) continue;
    const key = pendingTempId(item.client_message_id);
    if (runtime.getPendingMap().has(key)) continue;
    const restoredItem = runtime.onRestore({
      ...item,
      id: key,
      delivery_status: 'waiting',
      failure_message: null,
      retryable: true,
      _ackTimer: null,
    });
    if (restoredItem) restored += 1;
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
  if (!runtime.ws.supports(MESSAGE_ACK_CAPABILITY) || !sendOutbound(pending)) {
    pending.delivery_status = 'waiting';
    pendingChanged();
    return;
  }
  pending.attempts += 1;
  pending.last_connection_id = runtime.ws.connectionId;
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
  let changed = false;
  for (const pending of runtime.getPendingMap().values()) {
    if (!pending.client_message_id || pending.delivery_status === 'failed') continue;
    if (!supportsAck) {
      if (sendOutbound(pending)) {
        pending.attempts += 1;
        pending.last_connection_id = runtime.ws.connectionId;
        pending.delivery_status = 'sending';
      }
    } else if (pending.last_connection_id !== runtime.ws.connectionId) {
      retryPending(pending);
    } else {
      armRetry(pending, ACK_TIMEOUT_MS);
    }
    changed = true;
  }
  if (changed) pendingChanged();
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
  runtime?.onPendingChanged();
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
