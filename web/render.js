// render.js — DOM helpers, escaping, message/room/online rendering.
// All user-controlled strings are inserted via textContent or element attributes,
// never via innerHTML. The few innerHTML usages below are TEMPLATE STRINGS WITH NO
// INTERPOLATION (static skeleton); user content is filled in afterwards using
// textContent / setAttribute. Avatars derive a hue from a hashed id (numeric only).

// ---------- escape (used for any future legitimate HTML construction) ----------
const ESC = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' };
export function escapeHtml(s) {
  if (s == null) return '';
  return String(s).replace(/[&<>"']/g, (c) => ESC[c]);
}

// ---------- helpers ----------
function el(tag, opts = {}) {
  const n = document.createElement(tag);
  if (opts.className) n.className = opts.className;
  if (opts.text != null) n.textContent = String(opts.text);
  if (opts.dataset) for (const [k, v] of Object.entries(opts.dataset)) n.dataset[k] = String(v ?? '');
  if (opts.style) n.setAttribute('style', opts.style);
  if (opts.attrs) for (const [k, v] of Object.entries(opts.attrs)) n.setAttribute(k, String(v));
  return n;
}

// ---------- avatar ----------
function hashHue(s) {
  let h = 0;
  if (!s) return 210;
  for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) >>> 0;
  return h % 360;
}
function avatarStyleStr(id) {
  // numeric output only; no user content goes into the style string verbatim
  const h = hashHue(id || '') | 0;
  const h2 = (h + 40) % 360;
  return `background: linear-gradient(135deg, hsl(${h} 70% 55%), hsl(${h2} 70% 50%));`;
}
export function initialOf(name) {
  if (!name) return '?';
  const ch = String(name).trim()[0];
  return ch ? ch.toUpperCase() : '?';
}
function buildAvatar(id, label, sizeClass = '') {
  const a = el('div', { className: 'avatar' + (sizeClass ? ' ' + sizeClass : ''), style: avatarStyleStr(id) });
  a.textContent = initialOf(label);
  return a;
}

// ---------- time ----------
export function formatHM(iso) {
  if (!iso) return '';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '';
  const hh = String(d.getHours()).padStart(2, '0');
  const mm = String(d.getMinutes()).padStart(2, '0');
  return `${hh}:${mm}`;
}
function shortId(id) {
  if (!id) return '?';
  return String(id).slice(0, 6) + '…';
}

// ---------- block rendering (safe: builds DOM nodes) ----------
function appendBlock(parent, b) {
  if (!b || typeof b !== 'object') return;
  switch (b.type) {
    case 'text': {
      const span = el('span');
      span.textContent = b.content ?? '';
      parent.appendChild(span);
      return;
    }
    case 'code': {
      const lang = b.lang || b.language || '';
      if (lang) {
        const tag = el('span', { className: 'code-lang' });
        tag.textContent = lang;
        parent.appendChild(tag);
      }
      const pre = el('pre');
      const code = el('code');
      code.textContent = b.content ?? '';
      pre.appendChild(code);
      parent.appendChild(pre);
      return;
    }
    default: {
      const s = el('span', { className: 'unknown-block' });
      s.textContent = `[${String(b.type || 'unknown')}]`;
      parent.appendChild(s);
    }
  }
}
function buildBlocks(blocks) {
  const frag = document.createDocumentFragment();
  if (!Array.isArray(blocks) || !blocks.length) {
    const empty = el('span', { className: 'unknown-block', text: '[empty]' });
    frag.appendChild(empty);
    return frag;
  }
  for (const b of blocks) appendBlock(frag, b);
  return frag;
}

// ---------- message ----------
/**
 * @param {object} m message
 * @param {string} mePid my participant id
 * @param {Map<string,object>} participants id -> {display_name}
 * @param {{pending?: boolean}} opts
 */
export function renderMessage(m, mePid, participants, opts = {}) {
  const isSelf = m.sender_id === mePid;
  const sender = participants.get(m.sender_id);
  const senderName = sender?.display_name || (isSelf ? '我' : shortId(m.sender_id));

  const wrap = el('div', {
    className: 'msg' + (isSelf ? ' self' : '') + (opts.pending ? ' pending' : ''),
    dataset: {
      msgId: m.id,
      senderId: m.sender_id || '',
      createdAt: m.created_at || '',
    },
  });

  const avatar = buildAvatar(m.sender_id, senderName);

  const body = el('div', { className: 'msg-body' });

  const meta = el('div', { className: 'msg-meta' });
  const senderEl = el('span', { className: 'sender', text: senderName });
  const timeEl = el('span', { className: 'time', text: formatHM(m.created_at) });
  meta.appendChild(senderEl);
  meta.appendChild(timeEl);

  const bubble = el('div', { className: 'msg-bubble' });
  bubble.appendChild(buildBlocks(m.blocks));

  body.appendChild(meta);
  body.appendChild(bubble);

  wrap.appendChild(avatar);
  wrap.appendChild(body);
  return wrap;
}

// ---------- room item ----------
export function renderRoomItem(room, { active = false } = {}) {
  const wrap = el('div', {
    className: 'room-item' + (active ? ' active' : ''),
    dataset: { roomId: room.id },
  });
  const label = room.name || (room.kind === 'direct' ? 'Direct Message' : `Room ${shortId(room.id)}`);
  const avatar = buildAvatar(room.id, label, 'sm');
  const text = el('div', { className: 'room-text' });
  text.appendChild(el('div', { className: 'room-title', text: label }));
  text.appendChild(el('div', { className: 'room-sub', text: `${room.kind || 'room'} · ${shortId(room.id)}` }));
  wrap.appendChild(avatar);
  wrap.appendChild(text);
  return wrap;
}

// ---------- online list ----------
export function renderOnlineItem(pid, participant) {
  const name = participant?.display_name || shortId(pid);
  const wrap = el('div', { className: 'online-item' });
  const avatar = buildAvatar(pid, name);
  const dot = el('span', { className: 'dot' });
  dot.setAttribute('aria-hidden', 'true');
  const nameEl = el('span', { className: 'name', text: name });
  wrap.appendChild(avatar);
  wrap.appendChild(dot);
  wrap.appendChild(nameEl);
  return wrap;
}

// ---------- toast ----------
export function toast(message, kind = 'info', timeout = 3500) {
  const stack = document.getElementById('toast-stack');
  if (!stack) { console.log(`[toast/${kind}]`, message); return; }
  const node = el('div', { className: `toast ${kind}` });
  node.textContent = String(message);
  stack.appendChild(node);
  setTimeout(() => {
    node.style.transition = 'opacity .2s ease, transform .2s ease';
    node.style.opacity = '0';
    node.style.transform = 'translateX(8px)';
    setTimeout(() => node.remove(), 220);
  }, timeout);
}
