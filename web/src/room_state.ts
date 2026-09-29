import type { Message } from './api'

function terminalRank(message: Message): number {
  if (message.deleted_at) return 2
  if (message.recalled_at) return 1
  return 0
}

/** Prefer terminal state and higher message revisions over stale duplicate events. */
export function newestRoomMessage(existing: Message | undefined, incoming: Message): Message {
  if (!existing) return incoming
  const existingTerminal = terminalRank(existing)
  const incomingTerminal = terminalRank(incoming)
  if (existingTerminal !== incomingTerminal) {
    return existingTerminal > incomingTerminal ? existing : incoming
  }
  const existingVersion = existing.version
  const incomingVersion = incoming.version
  if (typeof existingVersion === 'number' && Number.isInteger(existingVersion)
    && (typeof incomingVersion !== 'number' || !Number.isInteger(incomingVersion)
      || existingVersion > incomingVersion)) return existing
  return incoming
}

/** Merge history with room events without letting stale revisions undo newer state. */
export function mergeRoomMessages(base: Message[], updates: Message[]): Message[] {
  const merged = new Map<string, Message>()
  for (const message of base) merged.set(message.id, message)
  for (const message of updates) {
    merged.set(message.id, newestRoomMessage(merged.get(message.id), message))
  }
  return [...merged.values()].sort((a, b) =>
    (a.created_at ?? '').localeCompare(b.created_at ?? ''),
  )
}
