import {
  createEffect,
  createSignal,
  For,
  onCleanup,
  onMount,
  Show,
  type JSX,
} from 'solid-js'
import {
  IrisAlert,
  IrisAvatar,
  IrisBadge,
  IrisButton,
  IrisCard,
  IrisIcon,
  IrisInput,
  IrisSpinner,
} from '@iris-ui-kit/solid'
import {
  api,
  messageText,
  sessionStorage,
  type Message,
  type Participant,
  type Room,
} from './api'
import { CanvasPanel } from './CanvasPanel'
import { mergeRoomMessages } from './room_state'
import { WsClient } from '../ws.js'
import { refreshWsAccessToken } from '../ws_auth.js'

interface ChatShellProps {
  participant: Participant
  onLogout: () => Promise<void>
}

type SocketStatus = 'connecting' | 'online' | 'offline'

interface ServerFrame {
  type?: string
  room_id?: string
  participant?: string
  online?: string[]
  message?: Message
  event?: Message
  id?: string
  blocks?: Message['blocks']
}

function roomLabel(room: Room): string {
  return room.name?.trim() || `${room.kind ?? 'room'} · ${room.id.slice(0, 8)}`
}

function participantLabel(id: string, current: Participant): string {
  if (id === current.id) return current.display_name?.trim() || current.email || '我'
  return id.slice(0, 10)
}

function messageTime(value?: string): string {
  if (!value) return ''
  const date = new Date(value)
  return Number.isNaN(date.getTime())
    ? ''
    : date.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit' })
}

export function ChatShell(props: ChatShellProps): JSX.Element {
  const [rooms, setRooms] = createSignal<Room[]>([])
  const [currentRoomId, setCurrentRoomId] = createSignal('')
  const [messagesByRoom, setMessagesByRoom] = createSignal<Record<string, Message[]>>({})
  const messages = (): Message[] => messagesByRoom()[currentRoomId()] ?? []
  const [online, setOnline] = createSignal<string[]>([])
  const [draft, setDraft] = createSignal('')
  const [newRoomName, setNewRoomName] = createSignal('')
  const [newRoomKind, setNewRoomKind] = createSignal<'group' | 'channel'>('group')
  const [loadingRooms, setLoadingRooms] = createSignal(true)
  const [loadingMessages, setLoadingMessages] = createSignal(false)
  const [sending, setSending] = createSignal(false)
  const [socketStatus, setSocketStatus] = createSignal<SocketStatus>('connecting')
  const [error, setError] = createSignal('')
  const [showNewRoom, setShowNewRoom] = createSignal(false)
  const [canvasDraftDirty, setCanvasDraftDirty] = createSignal(false)
  const wsClient = new WsClient()
  const unsubscribe: Array<() => void> = []
  let messageLoadGeneration = 0
  let sendResetTimer: number | undefined
  let disposed = false

  const selectedRoom = (): Room | undefined =>
    rooms().find((room) => room.id === currentRoomId())

  const sendFrame = (frame: Record<string, unknown>): boolean => wsClient.send(frame)

  const joinCurrentRoom = (): void => {
    const roomId = currentRoomId()
    if (roomId) wsClient.joinRoom(roomId)
  }

  const loadRooms = async (): Promise<void> => {
    setLoadingRooms(true)
    try {
      const result = await api.listRooms()
      if (disposed) return
      setRooms(Array.isArray(result) ? result : [])
      if (!currentRoomId() && result[0]?.id) setCurrentRoomId(result[0].id)
    } catch (reason) {
      if (!disposed) setError(reason instanceof Error ? reason.message : '房间加载失败')
    } finally {
      if (!disposed) setLoadingRooms(false)
    }
  }

  const loadMessages = async (roomId: string): Promise<void> => {
    const generation = ++messageLoadGeneration
    setLoadingMessages(true)
    setError('')
    wsClient.joinRoom(roomId)
    try {
      const result = await api.listMessages(roomId)
      if (generation !== messageLoadGeneration || roomId !== currentRoomId()) return
      const history = Array.isArray(result) ? result.slice() : []
      history.sort((a, b) => (a.created_at ?? '').localeCompare(b.created_at ?? ''))
      setMessagesByRoom((current) => ({
        ...current,
        [roomId]: mergeRoomMessages(history, current[roomId] ?? []),
      }))
      setOnline([])
    } catch (reason) {
      if (generation !== messageLoadGeneration || roomId !== currentRoomId()) return
      setError(reason instanceof Error ? reason.message : '消息加载失败')
    } finally {
      if (generation === messageLoadGeneration && roomId === currentRoomId()) {
        setLoadingMessages(false)
      }
    }
  }

  const handleFrame = (frame: ServerFrame): void => {
    if (frame.type === 'message' && frame.message?.room_id) {
      const roomId = frame.message.room_id
      setMessagesByRoom((current) => ({
        ...current,
        [roomId]: mergeRoomMessages(current[roomId] ?? [], [frame.message!]),
      }))
      return
    }
    if (frame.type === 'edited' && frame.event?.room_id) {
      const roomId = frame.event.room_id
      setMessagesByRoom((current) => ({
        ...current,
        [roomId]: mergeRoomMessages(current[roomId] ?? [], [frame.event!]),
      }))
      return
    }
    if (frame.type === 'deleted' && frame.room_id && frame.id) {
      setMessagesByRoom((current) => ({
        ...current,
        [frame.room_id!]: (current[frame.room_id!] ?? []).map((message) => message.id === frame.id
          ? { ...message, deleted_at: new Date().toISOString(), blocks: [] }
          : message),
      }))
      return
    }
    if (frame.type === 'presence' && frame.room_id === currentRoomId()) {
      setOnline(Array.isArray(frame.online) ? frame.online : [])
    }
  }

  const connect = (): void => {
    const token = sessionStorage.token
    if (!token) return
    const auth = {
      getRefresh: () => sessionStorage.refresh,
      setSession: (access: string, refresh: string, participantId: string) => {
        sessionStorage.set({
          access_token: access,
          refresh_token: refresh,
          participant: { id: participantId },
        })
      },
    }
    setSocketStatus('connecting')
    wsClient.connect(token, props.participant.id, {
      refreshAccessToken: () => refreshWsAccessToken(
        api,
        auth,
        { me: props.participant },
      ),
    })
  }

  const selectRoom = (roomId: string): void => {
    if (roomId === currentRoomId()) return
    if (canvasDraftDirty() && !window.confirm('切换房间会离开未保存的 Canvas 草稿，是否继续？')) return
    setCurrentRoomId(roomId)
  }

  const createRoom = async (event: Event): Promise<void> => {
    event.preventDefault()
    try {
      const room = await api.createRoom(newRoomKind(), newRoomName())
      if (disposed) return
      const shouldSelect = !canvasDraftDirty()
        || window.confirm('新房间已创建。切换过去会离开未保存的 Canvas 草稿，是否继续？')
      setRooms((current) => [...current, room])
      setNewRoomName('')
      setShowNewRoom(false)
      if (shouldSelect) setCurrentRoomId(room.id)
    } catch (reason) {
      if (!disposed) setError(reason instanceof Error ? reason.message : '创建房间失败')
    }
  }

  const sendMessage = (event: Event): void => {
    event.preventDefault()
    const roomId = currentRoomId()
    const text = draft().trim()
    if (!roomId || !text || sending()) return
    if (!sendFrame({
      type: 'send_message',
      room_id: roomId,
      blocks: [{ type: 'text', content: text }],
      reply_to: null,
    })) {
      setError('实时连接尚未建立，消息未发送')
      return
    }
    setSending(true)
    setDraft('')
    if (sendResetTimer !== undefined) window.clearTimeout(sendResetTimer)
    sendResetTimer = window.setTimeout(() => {
      sendResetTimer = undefined
      if (!disposed) setSending(false)
    }, 250)
  }

  onMount(() => {
    unsubscribe.push(
      wsClient.on('status', (status) => {
        if (status === 'up') setSocketStatus('online')
        else if (status === 'connecting' || status === 'wait') setSocketStatus('connecting')
        else setSocketStatus('offline')
      }),
      wsClient.on('open', joinCurrentRoom),
      wsClient.on('message', (frame) => handleFrame(frame as ServerFrame)),
      wsClient.on('auth_expired', () => {
        setSocketStatus('offline')
        setError('登录已过期，请重新登录。')
      }),
    )
    void loadRooms()
    connect()
  })

  onCleanup(() => {
    disposed = true
    messageLoadGeneration += 1
    if (sendResetTimer !== undefined) window.clearTimeout(sendResetTimer)
    for (const off of unsubscribe) off()
    wsClient.close()
  })

  createEffect(() => {
    const roomId = currentRoomId()
    if (roomId) {
      setOnline([])
      void loadMessages(roomId)
    }
  })

  return (
    <div class="chat-page">
      <header class="topbar">
        <div class="brand-lockup compact">
          <div class="brand-mark">A</div>
          <div>
            <strong>Aero IM</strong>
            <span>SolidJS / Iris UI</span>
          </div>
        </div>
        <div class="topbar-actions">
          <IrisBadge tone={socketStatus() === 'online' ? 'success' : 'warning'} variant="subtle">
            {socketStatus() === 'online' ? '实时在线' : socketStatus() === 'connecting' ? '连接中' : '重连中'}
          </IrisBadge>
          <IrisAvatar name={props.participant.display_name ?? props.participant.email ?? 'A'} size={30} />
          <IrisButton variant="ghost" size="sm" onClick={() => void props.onLogout()}>
            退出
          </IrisButton>
        </div>
      </header>

      <div class="chat-layout">
        <aside class="room-sidebar">
          <div class="sidebar-heading">
            <div>
              <span class="eyebrow">WORKSPACE</span>
              <h2>房间</h2>
            </div>
            <IrisButton variant="outline" size="sm" aria-label="新建房间" onClick={() => setShowNewRoom(!showNewRoom())}>
              <IrisIcon name="plus" size={16} />
            </IrisButton>
          </div>

          <Show when={showNewRoom()}>
            <form class="new-room-form" onSubmit={createRoom}>
              <IrisInput
                value={newRoomName()}
                maxlength={128}
                placeholder="房间名称"
                onInput={(event: InputEvent & { currentTarget: HTMLInputElement }) => setNewRoomName(event.currentTarget.value)}
              />
              <select value={newRoomKind()} onChange={(event) => setNewRoomKind(event.currentTarget.value as 'group' | 'channel')}>
                <option value="group">群组</option>
                <option value="channel">频道</option>
              </select>
              <IrisButton type="submit" size="sm" variant="solid">创建</IrisButton>
            </form>
          </Show>

          <Show when={!loadingRooms()} fallback={<div class="sidebar-state"><IrisSpinner size="sm" /></div>}>
            <Show when={rooms().length > 0} fallback={<div class="sidebar-state">还没有房间</div>}>
              <nav class="room-list" aria-label="房间列表">
                <For each={rooms()}>
                  {(room) => (
                    <button
                      type="button"
                      class="room-item"
                      classList={{ active: room.id === currentRoomId() }}
                      onClick={() => selectRoom(room.id)}
                    >
                      <span class="room-icon">{room.kind === 'channel' ? '#' : '◉'}</span>
                      <span class="room-copy">
                        <strong>{roomLabel(room)}</strong>
                        <small>{room.kind ?? 'group'}</small>
                      </span>
                    </button>
                  )}
                </For>
              </nav>
            </Show>
          </Show>
        </aside>

        <main class="conversation">
          <Show when={selectedRoom()} fallback={<div class="empty-state">选择一个房间开始聊天</div>}>
            {(room) => (
              <>
                <header class="conversation-header">
                  <div>
                    <span class="eyebrow">{room().kind ?? 'ROOM'}</span>
                    <h1>{roomLabel(room())}</h1>
                    <p>{room().topic ?? '与团队实时交流'}</p>
                  </div>
                  <span class="room-id">{room().id.slice(0, 12)}…</span>
                </header>

                <Show when={error()}>
                  <IrisAlert tone="danger" class="inline-alert">{error()}</IrisAlert>
                </Show>

                <CanvasPanel
                  roomId={() => currentRoomId()}
                  ws={wsClient}
                  participantId={props.participant.id}
                  onDraftStateChange={setCanvasDraftDirty}
                />

                <section class="message-scroll" aria-live="polite">
                  <Show when={!loadingMessages()} fallback={<div class="empty-state"><IrisSpinner /></div>}>
                    <Show when={messages().length > 0} fallback={<div class="empty-state">暂无消息，发起第一条消息吧。</div>}>
                      <For each={messages()}>
                        {(message) => (
                          <article class="message-row" classList={{ mine: message.sender_id === props.participant.id }}>
                            <IrisAvatar name={participantLabel(message.sender_id, props.participant)} size={32} />
                            <div class="message-body">
                              <div class="message-meta">
                                <strong>{participantLabel(message.sender_id, props.participant)}</strong>
                                <time>{messageTime(message.created_at)}</time>
                              </div>
                              <div class="message-bubble">
                                <Show when={!message.deleted_at && !message.recalled_at} fallback={<em>消息已撤回</em>}>
                                  {messageText(message) || '[非文本消息]'}
                                </Show>
                              </div>
                            </div>
                          </article>
                        )}
                      </For>
                    </Show>
                  </Show>
                </section>

                <form class="composer" onSubmit={sendMessage}>
                  <textarea
                    value={draft()}
                    rows="2"
                    maxlength="8000"
                    placeholder="输入消息，Enter 发送，Shift+Enter 换行"
                    onInput={(event) => setDraft(event.currentTarget.value)}
                    onKeyDown={(event) => {
                      if (event.key === 'Enter' && !event.shiftKey) {
                        event.preventDefault()
                        event.currentTarget.form?.requestSubmit()
                      }
                    }}
                  />
                  <IrisButton type="submit" variant="solid" disabled={!draft().trim() || sending()}>
                    发送
                  </IrisButton>
                </form>
              </>
            )}
          </Show>
        </main>

        <aside class="presence-sidebar">
          <div class="sidebar-heading">
            <div>
              <span class="eyebrow">PRESENCE</span>
              <h2>在线成员</h2>
            </div>
            <IrisBadge tone="neutral" variant="subtle">{online().length}</IrisBadge>
          </div>
          <Show when={online().length > 0} fallback={<p class="muted">加入房间后显示在线成员</p>}>
            <ul class="presence-list">
              <For each={online()}>
                {(id) => <li><span class="online-dot" />{participantLabel(id, props.participant)}</li>}
              </For>
            </ul>
          </Show>
        </aside>
      </div>
    </div>
  )
}
