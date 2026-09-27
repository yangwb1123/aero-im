export function createCanvasClientOpId(): string

export interface CanvasSubmissionApi {
  appendCanvasOp(
    roomId: string,
    canvasId: string,
    op: Record<string, unknown>,
    clientOpId: string,
  ): Promise<CanvasOperation>
}

export interface CanvasOperation {
  id: string
  canvas_id: string
  seq: number
  author_id: string
  op: Record<string, unknown>
}

export function appendCanvasOperationWithRetry(
  api: CanvasSubmissionApi,
  roomId: string,
  canvasId: string,
  op: Record<string, unknown>,
  clientOpId?: string,
): Promise<{ clientOpId: string; operation: CanvasOperation }>
