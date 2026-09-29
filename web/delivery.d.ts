import type { Message, MessageBlock } from './src/api'
import type { WsClient } from './ws'

export const MESSAGE_ACK_CAPABILITY: string

export type PendingOutbound =
  | { kind: 'blocks'; roomId: string; blocks: MessageBlock[]; replyTo?: string | null }
  | {
      kind: 'markdown'
      roomId: string
      markdown: string
      replyTo?: string | null
      expiresAfterSecs?: number | null
    }

export interface PendingDelivery extends Message {
  client_message_id: string
  outbound: PendingOutbound | null
  attempts: number
  last_connection_id?: number | string
  delivery_status: 'sending' | 'waiting' | 'retrying' | 'failed'
  retryable: boolean
  failure_message?: string | null
  persistence_warning?: boolean
  _ackTimer?: ReturnType<typeof setTimeout> | null
}

export function pendingTempId(clientMessageId: string): string
export function clearPendingDelivery(pending?: PendingDelivery | null): void
export function discardPersistedDeliveries(participantId?: string | null): void
export function findPendingMatch(
  serverMessage: Message,
  pending: Map<string, PendingDelivery>,
  participantId?: string | null,
): string | null
export function initReliableDelivery(options: {
  ws: WsClient
  getPendingMap: () => Map<string, PendingDelivery>
  onCanonical: (message: Message, clientMessageId: string) => void
  onRestore?: (pending: PendingDelivery) => PendingDelivery | null
  onPendingChanged?: () => void
  onFailure?: (message: string) => void
  onNotice?: (message: string, kind?: string) => void
}): () => void
export function retryPendingMessage(clientMessageId: string): boolean
export function sendOptimistically(
  sendFrame: (clientMessageId: string) => boolean,
  addPending: (delivery: Pick<PendingDelivery, 'client_message_id' | 'attempts'
    | 'last_connection_id' | 'delivery_status' | 'retryable' | 'failure_message'>
    & { outbound: PendingOutbound | null }) => PendingDelivery | null | undefined,
  outbound?: PendingOutbound | null,
): boolean
