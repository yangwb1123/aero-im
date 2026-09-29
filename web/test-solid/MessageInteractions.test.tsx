// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { render } from 'solid-js/web'
import { ChatShell } from '../src/ChatShell'

const mockApi = vi.hoisted(() => ({
  listRooms: vi.fn(),
  listSessions: vi.fn(),
  revokeOtherSessions: vi.fn(),
  createRoom: vi.fn(),
  listMessages: vi.fn(),
  getMessage: vi.fn(),
  recallMessage: vi.fn(),
  deleteMessage: vi.fn(),
  toggleReaction: vi.fn(),
  reactionsBatch: vi.fn(),
  editMessage: vi.fn(),
  listCanvases: vi.fn(),
  createCanvas: vi.fn(),
  getCanvas: vi.fn(),
  listCanvasOps: vi.fn(),
  appendCanvasOp: vi.fn(),
  refresh: vi.fn(),
}))

vi.mock('../src/api', () => ({
  api: mockApi,
  messageText: (message: { blocks?: Array<{ type?: string; content?: string }> }) => (
    (message.blocks ?? []).filter((block) => block.type === 'text' || !block.type)
      .map((block) => block.content ?? '').join('')
  ),
  sessionStorage: {
    get token() { return 'test-access-token' },
    get refresh() { return null },
    get participantId() { return 'participant-1' },
    set() {},
    clear() {},
  },
}))

interface FakeEvent {
  data?: string
  code?: number
  reason?: string
}

class FakeWebSocket {
  static OPEN = 1
  static instances: FakeWebSocket[] = []
  readyState = 0
  readonly url: string
  readonly sent: string[] = []
  private listeners = new Map<string, Set<(event: FakeEvent) => void>>()

  constructor(url: string) {
    this.url = url
    FakeWebSocket.instances.push(this)
  }

  addEventListener(name: string, callback: (event: FakeEvent) => void): void {
    if (!this.listeners.has(name)) this.listeners.set(name, new Set())
    this.listeners.get(name)!.add(callback)
  }

  emit(name: string, event: FakeEvent = {}): void {
    for (const callback of this.listeners.get(name) ?? []) callback(event)
  }

  message(frame: Record<string, unknown>): void {
    this.emit('message', { data: JSON.stringify(frame) })
  }

  open(): void {
    this.readyState = FakeWebSocket.OPEN
    this.emit('open')
  }

  send(raw: string): void { this.sent.push(raw) }
  close(): void { this.readyState = 3 }
}

const participant = { id: 'participant-1', display_name: 'Ari' }
const roomA = { id: 'room-a', name: 'Room A', kind: 'group' }
const roomB = { id: 'room-b', name: 'Room B', kind: 'channel' }

function message(id: string, roomId: string, text: string, version = 1, senderId = participant.id) {
  return {
    id,
    room_id: roomId,
    sender_id: senderId,
    created_at: '2026-05-01T12:00:00Z',
    version,
    blocks: [{ type: 'text', content: text }],
  }
}

async function flushPromises(): Promise<void> {
  for (let index = 0; index < 12; index += 1) await Promise.resolve()
}

async function waitFor(check: () => boolean, label: string): Promise<void> {
  for (let index = 0; index < 100; index += 1) {
    await flushPromises()
    if (check()) return
  }
  throw new Error(`Timed out waiting for ${label}`)
}

function mountChat(): { container: HTMLElement; unmount: () => void } {
  const container = document.createElement('div')
  document.body.appendChild(container)
  const dispose = render(() => <ChatShell participant={participant} onLogout={async () => {}} />, container)
  return { container, unmount: () => { dispose(); container.remove() } }
}

function enterEdit(container: HTMLElement, messageId: string): HTMLTextAreaElement {
  const button = container.querySelector<HTMLButtonElement>(`[aria-label="编辑消息 ${messageId}"]`)
  if (!button) throw new Error(`Missing edit action for ${messageId}`)
  button.click()
  const editor = container.querySelector<HTMLTextAreaElement>(`[aria-label="编辑消息内容 ${messageId}"]`)
  if (!editor) throw new Error(`Missing editor for ${messageId}`)
  return editor
}

function setInput(editor: HTMLTextAreaElement, value: string): void {
  editor.value = value
  editor.dispatchEvent(new Event('input', { bubbles: true }))
}

function createMemoryStorage(): Storage {
  const values = new Map<string, string>()
  return {
    get length() { return values.size },
    clear() { values.clear() },
    getItem(key: string) { return values.get(String(key)) ?? null },
    key(index: number) { return [...values.keys()][index] ?? null },
    removeItem(key: string) { values.delete(String(key)) },
    setItem(key: string, value: string) { values.set(String(key), String(value)) },
  }
}

function pendingEntryKey(participantId: string, clientMessageId: string): string {
  return `aero_pending_delivery_v1:${participantId}:item:${clientMessageId}`
}

function storedPendingEntry(storage: Storage, participantId: string): {
  key: string
  item: Record<string, unknown>
} | null {
  const prefix = `aero_pending_delivery_v1:${participantId}:item:`
  for (let index = 0; index < storage.length; index += 1) {
    const key = storage.key(index)
    if (!key?.startsWith(prefix)) continue
    try {
      const record = JSON.parse(storage.getItem(key) ?? 'null') as { item?: unknown } | null
      if (record?.item && typeof record.item === 'object') {
        return { key, item: record.item as Record<string, unknown> }
      }
    } catch { /* skip malformed test storage entries */ }
  }
  return null
}

describe('Solid message actions', () => {
  let previousWebSocket: typeof WebSocket | undefined
  let previousConfirm: typeof window.confirm
  let previousLocalStorage: PropertyDescriptor | undefined
  let previousWindowLocalStorage: PropertyDescriptor | undefined
  let testLocalStorage: Storage
  let mounted: ReturnType<typeof mountChat> | undefined

  beforeEach(() => {
    vi.clearAllMocks()
    previousWebSocket = globalThis.WebSocket
    previousConfirm = window.confirm
    previousLocalStorage = Object.getOwnPropertyDescriptor(globalThis, 'localStorage')
    previousWindowLocalStorage = Object.getOwnPropertyDescriptor(window, 'localStorage')
    testLocalStorage = createMemoryStorage()
    Object.defineProperty(globalThis, 'localStorage', { configurable: true, value: testLocalStorage })
    Object.defineProperty(window, 'localStorage', { configurable: true, value: testLocalStorage })
    FakeWebSocket.instances = []
    globalThis.WebSocket = FakeWebSocket as unknown as typeof WebSocket
    mockApi.listRooms.mockResolvedValue([roomA])
    mockApi.listSessions.mockResolvedValue([])
    mockApi.revokeOtherSessions.mockResolvedValue({ revoked_count: 0 })
    mockApi.createRoom.mockResolvedValue(roomB)
    mockApi.listMessages.mockResolvedValue([])
    mockApi.getMessage.mockResolvedValue(message('message-1', 'room-a', 'Fresh server version', 2))
    mockApi.recallMessage.mockResolvedValue(message('message-1', 'room-a', 'Recalled'))
    mockApi.deleteMessage.mockResolvedValue(undefined)
    mockApi.toggleReaction.mockResolvedValue({ message_id: 'message-1', emoji: '👍', op: 'add' })
    mockApi.reactionsBatch.mockResolvedValue({})
    mockApi.editMessage.mockImplementation(async (
      id: string,
      blocks: Array<{ type: string; content: string }>,
      expectedVersion?: number,
    ) => ({
      ...message(id, 'room-a', blocks.map((block) => block.content).join(''), (expectedVersion ?? 1) + 1),
      edited_at: '2026-05-01T12:01:00Z',
    }))
    mockApi.listCanvases.mockResolvedValue([])
    mockApi.createCanvas.mockResolvedValue({})
    mockApi.getCanvas.mockResolvedValue({})
    mockApi.listCanvasOps.mockResolvedValue({ canvas_id: '', since: 0, ops: [] })
    mockApi.appendCanvasOp.mockResolvedValue({})
    mockApi.refresh.mockRejectedValue(new Error('not used'))
    testLocalStorage.setItem('aero_pid', participant.id)
  })

  afterEach(() => {
    mounted?.unmount()
    mounted = undefined
    if (previousWebSocket) globalThis.WebSocket = previousWebSocket
    else Reflect.deleteProperty(globalThis, 'WebSocket')
    window.confirm = previousConfirm
    testLocalStorage.clear()
    if (previousLocalStorage) Object.defineProperty(globalThis, 'localStorage', previousLocalStorage)
    else Reflect.deleteProperty(globalThis, 'localStorage')
    if (previousWindowLocalStorage) Object.defineProperty(window, 'localStorage', previousWindowLocalStorage)
    else Reflect.deleteProperty(window, 'localStorage')
  })

  it('shows an optimistic outgoing message and settles it by the exact ACK id', async () => {
    mockApi.listMessages.mockResolvedValue([])
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelector('.composer') !== null, 'composer')
    const socket = FakeWebSocket.instances[0]
    socket.open()
    socket.message({ type: 'welcome', participant: participant.id, capabilities: ['message_ack_v1'] })
    const composer = mounted.container.querySelector<HTMLFormElement>('.composer')!
    setInput(composer.querySelector<HTMLTextAreaElement>('textarea')!, 'Durable send')
    composer.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
    await waitFor(() => mounted!.container.querySelector('.message-delivery-status') !== null, 'optimistic delivery')
    const frame = socket.sent.map((raw) => JSON.parse(raw) as Record<string, unknown>)
      .find((sent) => sent.type === 'send_message')
    expect(frame?.client_message_id).toMatch(/^[0-9a-f-]{36}$/)
    expect(mounted.container.textContent).toContain('Durable send')
    expect(mounted.container.textContent).toContain('发送中')

    socket.message({
      type: 'message_ack',
      client_message_id: frame!.client_message_id,
      message: message('canonical-message', 'room-a', 'Durable send'),
    })
    await waitFor(() => mounted!.container.querySelector('.message-delivery-status') === null, 'canonical ACK')
    expect(mounted.container.textContent).toContain('Durable send')
    expect(mounted.container.querySelectorAll('.message-row')).toHaveLength(1)
  })

  it('settles a restored send when history already contains its client id', async () => {
    mockApi.listMessages.mockResolvedValue([])
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelector('.composer') !== null, 'composer')
    const composer = mounted.container.querySelector<HTMLFormElement>('.composer')!
    setInput(composer.querySelector<HTMLTextAreaElement>('textarea')!, 'Found in history')
    composer.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
    await waitFor(() => mounted!.container.textContent?.includes('已加入待发送队列') === true, 'offline queue')
    const saved = storedPendingEntry(testLocalStorage, participant.id)
    expect(saved).not.toBeNull()
    const clientMessageId = saved!.item.client_message_id as string
    mounted.unmount()
    mounted = undefined

    let resolveHistory: ((history: Array<ReturnType<typeof message>>) => void) | undefined
    mockApi.listMessages.mockImplementation(() => new Promise((resolve) => {
      resolveHistory = resolve
    }))
    mounted = mountChat()
    await waitFor(() => resolveHistory !== undefined, 'deferred history request')
    const socket = FakeWebSocket.instances[1]
    socket.open()
    socket.message({ type: 'welcome', participant: participant.id, capabilities: ['message_ack_v1'] })
    await waitFor(() => socket.sent.some((raw) => {
      const frame = JSON.parse(raw) as Record<string, unknown>
      return frame.type === 'send_message' && frame.client_message_id === clientMessageId
    }), 'restored outgoing message')

    resolveHistory?.([{ ...message('canonical-from-history', 'room-a', 'Found in history'), client_message_id: clientMessageId }])
    await waitFor(() => mounted!.container.querySelector('.message-delivery-status') === null, 'history reconciliation')
    expect(mounted.container.textContent).toContain('Found in history')
    expect(mounted.container.querySelectorAll('.message-row')).toHaveLength(1)
  })

  it('offers an explicit retry for a non-retryable NACK with the same client id', async () => {
    mockApi.listMessages.mockResolvedValue([])
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelector('.composer') !== null, 'composer')
    const socket = FakeWebSocket.instances[0]
    socket.open()
    socket.message({ type: 'welcome', participant: participant.id, capabilities: ['message_ack_v1'] })
    const composer = mounted.container.querySelector<HTMLFormElement>('.composer')!
    setInput(composer.querySelector<HTMLTextAreaElement>('textarea')!, 'Retry me')
    composer.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
    await waitFor(() => mounted!.container.querySelector('.message-delivery-status') !== null, 'outgoing message')
    const sendFrames = (): Array<Record<string, unknown>> => socket.sent
      .map((raw) => JSON.parse(raw) as Record<string, unknown>)
      .filter((frame) => frame.type === 'send_message')
    const originalId = sendFrames()[0]?.client_message_id
    expect(originalId).toBeTruthy()
    socket.message({ type: 'message_nack', client_message_id: originalId, msg: 'Rejected once', retryable: false })
    await waitFor(() => mounted!.container.querySelector('[data-testid="message-retry"]') !== null, 'failed send retry button')
    expect(mounted.container.textContent).toContain('Rejected once')
    const storedFailure = storedPendingEntry(testLocalStorage, participant.id)
    expect(storedFailure?.item.delivery_status).toBe('failed')

    mounted.unmount()
    mounted = undefined
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelector('.composer') !== null, 'remounted composer')
    const retrySocket = FakeWebSocket.instances[1]
    retrySocket.open()
    retrySocket.message({ type: 'welcome', participant: participant.id, capabilities: ['message_ack_v1'] })
    await waitFor(() => mounted!.container.querySelector('[data-testid="message-retry"]') !== null, 'restored failed row')
    const retryFrames = (): Array<Record<string, unknown>> => retrySocket.sent
      .map((raw) => JSON.parse(raw) as Record<string, unknown>)
      .filter((frame) => frame.type === 'send_message')
    expect(retryFrames()).toHaveLength(0)
    mounted.container.querySelector<HTMLButtonElement>('[data-testid="message-retry"]')!.click()
    await waitFor(() => retryFrames().length === 1, 'manual retry')
    expect(retryFrames()[0].client_message_id).toBe(originalId)
    retrySocket.message({
      type: 'message_ack',
      client_message_id: originalId,
      message: message('canonical-retried', 'room-a', 'Retry me'),
    })
    await waitFor(() => mounted!.container.querySelector('.message-delivery-status') === null, 'retried ACK')
  })

  it('warns visibly when the browser cannot persist an offline send', async () => {
    mockApi.listMessages.mockResolvedValue([])
    const setItem = testLocalStorage.setItem.bind(testLocalStorage)
    testLocalStorage.setItem = (key: string, value: string): void => {
      if (key.startsWith(`aero_pending_delivery_v1:${participant.id}:item:`)) {
        throw new Error('quota exceeded')
      }
      setItem(key, value)
    }
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelector('.composer') !== null, 'composer')
    const composer = mounted.container.querySelector<HTMLFormElement>('.composer')!
    setInput(composer.querySelector<HTMLTextAreaElement>('textarea')!, 'Not durable')
    composer.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
    await waitFor(() => mounted!.container.querySelector('.message-persistence-warning') !== null, 'durability warning')
    expect(mounted.container.textContent).toContain('刷新可能丢失此消息')
  })

  it('persists an offline send and retries the same client id after remount', async () => {
    mockApi.listMessages.mockResolvedValue([])
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelector('.composer') !== null, 'composer')
    const firstSocket = FakeWebSocket.instances[0]
    const composer = mounted.container.querySelector<HTMLFormElement>('.composer')!
    setInput(composer.querySelector<HTMLTextAreaElement>('textarea')!, 'Survive reconnect')
    composer.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
    await waitFor(() => mounted!.container.textContent?.includes('已加入待发送队列') === true, 'offline queue')
    const saved = storedPendingEntry(testLocalStorage, participant.id)
    expect(saved).not.toBeNull()
    const clientMessageId = saved!.item.client_message_id as string
    expect(firstSocket.sent.map((raw) => JSON.parse(raw).type)).not.toContain('send_message')

    mounted.unmount()
    mounted = undefined
    mounted = mountChat()
    await waitFor(() => FakeWebSocket.instances.length === 2, 'reconnected socket')
    const reconnectedSocket = FakeWebSocket.instances[1]
    reconnectedSocket.open()
    reconnectedSocket.message({ type: 'welcome', participant: participant.id, capabilities: ['message_ack_v1'] })
    await waitFor(() => reconnectedSocket.sent.some((raw) => {
      const sent = JSON.parse(raw) as Record<string, unknown>
      return sent.type === 'send_message' && sent.client_message_id === clientMessageId
    }), 'stable-id retry')
    expect(mounted.container.textContent).toContain('Survive reconnect')
    reconnectedSocket.message({
      type: 'message_ack',
      client_message_id: clientMessageId,
      message: message('canonical-restored', 'room-a', 'Survive reconnect'),
    })
    await waitFor(() => mounted!.container.querySelector('.message-delivery-status') === null, 'restored ACK')
    expect(testLocalStorage.getItem(pendingEntryKey(participant.id, clientMessageId))).toBeNull()
  })

  it('deletes only the author message after confirmation and renders the terminal placeholder', async () => {
    const own = message('message-delete-own', 'room-a', 'Delete me')
    const other = message('message-delete-other', 'room-a', 'Keep me', 1, 'participant-2')
    mockApi.listMessages.mockResolvedValue([own, other])
    mounted = mountChat()
    await waitFor(() => mounted!.container.textContent?.includes('Delete me') === true, 'history')
    const deleteButtons = mounted.container.querySelectorAll<HTMLButtonElement>('[data-testid="message-delete"]')
    expect(deleteButtons).toHaveLength(1)

    window.confirm = vi.fn().mockReturnValueOnce(false).mockReturnValueOnce(true)
    deleteButtons[0].click()
    expect(mockApi.deleteMessage).not.toHaveBeenCalled()
    deleteButtons[0].click()
    await waitFor(() => mounted!.container.textContent?.includes('消息已删除') === true, 'delete result')
    expect(mockApi.deleteMessage).toHaveBeenCalledWith(own.id)
    expect(mounted.container.textContent).not.toContain('Delete me')
    expect(mounted.container.textContent).toContain('Keep me')
    expect(mounted.container.querySelector('[aria-label="编辑消息 message-delete-own"]')).toBeNull()
    expect(mounted.container.querySelector('[aria-label="撤回消息 message-delete-own"]')).toBeNull()
    expect(mounted.container.querySelector('[aria-label="删除消息 message-delete-own"]')).toBeNull()
  })

  it('keeps a deleted message terminal when a delayed edit event arrives', async () => {
    const own = message('message-delete-event', 'room-a', 'Secret')
    mockApi.listMessages.mockResolvedValue([own])
    mounted = mountChat()
    await waitFor(() => mounted!.container.textContent?.includes('Secret') === true, 'history')
    const socket = FakeWebSocket.instances[0]
    socket.message({ type: 'deleted', room_id: 'room-a', id: own.id })
    await waitFor(() => mounted!.container.textContent?.includes('消息已删除') === true, 'delete event')
    socket.message({
      type: 'edited',
      event: { ...own, version: 2, blocks: [{ type: 'text', content: 'Stale content' }] },
    })
    await flushPromises()
    expect(mounted.container.textContent).not.toContain('Secret')
    expect(mounted.container.textContent).not.toContain('Stale content')
    expect(mounted.container.textContent).toContain('消息已删除')
  })

  it('reconciles a lost delete response as deleted when the message endpoint returns 404', async () => {
    const own = message('message-delete-404', 'room-a', 'Possibly deleted')
    mockApi.listMessages.mockResolvedValue([own])
    mockApi.deleteMessage.mockRejectedValueOnce(Object.assign(new Error('response lost'), { status: 0 }))
    mockApi.getMessage.mockRejectedValueOnce(Object.assign(new Error('not found'), { status: 404 }))
    mounted = mountChat()
    await waitFor(() => mounted!.container.textContent?.includes('Possibly deleted') === true, 'history')
    window.confirm = vi.fn(() => true)
    mounted.container.querySelector<HTMLButtonElement>('[data-testid="message-delete"]')!.click()
    await waitFor(() => mounted!.container.textContent?.includes('消息已删除') === true, '404 reconciliation')
    expect(mockApi.deleteMessage).toHaveBeenCalledTimes(1)
    expect(mounted.container.querySelector('.message-delete-feedback')).toBeNull()
  })

  it('keeps the message visible after a failed delete and allows a deliberate retry', async () => {
    const own = message('message-delete-retry', 'room-a', 'Still here')
    mockApi.listMessages.mockResolvedValue([own])
    mockApi.deleteMessage.mockRejectedValueOnce(Object.assign(new Error('network down'), { status: 0 }))
    mockApi.getMessage.mockResolvedValueOnce(own)
    mounted = mountChat()
    await waitFor(() => mounted!.container.textContent?.includes('Still here') === true, 'history')
    window.confirm = vi.fn(() => true)
    mounted.container.querySelector<HTMLButtonElement>('[data-testid="message-delete"]')!.click()
    await waitFor(() => mounted!.container.querySelector('.message-delete-feedback') !== null, 'delete failure')
    expect(mounted.container.textContent).toContain('Still here')
    expect(mounted.container.textContent).toContain('删除尚未完成')

    mounted.container.querySelector<HTMLButtonElement>('[data-testid="message-delete"]')!.click()
    await waitFor(() => mounted!.container.textContent?.includes('消息已删除') === true, 'delete retry')
    expect(mockApi.deleteMessage).toHaveBeenCalledTimes(2)
  })

  it('exposes editing only for own plain-text messages and saves with the observed version', async () => {
    const own = message('message-own', 'room-a', 'Before')
    const other = message('message-other', 'room-a', 'Other member', 1, 'participant-2')
    const rich = { ...message('message-rich', 'room-a', ''), blocks: [{ type: 'image', content: 'blob' }] }
    const formatted = {
      ...message('message-formatted', 'room-a', 'Styled'),
      blocks: [{ type: 'text', content: 'Styled', spans: [{ start: 0, end: 6, style: 'bold' }] }],
    }
    mockApi.listMessages.mockResolvedValue([own, other, rich, formatted])
    mounted = mountChat()
    await waitFor(() => mounted!.container.textContent?.includes('Before') === true, 'history')
    expect(mounted.container.querySelectorAll('[data-testid="message-edit"]')).toHaveLength(1)

    const editor = enterEdit(mounted.container, own.id)
    expect(editor.value).toBe('Before')
    setInput(editor, 'After')
    mounted.container.querySelector<HTMLFormElement>('.message-edit-form')!
      .dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
    await waitFor(() => mounted!.container.textContent?.includes('After') === true, 'saved edit')
    expect(mockApi.editMessage).toHaveBeenCalledWith(
      own.id,
      [{ type: 'text', content: 'After' }],
      1,
    )
    expect(mounted.container.textContent).not.toContain('Before')
    expect(mounted.container.querySelectorAll('[data-testid="message-edit"]')).toHaveLength(1)
  })

  it('preserves an edit draft on version conflict and requires an explicit resolution', async () => {
    const own = message('message-conflict', 'room-a', 'Original', 1)
    const latest = message('message-conflict', 'room-a', 'Remote edit', 2)
    mockApi.listMessages.mockResolvedValue([own])
    mockApi.editMessage.mockRejectedValueOnce(Object.assign(
      new Error('conflict: message version mismatch'),
      { status: 409 },
    ))
    mockApi.getMessage.mockResolvedValueOnce(latest)
    mounted = mountChat()
    await waitFor(() => mounted!.container.textContent?.includes('Original') === true, 'history')
    const editor = enterEdit(mounted.container, own.id)
    setInput(editor, 'My draft')
    mounted.container.querySelector<HTMLFormElement>('.message-edit-form')!
      .dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
    await waitFor(() => mounted!.container.querySelector('.message-edit-conflict') !== null, 'conflict resolution')
    expect(mounted.container.querySelector<HTMLTextAreaElement>(`[aria-label="编辑消息内容 ${own.id}"]`)?.value)
      .toBe('My draft')
    expect(mounted.container.querySelector('.message-edit-conflict')?.textContent).toContain('Remote edit')
    expect(mockApi.editMessage).toHaveBeenCalledTimes(1)

    window.confirm = vi.fn(() => true)
    const override = [...mounted.container.querySelectorAll<HTMLButtonElement>('.message-edit-conflict button')]
      .find((button) => button.textContent?.includes('覆盖最新版本'))
    override!.click()
    mounted.container.querySelector<HTMLFormElement>('.message-edit-form')!
      .dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
    await waitFor(() => mounted!.container.querySelector('.message-edit-form') === null, 'explicitly rebased edit')
    expect(mockApi.editMessage.mock.calls[1][2]).toBe(2)
    expect(mounted.container.textContent).toContain('My draft')
  })

  it('reconciles a lost edit response when the server already has the submitted revision', async () => {
    const own = message('message-uncertain', 'room-a', 'Before', 1)
    const committed = message('message-uncertain', 'room-a', 'Committed draft', 2)
    mockApi.listMessages.mockResolvedValue([own])
    mockApi.editMessage.mockRejectedValueOnce(Object.assign(new Error('response lost'), { status: 0 }))
    mockApi.getMessage.mockResolvedValueOnce(committed)
    mounted = mountChat()
    await waitFor(() => mounted!.container.textContent?.includes('Before') === true, 'history')
    const editor = enterEdit(mounted.container, own.id)
    setInput(editor, 'Committed draft')
    mounted.container.querySelector<HTMLFormElement>('.message-edit-form')!
      .dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
    await waitFor(() => mounted!.container.querySelector('.message-edit-form') === null, 'lost-response reconciliation')
    expect(mockApi.editMessage).toHaveBeenCalledTimes(1)
    expect(mockApi.getMessage).toHaveBeenCalledWith(own.id)
    expect(mounted.container.textContent).toContain('Committed draft')
    expect(mounted.container.querySelector('.message-edit-conflict')).toBeNull()
  })

  it('keeps a newer message revision when an older edit event arrives late', async () => {
    const original = message('message-revisions', 'room-a', 'Revision one', 1)
    mockApi.listMessages.mockResolvedValue([original])
    mounted = mountChat()
    await waitFor(() => mounted!.container.textContent?.includes('Revision one') === true, 'history')
    const socket = FakeWebSocket.instances[0]
    socket.message({
      type: 'edited',
      event: { ...original, version: 3, blocks: [{ type: 'text', content: 'Revision three' }] },
    })
    await waitFor(() => mounted!.container.textContent?.includes('Revision three') === true, 'newer revision')
    socket.message({
      type: 'edited',
      event: { ...original, version: 2, blocks: [{ type: 'text', content: 'Revision two' }] },
    })
    await flushPromises()
    expect(mounted.container.textContent).toContain('Revision three')
    expect(mounted.container.textContent).not.toContain('Revision two')
  })

  it('does not show a late Room A edit failure in Room B', async () => {
    mockApi.listRooms.mockResolvedValue([roomA, roomB])
    const own = message('message-late-edit', 'room-a', 'Before', 1)
    mockApi.listMessages.mockImplementation(async (roomId: string) => (
      roomId === 'room-a' ? [own] : []
    ))
    mockApi.getMessage.mockResolvedValue(message('message-late-edit', 'room-a', 'Remote', 2))
    let rejectEdit!: (reason: unknown) => void
    mockApi.editMessage.mockImplementationOnce(() => new Promise((_resolve, reject) => {
      rejectEdit = reject
    }))
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelectorAll('.room-item').length === 2, 'rooms')
    const editor = enterEdit(mounted.container, own.id)
    setInput(editor, 'Local draft')
    mounted.container.querySelector<HTMLFormElement>('.message-edit-form')!
      .dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
    await waitFor(() => mockApi.editMessage.mock.calls.length === 1, 'pending edit')
    window.confirm = vi.fn(() => true)
    mounted.container.querySelectorAll<HTMLButtonElement>('.room-item')[1].click()
    await waitFor(() => mounted!.container.querySelector('h1')?.textContent?.includes('Room B') === true, 'Room B')
    rejectEdit(Object.assign(new Error('conflict: message version mismatch'), { status: 409 }))
    await flushPromises()
    expect(mounted.container.querySelector('h1')?.textContent).toContain('Room B')
    expect(mounted.container.querySelector('.message-edit-error')).toBeNull()
    expect(mounted.container.textContent).not.toContain('Local draft')
  })

  it('hydrates reaction summaries and refreshes after an authorized toggle', async () => {
    const own = message('message-reaction', 'room-a', 'React to this')
    const first = { emoji: '👍', count: 2, participants: [participant.id, 'participant-2'] }
    const afterToggle = { emoji: '👍', count: 1, participants: ['participant-2'] }
    mockApi.listMessages.mockResolvedValue([own])
    mockApi.reactionsBatch
      .mockResolvedValueOnce({ [own.id]: [first] })
      .mockResolvedValueOnce({ [own.id]: [afterToggle] })
    mockApi.toggleReaction.mockResolvedValue({ message_id: own.id, emoji: '👍', op: 'remove' })
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelector('.reaction-pill') !== null, 'reaction summary')
    const pill = mounted.container.querySelector<HTMLButtonElement>('.reaction-pill')!
    expect(pill.textContent).toContain('2')
    expect(pill.getAttribute('aria-pressed')).toBe('true')
    pill.click()
    await waitFor(() => mockApi.toggleReaction.mock.calls.length === 1, 'reaction toggle')
    await waitFor(() => mounted!.container.querySelector('.reaction-pill')?.textContent?.includes('1') === true,
      'reconciled reaction count')
    expect(mockApi.toggleReaction).toHaveBeenCalledWith(own.id, '👍')
    expect(mounted.container.querySelector('.reaction-pill')?.getAttribute('aria-pressed')).toBe('false')
  })

  it('reconciles an uncertain reaction result without blindly toggling twice', async () => {
    const own = message('message-reaction-uncertain', 'room-a', 'Reaction response may be lost')
    const committed = { emoji: '🎉', count: 1, participants: [participant.id] }
    mockApi.listMessages.mockResolvedValue([own])
    mockApi.reactionsBatch
      .mockResolvedValueOnce({})
      .mockResolvedValueOnce({ [own.id]: [committed] })
    mockApi.toggleReaction.mockRejectedValueOnce(Object.assign(new Error('response lost'), { status: 0 }))
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelector('.reaction-picker') !== null, 'reaction picker')
    const picker = mounted.container.querySelector<HTMLDetailsElement>('.reaction-picker')!
    picker.open = true
    picker.querySelector<HTMLButtonElement>('[aria-label="回应 🎉"]')!.click()
    await waitFor(() => mounted!.container.querySelector('.reaction-pill') !== null, 'reconciled reaction')
    expect(mockApi.toggleReaction).toHaveBeenCalledTimes(1)
    expect(mounted.container.textContent).toContain('请求结果不确定')
    expect(mounted.container.querySelector('.reaction-pill')?.textContent).toContain('1')
  })

  it('refreshes visible reactions when a room WebSocket reaction event arrives', async () => {
    const own = message('message-reaction-event', 'room-a', 'Live reaction')
    mockApi.listMessages.mockResolvedValue([own])
    mockApi.reactionsBatch
      .mockResolvedValueOnce({})
      .mockResolvedValueOnce({ [own.id]: [{ emoji: '👀', count: 1, participants: ['participant-2'] }] })
    mounted = mountChat()
    await waitFor(() => mounted!.container.textContent?.includes('Live reaction') === true, 'history')
    FakeWebSocket.instances[0].message({
      type: 'reaction',
      room_id: 'room-a',
      message_id: own.id,
      participant: 'participant-2',
      emoji: '👀',
      op: 'add',
    })
    await waitFor(() => mounted!.container.querySelector('.reaction-pill') !== null, 'live summary refresh')
    expect(mounted.container.querySelector('.reaction-pill')?.textContent).toContain('👀')
    expect(mockApi.reactionsBatch).toHaveBeenCalledTimes(2)
  })

  it('sends the selected reply target with the outgoing WebSocket message', async () => {
    const parent = message('message-reply-parent', 'room-a', 'Parent context')
    mockApi.listMessages.mockResolvedValue([parent])
    mounted = mountChat()
    await waitFor(() => mounted!.container.textContent?.includes('Parent context') === true, 'history')
    mounted.container.querySelector<HTMLButtonElement>(`[aria-label="回复消息 ${parent.id}"]`)!.click()
    expect(mounted.container.querySelector('.composer-reply-context')?.textContent).toContain('Parent context')
    const socket = FakeWebSocket.instances[0]
    socket.open()
    const composer = mounted.container.querySelector<HTMLFormElement>('.composer')!
    const input = composer.querySelector<HTMLTextAreaElement>('textarea')!
    setInput(input, 'A reply')
    composer.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
    const sent = socket.sent.map((raw) => JSON.parse(raw) as Record<string, unknown>)
      .find((frame) => frame.type === 'send_message')
    expect(sent).toMatchObject({
      room_id: 'room-a',
      reply_to: parent.id,
      blocks: [{ type: 'text', content: 'A reply' }],
    })
    expect(mounted.container.querySelector('.composer-reply-context')).toBeNull()
  })

  it('updates reply previews when the referenced message is recalled', async () => {
    const parent = message('message-reply-parent-terminal', 'room-a', 'Original secret')
    const child = { ...message('message-reply-child', 'room-a', 'A response'), reply_to: parent.id }
    mockApi.listMessages.mockResolvedValue([parent, child])
    mounted = mountChat()
    await waitFor(() => mounted!.container.textContent?.includes('Original secret') === true, 'reply preview')
    FakeWebSocket.instances[0].message({
      type: 'recalled',
      message: {
        ...parent,
        recalled_at: '2026-05-01T12:02:00Z',
        blocks: [{ type: 'text', content: '[此消息已被撤回]' }],
      },
    })
    await waitFor(() => mounted!.container.textContent?.includes('原消息已撤回') === true, 'recalled reply target')
    expect(mounted.container.textContent).not.toContain('Original secret')
  })

  it('requires confirmation before dropping a reply context on room navigation', async () => {
    mockApi.listRooms.mockResolvedValue([roomA, roomB])
    const parent = message('message-reply-navigation', 'room-a', 'Reply context')
    mockApi.listMessages.mockImplementation(async (roomId: string) => (
      roomId === 'room-a' ? [parent] : []
    ))
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelectorAll('.room-item').length === 2, 'rooms')
    mounted.container.querySelector<HTMLButtonElement>(`[aria-label="回复消息 ${parent.id}"]`)!.click()
    window.confirm = vi.fn(() => false)
    mounted.container.querySelectorAll<HTMLButtonElement>('.room-item')[1].click()
    expect(mounted.container.querySelector('h1')?.textContent).toContain('Room A')
    expect(mounted.container.querySelector('.composer-reply-context')?.textContent).toContain('Reply context')
    window.confirm = vi.fn(() => true)
    mounted.container.querySelectorAll<HTMLButtonElement>('.room-item')[1].click()
    await waitFor(() => mounted!.container.querySelector('h1')?.textContent?.includes('Room B') === true, 'room switch')
    expect(mounted.container.querySelector('.composer-reply-context')).toBeNull()
  })

  it('keeps the edit draft when room navigation is canceled', async () => {
    mockApi.listRooms.mockResolvedValue([roomA, roomB])
    const own = message('message-navigation', 'room-a', 'Before', 1)
    mockApi.listMessages.mockImplementation(async (roomId: string) => (
      roomId === 'room-a' ? [own] : []
    ))
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelectorAll('.room-item').length === 2, 'rooms')
    const editor = enterEdit(mounted.container, own.id)
    setInput(editor, 'Unsent draft')
    window.confirm = vi.fn(() => false)
    mounted.container.querySelectorAll<HTMLButtonElement>('.room-item')[1].click()
    expect(mounted.container.querySelector<HTMLTextAreaElement>(`[aria-label="编辑消息内容 ${own.id}"]`)?.value)
      .toBe('Unsent draft')
    expect(mounted.container.querySelector('h1')?.textContent).toContain('Room A')

    window.confirm = vi.fn(() => true)
    mounted.container.querySelectorAll<HTMLButtonElement>('.room-item')[1].click()
    await waitFor(() => mounted!.container.querySelector('h1')?.textContent?.includes('Room B') === true, 'confirmed room switch')
    expect(mounted.container.querySelector('.message-edit-form')).toBeNull()
  })
})
