export const CANVAS_OP_PAGE_SIZE: number

export interface NormalizedCanvasOperation {
  id: string
  canvas_id: string
  seq: number
  author_id: string | null
  op: Record<string, unknown>
  created_at: string | null
}

export interface CanvasReducerSnapshot {
  canvas_id: string
  title: string
  version: number
  blocks: unknown[]
  snapshot_op_seq: number
  op_seq: number
  pending: number[]
  pending_unsequenced: number
  unknown_ops: number
}

export class CanvasReducer {
  constructor(canvas?: Record<string, unknown>)
  canvasId: string
  title: string
  version: number
  blocks: unknown[]
  snapshotOpSeq: number
  cursor: number
  pending: Map<number, NormalizedCanvasOperation>
  pendingUnsequenced: Map<string, unknown>
  reset(canvas?: Record<string, unknown>): CanvasReducerSnapshot
  snapshot(): CanvasReducerSnapshot
  ingest(raw: unknown): { status: string; applied: NormalizedCanvasOperation[]; gap: boolean }
  ingestMany(rows: unknown[]): {
    results: Array<{ status: string; applied: NormalizedCanvasOperation[]; gap: boolean }>
    conflict: boolean
    gap: boolean
    applied: NormalizedCanvasOperation[]
  }
}

export function normalizeCanvasOp(raw: unknown): NormalizedCanvasOperation | null
export function reduceCanvasBlocks(blocks: unknown[], op: Record<string, unknown>): unknown[]
export function isSupportedCanvasOp(op: unknown): boolean
export function bindCanvasRealtime(
  wsClient: WsClient,
  handlers?: { onCanvasOp?: (frame: Record<string, unknown>) => void; onReconnect?: () => void },
): () => void

import type { WsClient } from './ws.js'
