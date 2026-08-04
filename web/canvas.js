// canvas.js — practical collaborative Canvas UI over the server's ordered op log.
//
// This is deliberately an ordered-log reducer, not a CRDT. The server assigns a
// gap-free `op_seq` per canvas; the separate WS `seq` belongs to the room bus and
// must never be used as the document cursor. Missing/reordered live frames are
// recovered from GET /api/rooms/:room/canvases/:id/ops?since=<last op_seq>.

import { api } from './api.js';
import {
  bindCanvasRealtime,
  CANVAS_OP_PAGE_SIZE,
  CanvasReducer,
  isSupportedCanvasOp,
} from './canvas_model.js';

export {
  bindCanvasRealtime,
  CANVAS_OP_PAGE_SIZE,
  CanvasReducer,
  isSupportedCanvasOp,
};
export { normalizeCanvasOp, reduceCanvasBlocks } from './canvas_model.js';

function isRecord(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function makeId() {
  if (globalThis.crypto?.randomUUID) return globalThis.crypto.randomUUID();
  return `note-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

function el(tag, opts = {}, children = []) {
  const node = document.createElement(tag);
  if (opts.class) node.className = opts.class;
  if (opts.text != null) node.textContent = opts.text;
  if (opts.attrs) {
    for (const [key, value] of Object.entries(opts.attrs)) node.setAttribute(key, value);
  }
  for (const child of children) if (child) node.appendChild(child);
  return node;
}

class CanvasUi {
  constructor({ state, ws, openModal, toast }) {
    this.state = state;
    this.ws = ws;
    this.openModal = openModal;
    this.toast = toast;
    this.modal = document.querySelector('#modal-canvas');
    this.list = document.querySelector('#canvas-list');
    this.empty = document.querySelector('#canvas-empty');
    this.editor = document.querySelector('#canvas-editor');
    this.title = document.querySelector('#canvas-title');
    this.meta = document.querySelector('#canvas-meta');
    this.syncState = document.querySelector('#canvas-sync-state');
    this.text = document.querySelector('#canvas-text');
    this.notes = document.querySelector('#canvas-notes');
    this.structured = document.querySelector('#canvas-structured-op');
    this.canvases = new Map();
    this.roomId = null;
    this.activeId = null;
    this.reducer = null;
    this.loadGeneration = 0;
    this.syncPromise = null;
    this.syncId = null;
    this.dirtyCanvases = new Set();
    this.textDirty = false;
  }

  init() {
    if (!this.modal || !this.list || !this.editor) return;
    document.querySelector('#btn-canvas')?.addEventListener('click', () => this.open());
    document.querySelector('#form-new-canvas')?.addEventListener('submit', (event) => (
      this.create(event)
    ));
    document.querySelector('#form-canvas-text')?.addEventListener('submit', (event) => (
      this.saveText(event)
    ));
    document.querySelector('#form-canvas-note')?.addEventListener('submit', (event) => (
      this.addNote(event)
    ));
    document.querySelector('#form-canvas-structured')?.addEventListener('submit', (event) => (
      this.submitStructured(event)
    ));
    document.querySelector('#canvas-reload')?.addEventListener('click', () => (
      this.rebuildActive()
    ));
    this.text?.addEventListener('input', () => { this.textDirty = true; });
    this.modal.addEventListener('click', (event) => {
      const close = event.target instanceof Element
        && event.target.closest('[data-close-canvas]');
      if (event.target === this.modal || close) this.modal.hidden = true;
    });
    document.addEventListener('keydown', (event) => {
      if (event.key === 'Escape' && !this.modal.hidden) this.modal.hidden = true;
    });
    bindCanvasRealtime(this.ws, {
      onCanvasOp: (frame) => this.onLiveOp(frame),
      onReconnect: () => {
        if (!this.modal.hidden && this.activeId) this.catchUp({ verifySnapshot: true });
      },
    });
  }

  async open() {
    const roomId = this.state.currentRoomId;
    if (!roomId) {
      this.toast('请先进入一个房间', 'error');
      return;
    }
    if (roomId !== this.roomId) {
      this.roomId = roomId;
      this.activeId = null;
      this.reducer = null;
      this.canvases.clear();
      this.dirtyCanvases.clear();
      this.renderEditor();
    }
    this.openModal(this.modal);
    await this.loadList();
  }

  async loadList(preferredId = this.activeId) {
    this.list.replaceChildren(el('p', { class: 'muted canvas-placeholder', text: '加载中…' }));
    try {
      const rows = await api.listCanvases(this.roomId);
      this.canvases = new Map((Array.isArray(rows) ? rows : []).map((row) => [String(row.id), row]));
      this.renderList();
      const chosen = preferredId && this.canvases.has(preferredId)
        ? preferredId
        : this.canvases.keys().next().value;
      if (chosen && chosen === this.activeId && this.reducer) {
        await this.catchUp({ verifySnapshot: true });
      } else if (chosen) {
        await this.select(chosen);
      }
      else {
        this.activeId = null;
        this.reducer = null;
        this.renderEditor();
      }
    } catch (error) {
      this.list.replaceChildren(el('p', {
        class: 'muted canvas-placeholder',
        text: `加载失败：${error.message || error}`,
      }));
    }
  }

  renderList() {
    const items = [];
    for (const canvas of this.canvases.values()) {
      const button = el('button', {
        class: `canvas-list-item${String(canvas.id) === this.activeId ? ' active' : ''}`,
        attrs: { type: 'button' },
      }, [
        el('span', { class: 'canvas-list-title', text: canvas.title || '未命名 Canvas' }),
        el('span', {
          class: 'canvas-list-meta',
          text: `快照 v${Number(canvas.version) || 0}${this.dirtyCanvases.has(String(canvas.id)) ? ' · 有更新' : ''}`,
        }),
      ]);
      button.addEventListener('click', () => this.select(String(canvas.id)));
      items.push(button);
    }
    this.list.replaceChildren(...items);
  }

  async create(event) {
    event.preventDefault();
    if (!this.roomId) return;
    const form = event.currentTarget;
    const title = String(new FormData(form).get('title') || '').trim();
    if (!title) {
      this.toast('请输入 Canvas 标题', 'error');
      return;
    }
    const submit = form.querySelector('[type="submit"]');
    if (submit) submit.disabled = true;
    try {
      const created = await api.createCanvas(this.roomId, {
        title,
        blocks: [{ type: 'text', content: '' }],
      });
      form.reset();
      await this.loadList(String(created.id));
    } catch (error) {
      this.toast(`创建失败：${error.message || error}`, 'error');
    } finally {
      if (submit) submit.disabled = false;
    }
  }

  async select(canvasId) {
    if (!canvasId || (canvasId === this.activeId && this.reducer
      && !this.dirtyCanvases.has(canvasId))) return;
    const generation = ++this.loadGeneration;
    this.activeId = canvasId;
    this.reducer = null;
    this.textDirty = false;
    this.renderList();
    this.renderEditor('正在重建 Canvas…');
    try {
      const canvas = await api.getCanvas(this.roomId, canvasId);
      if (generation !== this.loadGeneration || canvasId !== this.activeId) return;
      this.reducer = new CanvasReducer(canvas);
      this.canvases.set(canvasId, canvas);
      await this.fetchOps(this.reducer, generation);
      if (generation !== this.loadGeneration || canvasId !== this.activeId) return;
      this.dirtyCanvases.delete(canvasId);
      this.renderList();
      this.renderEditor(null, true);
    } catch (error) {
      if (generation !== this.loadGeneration) return;
      this.renderEditor(`加载失败：${error.message || error}`);
    }
  }

  async fetchOps(reducer, generation = this.loadGeneration) {
    for (;;) {
      const before = reducer.cursor;
      const payload = await api.listCanvasOps(this.roomId, reducer.canvasId, {
        since: before,
        limit: CANVAS_OP_PAGE_SIZE,
      });
      if (generation !== this.loadGeneration || reducer !== this.reducer) return;
      const rows = Array.isArray(payload?.ops) ? payload.ops : [];
      const result = reducer.ingestMany(rows);
      if (result.conflict) throw new Error('操作序号冲突，需要重新加载');
      if (rows.length > 0 && reducer.cursor <= before) {
        throw new Error('操作日志未前进，需要重新加载');
      }
      if (rows.length < CANVAS_OP_PAGE_SIZE) break;
    }
    if (reducer.pending.size > 0) throw new Error('操作日志存在缺口，需要重新加载');
  }

  async catchUp({ verifySnapshot = true } = {}) {
    const canvasId = this.activeId;
    if (!canvasId || !this.reducer) return;
    if (this.syncPromise && this.syncId === canvasId) return this.syncPromise;
    const generation = this.loadGeneration;
    this.syncId = canvasId;
    this.syncPromise = (async () => {
      this.setSync('正在同步…');
      let reducer = this.reducer;
      if (verifySnapshot) {
        const latest = await api.getCanvas(this.roomId, canvasId);
        if (generation !== this.loadGeneration || canvasId !== this.activeId) return;
        if (Number(latest.version) !== reducer.version) {
          reducer = new CanvasReducer(latest);
          this.reducer = reducer;
          this.canvases.set(canvasId, latest);
          this.textDirty = false;
        }
      }
      await this.fetchOps(reducer, generation);
      if (generation !== this.loadGeneration || canvasId !== this.activeId) return;
      this.dirtyCanvases.delete(canvasId);
      this.renderList();
      this.renderEditor(null, true);
    })().catch((error) => {
      if (generation === this.loadGeneration && canvasId === this.activeId) {
        this.setSync(`同步失败：${error.message || error}`, true);
      }
    }).finally(() => {
      if (this.syncId === canvasId) {
        this.syncPromise = null;
        this.syncId = null;
      }
    });
    return this.syncPromise;
  }

  rebuildActive() {
    if (this.activeId) {
      this.dirtyCanvases.add(this.activeId);
      this.select(this.activeId);
    }
  }

  onLiveOp(frame) {
    const canvasId = String(frame?.canvas_id ?? '');
    if (!canvasId || frame?.room_id !== this.roomId) return;
    if (canvasId !== this.activeId || !this.reducer) {
      this.dirtyCanvases.add(canvasId);
      if (!this.modal.hidden) this.renderList();
      return;
    }
    const result = this.reducer.ingest(frame);
    if (result.status === 'conflict') {
      this.setSync('检测到序号冲突，正在重建…', true);
      this.rebuildActive();
      return;
    }
    if (result.status === 'queued' || result.gap) {
      this.renderEditor('检测到操作缺口，正在补齐…');
      this.catchUp({ verifySnapshot: false });
      return;
    }
    if (result.status === 'applied') this.renderEditor();
  }

  async appendOperation(op) {
    if (!this.reducer || !this.activeId) return false;
    // One caller-generated identity per logical operation makes an uncertain
    // network retry safe: a committed first attempt returns its canonical row.
    const clientOpId = crypto.randomUUID();
    try {
      let row;
      try {
        row = await api.appendCanvasOp(this.roomId, this.activeId, op, clientOpId);
      } catch (error) {
        if (error?.status !== 0) throw error;
        row = await api.appendCanvasOp(this.roomId, this.activeId, op, clientOpId);
      }
      const result = this.reducer.ingest(row);
      if (result.status === 'conflict') {
        this.rebuildActive();
        return false;
      }
      if (result.status === 'queued' || result.gap) {
        await this.catchUp({ verifySnapshot: false });
      } else {
        this.renderEditor();
      }
      if (!isSupportedCanvasOp(op)) {
        this.toast(`操作 ${String(op.type || '(无类型)')} 已记录，但当前客户端不渲染该类型`);
      }
      return true;
    } catch (error) {
      this.toast(`保存失败：${error.message || error}`, 'error');
      return false;
    }
  }

  async saveText(event) {
    event.preventDefault();
    const button = event.currentTarget.querySelector('[type="submit"]');
    if (button) button.disabled = true;
    const ok = await this.appendOperation({ type: 'set_text', text: this.text?.value ?? '' });
    if (ok) this.textDirty = false;
    if (button) button.disabled = false;
  }

  async addNote(event) {
    event.preventDefault();
    const form = event.currentTarget;
    const input = form.elements.note;
    const content = String(input?.value || '').trim();
    if (!content) return;
    const button = form.querySelector('[type="submit"]');
    if (button) button.disabled = true;
    const noteId = form.dataset.pendingNoteId || makeId();
    form.dataset.pendingNoteId = noteId;
    const ok = await this.appendOperation({
      type: 'add_note',
      note_id: noteId,
      text: content,
      author_id: this.state.me?.id ?? null,
    });
    if (ok) {
      form.reset();
      delete form.dataset.pendingNoteId;
    }
    if (button) button.disabled = false;
  }

  async submitStructured(event) {
    event.preventDefault();
    const button = event.currentTarget.querySelector('[type="submit"]');
    let op;
    try {
      op = JSON.parse(this.structured?.value || '');
      if (!isRecord(op)) throw new Error('操作必须是 JSON 对象');
    } catch (error) {
      this.toast(`JSON 无效：${error.message || error}`, 'error');
      return;
    }
    if (button) button.disabled = true;
    const ok = await this.appendOperation(op);
    if (ok && this.structured) this.structured.value = '';
    if (button) button.disabled = false;
  }

  setSync(message, error = false) {
    if (!this.syncState) return;
    this.syncState.textContent = message || '';
    this.syncState.classList.toggle('error', error);
  }

  renderEditor(message = null, forceText = false) {
    const hasCanvas = Boolean(this.reducer && this.activeId);
    if (this.empty) this.empty.hidden = hasCanvas;
    this.editor.hidden = !hasCanvas;
    if (!hasCanvas) {
      if (message && this.empty) this.empty.textContent = message;
      else if (this.empty) this.empty.textContent = '选择或新建一个 Canvas';
      return;
    }
    const snapshot = this.reducer.snapshot();
    if (this.title) this.title.textContent = snapshot.title || '未命名 Canvas';
    if (this.meta) {
      this.meta.textContent = `快照 v${snapshot.version} · 操作 #${snapshot.op_seq}`
        + (snapshot.unknown_ops ? ` · ${snapshot.unknown_ops} 个未渲染操作` : '');
    }
    this.setSync(message || (snapshot.pending.length ? '等待缺失操作…' : '已同步'), Boolean(message));

    const textBlock = snapshot.blocks.find((block) => isRecord(block) && block.type === 'text');
    if (this.text && (forceText || !this.textDirty)) {
      this.text.value = String(textBlock?.content ?? textBlock?.text ?? '');
    }

    const notes = snapshot.blocks.filter((block) => isRecord(block) && block.type === 'note');
    const noteNodes = notes.map((note) => {
      const remove = el('button', {
        class: 'btn-icon canvas-note-delete',
        text: '×',
        attrs: { type: 'button', title: '删除便笺' },
      });
      remove.addEventListener('click', () => this.appendOperation({
        type: 'delete_note',
        note_id: String(note.note_id ?? ''),
      }));
      return el('article', { class: 'canvas-note' }, [
        el('p', { text: String(note.content ?? note.text ?? '') }),
        remove,
      ]);
    });
    this.notes?.replaceChildren(...(noteNodes.length ? noteNodes : [
      el('p', { class: 'muted canvas-placeholder', text: '暂无便笺' }),
    ]));

    const structured = snapshot.blocks.filter((block) => !(
      isRecord(block) && (block.type === 'text' || block.type === 'note')
    ));
    const structuredList = document.querySelector('#canvas-structured-blocks');
    structuredList?.replaceChildren(...(structured.length ? structured.map((block) => (
      el('pre', { class: 'canvas-json-block', text: JSON.stringify(block, null, 2) })
    )) : [el('p', { class: 'muted canvas-placeholder', text: '暂无其他结构化块' })]));
  }
}

export async function initCanvasUi() {
  const [{ state, ws, openModal }, { toast }] = await Promise.all([
    import('./context.js'),
    import('./render.js'),
  ]);
  const ui = new CanvasUi({ state, ws, openModal, toast });
  ui.init();
  return ui;
}

if (typeof document !== 'undefined') {
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', () => { initCanvasUi(); });
  } else {
    initCanvasUi();
  }
}
