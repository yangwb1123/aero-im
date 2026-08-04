// Pure Canvas ordered-log model. No DOM, storage, or network dependencies.

export const CANVAS_OP_PAGE_SIZE = 500;

function isRecord(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function jsonClone(value) {
  return JSON.parse(JSON.stringify(value));
}

function asBlocks(value) {
  return Array.isArray(value) ? jsonClone(value) : [];
}

function opId(raw, canvasId, seq) {
  const value = raw.op_id ?? raw.id;
  return value == null || value === '' ? `${canvasId}:${seq}` : String(value);
}

// Durable REST rows carry `seq`; live WS frames carry both room-bus `seq` and
// durable `op_seq`. Prefer op_seq whenever it exists.
export function normalizeCanvasOp(raw) {
  if (!isRecord(raw) || !isRecord(raw.op)) return null;
  const seq = Number(raw.op_seq ?? raw.seq);
  const canvasId = String(raw.canvas_id ?? '');
  if (!canvasId || !Number.isSafeInteger(seq) || seq < 1) return null;
  return {
    id: opId(raw, canvasId, seq),
    canvas_id: canvasId,
    seq,
    author_id: raw.author_id == null ? null : String(raw.author_id),
    op: jsonClone(raw.op),
    created_at: raw.created_at ?? null,
  };
}

export function isSupportedCanvasOp(op) {
  return isRecord(op) && [
    'set_text',
    'add_note',
    'update_note',
    'delete_note',
    'set_blocks',
    'upsert_block',
    'delete_block',
  ].includes(op.type);
}

// Reduce one operation without mutating the caller's snapshot. Operations use
// stable note/block ids, so replay and a POST response followed by its WS echo
// are deterministic and idempotent.
export function reduceCanvasBlocks(blocks, op) {
  const next = asBlocks(blocks);
  if (!isRecord(op)) return next;

  if (op.type === 'set_blocks') {
    return Array.isArray(op.blocks) ? asBlocks(op.blocks) : next;
  }

  if (op.type === 'set_text') {
    const content = String(op.text ?? op.content ?? '');
    const index = next.findIndex((block) => isRecord(block) && block.type === 'text');
    const block = index >= 0
      ? { ...next[index], type: 'text', content }
      : { type: 'text', content };
    if (index >= 0) next[index] = block;
    else next.unshift(block);
    return next;
  }

  if (op.type === 'add_note' || op.type === 'update_note') {
    const noteId = String(op.note_id ?? '').trim();
    if (!noteId) return next;
    const content = String(op.text ?? op.content ?? '');
    const index = next.findIndex((block) => (
      isRecord(block) && block.type === 'note' && String(block.note_id ?? '') === noteId
    ));
    const note = {
      ...(index >= 0 ? next[index] : {}),
      type: 'note',
      note_id: noteId,
      content,
    };
    if (op.author_id != null) note.author_id = String(op.author_id);
    if (index >= 0) next[index] = note;
    else next.push(note);
    return next;
  }

  if (op.type === 'delete_note') {
    const noteId = String(op.note_id ?? '').trim();
    return noteId
      ? next.filter((block) => !(
        isRecord(block) && block.type === 'note' && String(block.note_id ?? '') === noteId
      ))
      : next;
  }

  if (op.type === 'upsert_block') {
    if (!isRecord(op.block) || op.block.id == null || String(op.block.id) === '') return next;
    const block = jsonClone(op.block);
    const blockId = String(block.id);
    const index = next.findIndex((item) => isRecord(item) && String(item.id ?? '') === blockId);
    if (index >= 0) next[index] = block;
    else next.push(block);
    return next;
  }

  if (op.type === 'delete_block') {
    const blockId = String(op.block_id ?? '').trim();
    return blockId
      ? next.filter((block) => !(isRecord(block) && String(block.id ?? '') === blockId))
      : next;
  }

  return next;
}

export class CanvasReducer {
  constructor(canvas = {}) {
    this.reset(canvas);
  }

  reset(canvas = {}) {
    this.canvasId = String(canvas.id ?? '');
    this.title = String(canvas.title ?? '');
    this.version = Number.isSafeInteger(Number(canvas.version)) ? Number(canvas.version) : 0;
    this.blocks = asBlocks(canvas.blocks);
    const snapshotOpSeq = Number(canvas.snapshot_op_seq);
    this.snapshotOpSeq = Number.isSafeInteger(snapshotOpSeq) && snapshotOpSeq >= 0
      ? snapshotOpSeq
      : 0;
    // `blocks` already materializes every op through this durable baseline.
    // Starting the cursor here prevents a reconnect from reducing those ops a
    // second time (which is destructive for non-idempotent operations).
    this.cursor = this.snapshotOpSeq;
    this.pending = new Map();
    this.appliedIds = new Map();
    this.unknownOps = 0;
    return this.snapshot();
  }

  snapshot() {
    return {
      canvas_id: this.canvasId,
      title: this.title,
      version: this.version,
      blocks: asBlocks(this.blocks),
      snapshot_op_seq: this.snapshotOpSeq,
      op_seq: this.cursor,
      pending: [...this.pending.keys()].sort((a, b) => a - b),
      unknown_ops: this.unknownOps,
    };
  }

  ingest(raw) {
    const normalized = normalizeCanvasOp(raw);
    if (!normalized || (this.canvasId && normalized.canvas_id !== this.canvasId)) {
      return { status: 'invalid', applied: [], gap: this.pending.size > 0 };
    }

    const knownId = this.appliedIds.get(normalized.seq);
    if (normalized.seq <= this.cursor) {
      return {
        status: knownId && knownId !== normalized.id ? 'conflict' : 'duplicate',
        applied: [],
        gap: this.pending.size > 0,
      };
    }

    const queued = this.pending.get(normalized.seq);
    if (queued) {
      return {
        status: queued.id === normalized.id ? 'duplicate' : 'conflict',
        applied: [],
        gap: true,
      };
    }

    if (normalized.seq > this.cursor + 1) {
      this.pending.set(normalized.seq, normalized);
      return { status: 'queued', applied: [], gap: true };
    }

    const applied = [];
    this.applyOne(normalized);
    applied.push(normalized);
    while (this.pending.has(this.cursor + 1)) {
      const next = this.pending.get(this.cursor + 1);
      this.pending.delete(this.cursor + 1);
      this.applyOne(next);
      applied.push(next);
    }
    return { status: 'applied', applied, gap: this.pending.size > 0 };
  }

  ingestMany(rows) {
    const normalized = (Array.isArray(rows) ? rows : [])
      .map(normalizeCanvasOp)
      .filter(Boolean)
      .sort((a, b) => a.seq - b.seq);
    const results = normalized.map((row) => this.ingest(row));
    return {
      results,
      conflict: results.some((result) => result.status === 'conflict'),
      gap: this.pending.size > 0,
      applied: results.flatMap((result) => result.applied),
    };
  }

  applyOne(row) {
    this.blocks = reduceCanvasBlocks(this.blocks, row.op);
    this.cursor = row.seq;
    this.appliedIds.set(row.seq, row.id);
    if (!isSupportedCanvasOp(row.op)) this.unknownOps += 1;
  }
}

export function bindCanvasRealtime(wsClient, { onCanvasOp, onReconnect } = {}) {
  const offCanvas = wsClient.on('msg:canvas_op', (frame) => onCanvasOp?.(frame));
  const offOpen = wsClient.on('open', () => onReconnect?.());
  return () => {
    offCanvas?.();
    offOpen?.();
  };
}
