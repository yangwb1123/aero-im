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
function appendBlock(parent, b, ctx = {}) {
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
    case 'mention': {
      const pid = b.participant;
      const name = ctx.participants?.get?.(pid)?.display_name || shortId(pid);
      const tag = el('span', { className: 'mention' });
      tag.textContent = `@${name}`;
      parent.appendChild(tag);
      return;
    }
    case 'file': {
      const url = b.blob_id ? `/api/blobs/${encodeURIComponent(b.blob_id)}` : '#';
      const isImg = b.kind === 'image';
      if (isImg) {
        const img = el('img', { className: 'attachment-img', attrs: { src: url, alt: b.name || '' } });
        parent.appendChild(img);
      } else {
        const a = el('a', {
          className: 'attachment-file',
          attrs: { href: url, target: '_blank', rel: 'noopener' },
        });
        const ic = el('span', { className: 'attachment-icon', text: iconForKind(b.kind) });
        const meta = el('span', { className: 'attachment-meta' });
        meta.appendChild(el('span', { className: 'attachment-name', text: b.name || '附件' }));
        meta.appendChild(el('span', { className: 'attachment-size muted', text: humanSize(b.size) }));
        a.appendChild(ic);
        a.appendChild(meta);
        parent.appendChild(a);
      }
      return;
    }
    case 'voice': {
      const url = b.blob_id ? `/api/blobs/${encodeURIComponent(b.blob_id)}` : '';
      const wrap = el('div', { className: 'voice-block' });
      const audio = el('audio', { attrs: { src: url, controls: 'controls', preload: 'none' } });
      wrap.appendChild(audio);
      if (b.transcript) {
        wrap.appendChild(el('div', { className: 'voice-transcript', text: b.transcript }));
      }
      parent.appendChild(wrap);
      return;
    }
    case 'card': {
      if (b.schema === 'stream' && b.payload && b.payload.hls_url) {
        parent.appendChild(buildStreamCard(b.payload, ctx));
        return;
      }
      const wrap = el('div', { className: 'card-block' });
      const title = b.payload?.title || b.schema || 'card';
      wrap.appendChild(el('div', { className: 'card-title', text: String(title) }));
      const body = b.payload?.body || b.payload?.text || '';
      if (body) wrap.appendChild(el('div', { className: 'card-body', text: String(body) }));
      parent.appendChild(wrap);
      return;
    }
    case 'tool_call': {
      const wrap = el('div', { className: 'tool-call' });
      wrap.appendChild(el('div', { className: 'tool-call-head', text: `🛠 ${b.tool || 'tool'}` }));
      const args = el('pre', { className: 'tool-call-args' });
      args.textContent = JSON.stringify(b.args ?? {}, null, 2);
      wrap.appendChild(args);
      if (b.result !== undefined && b.result !== null) {
        const r = el('pre', { className: 'tool-call-result' });
        r.textContent = JSON.stringify(b.result, null, 2);
        wrap.appendChild(r);
      }
      parent.appendChild(wrap);
      return;
    }
    case 'thought': {
      if (b.hidden) return;
      const wrap = el('div', { className: 'thought' });
      wrap.appendChild(el('span', { className: 'thought-mark', text: '💭' }));
      wrap.appendChild(el('span', { text: b.content || '' }));
      parent.appendChild(wrap);
      return;
    }
    default: {
      const s = el('span', { className: 'unknown-block' });
      s.textContent = `[${String(b.type || 'unknown')}]`;
      parent.appendChild(s);
    }
  }
}

// Interactive live-stream card: HLS video + danmaku overlay + gift bar +
// viewer count. `ctx.live` (optional) supplies the catalog + the watch/chat/gift
// callbacks and a `register(streamId, controller)` hook so the app can route
// incoming `stream_event` frames back to this card.
function buildStreamCard(payload, ctx = {}) {
  const live = ctx.live || {};
  const streamId = payload.stream_id || '';
  const wrap = el('div', { className: 'stream-card' });
  if (streamId) wrap.dataset.streamId = streamId;

  // ---- head: LIVE badge · title · viewer count (· end for owner) ----
  const head = el('div', { className: 'stream-card-head' });
  const liveBadge = el('span', { className: 'stream-card-live', text: 'LIVE' });
  const title = el('div', { className: 'stream-card-title', text: payload.title || '直播' });
  const viewers = el('span', { className: 'stream-card-viewers', text: '👁 0' });
  head.appendChild(liveBadge);
  head.appendChild(title);
  head.appendChild(viewers);
  if (payload.owner_id && live.meId && payload.owner_id === live.meId && typeof live.end === 'function') {
    const endBtn = el('button', { className: 'stream-card-end', attrs: { type: 'button', title: '结束直播' }, text: '结束' });
    endBtn.addEventListener('click', () => live.end(streamId));
    head.appendChild(endBtn);
  }
  wrap.appendChild(head);

  // ---- stage: video + danmaku overlay + gift float layer ----
  const stage = el('div', { className: 'stream-card-stage' });
  const video = el('video', {
    attrs: { controls: 'controls', playsinline: 'true', muted: 'true', preload: 'metadata' },
    className: 'stream-card-video',
  });
  attachHls(video, payload.hls_url);
  const danmaku = el('div', { className: 'danmaku-layer' });
  stage.appendChild(video);
  stage.appendChild(danmaku);
  wrap.appendChild(stage);

  // ---- gift bar ----
  const giftBar = el('div', { className: 'gift-bar' });
  const catalog = Array.isArray(live.gifts) ? live.gifts : [];
  for (const g of catalog) {
    const btn = el('button', {
      className: 'gift-btn',
      attrs: { type: 'button', title: `${g.name} · ${g.coins} 币` },
    });
    btn.appendChild(el('span', { className: 'gift-icon', text: g.icon }));
    btn.appendChild(el('span', { className: 'gift-coins', text: String(g.coins) }));
    btn.addEventListener('click', () => {
      if (typeof live.gift === 'function') live.gift(streamId, g.id, 1);
    });
    giftBar.appendChild(btn);
  }
  if (catalog.length) wrap.appendChild(giftBar);

  // ---- recent-gift ticker ----
  const giftFeed = el('div', { className: 'gift-feed' });
  wrap.appendChild(giftFeed);

  // ---- danmaku composer ----
  const composer = el('form', { className: 'danmaku-composer' });
  const input = el('input', {
    className: 'danmaku-input',
    attrs: { type: 'text', maxlength: '200', placeholder: '发条弹幕…' },
  });
  const send = el('button', { className: 'danmaku-send', attrs: { type: 'submit' }, text: '发送' });
  composer.appendChild(input);
  composer.appendChild(send);
  composer.addEventListener('submit', (e) => {
    e.preventDefault();
    const v = input.value.trim();
    if (!v) return;
    if (typeof live.chat === 'function') live.chat(streamId, v);
    input.value = '';
  });
  wrap.appendChild(composer);

  const meta = el('div', { className: 'stream-card-meta muted' });
  meta.textContent = `${(payload.protocol || 'rtmp').toUpperCase()} · ${payload.hls_url}`;
  wrap.appendChild(meta);

  // ---- controller handed to the app for live event routing ----
  const controller = {
    streamId,
    addChat(line) { spawnDanmaku(danmaku, line); },
    addGift(line) { spawnGift(giftFeed, danmaku, line); },
    setViewers(n) { viewers.textContent = `👁 ${Number(n) || 0}`; },
    setStatus(status) {
      if (status === 'ended') {
        liveBadge.textContent = 'ENDED';
        liveBadge.classList.add('ended');
      } else {
        liveBadge.textContent = 'LIVE';
        liveBadge.classList.remove('ended');
      }
    },
  };
  if (streamId && typeof live.register === 'function') live.register(streamId, controller);
  return wrap;
}

// Fly one danmaku line across the overlay. DOM-only; auto-removes after the
// CSS animation (with a hard timeout as a safety net).
function spawnDanmaku(layer, line) {
  if (!layer) return;
  const item = el('div', { className: 'danmaku-item' });
  item.appendChild(el('span', { className: 'danmaku-who', text: `${line.sender_name || '匿名'}: ` }));
  item.appendChild(el('span', { className: 'danmaku-text', text: line.body || '' }));
  const track = Math.floor(Math.random() * 4); // 4 vertical lanes
  item.style.top = `${6 + track * 22}%`;
  layer.appendChild(item);
  const drop = () => item.remove();
  item.addEventListener('animationend', drop);
  setTimeout(drop, 12000);
}

// Show a gift: a ticker row in the feed + a big floating glyph over the video.
function spawnGift(feed, layer, line) {
  if (feed) {
    const row = el('div', { className: 'gift-feed-row' });
    row.appendChild(el('span', { className: 'gift-feed-icon', text: line.gift_icon || '🎁' }));
    const label = `${line.sender_name || '匿名'} 送出 ${line.gift_name || line.gift_id || '礼物'} ×${line.qty || 1}`;
    row.appendChild(el('span', { className: 'gift-feed-text', text: label }));
    feed.prepend(row);
    while (feed.childElementCount > 5) feed.lastElementChild.remove();
    setTimeout(() => row.remove(), 8000);
  }
  if (layer) {
    const float = el('div', { className: 'gift-float', text: line.gift_icon || '🎁' });
    layer.appendChild(float);
    const drop = () => float.remove();
    float.addEventListener('animationend', drop);
    setTimeout(drop, 4000);
  }
}

function attachHls(video, src) {
  if (!src) return;
  // Safari (and iOS) natively supports HLS.
  if (video.canPlayType('application/vnd.apple.mpegurl')) {
    video.src = src;
    return;
  }
  // hls.js is loaded from CDN in index.html; if unavailable, fall back.
  const HlsLib = window.Hls;
  if (HlsLib && typeof HlsLib === 'function' && HlsLib.isSupported && HlsLib.isSupported()) {
    const hls = new HlsLib({ lowLatencyMode: true, liveSyncDurationCount: 2 });
    hls.loadSource(src);
    hls.attachMedia(video);
  } else {
    video.src = src;
  }
}

function iconForKind(k) {
  switch (k) {
    case 'image': return '🖼';
    case 'video': return '🎬';
    case 'audio': return '🎵';
    case 'document': return '📄';
    default: return '📎';
  }
}
function humanSize(n) {
  if (!Number.isFinite(n)) return '';
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}
function buildBlocks(blocks, ctx = {}) {
  const frag = document.createDocumentFragment();
  if (!Array.isArray(blocks) || !blocks.length) {
    const empty = el('span', { className: 'unknown-block', text: '[empty]' });
    frag.appendChild(empty);
    return frag;
  }
  for (const b of blocks) appendBlock(frag, b, ctx);
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
  const isDeleted = Boolean(m.deleted_at);

  const wrap = el('div', {
    className:
      'msg' +
      (isSelf ? ' self' : '') +
      (opts.pending ? ' pending' : '') +
      (isDeleted ? ' deleted' : ''),
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
  if (m.edited_at) {
    meta.appendChild(el('span', { className: 'edited muted', text: '· 已编辑' }));
  }

  // Optional reply chip — references the parent message.
  if (m.reply_to && opts.replyTarget) {
    const r = opts.replyTarget;
    const chip = el('div', { className: 'reply-chip' });
    chip.dataset.targetId = r.id;
    const sName = participants.get(r.sender_id)?.display_name || shortId(r.sender_id);
    chip.appendChild(el('span', { className: 'reply-sender', text: sName }));
    chip.appendChild(el('span', { className: 'reply-text', text: textPreviewOf(r.blocks).slice(0, 80) }));
    body.appendChild(chip);
  }

  const bubble = el('div', { className: 'msg-bubble' });
  if (isDeleted) {
    bubble.appendChild(el('span', { className: 'muted', text: '消息已删除' }));
  } else {
    bubble.appendChild(buildBlocks(m.blocks, { participants, live: opts.live }));
  }

  // hover actions row (right-aligned mini buttons for owner; reactions for all)
  const actions = el('div', { className: 'msg-actions' });
  if (!isDeleted) {
    const btnReact = el('button', { className: 'msg-act', attrs: { title: '反应' } });
    btnReact.textContent = '☺';
    btnReact.dataset.action = 'react';
    const btnReply = el('button', { className: 'msg-act', attrs: { title: '回复' } });
    btnReply.textContent = '↩';
    btnReply.dataset.action = 'reply';
    actions.appendChild(btnReact);
    actions.appendChild(btnReply);
    if (isSelf) {
      const btnEdit = el('button', { className: 'msg-act', attrs: { title: '编辑' } });
      btnEdit.textContent = '✏';
      btnEdit.dataset.action = 'edit';
      const btnDel = el('button', { className: 'msg-act', attrs: { title: '删除' } });
      btnDel.textContent = '🗑';
      btnDel.dataset.action = 'delete';
      actions.appendChild(btnEdit);
      actions.appendChild(btnDel);
    }
  }

  // reactions row (filled in by app after summaries fetch)
  const reactions = el('div', { className: 'msg-reactions', dataset: { msgId: m.id } });

  body.appendChild(meta);
  body.appendChild(bubble);
  body.appendChild(reactions);
  body.appendChild(actions);

  wrap.appendChild(avatar);
  wrap.appendChild(body);
  return wrap;
}

// Render reaction chips into the message's `.msg-reactions` slot.
export function renderReactionsInto(node, summaries, myPid, onClick) {
  node.replaceChildren();
  if (!Array.isArray(summaries) || !summaries.length) return;
  for (const s of summaries) {
    const chip = el('button', {
      className: 'reaction-chip' + (s.participants?.includes?.(myPid) ? ' mine' : ''),
      attrs: { type: 'button', title: (s.participants || []).join(', ') },
    });
    chip.dataset.emoji = s.emoji;
    chip.appendChild(el('span', { className: 'reaction-emoji', text: s.emoji }));
    chip.appendChild(el('span', { className: 'reaction-count', text: String(s.count || 0) }));
    if (typeof onClick === 'function') {
      chip.addEventListener('click', () => onClick(s.emoji));
    }
    node.appendChild(chip);
  }
}

// Render typing indicator under the message list.
export function renderTypingInto(node, names) {
  node.replaceChildren();
  if (!names || !names.length) {
    node.hidden = true;
    return;
  }
  node.hidden = false;
  const text =
    names.length === 1
      ? `${names[0]} 正在输入…`
      : `${names.slice(0, 2).join('、')} 等 ${names.length} 人正在输入…`;
  node.appendChild(el('span', { className: 'typing-dots', text: '•••' }));
  node.appendChild(el('span', { text }));
}

function textPreviewOf(blocks) {
  if (!Array.isArray(blocks)) return '';
  const out = [];
  for (const b of blocks) {
    if (!b || typeof b !== 'object') continue;
    switch (b.type) {
      case 'text': out.push(b.content || ''); break;
      case 'code': out.push('[code]'); break;
      case 'file': out.push('[' + (b.name || 'file') + ']'); break;
      case 'voice': out.push('[voice]'); break;
      case 'mention': out.push('@…'); break;
      case 'card': out.push('[card]'); break;
      default: break;
    }
  }
  return out.join(' ').trim();
}

// ---------- room item ----------
export function renderRoomItem(room, { active = false, unread = 0 } = {}) {
  const wrap = el('div', {
    className: 'room-item' + (active ? ' active' : '') + (unread > 0 ? ' has-unread' : ''),
    dataset: { roomId: room.id },
  });
  const label = room.name || (room.kind === 'direct' ? 'Direct Message' : `Room ${shortId(room.id)}`);
  const avatar = buildAvatar(room.id, label, 'sm');
  const text = el('div', { className: 'room-text' });
  text.appendChild(el('div', { className: 'room-title', text: label }));
  text.appendChild(el('div', { className: 'room-sub', text: `${room.kind || 'room'} · ${shortId(room.id)}` }));
  wrap.appendChild(avatar);
  wrap.appendChild(text);
  if (unread > 0) {
    const badge = el('span', { className: 'room-badge', text: unread > 99 ? '99+' : String(unread) });
    wrap.appendChild(badge);
  }
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
