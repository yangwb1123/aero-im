export interface CanvasCreateIntent {
  title: string
  clientCreateId: string
}

export interface CanvasCreateIntentStorage {
  getItem(key: string): string | null
  setItem(key: string, value: string): void
  removeItem(key: string): void
}

export function canvasCreateIntentStorageKey(participantId: string, roomId: string): string
export function loadCanvasCreateIntent(
  storage: CanvasCreateIntentStorage | null,
  participantId: string,
  roomId: string,
): CanvasCreateIntent | null
export function saveCanvasCreateIntent(
  storage: CanvasCreateIntentStorage | null,
  participantId: string,
  roomId: string,
  intent: CanvasCreateIntent,
): boolean
export function clearCanvasCreateIntent(
  storage: CanvasCreateIntentStorage | null,
  participantId: string,
  roomId: string,
  expectedClientCreateId: string,
): boolean
