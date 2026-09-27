// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { render } from 'solid-js/web'
import { ChatShell } from '../src/ChatShell'

const mockApi = vi.hoisted(() => ({
  listRooms: vi.fn(),
  createRoom: vi.fn(),
  listMessages: vi.fn(),
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
    (message.blocks ?? []).map((block) => block.content ?? '').join('')
  ),
  sessionStorage: {
    get token() { return 'test-access-token' },
    get refresh() { return null },
    get participantId() { return 'participant-1' },
    set() {},
    clear() {},
  },
}))

class FakeWebSocket {
  static OPEN = 1
  static instances: FakeWebSocket[] = []
  readyState = 0
  readonly url: string
  readonly sent: Array<Record<string, unknown>> = []
  readonly closed: Array<{ code: number; reason: string }> = []
  private listeners = new Map<string, Set<(event: any) => void>>()

  constructor(url: string) {
    this.url = url
    FakeWebSocket.instances.push(this)
  }

  addEventListener(event: string, callback: (event: any) => void): void {
    if (!this.listeners.has(event)) this.listeners.set(event, new Set())
    this.listeners.get(event)!.add(callback)
  }

  emit(event: string, data: unknown = {}): void {
    for (const callback of this.listeners.get(event) ?? []) callback(data)
  }

  open(): void {
    this.readyState = FakeWebSocket.OPEN
    this.emit('open')
  }

  message(frame: Record<string, unknown>): void {
    this.emit('message', { data: JSON.stringify(frame) })
  }

  send(raw: string): void {
    this.sent.push(JSON.parse(raw) as Record<string, unknown>)
  }

  close(code = 1000, reason = ''): void {
    this.closed.push({ code, reason })
    this.readyState = 3
  }
}

const participant = { id: 'participant-1', display_name: 'Ari', email: 'ari@example.test' }
const roomA = { id: 'room-a', name: 'Room A', kind: 'group' }
const roomB = { id: 'room-b', name: 'Room B', kind: 'channel' }

function canvas(id: string, title: string, content: string) {
  return {
    id,
    room_id: 'room-a',
    title,
    version: 1,
    snapshot_op_seq: 0,
    blocks: [{ type: 'text', content }],
  }
}

function message(id: string, roomId: string, text: string) {
  return {
    id,
    room_id: roomId,
    sender_id: participant.id,
    created_at: '2026-05-01T12:00:00Z',
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

function input(selector: string, value: string, root = document): void {
  const field = root.querySelector<HTMLInputElement | HTMLTextAreaElement>(selector)
  if (!field) throw new Error(`Missing input: ${selector}`)
  field.value = value
  field.dispatchEvent(new Event('input', { bubbles: true }))
}

function submit(selector: string, root = document): void {
  const form = root.querySelector<HTMLFormElement>(selector)
  if (!form) throw new Error(`Missing form: ${selector}`)
  form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
}

function mountChat(): { container: HTMLElement; unmount: () => void } {
  const container = document.createElement('div')
  document.body.appendChild(container)
  const dispose = render(() => <ChatShell participant={participant} onLogout={async () => {}} />, container)
  return {
    container,
    unmount: () => {
      dispose()
      container.remove()
    },
  }
}

describe('ChatShell collaborative Canvas integration', () => {
  let previousWebSocket: typeof WebSocket | undefined
  let mounted: { container: HTMLElement; unmount: () => void } | undefined

  beforeEach(() => {
    vi.clearAllMocks()
    previousWebSocket = globalThis.WebSocket
    FakeWebSocket.instances = []
    globalThis.WebSocket = FakeWebSocket as unknown as typeof WebSocket
    mockApi.listRooms.mockResolvedValue([roomA])
    mockApi.listMessages.mockResolvedValue([])
    mockApi.listCanvases.mockResolvedValue([canvas('canvas-1', 'Planning', 'Initial text')])
    mockApi.createCanvas.mockResolvedValue(canvas('canvas-2', 'New board', 'New text'))
    mockApi.getCanvas.mockImplementation(async (_roomId: string, canvasId: string) => (
      canvasId === 'canvas-2'
        ? canvas('canvas-2', 'New board', 'New text')
        : canvas('canvas-1', 'Planning', 'Initial text')
    ))
    mockApi.listCanvasOps.mockResolvedValue({ canvas_id: 'canvas-1', since: 0, ops: [] })
    let nextSeq = 0
    mockApi.appendCanvasOp.mockImplementation(async (
      _roomId: string,
      canvasId: string,
      op: Record<string, unknown>,
      clientOpId: string,
    ) => ({
      id: `op-${++nextSeq}`,
      client_op_id: clientOpId,
      canvas_id: canvasId,
      seq: nextSeq,
      author_id: participant.id,
      op,
    }))
    mockApi.refresh.mockRejectedValue(new Error('not used'))
  })

  afterEach(() => {
    mounted?.unmount()
    mounted = undefined
    vi.useRealTimers()
    if (previousWebSocket) globalThis.WebSocket = previousWebSocket
    else Reflect.deleteProperty(globalThis, 'WebSocket')
  })

  it('opens, creates and selects room canvases, applies live/reconnected ops, and retains failed edits', async () => {
    vi.useFakeTimers()
    mounted = mountChat()
    await waitFor(() => FakeWebSocket.instances.length === 1, 'the room WebSocket')
    const socket = FakeWebSocket.instances[0]
    socket.open()
    await waitFor(() => mounted!.container.querySelector('.room-item') !== null, 'room list')
    await waitFor(() => mockApi.listMessages.mock.calls.length === 1, 'room history')

    expect(mockApi.listCanvases).not.toHaveBeenCalled()
    mounted.container.querySelector<HTMLButtonElement>('.canvas-toggle')!.click()
    await waitFor(() => mockApi.listCanvases.mock.calls.length === 1, 'Canvas list')
    await waitFor(() => mockApi.getCanvas.mock.calls.length === 1, 'selected Canvas snapshot')
    await waitFor(() => (
      mounted!.container.querySelector<HTMLTextAreaElement>('[aria-label="Canvas 正文"]')?.value
      === 'Initial text'
    ), 'initial Canvas content')
    expect(mockApi.listCanvases).toHaveBeenCalledWith('room-a')
    expect(mockApi.listCanvasOps).toHaveBeenCalledWith('room-a', 'canvas-1', {
      since: 0,
      limit: 500,
    })
    expect(FakeWebSocket.instances).toHaveLength(1)

    // A blank title is rejected visibly without issuing a create request.
    submit('.canvas-create-form', mounted.container)
    await waitFor(() => mounted!.container.querySelector('.canvas-error')?.textContent?.includes('标题'), 'blank title feedback')
    expect(mockApi.createCanvas).not.toHaveBeenCalled()

    input('[aria-label="新 Canvas 标题"]', 'New board', mounted.container)
    submit('.canvas-create-form', mounted.container)
    await waitFor(() => mockApi.createCanvas.mock.calls.length === 1, 'Canvas creation')
    await waitFor(() => (
      mounted!.container.querySelector<HTMLTextAreaElement>('[aria-label="Canvas 正文"]')?.value
      === 'New text'
    ), 'new Canvas snapshot')
    expect(mockApi.createCanvas).toHaveBeenCalledWith('room-a', {
      title: 'New board',
      blocks: [{ type: 'text', content: '' }],
    })

    const planningButton = [...mounted.container.querySelectorAll<HTMLButtonElement>('.canvas-list-item')]
      .find((button) => button.textContent?.includes('Planning'))
    expect(planningButton).toBeDefined()
    planningButton!.click()
    await waitFor(() => mockApi.getCanvas.mock.calls.length === 3, 'selecting an existing Canvas')
    await waitFor(() => (
      mounted!.container.querySelector<HTMLTextAreaElement>('[aria-label="Canvas 正文"]')?.value
      === 'Initial text'
    ), 'selected Canvas content')

    socket.message({
      type: 'canvas_op',
      room_id: 'room-a',
      canvas_id: 'canvas-1',
      op_id: 'remote-op-1',
      op_seq: 1,
      seq: 901,
      author_id: 'participant-2',
      op: { type: 'set_text', text: 'Live update' },
    })
    await waitFor(() => (
      mounted!.container.querySelector<HTMLTextAreaElement>('[aria-label="Canvas 正文"]')?.value
      === 'Live update'
    ), 'live Canvas op')

    mockApi.listCanvasOps.mockImplementation(async (
      _roomId: string,
      _canvasId: string,
      query: { since: number },
    ) => query.since === 1
      ? {
        canvas_id: 'canvas-1',
        since: 1,
        ops: [{
          id: 'remote-op-2',
          canvas_id: 'canvas-1',
          seq: 2,
          author_id: 'participant-2',
          op: { type: 'set_text', text: 'Recovered after reconnect' },
        }],
      }
      : { canvas_id: 'canvas-1', since: query.since, ops: [] })

    socket.readyState = 3
    socket.emit('close', { code: 1006 })
    await vi.advanceTimersByTimeAsync(1000)
    await waitFor(() => FakeWebSocket.instances.length === 2, 'reconnected WebSocket')
    FakeWebSocket.instances[1].open()
    await waitFor(() => (
      mounted!.container.querySelector<HTMLTextAreaElement>('[aria-label="Canvas 正文"]')?.value
      === 'Recovered after reconnect'
    ), 'Canvas operation catch-up')
    expect(mockApi.getCanvas).toHaveBeenCalledTimes(4)
    expect(mockApi.listCanvasOps.mock.calls.some((call) => call[2]?.since === 1)).toBe(true)
    expect(FakeWebSocket.instances).toHaveLength(2, 'Canvas must reuse ChatShell sockets')

    const text = mounted.container.querySelector<HTMLTextAreaElement>('[aria-label="Canvas 正文"]')!
    input('[aria-label="Canvas 正文"]', 'Unsent user input', mounted.container)
    const appendStart = mockApi.appendCanvasOp.mock.calls.length
    const uncertain = Object.assign(new Error('response lost'), { status: 0 })
    mockApi.appendCanvasOp
      .mockRejectedValueOnce(uncertain)
      .mockRejectedValueOnce(uncertain)
      .mockResolvedValueOnce({
        id: 'op-3',
        canvas_id: 'canvas-1',
        seq: 3,
        author_id: participant.id,
        op: { type: 'set_text', text: 'Unsent user input' },
      })
    submit('.canvas-edit-form', mounted.container)
    await waitFor(() => mounted!.container.querySelector('.canvas-error')?.textContent?.includes('保存失败'), 'visible failed-save error')
    expect(text.value).toBe('Unsent user input')
    expect(mounted.container.querySelector('.canvas-save-status')?.textContent).toBe('保存失败')
    expect(mounted.container.querySelector('.canvas-save-status')?.textContent).not.toContain('已保存')
    const retryIds = mockApi.appendCanvasOp.mock.calls.slice(appendStart).map((call) => call[3])
    expect(retryIds).toHaveLength(2)
    expect(retryIds[0]).toBe(retryIds[1])

    submit('.canvas-edit-form', mounted.container)
    await waitFor(() => mounted!.container.querySelector('.canvas-save-status')?.textContent === '已保存', 'manual retry success')
    expect(mockApi.appendCanvasOp.mock.calls[appendStart + 2][3]).toBe(retryIds[0])
    expect(FakeWebSocket.instances).toHaveLength(2)
  })

  it('merges live room events with deferred history and ignores stale room responses', async () => {
    mockApi.listRooms.mockResolvedValue([roomA, roomB])
    let resolveOldHistory!: (rows: ReturnType<typeof message>[]) => void
    let roomACalls = 0
    mockApi.listMessages.mockImplementation((roomId: string) => {
      if (roomId === 'room-a') {
        roomACalls += 1
        if (roomACalls === 1) {
          return new Promise((resolve) => { resolveOldHistory = resolve })
        }
        return Promise.resolve([])
      }
      return Promise.resolve([message('history-b', 'room-b', 'Room B history')])
    })
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelectorAll('.room-item').length === 2, 'both rooms')
    const socket = FakeWebSocket.instances[0]
    socket.open()
    await waitFor(() => roomACalls === 1, 'deferred first room history')

    socket.message({
      type: 'message',
      seq: 33,
      delivery_ordinal: 1,
      message: message('live-a', 'room-a', 'Live room A message'),
    })
    mounted.container.querySelectorAll<HTMLButtonElement>('.room-item')[1].click()
    await waitFor(() => mounted!.container.textContent?.includes('Room B history') === true, 'second room history')
    resolveOldHistory([message('stale-a', 'room-a', 'Late room A history')])
    await flushPromises()
    expect(mounted.container.textContent).toContain('Room B history')
    expect(mounted.container.textContent).not.toContain('Late room A history')

    mounted.container.querySelectorAll<HTMLButtonElement>('.room-item')[0].click()
    await waitFor(() => mounted!.container.textContent?.includes('Live room A message') === true, 'merged live state after room switch')
    expect(mounted.container.textContent).not.toContain('Late room A history')
    expect(FakeWebSocket.instances).toHaveLength(1)
  })
})
