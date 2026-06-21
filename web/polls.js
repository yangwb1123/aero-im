// polls.js — room polls UI: create / list / vote / live tally / creator-close.
//
// The backend ships full poll CRUD (POST /api/rooms/:id/polls, GET /api/polls/:id,
// vote, close) + a list endpoint + a `Poll` RoomEvent, but the web client had no
// surface to reach it. This adds a modal that lists the current room's polls with
// vote buttons + live results (and a creator-only close), plus a create form, and
// refreshes live on the `msg:poll` frame.
//
// Self-contained: loaded as its own <script type="module"> and auto-initialised on
// DOM-ready, so app.js (at its line-size limit) needs no changes. It shares the
// SAME singleton `ws` / `state` as app.js (ES modules evaluate once), so the
// `msg:poll` handler it registers fires on the live socket.

import { api, ApiError } from './api.js';
import { state, ws, openModal } from './context.js';
import { toast } from './render.js';

const $ = (sel) => document.querySelector(sel);

// Safe element builder — textContent only, never innerHTML, so a poll's
// user-supplied question/options can never inject markup.
function el(tag, opts = {}, children = []) {
  const n = document.createElement(tag);
  if (opts.class) n.className = opts.class;
  if (opts.text != null) n.textContent = opts.text;
  if (opts.attrs) for (const [k, v] of Object.entries(opts.attrs)) n.setAttribute(k, v);
  for (const c of children) if (c) n.appendChild(c);
  return n;
}

let modal = null;
let listEl = null;

function initPolls() {
  modal = $('#modal-polls');
  listEl = $('#polls-list');
  if (!modal || !listEl) return; // not the chat view (e.g. auth screen)

  const btn = $('#btn-polls');
  if (btn) btn.addEventListener('click', openPolls);

  const form = $('#form-new-poll');
  if (form) form.addEventListener('submit', onCreate);

  // Live refresh: a poll created/voted/closed in the current room re-renders the
  // open modal. The server fans RoomEvent::Poll out as a `msg:poll` frame.
  ws.on('msg:poll', (f) => {
    if (modal && !modal.hidden && f && f.room_id === state.currentRoomId) loadAndRender();
  });
}

function openPolls() {
  if (!state.currentRoomId) { toast('请先进入一个房间', 'error'); return; }
  openModal(modal);
  loadAndRender();
}

async function loadAndRender() {
  if (!listEl) return;
  listEl.replaceChildren(el('p', { class: 'muted', text: '加载中…' }));
  try {
    const polls = await api.listPolls(state.currentRoomId);
    if (!Array.isArray(polls) || polls.length === 0) {
      listEl.replaceChildren(el('p', { class: 'muted', text: '还没有投票,新建一个吧。' }));
      return;
    }
    // Each poll's live tally (bounded to the room's polls — a small set).
    const cards = [];
    for (const p of polls) {
      try {
        cards.push(renderCard(await api.getPoll(p.id)));
      } catch { /* a poll that vanished mid-load — skip it */ }
    }
    listEl.replaceChildren(...cards);
  } catch (e) {
    listEl.replaceChildren(el('p', { class: 'muted', text: `加载失败:${e.message || e}` }));
  }
}

// g = { poll, counts, total, voted, anonymous }
function renderCard(g) {
  const poll = g.poll || {};
  const options = Array.isArray(poll.options) ? poll.options : [];
  const counts = Array.isArray(g.counts) ? g.counts : [];
  const total = g.total || 0;
  const closed = !!poll.closed_at;
  const mine = state.me && poll.created_by === state.me.id;

  const card = el('div', { class: 'poll-card' });
  card.appendChild(el('div', { class: 'poll-q', text: poll.question || '(无标题)' }));

  const tags = [];
  if (poll.multi) tags.push('多选');
  if (g.anonymous) tags.push('匿名');
  tags.push(closed ? '已结束' : '进行中');
  tags.push(`${total} 票`);
  if (g.voted) tags.push('已投');
  card.appendChild(el('div', { class: 'poll-meta muted', text: tags.join(' · ') }));

  options.forEach((label, i) => {
    const c = counts[i] || 0;
    const pct = total > 0 ? Math.round((c * 100) / total) : 0;
    const row = el('div', { class: 'poll-opt' });
    if (!closed) {
      const b = el('button', { class: 'poll-vote-btn', text: label, attrs: { type: 'button' } });
      b.addEventListener('click', () => vote(poll.id, i, !!poll.multi));
      row.appendChild(b);
    } else {
      row.appendChild(el('span', { class: 'poll-opt-label', text: label }));
    }
    row.appendChild(el('div', { class: 'poll-bar' }, [
      el('div', { class: 'poll-bar-fill', attrs: { style: `width:${pct}%` } }),
    ]));
    row.appendChild(el('span', { class: 'poll-count muted', text: `${c} (${pct}%)` }));
    card.appendChild(row);
  });

  if (!closed && mine) {
    const close = el('button', { class: 'btn-ghost poll-close', text: '结束投票', attrs: { type: 'button' } });
    close.addEventListener('click', async () => {
      try { await api.closePoll(poll.id); loadAndRender(); }
      catch (e) { toast(`结束失败:${e.message || e}`, 'error'); }
    });
    card.appendChild(close);
  }
  return card;
}

async function vote(pollId, idx, multi) {
  try {
    await api.votePoll(pollId, multi ? { optionIdxs: [idx] } : { optionIdx: idx });
    loadAndRender();
  } catch (e) {
    const msg = e instanceof ApiError && e.status === 409 ? '投票已结束' : (e.message || e);
    toast(`投票失败:${msg}`, 'error');
  }
}

async function onCreate(e) {
  e.preventDefault();
  const form = e.currentTarget;
  const fd = new FormData(form);
  const question = String(fd.get('question') || '').trim();
  const options = String(fd.get('options') || '')
    .split('\n').map((s) => s.trim()).filter(Boolean);
  if (!question) { toast('请填写问题', 'error'); return; }
  if (options.length < 2) { toast('至少需要 2 个选项', 'error'); return; }
  if (options.length > 10) { toast('最多 10 个选项', 'error'); return; }
  try {
    await api.createPoll(state.currentRoomId, {
      question,
      options,
      multi: !!fd.get('multi'),
      anonymous: !!fd.get('anonymous'),
    });
    form.reset();
    const wrap = $('#poll-create-wrap');
    if (wrap) wrap.open = false;
    loadAndRender();
  } catch (e2) {
    toast(`创建失败:${e2.message || e2}`, 'error');
  }
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', initPolls);
} else {
  initPolls();
}
