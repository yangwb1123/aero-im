export function createCanvasClientOpId() {
  if (globalThis.crypto?.randomUUID) return globalThis.crypto.randomUUID();
  return 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, (digit) => {
    const value = Math.floor(Math.random() * 16);
    return (digit === 'x' ? value : (value & 0x3) | 0x8).toString(16);
  });
}

/** Append one logical edit. A transport-uncertain retry reuses its stable key. */
export async function appendCanvasOperationWithRetry(
  api,
  roomId,
  canvasId,
  op,
  clientOpId = createCanvasClientOpId(),
) {
  let operation;
  try {
    operation = await api.appendCanvasOp(roomId, canvasId, op, clientOpId);
  } catch (error) {
    if (error?.status !== 0) throw error;
    operation = await api.appendCanvasOp(roomId, canvasId, op, clientOpId);
  }
  return { clientOpId, operation };
}
