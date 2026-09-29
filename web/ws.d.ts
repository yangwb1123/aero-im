export class WsClient {
  constructor(options?: { cursorStorage?: Storage | null })
  readonly ws: WebSocket | null
  token: string | null
  connectionId: number
  connect(
    token: string,
    participantId?: string | null,
    options?: { refreshAccessToken?: (() => Promise<string>) | null },
  ): void
  on(event: string, callback: (...args: any[]) => unknown): () => void
  send(frame: Record<string, unknown>): boolean
  sendMessage(roomId: string, blocks: unknown[], replyTo?: string | null, clientMessageId?: string | null): boolean
  sendMarkdown(
    roomId: string,
    markdown: string,
    replyTo?: string | null,
    expiresAfterSecs?: number | null,
    clientMessageId?: string | null,
  ): boolean
  joinRoom(roomId: string): boolean
  close(): void
  supports(capability: string): boolean
  pauseDeliveryAcks(): (success: boolean) => void
}

export function accessTokenNeedsRefresh(token: string | null, now?: number): boolean
export function reconnectDelay(attempts: number, closeCode?: number | null): number
