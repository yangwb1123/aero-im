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
  listCanvases: vi.fn(),
  createCanvas: vi.fn(),
  getCanvas: vi.fn(),
  listCanvasOps: vi.fn(),
  appendCanvasOp: vi.fn(),
  refresh: vi.fn(),
}))

const mockSession = vi.hoisted(() => ({ refresh: null as string | null, clear: vi.fn() }))

vi.mock('../src/api', () => ({
  api: mockApi,
  messageText: (message: { blocks?: Array<{ type?: string; content?: string }> }) => (
    (message.blocks ?? []).map((block) => block.content ?? '').join('')
  ),
  sessionStorage: {
    get token() { return 'test-access-token' },
    get refresh() { return mockSession.refresh },
    get participantId() { return 'participant-1' },
    set() {},
    clear: mockSession.clear,
  },
}))

interface FakeSocketEvent {
  data?: string
  code?: number
  reason?: string
}

class FakeWebSocket {
  static OPEN = 1
  static instances: FakeWebSocket[] = []
  readyState = 0
  readonly url: string
  readonly sent: Array<Record<string, unknown>> = []
  readonly closed: Array<{ code: number; reason: string }> = []
  private listeners = new Map<string, Set<(event: FakeSocketEvent) => void>>()

  constructor(url: string) {
    this.url = url
    FakeWebSocket.instances.push(this)
  }

  addEventListener(event: string, callback: (event: FakeSocketEvent) => void): void {
    if (!this.listeners.has(event)) this.listeners.set(event, new Set())
    this.listeners.get(event)!.add(callback)
  }

  emit(event: string, data: FakeSocketEvent = {}): void {
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

function authSession(
  id: string,
  userAgent: string | null = 'Firefox on Linux',
  lastSeen = '2026-05-01T12:34:56Z',
) {
  return {
    id,
    participant_id: participant.id,
    token_prefix: `prefix-${id}`,
    user_agent: userAgent,
    created_at: '2026-04-01T12:00:00Z',
    last_seen_at: lastSeen,
    revoked_at: null,
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

function mountChat(onLogout: () => Promise<void> = async () => {}): { container: HTMLElement; unmount: () => void } {
  const container = document.createElement('div')
  document.body.appendChild(container)
  const dispose = render(() => <ChatShell participant={participant} onLogout={onLogout} />, container)
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
  let previousConfirm: typeof window.confirm
  let mounted: { container: HTMLElement; unmount: () => void } | undefined

  beforeEach(() => {
    vi.clearAllMocks()
    previousWebSocket = globalThis.WebSocket
    previousConfirm = window.confirm
    FakeWebSocket.instances = []
    globalThis.WebSocket = FakeWebSocket as unknown as typeof WebSocket
    mockApi.listRooms.mockResolvedValue([roomA])
    mockApi.listSessions.mockResolvedValue([])
    mockApi.revokeOtherSessions.mockResolvedValue({ revoked_count: 1 })
    mockSession.refresh = null
    mockSession.clear.mockClear()
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
    window.confirm = previousConfirm
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
    expect(mounted.container.querySelector('.canvas-save-status')?.textContent).toContain('保存失败')
    expect(mounted.container.querySelector('.canvas-save-status')?.textContent).not.toContain('已保存')
    const retryIds = mockApi.appendCanvasOp.mock.calls.slice(appendStart).map((call) => call[3])
    expect(retryIds).toHaveLength(2)
    expect(retryIds[0]).toBe(retryIds[1])

    const reconnectingSocket = FakeWebSocket.instances[1]
    reconnectingSocket.readyState = 3
    reconnectingSocket.emit('close', { code: 1006 })
    await vi.advanceTimersByTimeAsync(1000)
    await waitFor(() => FakeWebSocket.instances.length === 3, 'second reconnect')
    FakeWebSocket.instances[2].open()
    await waitFor(() => mockApi.listCanvasOps.mock.calls.some((call) => call[2]?.since === 2), 'post-failure catch-up')
    expect(mounted.container.querySelector('.canvas-save-status')?.textContent).toContain('保存失败')
    expect(mounted.container.querySelector('.canvas-save-status')?.textContent).not.toContain('已同步')
    expect(mounted.container.querySelector('.canvas-error')?.textContent).toContain('保存失败')

    submit('.canvas-edit-form', mounted.container)
    await waitFor(() => mounted!.container.querySelector('.canvas-save-status')?.textContent?.includes('已保存') === true, 'manual retry success')
    expect(mockApi.appendCanvasOp.mock.calls[appendStart + 2][3]).toBe(retryIds[0])
    expect(FakeWebSocket.instances).toHaveLength(3)
  })

  it('merges live room events with deferred history and ignores stale room responses', async () => {
    mockApi.listRooms.mockResolvedValue([roomA, roomB])
    let resolveOldHistory!: (rows: ReturnType<typeof message>[]) => void
    let resolveCurrentHistory!: (rows: ReturnType<typeof message>[]) => void
    let roomACalls = 0
    mockApi.listMessages.mockImplementation((roomId: string) => {
      if (roomId === 'room-a') {
        roomACalls += 1
        if (roomACalls === 1) {
          return new Promise((resolve) => { resolveOldHistory = resolve })
        }
        return new Promise((resolve) => { resolveCurrentHistory = resolve })
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
    await waitFor(() => roomACalls === 2, 'current room A history')
    socket.message({
      type: 'message',
      seq: 34,
      delivery_ordinal: 2,
      message: message('live-a-2', 'room-a', 'Second live room A message'),
    })
    resolveCurrentHistory([message('history-a', 'room-a', 'Room A history')])
    await waitFor(() => mounted!.container.textContent?.includes('Room A history') === true, 'Room A history merge')
    expect(mounted.container.textContent).toContain('Live room A message')
    expect(mounted.container.textContent).toContain('Second live room A message')
    expect(mounted.container.textContent).not.toContain('Late room A history')
    expect(FakeWebSocket.instances).toHaveLength(1)
  })

  it('loads login sessions and shows device/activity details without enabling one-session revocation', async () => {
    mockSession.refresh = 'stored-refresh-secret'
    let resolveSessions!: (rows: ReturnType<typeof authSession>[]) => void
    mockApi.listSessions.mockImplementationOnce(() => new Promise((resolve) => {
      resolveSessions = resolve
    }))
    mounted = mountChat()
    mounted.container.querySelector<HTMLButtonElement>('.session-trigger')!.click()
    await waitFor(() => mockApi.listSessions.mock.calls.length === 1, 'session list request')

    const dialog = document.body.querySelector<HTMLElement>('.session-dialog')!
    expect(dialog.textContent).toContain('正在加载登录会话')
    const revokeButton = [...dialog.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent?.includes('退出其他设备'))
    expect(revokeButton?.disabled).toBe(true)

    resolveSessions([authSession('session-current', null, '2026-05-02T08:09:10Z')])
    await waitFor(() => dialog.textContent?.includes('未记录设备信息') === true, 'session details')
    expect(dialog.textContent).toContain('最近活动：2026-05-02T08:09:10Z')
    expect(revokeButton?.disabled).toBe(true)
    expect(dialog.textContent).not.toContain('prefix-session-current')
  })

  it('distinguishes session load errors from empty results, retries, and rejects invalid responses', async () => {
    mockApi.listSessions
      .mockRejectedValueOnce(new Error('offline'))
      .mockResolvedValueOnce([])
      .mockResolvedValueOnce({ sessions: [] })
    mounted = mountChat()
    mounted.container.querySelector<HTMLButtonElement>('.session-trigger')!.click()
    await waitFor(() => mockApi.listSessions.mock.calls.length === 1, 'initial session load')
    await waitFor(() => document.body.querySelector('.session-error') !== null, 'session load error')
    expect(document.body.textContent).not.toContain('当前没有活跃登录会话')

    document.body.querySelector<HTMLButtonElement>('.session-retry')!.click()
    await waitFor(() => mockApi.listSessions.mock.calls.length === 2, 'session load retry')
    await waitFor(() => document.body.textContent?.includes('当前没有活跃登录会话') === true, 'valid empty sessions')
    expect(document.body.querySelector('.session-error')).toBeNull()

    document.body.querySelector<HTMLButtonElement>('.session-close')!.click()
    mounted.container.querySelector<HTMLButtonElement>('.session-trigger')!.click()
    await waitFor(() => mockApi.listSessions.mock.calls.length === 3, 'invalid session response')
    await waitFor(() => document.body.querySelector('.session-error') !== null, 'invalid response error')
    expect(document.body.textContent).not.toContain('当前没有活跃登录会话')
  })

  it('requires a refresh token and at least two loaded sessions to enable revoking others', async () => {
    mockApi.listSessions
      .mockResolvedValueOnce([authSession('one'), authSession('two')])
      .mockResolvedValueOnce([authSession('only-one')])
    mounted = mountChat()
    mounted.container.querySelector<HTMLButtonElement>('.session-trigger')!.click()
    await waitFor(() => document.body.querySelectorAll('.session-row').length === 2, 'two sessions')
    let revokeButton = [...document.body.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent?.includes('退出其他设备'))!
    expect(revokeButton.disabled).toBe(true)

    document.body.querySelector<HTMLButtonElement>('.session-close')!.click()
    mockSession.refresh = 'available-refresh-secret'
    mounted.container.querySelector<HTMLButtonElement>('.session-trigger')!.click()
    await waitFor(() => mockApi.listSessions.mock.calls.length === 2, 'one-session reload')
    await waitFor(() => document.body.querySelectorAll('.session-row').length === 1, 'single session')
    revokeButton = [...document.body.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent?.includes('退出其他设备'))!
    expect(revokeButton.disabled).toBe(true)
    expect(mockApi.revokeOtherSessions).not.toHaveBeenCalled()
  })

  it('waits for confirmation, sends the refresh token only to the API, and reloads sessions', async () => {
    const refreshToken = 'current-refresh-secret-not-for-display'
    mockSession.refresh = refreshToken
    mockApi.listSessions
      .mockResolvedValueOnce([authSession('current'), authSession('other')])
      .mockResolvedValueOnce([authSession('current')])
    const onLogout = vi.fn(async () => {})
    mounted = mountChat(onLogout)
    window.confirm = vi.fn(() => false)
    mounted.container.querySelector<HTMLButtonElement>('.session-trigger')!.click()
    await waitFor(() => document.body.querySelectorAll('.session-row').length === 2, 'loaded sessions')
    const revokeButton = [...document.body.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent?.includes('退出其他设备'))!
    revokeButton.click()
    expect(window.confirm).toHaveBeenCalledTimes(1)
    expect(mockApi.revokeOtherSessions).not.toHaveBeenCalled()

    window.confirm = vi.fn(() => true)
    revokeButton.click()
    await waitFor(() => mockApi.revokeOtherSessions.mock.calls.length === 1, 'session revocation')
    expect(mockApi.revokeOtherSessions).toHaveBeenCalledWith(refreshToken)
    await waitFor(() => mockApi.listSessions.mock.calls.length === 2, 'session list refresh')
    await waitFor(() => document.body.querySelectorAll('.session-row').length === 1, 'refreshed active session')
    expect(document.body.textContent).not.toContain(refreshToken)
    expect(document.body.innerHTML).not.toContain(refreshToken)
    expect(onLogout).not.toHaveBeenCalled()
    expect(mockSession.clear).not.toHaveBeenCalled()
  })

  it('prevents duplicate revocations and allows retry after a failed request without logging out', async () => {
    mockSession.refresh = 'current-refresh-for-retry'
    mockApi.listSessions.mockResolvedValue([authSession('current'), authSession('other')])
    let rejectRevocation!: (reason: Error) => void
    mockApi.revokeOtherSessions
      .mockImplementationOnce(() => new Promise((_resolve, reject) => { rejectRevocation = reject }))
      .mockResolvedValueOnce({ revoked_count: 1 })
    const onLogout = vi.fn(async () => {})
    mounted = mountChat(onLogout)
    window.confirm = vi.fn(() => true)
    mounted.container.querySelector<HTMLButtonElement>('.session-trigger')!.click()
    await waitFor(() => document.body.querySelectorAll('.session-row').length === 2, 'loaded sessions')
    let revokeButton = [...document.body.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent?.includes('退出其他设备'))!
    revokeButton.click()
    await waitFor(() => mockApi.revokeOtherSessions.mock.calls.length === 1, 'first revocation')
    expect(revokeButton.disabled).toBe(true)
    revokeButton.click()
    expect(mockApi.revokeOtherSessions).toHaveBeenCalledTimes(1)

    rejectRevocation(new Error('request failed'))
    await waitFor(() => document.body.textContent?.includes('退出其他设备失败') === true, 'revocation error')
    revokeButton = [...document.body.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent?.includes('退出其他设备'))!
    expect(revokeButton.disabled).toBe(false)
    revokeButton.click()
    await waitFor(() => mockApi.revokeOtherSessions.mock.calls.length === 2, 'revocation retry')
    await waitFor(() => mockApi.listSessions.mock.calls.length === 2, 'post-revocation reload')
    expect(mockApi.revokeOtherSessions).toHaveBeenNthCalledWith(2, 'current-refresh-for-retry')
    expect(onLogout).not.toHaveBeenCalled()
    expect(mockSession.clear).not.toHaveBeenCalled()
  })

  it('ignores a Canvas list response from a previous close/reopen cycle', async () => {
    let resolveOldList!: (rows: ReturnType<typeof canvas>[]) => void
    let resolveFreshList!: (rows: ReturnType<typeof canvas>[]) => void
    mockApi.listCanvases
      .mockImplementationOnce(() => new Promise((resolve) => { resolveOldList = resolve }))
      .mockImplementationOnce(() => new Promise((resolve) => { resolveFreshList = resolve }))
    mockApi.getCanvas.mockImplementation(async (_roomId: string, canvasId: string) => (
      canvasId === 'canvas-fresh'
        ? canvas('canvas-fresh', 'Fresh canvas', 'Fresh content')
        : canvas('canvas-stale', 'Stale canvas', 'Stale content')
    ))
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelector('.room-item') !== null, 'room list')
    mounted.container.querySelector<HTMLButtonElement>('.canvas-toggle')!.click()
    await waitFor(() => mockApi.listCanvases.mock.calls.length === 1, 'first Canvas list')

    mounted.container.querySelector<HTMLButtonElement>('.canvas-toggle')!.click()
    mounted.container.querySelector<HTMLButtonElement>('.canvas-toggle')!.click()
    await waitFor(() => mockApi.listCanvases.mock.calls.length === 2, 'reopened Canvas list')
    resolveFreshList([canvas('canvas-fresh', 'Fresh canvas', 'Fresh content')])
    await waitFor(() => (
      mounted!.container.querySelector<HTMLTextAreaElement>('[aria-label="Canvas 正文"]')?.value
      === 'Fresh content'
    ), 'fresh Canvas selection')

    resolveOldList([canvas('canvas-stale', 'Stale canvas', 'Stale content')])
    await flushPromises()
    expect(mounted.container.textContent).toContain('Fresh canvas')
    expect(mounted.container.textContent).not.toContain('Stale canvas')
    expect(mounted.container.querySelector<HTMLTextAreaElement>('[aria-label="Canvas 正文"]')?.value)
      .toBe('Fresh content')
  })

  it('keeps Canvas list errors visible and provides an explicit retry', async () => {
    mockApi.listCanvases
      .mockRejectedValueOnce(new Error('temporarily offline'))
      .mockResolvedValueOnce([])
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelector('.room-item') !== null, 'room list')
    mounted.container.querySelector<HTMLButtonElement>('.canvas-toggle')!.click()
    await waitFor(() => (
      mounted!.container.querySelector('.canvas-error')?.textContent?.includes('temporarily offline')
    ), 'visible Canvas list failure')
    const retry = [...mounted.container.querySelectorAll<HTMLButtonElement>('.canvas-error button')]
      .find((button) => button.textContent?.includes('刷新 Canvas 列表'))
    expect(retry).toBeDefined()
    expect(mounted.container.textContent).not.toContain('此房间还没有 Canvas')

    retry!.click()
    await waitFor(() => mockApi.listCanvases.mock.calls.length === 2, 'explicit list retry')
    await waitFor(() => mounted!.container.textContent?.includes('创建第一个协作空间') === true, 'recovered empty state')
    expect(mounted.container.querySelector('.canvas-error')).toBeNull()
  })

  it('asks before discarding a Canvas draft on panel or room navigation', async () => {
    mockApi.listRooms.mockResolvedValue([roomA, roomB])
    mockApi.listCanvases.mockImplementation((roomId: string) => Promise.resolve(
      roomId === 'room-a' ? [canvas('canvas-1', 'Planning', 'Initial text')] : [],
    ))
    window.confirm = vi.fn(() => false)
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelectorAll('.room-item').length === 2, 'both rooms')
    mounted.container.querySelector<HTMLButtonElement>('.canvas-toggle')!.click()
    await waitFor(() => (
      mounted!.container.querySelector<HTMLTextAreaElement>('[aria-label="Canvas 正文"]')?.value
      === 'Initial text'
    ), 'selected Canvas')
    input('[aria-label="Canvas 正文"]', 'Unsaved draft', mounted.container)

    mounted.container.querySelectorAll<HTMLButtonElement>('.room-item')[1].click()
    expect(mounted.container.querySelector('h1')?.textContent).toContain('Room A')
    expect(mounted.container.querySelector<HTMLTextAreaElement>('[aria-label="Canvas 正文"]')?.value)
      .toBe('Unsaved draft')
    mounted.container.querySelector<HTMLButtonElement>('.canvas-toggle')!.click()
    expect(mounted.container.querySelector('.canvas-panel')).not.toBeNull()

    window.confirm = vi.fn(() => true)
    mounted.container.querySelectorAll<HTMLButtonElement>('.room-item')[1].click()
    await waitFor(() => mounted!.container.querySelector('h1')?.textContent?.includes('Room B') === true, 'confirmed room navigation')
    await waitFor(() => mounted!.container.textContent?.includes('此房间还没有 Canvas') === true, 'new room Canvas state')
    expect(mounted.container.textContent).not.toContain('Unsaved draft')
    expect(mounted.container.querySelector('[aria-label="Canvas 正文"]')).toBeNull()
  })

  it('confirms shared note removal before appending the delete operation', async () => {
    mockApi.getCanvas.mockResolvedValue({
      ...canvas('canvas-1', 'Planning', 'Initial text'),
      blocks: [
        { type: 'text', content: 'Initial text' },
        { type: 'note', note_id: 'note-1', content: 'Shared note' },
      ],
    })
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelector('.room-item') !== null, 'room list')
    mounted.container.querySelector<HTMLButtonElement>('.canvas-toggle')!.click()
    await waitFor(() => mounted!.container.querySelector('[aria-label="便笺 note-1"]') !== null, 'shared note')

    const deleteButton = [...mounted.container.querySelectorAll<HTMLButtonElement>('.canvas-note-actions button')]
      .find((button) => button.textContent?.trim() === '删除便笺')
    expect(deleteButton).toBeDefined()
    deleteButton!.click()
    expect(mounted.container.textContent).toContain('从共享 Canvas 移除此便笺')
    expect(mockApi.appendCanvasOp).not.toHaveBeenCalled()

    mounted.container.querySelector<HTMLButtonElement>('.canvas-delete-confirm button:not([data-danger])')!.click()
    expect(mockApi.appendCanvasOp).not.toHaveBeenCalled()
    const removeButton = [...mounted.container.querySelectorAll<HTMLButtonElement>('.canvas-note-actions button')]
      .find((button) => button.textContent?.trim() === '删除便笺')
    removeButton!.click()
    mounted.container.querySelector<HTMLButtonElement>('[data-danger="true"]')!.click()
    await waitFor(() => mockApi.appendCanvasOp.mock.calls.length === 1, 'confirmed delete operation')
    expect(mockApi.appendCanvasOp.mock.calls[0][2]).toEqual({
      type: 'delete_note',
      note_id: 'note-1',
    })
    await waitFor(() => mounted!.container.textContent?.includes('暂无便笺') === true, 'note removal')
  })

  it('preserves only user-entered drafts when a save is forbidden', async () => {
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelector('.room-item') !== null, 'room list')
    mounted.container.querySelector<HTMLButtonElement>('.canvas-toggle')!.click()
    await waitFor(() => mounted!.container.querySelector('[aria-label="Canvas 正文"]') !== null, 'Canvas editor')
    input('[aria-label="Canvas 正文"]', 'User-authored draft', mounted.container)
    mockApi.appendCanvasOp.mockRejectedValueOnce(Object.assign(new Error('forbidden'), { status: 403 }))

    submit('.canvas-edit-form', mounted.container)
    await waitFor(() => mounted!.container.querySelector('.canvas-preserved-drafts') !== null, 'preserved draft panel')

    expect(mockApi.appendCanvasOp).toHaveBeenCalledTimes(1)
    expect(mounted.container.querySelector('[aria-label="Canvas 正文"]')).toBeNull()
    expect(mounted.container.querySelector<HTMLTextAreaElement>('[aria-label="保留草稿：正文"]')?.value)
      .toBe('User-authored draft')
    expect(mounted.container.textContent).not.toContain('Initial text')
    expect(mounted.container.querySelector<HTMLInputElement>('[aria-label="新 Canvas 标题"]')?.disabled)
      .toBe(true)
  })

  it('does not let a pre-create Canvas list response replace the created Canvas', async () => {
    let resolveOldList!: (rows: ReturnType<typeof canvas>[]) => void
    mockApi.listCanvases.mockImplementationOnce(() => new Promise((resolve) => { resolveOldList = resolve }))
    mockApi.getCanvas.mockImplementation(async (_roomId: string, canvasId: string) => (
      canvasId === 'canvas-2'
        ? canvas('canvas-2', 'New board', 'New text')
        : canvas('canvas-1', 'Planning', 'Initial text')
    ))
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelector('.room-item') !== null, 'room list')
    mounted.container.querySelector<HTMLButtonElement>('.canvas-toggle')!.click()
    await waitFor(() => mockApi.listCanvases.mock.calls.length === 1, 'deferred Canvas list')

    input('[aria-label="新 Canvas 标题"]', 'New board', mounted.container)
    submit('.canvas-create-form', mounted.container)
    await waitFor(() => mounted!.container.querySelector<HTMLTextAreaElement>('[aria-label="Canvas 正文"]')?.value === 'New text', 'created Canvas selection')
    resolveOldList([canvas('canvas-stale', 'Stale list item', 'Stale text')])
    await flushPromises()

    expect(mounted.container.textContent).toContain('New board')
    expect(mounted.container.textContent).not.toContain('Stale list item')
    expect(mounted.container.querySelector<HTMLTextAreaElement>('[aria-label="Canvas 正文"]')?.value)
      .toBe('New text')
  })

  it('reconciles an uncertain create by refreshing the room list before allowing another POST', async () => {
    const recovered = canvas('canvas-2', 'New board', 'New text')
    mockApi.listCanvases
      .mockResolvedValueOnce([canvas('canvas-1', 'Planning', 'Initial text')])
      .mockResolvedValueOnce([canvas('canvas-1', 'Planning', 'Initial text'), recovered])
    mockApi.getCanvas.mockImplementation(async (_roomId: string, canvasId: string) => (
      canvasId === 'canvas-2' ? recovered : canvas('canvas-1', 'Planning', 'Initial text')
    ))
    mockApi.createCanvas.mockRejectedValueOnce(Object.assign(new Error('response lost'), { status: 0 }))
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelector('.room-item') !== null, 'room list')
    mounted.container.querySelector<HTMLButtonElement>('.canvas-toggle')!.click()
    await waitFor(() => mounted!.container.querySelector('[aria-label="Canvas 正文"]') !== null, 'initial Canvas')

    input('[aria-label="新 Canvas 标题"]', 'New board', mounted.container)
    submit('.canvas-create-form', mounted.container)
    await waitFor(() => mounted!.container.querySelector('.canvas-error')?.textContent?.includes('结果暂时无法确认'), 'uncertain create error')
    expect(mounted.container.querySelector<HTMLButtonElement>('.canvas-create-form button')?.disabled).toBe(true)
    submit('.canvas-create-form', mounted.container)
    expect(mockApi.createCanvas).toHaveBeenCalledTimes(1)

    mounted.container.querySelector<HTMLButtonElement>('.canvas-error button')!.click()
    await waitFor(() => mounted!.container.querySelector<HTMLTextAreaElement>('[aria-label="Canvas 正文"]')?.value === 'New text', 'recovered created Canvas')
    expect(mockApi.listCanvases).toHaveBeenCalledTimes(2)
    expect(mockApi.createCanvas).toHaveBeenCalledTimes(1)
    expect(mounted.container.textContent).toContain('New board')
  })

  it('keeps a new room usable while a create request for the previous room is pending', async () => {
    mockApi.listRooms.mockResolvedValue([roomA, roomB])
    mockApi.listCanvases.mockImplementation((roomId: string) => Promise.resolve(
      roomId === 'room-a' ? [canvas('canvas-1', 'Planning', 'Initial text')] : [],
    ))
    let resolveRoomACreate!: (value: ReturnType<typeof canvas>) => void
    mockApi.createCanvas
      .mockImplementationOnce(() => new Promise((resolve) => { resolveRoomACreate = resolve }))
      .mockResolvedValueOnce(canvas('canvas-b', 'Board B', 'Room B Canvas'))
    mockApi.getCanvas.mockImplementation(async (_roomId: string, canvasId: string) => (
      canvasId === 'canvas-b' ? canvas('canvas-b', 'Board B', 'Room B Canvas')
        : canvas('canvas-1', 'Planning', 'Initial text')
    ))
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelectorAll('.room-item').length === 2, 'both rooms')
    mounted.container.querySelector<HTMLButtonElement>('.canvas-toggle')!.click()
    await waitFor(() => mounted!.container.querySelector('[aria-label="Canvas 正文"]') !== null, 'room A Canvas')

    input('[aria-label="新 Canvas 标题"]', 'Board A', mounted.container)
    submit('.canvas-create-form', mounted.container)
    await waitFor(() => mockApi.createCanvas.mock.calls.length === 1, 'pending room A create')
    mounted.container.querySelectorAll<HTMLButtonElement>('.room-item')[1].click()
    await waitFor(() => mounted!.container.querySelector('h1')?.textContent?.includes('Room B') === true, 'room B')
    await waitFor(() => mounted!.container.textContent?.includes('创建第一个协作空间') === true, 'room B empty Canvas state')
    expect(mounted.container.querySelector<HTMLButtonElement>('.canvas-create-form button')?.disabled).toBe(false)

    input('[aria-label="新 Canvas 标题"]', 'Board B', mounted.container)
    submit('.canvas-create-form', mounted.container)
    await waitFor(() => mounted!.container.querySelector<HTMLTextAreaElement>('[aria-label="Canvas 正文"]')?.value === 'Room B Canvas', 'room B create')
    resolveRoomACreate(canvas('canvas-a', 'Board A', 'Room A Canvas'))
    await flushPromises()
    expect(mockApi.createCanvas).toHaveBeenCalledTimes(2)
    expect(mounted.container.querySelector<HTMLTextAreaElement>('[aria-label="Canvas 正文"]')?.value)
      .toBe('Room B Canvas')
    expect(mounted.container.textContent).not.toContain('Room A Canvas')
  })

  it('treats clearing an existing note as an unsaved edit requiring confirmation', async () => {
    mockApi.listRooms.mockResolvedValue([roomA, roomB])
    mockApi.getCanvas.mockResolvedValue({
      ...canvas('canvas-1', 'Planning', 'Initial text'),
      blocks: [
        { type: 'text', content: 'Initial text' },
        { type: 'note', note_id: 'note-1', content: 'Shared note' },
      ],
    })
    window.confirm = vi.fn(() => false)
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelectorAll('.room-item').length === 2, 'both rooms')
    mounted.container.querySelector<HTMLButtonElement>('.canvas-toggle')!.click()
    await waitFor(() => mounted!.container.querySelector('[aria-label="便笺 note-1"]') !== null, 'shared note')

    input('[aria-label="便笺 note-1"]', '', mounted.container)
    mounted.container.querySelectorAll<HTMLButtonElement>('.room-item')[1].click()
    expect(window.confirm).toHaveBeenCalled()
    expect(mounted.container.querySelector('h1')?.textContent).toContain('Room A')
    expect(mounted.container.querySelector<HTMLTextAreaElement>('[aria-label="便笺 note-1"]')?.value)
      .toBe('')
  })

  it('retries a failed same-Canvas snapshot and operation-log load', async () => {
    mockApi.listCanvasOps
      .mockRejectedValueOnce(new Error('ops unavailable'))
      .mockResolvedValueOnce({ canvas_id: 'canvas-1', since: 0, ops: [] })
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelector('.room-item') !== null, 'room list')
    mounted.container.querySelector<HTMLButtonElement>('.canvas-toggle')!.click()
    await waitFor(() => mounted!.container.querySelector('.canvas-error')?.textContent?.includes('ops unavailable'), 'operation-log failure')
    expect(mockApi.getCanvas).toHaveBeenCalledTimes(1)

    const retry = [...mounted.container.querySelectorAll<HTMLButtonElement>('.canvas-error button')]
      .find((button) => button.textContent?.includes('重新加载此 Canvas'))
    expect(retry).toBeDefined()
    retry!.click()
    await waitFor(() => mockApi.getCanvas.mock.calls.length === 2, 'forced Canvas reload')
    await waitFor(() => mounted!.container.querySelector('.canvas-error') === null, 'recovered Canvas')
    expect(mockApi.listCanvasOps).toHaveBeenCalledTimes(2)
    expect(mounted.container.querySelector<HTMLTextAreaElement>('[aria-label="Canvas 正文"]')?.value)
      .toBe('Initial text')
  })

  it('does not let an old in-flight save block or mutate the newly selected Canvas', async () => {
    mockApi.listCanvases.mockResolvedValue([
      canvas('canvas-1', 'Planning', 'Initial text'),
      canvas('canvas-2', 'Research', 'Second Canvas'),
    ])
    mockApi.getCanvas.mockImplementation(async (_roomId: string, canvasId: string) => (
      canvasId === 'canvas-2'
        ? canvas('canvas-2', 'Research', 'Second Canvas')
        : canvas('canvas-1', 'Planning', 'Initial text')
    ))
    let resolveFirstSave!: (operation: Record<string, unknown>) => void
    mockApi.appendCanvasOp.mockImplementationOnce(() => new Promise((resolve) => {
      resolveFirstSave = resolve
    }))
    window.confirm = vi.fn(() => true)
    mounted = mountChat()
    await waitFor(() => mounted!.container.querySelector('.room-item') !== null, 'room list')
    mounted.container.querySelector<HTMLButtonElement>('.canvas-toggle')!.click()
    await waitFor(() => mounted!.container.querySelector<HTMLTextAreaElement>('[aria-label="Canvas 正文"]')?.value === 'Initial text', 'first Canvas')

    input('[aria-label="Canvas 正文"]', 'Pending old-room save', mounted.container)
    submit('.canvas-edit-form', mounted.container)
    await waitFor(() => mockApi.appendCanvasOp.mock.calls.length === 1, 'pending first Canvas save')
    const second = [...mounted.container.querySelectorAll<HTMLButtonElement>('.canvas-list-item')]
      .find((button) => button.textContent?.includes('Research'))
    expect(second).toBeDefined()
    second!.click()
    await waitFor(() => mounted!.container.querySelector<HTMLTextAreaElement>('[aria-label="Canvas 正文"]')?.value === 'Second Canvas', 'second Canvas')
    expect(mounted.container.querySelector<HTMLButtonElement>('.canvas-edit-form button')?.disabled)
      .toBe(false)

    input('[aria-label="Canvas 正文"]', 'Second Canvas saved', mounted.container)
    submit('.canvas-edit-form', mounted.container)
    await waitFor(() => mockApi.appendCanvasOp.mock.calls.length === 2, 'second Canvas save')
    await waitFor(() => mounted!.container.querySelector('.canvas-save-status')?.textContent?.includes('已保存') === true, 'second save completion')
    resolveFirstSave({
      id: 'old-op',
      canvas_id: 'canvas-1',
      seq: 1,
      author_id: participant.id,
      op: { type: 'set_text', text: 'Pending old-room save' },
    })
    await flushPromises()
    expect(mounted.container.querySelector<HTMLTextAreaElement>('[aria-label="Canvas 正文"]')?.value)
      .toBe('Second Canvas saved')
    expect(mounted.container.querySelector('.canvas-save-status')?.textContent).toContain('已保存')
  })
})
