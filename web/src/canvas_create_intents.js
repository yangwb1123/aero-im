const UUID_V7 = /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i

export function canvasCreateIntentStorageKey(participantId, roomId) {
  return `aero_canvas_create_intent:${encodeURIComponent(participantId)}:${encodeURIComponent(roomId)}`
}

function isIntent(value) {
  return value !== null
    && typeof value === 'object'
    && typeof value.title === 'string'
    && value.title.length <= 1024
    && value.title.trim().length > 0
    && Array.from(value.title).length <= 512
    && typeof value.clientCreateId === 'string'
    && UUID_V7.test(value.clientCreateId)
}

export function loadCanvasCreateIntent(storage, participantId, roomId) {
  if (!storage) return null
  const key = canvasCreateIntentStorageKey(participantId, roomId)
  let raw
  try {
    raw = storage.getItem(key)
  } catch {
    return null
  }
  if (raw === null) return null
  try {
    const value = JSON.parse(raw)
    if (isIntent(value)) return { title: value.title, clientCreateId: value.clientCreateId }
  } catch {
    // Malformed local state is discarded below; never retry an unvalidated key.
  }
  try {
    storage.removeItem(key)
  } catch {
    // Storage can be disabled or quota-limited; the in-memory flow still works.
  }
  return null
}

export function saveCanvasCreateIntent(storage, participantId, roomId, intent) {
  if (!storage || !isIntent(intent)) return false
  const key = canvasCreateIntentStorageKey(participantId, roomId)
  try {
    storage.setItem(
      key,
      JSON.stringify({ title: intent.title, clientCreateId: intent.clientCreateId }),
    )
    return true
  } catch {
    // A retry may hit quota even though the original intent is still intact.
    // Treat that exact existing record as durable; never proceed on a mismatch.
    try {
      const current = JSON.parse(storage.getItem(key) || 'null')
      return isIntent(current)
        && current.title === intent.title
        && current.clientCreateId === intent.clientCreateId
    } catch {
      return false
    }
  }
}

export function clearCanvasCreateIntent(storage, participantId, roomId, expectedClientCreateId) {
  if (!storage) return false
  const key = canvasCreateIntentStorageKey(participantId, roomId)
  try {
    const current = storage.getItem(key)
    if (current === null) return false
    const value = JSON.parse(current)
    if (value?.clientCreateId !== expectedClientCreateId) return false
    storage.removeItem(key)
    return true
  } catch {
    return false
  }
}
