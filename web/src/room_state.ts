import type { Message } from './api'

/** Merge a page of history with newer in-memory room events. `updates` wins
 * duplicate IDs so live edits received while history is in flight are retained. */
export function mergeRoomMessages(base: Message[], updates: Message[]): Message[] {
  const merged = new Map<string, Message>()
  for (const message of base) merged.set(message.id, message)
  for (const message of updates) merged.set(message.id, message)
  return [...merged.values()].sort((a, b) =>
    (a.created_at ?? '').localeCompare(b.created_at ?? ''),
  )
}
