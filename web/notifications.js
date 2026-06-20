// notifications.js — durable notification inbox panel (extracted from app.js).
//
// Bounded, additive panel that surfaces the backend inbox
// (GET /api/notifications, /api/notifications/count, POST /api/notifications/read).
// Realtime: the `msg:notify` WS frame (handled in app.js) bumps the badge /
// refreshes the open panel; the persisted count is source of truth on connect.
//
// `switchRoom` lives in app-core (room domain); to avoid a circular import it is
// injected once via initNotifications({ switchRoom }) rather than imported.
//
// Public surface:
//   • initNotifications({ switchRoom }) — wire buttons + capture the room switcher
//   • setNotifBadge(n) / bumpNotifBadge() / refreshNotifBadge() — badge control
//   • loadNotifications() — (re)populate the open panel

import { api } from './api.js';
import { state, els, loadingDiv, mutedDiv, formatTime, scrollToMessage } from './context.js';
import { toast } from './render.js';

let notifUnread = 0;
let switchRoom = (id) => {}; // injected by initNotifications

export function initNotifications(deps) {
  if (deps && typeof deps.switchRoom === 'function') switchRoom = deps.switchRoom;

  els.btnNotif.addEventListener('click', () => {
    els.drawerNotif.hidden = false;
    loadNotifications();
  });

  els.btnNotifReadAll.addEventListener('click', async () => {
    try {
      await api.markNotificationsRead({ all: true });
      setNotifBadge(0);
      // Reflect read state on any currently-rendered rows without a full re-pull.
      for (const row of els.notifList.querySelectorAll('.notif-row.notif-unread')) {
        row.classList.remove('notif-unread');
        const dot = row.querySelector('.notif-dot');
        if (dot) dot.remove();
      }
    } catch (err) { toast(`标记失败:${err.message}`, 'error'); }
  });
}

export function setNotifBadge(n) {
  notifUnread = Math.max(0, n | 0);
  if (!els.notifBadge) return;
  els.notifBadge.textContent = notifUnread > 99 ? '99+' : String(notifUnread);
  els.notifBadge.hidden = notifUnread <= 0;
}

export function bumpNotifBadge() { setNotifBadge(notifUnread + 1); }

export async function refreshNotifBadge() {
  try {
    const res = await api.notificationCount();
    setNotifBadge(Number(res?.unread) || 0);
  } catch { /* best-effort badge */ }
}

// Short human label for a notification kind (falls back to the raw token).
const NOTIF_KIND_LABEL = {
  mention: '提到了你',
  reply: '回复了你',
  reaction: '回应了你的消息',
  saved_search: '匹配了你的保存搜索',
  aggregate_reply: '在你关注的话题里有新回复',
};

function buildNotifRow(n) {
  // Builds DOM nodes only (no innerHTML) — every server string goes through
  // textContent, mirroring render.js's XSS-safe convention.
  const row = document.createElement('div');
  row.className = 'notif-row' + (n.read_at ? '' : ' notif-unread');
  if (typeof n.importance_score === 'number' && n.importance_score >= 0.8) {
    row.classList.add('notif-important');
  }
  row.dataset.notifId = n.id || '';

  const top = document.createElement('div');
  top.className = 'notif-row-top';
  const actor = n.actor_id ? state.participants.get(n.actor_id) : null;
  const who = actor?.display_name || (n.actor_id ? `${String(n.actor_id).slice(0, 6)}…` : '有人');
  const verb = NOTIF_KIND_LABEL[n.kind] || String(n.kind || '通知');
  const agg = (n.kind === 'aggregate_reply' && n.aggregate_count) ? `(${n.aggregate_count})` : '';
  const label = document.createElement('span');
  label.className = 'notif-text';
  label.textContent = `${who} ${verb}${agg ? ' ' + agg : ''}`;
  top.appendChild(label);
  if (!n.read_at) {
    const dot = document.createElement('span');
    dot.className = 'notif-dot';
    dot.title = '未读';
    top.appendChild(dot);
  }
  row.appendChild(top);

  const meta = document.createElement('div');
  meta.className = 'notif-meta muted';
  const room = state.rooms.get(n.room_id);
  const where = room?.name ? room.name : `Room ${String(n.room_id || '').slice(0, 6)}…`;
  meta.textContent = `${where} · ${formatTime(n.created_at)}`;
  row.appendChild(meta);

  row.addEventListener('click', () => onNotifClick(n, row));
  return row;
}

async function onNotifClick(n, row) {
  // Mark this one read (if unread), then jump to the room/message it points at.
  if (!n.read_at && n.id) {
    try {
      await api.markNotificationsRead({ ids: [n.id] });
      n.read_at = new Date().toISOString();
      row.classList.remove('notif-unread');
      const dot = row.querySelector('.notif-dot');
      if (dot) dot.remove();
      setNotifBadge(notifUnread - 1);
    } catch (err) { toast(`标记已读失败:${err.message}`, 'error'); }
  }
  if (n.room_id) {
    els.drawerNotif.hidden = true;
    if (n.room_id !== state.currentRoomId) {
      await switchRoom(n.room_id);
    }
    if (n.message_id) setTimeout(() => scrollToMessage(n.message_id), 200);
  }
}

export async function loadNotifications() {
  els.notifList.replaceChildren(loadingDiv('加载中…'));
  try {
    const list = await api.listNotifications({ limit: 50 });
    const arr = Array.isArray(list) ? list : [];
    els.notifList.replaceChildren();
    if (!arr.length) { els.notifList.appendChild(mutedDiv('暂无通知。')); return; }
    for (const n of arr) els.notifList.appendChild(buildNotifRow(n));
  } catch (err) {
    els.notifList.replaceChildren(mutedDiv(`加载失败:${err.message}`));
  }
}
