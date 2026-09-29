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
  IrisDialog,
  IrisDialogClose,
  IrisDialogContent,
  IrisDialogDescription,
  IrisDialogTitle,
  IrisDialogTrigger,
  IrisIcon,
  IrisInput,
  IrisSpinner,
} from '@iris-ui-kit/solid'
import {
  api,
  sessionStorage,
  type AuthSession,
  type Message,
  type MessageBlock,
  type Participant,
  type ReactionSummary,
  type Room,
} from './api'
import { recallErrorToast } from './recall_errors.js'
import type { RecallErrorNotice } from './recall_errors.js'
import { CanvasPanel } from './CanvasPanel'
import { mergeRoomMessages, newestRoomMessage } from './room_state'
import { MessageRow } from './MessageRow'
import {
  editableMessageText,
  isPlainTextMessage,
  messageVersion,
  participantLabel,
  replyPreview,
} from './message_format'
import { WsClient } from '../ws.js'
import { refreshWsAccessToken } from '../ws_auth.js'
import type { PendingDelivery } from '../delivery.js'
import {
  clearPendingDelivery,
  discardPersistedDeliveries,
  findPendingMatch,
  initReliableDelivery,
  pendingTempId,
  retryPendingMessage,
  sendOptimistically,
} from '../delivery.js'

interface ChatShellProps {
  participant: Participant
  onLogout: () => Promise<void>
}

type SocketStatus = 'connecting' | 'online' | 'offline'

type PendingOutbound = {
  kind: 'blocks'
  roomId: string
  blocks: MessageBlock[]
  replyTo?: string | null
}

type PendingMessage = Message & {
  client_message_id: string
  delivery_status: NonNullable<Message['delivery_status']>
  attempts: number
  outbound: PendingOutbound
  retryable: boolean
  persistence_warning?: boolean
  last_connection_id?: number | string
  _ackTimer?: ReturnType<typeof setTimeout> | null
}

interface ServerFrame {
  type?: string
  room_id?: string
  participant?: string
  online?: string[]
  message?: Message
  event?: Message
  id?: string
  message_id?: string
  client_message_id?: string
  emoji?: string
  op?: string
  blocks?: Message['blocks']
}

function roomLabel(room: Room): string {
  return room.name?.trim() || `${room.kind ?? 'room'} · ${room.id.slice(0, 8)}`
}

function normalizeReactionSummaries(value: unknown): ReactionSummary[] {
  if (!Array.isArray(value)) return []
  return value.flatMap((item): ReactionSummary[] => {
    if (!item || typeof item !== 'object') return []
    const row = item as Record<string, unknown>
    if (typeof row.emoji !== 'string' || !row.emoji
      || !Number.isSafeInteger(row.count) || Number(row.count) < 1
      || !Array.isArray(row.participants)) return []
    return [{
      emoji: row.emoji,
      count: Number(row.count),
      participants: [...new Set(row.participants.filter((id): id is string => typeof id === 'string'))],
    }]
  })
}

function isUnknownEditOutcome(reason: unknown): boolean {
  if (!reason || typeof reason !== 'object' || !('status' in reason)) return true
  const status = Number((reason as { status?: unknown }).status)
  return status === 0 || status >= 500
}

function isAuthSession(value: unknown): value is AuthSession {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return false
  const session = value as Record<string, unknown>
  return typeof session.id === 'string'
    && typeof session.participant_id === 'string'
    && typeof session.token_prefix === 'string'
    && (session.user_agent === undefined || session.user_agent === null
      || typeof session.user_agent === 'string')
    && typeof session.created_at === 'string'
    && typeof session.last_seen_at === 'string'
    && (session.revoked_at === undefined || session.revoked_at === null
      || typeof session.revoked_at === 'string')
}

export function ChatShell(props: ChatShellProps): JSX.Element {
  const [rooms, setRooms] = createSignal<Room[]>([])
  const [currentRoomId, setCurrentRoomId] = createSignal('')
  const [messagesByRoom, setMessagesByRoom] = createSignal<Record<string, Message[]>>({})
  const [replyToId, setReplyToId] = createSignal('')
  const pendingByTempId = new Map<string, PendingDelivery>()
  const [pendingRevision, setPendingRevision] = createSignal(0)
  const messages = (): Message[] => {
    pendingRevision()
    const roomId = currentRoomId()
    const pending = [...pendingByTempId.values()].filter((message) => message.room_id === roomId)
    return mergeRoomMessages(messagesByRoom()[roomId] ?? [], pending)
  }
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
  const [sessionPanelOpen, setSessionPanelOpen] = createSignal(false)
  const [sessionRows, setSessionRows] = createSignal<AuthSession[] | null>(null)
  const [sessionLoadState, setSessionLoadState] = createSignal<'idle' | 'loading' | 'loaded' | 'error'>('idle')
  const [sessionLoadError, setSessionLoadError] = createSignal('')
  const [sessionRevokeError, setSessionRevokeError] = createSignal('')
  const [revokingOthers, setRevokingOthers] = createSignal(false)
  const [recallingMessages, setRecallingMessages] = createSignal<Record<string, boolean>>({})
  const [recallFeedback, setRecallFeedback] = createSignal<Record<string, RecallErrorNotice>>({})
  const [deletingMessages, setDeletingMessages] = createSignal<Record<string, boolean>>({})
  const [deleteFeedback, setDeleteFeedback] = createSignal<Record<string, string>>({})
  const [reactionsByMessage, setReactionsByMessage] = createSignal<Record<string, ReactionSummary[]>>({})
  const [reacting, setReacting] = createSignal<Record<string, boolean>>({})
  const [reactionFeedback, setReactionFeedback] = createSignal<Record<string, string>>({})
  const [editingMessageId, setEditingMessageId] = createSignal('')
  const [editDraft, setEditDraft] = createSignal('')
  const [editOriginal, setEditOriginal] = createSignal('')
  const [editBaseVersion, setEditBaseVersion] = createSignal<number>()
  const [editSaving, setEditSaving] = createSignal(false)
  const [editError, setEditError] = createSignal('')
  const [editConflict, setEditConflict] = createSignal<Message | null>(null)
  const wsClient = new WsClient()
  const unsubscribe: Array<() => void> = []
  let messageLoadGeneration = 0
  let sessionLoadGeneration = 0
  let editGeneration = 0
  const reactionGenerations = new Map<string, number>()
  let disposeReliableDelivery: (() => void) | undefined
  let sendResetTimer: number | undefined
  let disposed = false

  const selectedRoom = (): Room | undefined =>
    rooms().find((room) => room.id === currentRoomId())

  const replyTarget = (): Message | undefined => {
    const replyId = replyToId()
    return replyId ? (messagesByRoom()[currentRoomId()] ?? []).find((message) => message.id === replyId) : undefined
  }

  const replyParent = (message: Message): Message | undefined => {
    const replyId = message.reply_to
    return replyId ? (messagesByRoom()[message.room_id] ?? []).find((item) => item.id === replyId) : undefined
  }

  const beginReply = (message: Message): void => {
    if (message.room_id !== currentRoomId() || message.deleted_at || message.recalled_at) return
    setReplyToId(message.id)
  }

  const cancelReply = (): void => {
    setReplyToId('')
  }

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

  const loadSessions = async (): Promise<void> => {
    if (disposed || !sessionPanelOpen()) return
    const generation = ++sessionLoadGeneration
    setSessionRows(null)
    setSessionLoadError('')
    setSessionRevokeError('')
    setSessionLoadState('loading')
    try {
      const result: unknown = await api.listSessions()
      if (disposed || !sessionPanelOpen() || generation !== sessionLoadGeneration) return
      if (!Array.isArray(result) || !result.every(isAuthSession)) {
        throw new Error('invalid session response')
      }
      setSessionRows(result)
      setSessionLoadState('loaded')
    } catch {
      if (disposed || !sessionPanelOpen() || generation !== sessionLoadGeneration) return
      setSessionRows(null)
      setSessionLoadError('登录会话加载失败，请检查网络后重试。')
      setSessionLoadState('error')
    }
  }

  const setSessionPanelVisibility = (open: boolean): void => {
    setSessionPanelOpen(open)
    if (open) {
      void loadSessions()
      return
    }
    sessionLoadGeneration += 1
    setSessionRows(null)
    setSessionLoadError('')
    setSessionRevokeError('')
    setSessionLoadState('idle')
  }

  const canRevokeOthers = (): boolean => sessionLoadState() === 'loaded'
    && (sessionRows()?.length ?? 0) >= 2
    && Boolean(sessionStorage.refresh)
    && !revokingOthers()
    && sessionLoadState() !== 'loading'

  const revokeOtherSessions = async (): Promise<void> => {
    if (!canRevokeOthers() || !sessionPanelOpen()) return
    if (!window.confirm('确定退出除当前设备外的所有登录会话吗？当前设备将保持登录。')) return
    const refreshToken = sessionStorage.refresh
    if (!refreshToken) {
      setSessionRevokeError('无法读取当前登录凭据，请重新打开会话面板后重试。')
      return
    }
    setSessionRevokeError('')
    setRevokingOthers(true)
    try {
      await api.revokeOtherSessions(refreshToken)
      if (!disposed && sessionPanelOpen()) await loadSessions()
    } catch {
      if (!disposed) setSessionRevokeError('退出其他设备失败，请稍后重试。')
    } finally {
      if (!disposed) setRevokingOthers(false)
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
      for (const message of history) {
        if (message.client_message_id) {
          settlePendingMessage(message, message.client_message_id)
        }
      }
      setMessagesByRoom((current) => ({
        ...current,
        [roomId]: mergeRoomMessages(history, current[roomId] ?? []),
      }))
      if (history.length > 0) void refreshReactions(history.map((message) => message.id))
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

  const mergeRoomMessage = (roomId: string, incoming: Message): void => {
    setMessagesByRoom((current) => {
      const roomMessages = current[roomId] ?? []
      const existing = roomMessages.find((message) => message.id === incoming.id)
      const newest = newestRoomMessage(existing, incoming)
      return {
        ...current,
        [roomId]: mergeRoomMessages(roomMessages, [newest]),
      }
    })
  }

  const bumpPendingRevision = (): void => {
    setPendingRevision((revision) => revision + 1)
  }

  const addPendingMessage = (
    roomId: string,
    blocks: MessageBlock[],
    replyTo: string | null,
    delivery: Pick<PendingMessage, 'client_message_id' | 'delivery_status' | 'attempts'
      | 'outbound' | 'retryable' | 'last_connection_id'>
      & Partial<Pick<PendingMessage, 'failure_message'>>,
  ): PendingMessage => {
    const pending: PendingMessage = {
      id: pendingTempId(delivery.client_message_id),
      room_id: roomId,
      sender_id: props.participant.id,
      blocks,
      reply_to: replyTo,
      created_at: new Date().toISOString(),
      failure_message: null,
      ...delivery,
    }
    pendingByTempId.set(pending.id, pending)
    bumpPendingRevision()
    return pending
  }

  const restorePendingMessage = (item: PendingDelivery): PendingMessage | null => {
    if (!item.outbound) return null
    const restored = { ...item, id: pendingTempId(item.client_message_id) } as PendingMessage
    pendingByTempId.set(restored.id, restored)
    bumpPendingRevision()
    return restored
  }

  const settlePendingMessage = (message: Message, clientMessageId?: string): void => {
    if (disposed || !message.id || !message.room_id) return
    const exactKey = clientMessageId ? pendingTempId(clientMessageId) : ''
    const exactPending = exactKey ? pendingByTempId.get(exactKey) : undefined
    const exactMatch = Boolean(clientMessageId && exactPending
      && exactPending.client_message_id === clientMessageId
      && exactPending.sender_id === message.sender_id
      && exactPending.room_id === message.room_id)
    const pendingKey = exactMatch
      ? exactKey
      : clientMessageId ? '' : findPendingMatch(message, pendingByTempId, props.participant.id)
    if (pendingKey) {
      const pending = pendingByTempId.get(pendingKey)
      if (pending) clearPendingDelivery(pending)
      pendingByTempId.delete(pendingKey)
      bumpPendingRevision()
    }
    mergeRoomMessage(message.room_id, message)
  }

  const refreshReactions = async (messageIds: string[]): Promise<boolean> => {
    const ids = [...new Set(messageIds.filter((id) => typeof id === 'string' && id.length > 0))]
      .slice(0, 256)
    if (ids.length === 0) return true
    const generations = new Map<string, number>()
    for (const id of ids) {
      const generation = (reactionGenerations.get(id) ?? 0) + 1
      reactionGenerations.set(id, generation)
      generations.set(id, generation)
    }
    try {
      const summaries = await api.reactionsBatch(ids)
      if (disposed) return false
      setReactionsByMessage((current) => {
        const next = { ...current }
        for (const id of ids) {
          if (reactionGenerations.get(id) !== generations.get(id)) continue
          const value = summaries && Object.prototype.hasOwnProperty.call(summaries, id)
            ? summaries[id]
            : []
          next[id] = normalizeReactionSummaries(value)
        }
        return next
      })
      return true
    } catch {
      return false
    }
  }

  const clearReactionFeedback = (messageId: string): void => {
    setReactionFeedback((current) => {
      if (!current[messageId]) return current
      const next = { ...current }
      delete next[messageId]
      return next
    })
  }

  const reactionKey = (messageId: string, emoji: string): string => `${messageId}\u0000${emoji}`

  const toggleReaction = async (message: Message, emoji: string): Promise<void> => {
    if (disposed || message.room_id !== currentRoomId() || !message.id || !emoji
      || message.deleted_at || message.recalled_at) return
    const key = reactionKey(message.id, emoji)
    if (reacting()[key]) return
    clearReactionFeedback(message.id)
    setReacting((current) => ({ ...current, [key]: true }))
    try {
      const result = await api.toggleReaction(message.id, emoji)
      if (disposed) return
      if (result.message_id !== message.id || result.emoji !== emoji
        || (result.op !== 'add' && result.op !== 'remove')) {
        throw new Error('服务器返回了无效的表情回应结果')
      }
      const synced = await refreshReactions([message.id])
      if (disposed || currentRoomId() !== message.room_id) return
      if (synced) clearReactionFeedback(message.id)
      else {
        setReactionFeedback((current) => ({
          ...current,
          [message.id]: '反应已提交，但状态同步失败；请稍后刷新。',
        }))
      }
    } catch (reason) {
      if (disposed) return
      const status = Number((reason as { status?: unknown } | null)?.status)
      if (status === 401) {
        await props.onLogout()
        return
      }
      if (status === 0 || status >= 500 || !reason || typeof reason !== 'object' || !('status' in reason)) {
        const synced = await refreshReactions([message.id])
        if (disposed || currentRoomId() !== message.room_id) return
        setReactionFeedback((current) => ({
          ...current,
          [message.id]: synced
            ? '请求结果不确定，已同步当前回应状态；请确认后再操作。'
            : '请求结果不确定且状态同步失败，请勿盲目重试。',
        }))
      } else if (currentRoomId() === message.room_id) {
        const detail = reason instanceof Error ? reason.message : '请求失败'
        setReactionFeedback((current) => ({ ...current, [message.id]: `表情回应失败：${detail}` }))
      }
    } finally {
      if (!disposed) {
        setReacting((current) => {
          const next = { ...current }
          delete next[key]
          return next
        })
      }
    }
  }

  const clearDeleteFeedback = (messageId: string): void => {
    setDeleteFeedback((current) => {
      if (!current[messageId]) return current
      const next = { ...current }
      delete next[messageId]
      return next
    })
  }

  const markMessageDeleted = (roomId: string, messageId: string, fallback?: Message): void => {
    const existing = (messagesByRoom()[roomId] ?? []).find((message) => message.id === messageId)
    const current = existing ?? fallback
    if (current) {
      mergeRoomMessage(roomId, {
        ...current,
        deleted_at: current.deleted_at ?? new Date().toISOString(),
        blocks: [],
      })
    }
    clearDeleteFeedback(messageId)
  }

  const clearRecallFeedback = (roomId: string): void => {
    setRecallFeedback((current) => {
      if (!current[roomId]) return current
      const next = { ...current }
      delete next[roomId]
      return next
    })
  }

  const refreshMessage = async (roomId: string, messageId: string): Promise<void> => {
    try {
      const fresh = await api.getMessage(messageId)
      if (disposed || fresh.id !== messageId || fresh.room_id !== roomId) return
      mergeRoomMessage(roomId, fresh)
    } catch {
      if (!disposed) {
        setRecallFeedback((current) => ({
          ...current,
          [roomId]: { type: 'error', text: '消息状态已变化，但同步失败；请重新加载消息。' },
        }))
      }
    }
  }

  const recallMessage = async (message: Message): Promise<void> => {
    const roomId = message.room_id
    if (disposed || roomId !== currentRoomId() || !message.id
      || message.deleted_at || message.recalled_at || recallingMessages()[message.id]
      || deletingMessages()[message.id]) return
    if (!window.confirm('确定撤回这条消息吗？')) return
    clearRecallFeedback(roomId)
    setRecallingMessages((current) => ({ ...current, [message.id]: true }))
    try {
      const recalled = await api.recallMessage(message.id)
      if (disposed) return
      if (recalled.id !== message.id || recalled.room_id !== roomId || !recalled.recalled_at) {
        throw new Error('服务器返回了无效的撤回结果')
      }
      mergeRoomMessage(roomId, recalled)
      clearRecallFeedback(roomId)
    } catch (reason) {
      if (disposed) return
      if ((reason as { status?: unknown } | null)?.status === 401) {
        await props.onLogout()
        return
      }
      const notice = recallErrorToast(reason)
      if (notice) {
        setRecallFeedback((current) => ({ ...current, [roomId]: notice }))
      } else {
        await refreshMessage(roomId, message.id)
      }
    } finally {
      if (!disposed) {
        setRecallingMessages((current) => {
          const next = { ...current }
          delete next[message.id]
          return next
        })
      }
    }
  }

  const deleteMessage = async (message: Message): Promise<void> => {
    const roomId = message.room_id
    if (disposed || roomId !== currentRoomId() || !message.id
      || message.sender_id !== props.participant.id
      || message.deleted_at || message.recalled_at || deletingMessages()[message.id]
      || recallingMessages()[message.id]) return
    if (!window.confirm('确定删除这条消息吗？删除后它会从房间消息中隐藏。')) return
    clearDeleteFeedback(message.id)
    setDeletingMessages((current) => ({ ...current, [message.id]: true }))

    const showFeedback = (text: string): void => {
      if (disposed || currentRoomId() !== roomId) return
      setDeleteFeedback((current) => ({ ...current, [message.id]: text }))
    }
    const reconcile = async (): Promise<void> => {
      try {
        const latest = await api.getMessage(message.id)
        if (disposed || latest.id !== message.id || latest.room_id !== roomId) return
        if (latest.deleted_at) {
          markMessageDeleted(roomId, message.id, latest)
          return
        }
        mergeRoomMessage(roomId, latest)
        showFeedback('消息仍存在，删除尚未完成；你可以重试。')
      } catch (reason) {
        if (disposed) return
        const status = Number((reason as { status?: unknown } | null)?.status)
        if (status === 401) {
          await props.onLogout()
          return
        }
        if (status === 404) {
          markMessageDeleted(roomId, message.id, message)
          return
        }
        showFeedback('删除结果无法确认；消息和本地状态均已保留，请重试。')
      }
    }

    try {
      await api.deleteMessage(message.id)
      if (disposed) return
      markMessageDeleted(roomId, message.id, message)
    } catch (reason) {
      if (disposed) return
      const status = Number((reason as { status?: unknown } | null)?.status)
      if (status === 401) {
        await props.onLogout()
        return
      }
      if (status === 404) {
        markMessageDeleted(roomId, message.id, message)
        return
      }
      if (status === 409 || status === 0 || status >= 500
        || !reason || typeof reason !== 'object' || !('status' in reason)) {
        await reconcile()
        return
      }
      const detail = reason instanceof Error ? reason.message : '请求失败'
      showFeedback(`删除消息失败：${detail}`)
    } finally {
      if (!disposed) {
        setDeletingMessages((current) => {
          const next = { ...current }
          delete next[message.id]
          return next
        })
      }
    }
  }

  const hasUnsavedMessageEdit = (): boolean => Boolean(
    editingMessageId() && editDraft() !== editOriginal(),
  )

  const hasUnsavedChanges = (): boolean => canvasDraftDirty()
    || hasUnsavedMessageEdit() || Boolean(replyToId())

  const clearMessageEdit = (): void => {
    editGeneration += 1
    setEditingMessageId('')
    setEditDraft('')
    setEditOriginal('')
    setEditBaseVersion(undefined)
    setEditSaving(false)
    setEditError('')
    setEditConflict(null)
  }

  const beginMessageEdit = (message: Message): void => {
    if (message.room_id !== currentRoomId() || message.sender_id !== props.participant.id
      || message.deleted_at || message.recalled_at || !isPlainTextMessage(message)
      || editSaving()) return
    if (editingMessageId() && editingMessageId() !== message.id) {
      if (hasUnsavedMessageEdit()
        && !window.confirm('放弃另一条消息尚未保存的编辑草稿吗？')) return
      clearMessageEdit()
    }
    editGeneration += 1
    const text = editableMessageText(message)
    setEditingMessageId(message.id)
    setEditDraft(text)
    setEditOriginal(text)
    setEditBaseVersion(messageVersion(message))
    setEditSaving(false)
    setEditError('')
    setEditConflict(null)
  }

  const loadLatestEditVersion = (latest: Message): void => {
    if (latest.id !== editingMessageId()
      || !window.confirm('载入服务器最新版本会放弃当前编辑草稿，继续吗？')) return
    const text = editableMessageText(latest)
    setEditDraft(text)
    setEditOriginal(text)
    setEditBaseVersion(messageVersion(latest))
    setEditConflict(null)
    setEditError('')
  }

  const rebaseEditOnLatestVersion = (latest: Message): void => {
    if (latest.id !== editingMessageId()
      || !window.confirm('将用当前草稿覆盖其他设备的最新修改，继续吗？')) return
    setEditOriginal(editableMessageText(latest))
    setEditBaseVersion(messageVersion(latest))
    setEditConflict(null)
    setEditError('')
  }

  const saveMessageEdit = async (event: Event, message: Message): Promise<void> => {
    event.preventDefault()
    if (editSaving() || editingMessageId() !== message.id) return
    const roomId = message.room_id
    const text = editDraft()
    if (!text.trim()) {
      setEditError('消息内容不能为空。')
      return
    }
    if (text === editOriginal()) {
      clearMessageEdit()
      return
    }
    if (message.deleted_at || message.recalled_at) {
      setEditError('这条消息已删除或撤回，不能继续编辑；草稿仍保留。')
      return
    }
    const baseVersion = editBaseVersion()
    const editToken = editGeneration
    const blocks = [{ type: 'text', content: text }]
    const isCurrentEdit = (): boolean => editGeneration === editToken
      && editingMessageId() === message.id
    setEditSaving(true)
    setEditError('')
    try {
      const updated = await api.editMessage(message.id, blocks, baseVersion)
      if (disposed) return
      if (updated.id !== message.id || updated.room_id !== roomId
        || (baseVersion !== undefined && messageVersion(updated) !== undefined
          && messageVersion(updated)! <= baseVersion)) {
        throw new Error('服务器返回了无效的编辑结果')
      }
      mergeRoomMessage(roomId, updated)
      if (isCurrentEdit()) clearMessageEdit()
    } catch (reason) {
      if (disposed) return
      if ((reason as { status?: unknown } | null)?.status === 401) {
        await props.onLogout()
        return
      }
      const status = Number((reason as { status?: unknown } | null)?.status)
      if (status === 409 || isUnknownEditOutcome(reason)) {
        try {
          const latest = await api.getMessage(message.id)
          if (disposed || latest.id !== message.id || latest.room_id !== roomId) return
          mergeRoomMessage(roomId, latest)
          if (!isCurrentEdit()) return
          const latestVersion = messageVersion(latest)
          const advanced = baseVersion === undefined
            ? editableMessageText(latest) === text
            : latestVersion !== undefined && latestVersion > baseVersion
          if (advanced && editableMessageText(latest) === text) {
            clearMessageEdit()
            return
          }
          if ((latest.deleted_at || latest.recalled_at)
            || (baseVersion !== undefined && latestVersion !== undefined
              && latestVersion > baseVersion)) {
            setEditConflict(latest)
            setEditError(latest.deleted_at || latest.recalled_at
              ? '这条消息已删除或撤回；本地草稿仍保留，无法提交。'
              : '消息已在其他设备修改；本地草稿仍保留，请选择如何处理。')
            return
          }
        } catch (refreshReason) {
          if (isCurrentEdit()) {
            const detail = refreshReason instanceof Error ? refreshReason.message : '无法加载最新消息'
            setEditError(`编辑结果无法确认，草稿已保留。${detail}`)
          }
          return
        }
      }
      if (isCurrentEdit()) {
        setEditError(reason instanceof Error ? reason.message : '消息编辑失败，请重试。')
      }
    } finally {
      if (isCurrentEdit()) setEditSaving(false)
    }
  }

  const handleFrame = (frame: ServerFrame): void => {
    if (frame.type === 'message' && frame.message?.room_id) {
      settlePendingMessage(frame.message, frame.client_message_id ?? frame.message.client_message_id)
      return
    }
    if (frame.type === 'recalled' && frame.message?.room_id) {
      mergeRoomMessage(frame.message.room_id, frame.message)
      return
    }
    if (frame.type === 'edited' && frame.event?.room_id) {
      mergeRoomMessage(frame.event.room_id, frame.event)
      return
    }
    if (frame.type === 'deleted' && frame.room_id && frame.id) {
      markMessageDeleted(frame.room_id, frame.id)
      return
    }
    if (frame.type === 'reaction' && frame.room_id && frame.message_id && frame.emoji) {
      const message = (messagesByRoom()[frame.room_id] ?? [])
        .find((item) => item.id === frame.message_id)
      if (message && !message.deleted_at && !message.recalled_at) {
        void refreshReactions([frame.message_id])
      }
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
    if (hasUnsavedChanges()
      && !window.confirm('切换房间会离开未保存的 Canvas、消息编辑草稿或回复上下文，是否继续？')) return
    clearMessageEdit()
    cancelReply()
    setCurrentRoomId(roomId)
  }

  const createRoom = async (event: Event): Promise<void> => {
    event.preventDefault()
    try {
      const room = await api.createRoom(newRoomKind(), newRoomName())
      if (disposed) return
      const shouldSelect = !hasUnsavedChanges()
        || window.confirm('新房间已创建。切换过去会离开未保存的 Canvas、消息编辑草稿或回复上下文，是否继续？')
      setRooms((current) => [...current, room])
      setNewRoomName('')
      setShowNewRoom(false)
      if (shouldSelect) {
        clearMessageEdit()
        cancelReply()
        setCurrentRoomId(room.id)
      }
    } catch (reason) {
      if (!disposed) setError(reason instanceof Error ? reason.message : '创建房间失败')
    }
  }

  const logoutManually = async (): Promise<void> => {
    disposeReliableDelivery?.()
    disposeReliableDelivery = undefined
    wsClient.close()
    pendingByTempId.clear()
    bumpPendingRevision()
    discardPersistedDeliveries(props.participant.id)
    await props.onLogout()
  }

  const sendMessage = (event: Event): void => {
    event.preventDefault()
    const roomId = currentRoomId()
    const text = draft().trim()
    if (!roomId || !text || sending()) return
    if (pendingByTempId.size >= 50) {
      setError('待发送消息队列已满；请检查连接并处理失败消息后重试。')
      return
    }
    const blocks: MessageBlock[] = [{ type: 'text', content: text }]
    const replyTo = replyToId() || null
    const accepted = sendOptimistically(
      (clientMessageId) => wsClient.sendMessage(roomId, blocks, replyTo, clientMessageId),
      (delivery) => {
        const outbound = delivery.outbound
        if (!outbound || outbound.kind !== 'blocks') return null
        return addPendingMessage(roomId, blocks, replyTo, { ...delivery, outbound })
      },
      { kind: 'blocks', roomId, blocks, replyTo },
    )
    if (!accepted) {
      setError('实时连接尚未建立，消息未发送。')
      return
    }
    setError('')
    setSending(true)
    setDraft('')
    cancelReply()
    if (sendResetTimer !== undefined) window.clearTimeout(sendResetTimer)
    sendResetTimer = window.setTimeout(() => {
      sendResetTimer = undefined
      if (!disposed) setSending(false)
    }, 250)
  }

  onMount(() => {
    disposeReliableDelivery = initReliableDelivery({
      ws: wsClient,
      getPendingMap: () => pendingByTempId,
      onCanonical: settlePendingMessage,
      onRestore: restorePendingMessage,
      onPendingChanged: bumpPendingRevision,
      onFailure: () => {},
      onNotice: () => {},
    })
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
    sessionLoadGeneration += 1
    if (sendResetTimer !== undefined) window.clearTimeout(sendResetTimer)
    for (const off of unsubscribe) off()
    disposeReliableDelivery?.()
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
          <IrisDialog open={sessionPanelOpen()} onOpenChange={setSessionPanelVisibility}>
            <IrisDialogTrigger class="session-trigger">登录会话</IrisDialogTrigger>
            <IrisDialogContent class="session-dialog">
              <IrisDialogTitle>登录会话</IrisDialogTitle>
              <IrisDialogDescription>查看当前账户的活跃登录设备。退出其他设备不会结束当前会话。</IrisDialogDescription>
              <Show when={sessionRevokeError()}>
                <div class="session-error" role="alert">{sessionRevokeError()}</div>
              </Show>
              <Show when={sessionLoadState() === 'loading'}>
                <div class="session-state" role="status"><IrisSpinner size="sm" />正在加载登录会话…</div>
              </Show>
              <Show when={sessionLoadState() === 'error'}>
                <div class="session-error" role="alert">
                  <span>{sessionLoadError()}</span>
                  <IrisButton class="session-retry" variant="outline" size="sm" onClick={() => void loadSessions()}>
                    重试
                  </IrisButton>
                </div>
              </Show>
              <Show when={sessionLoadState() === 'loaded'}>
                <Show when={(sessionRows()?.length ?? 0) > 0} fallback={<p class="session-state">当前没有活跃登录会话。</p>}>
                  <ul class="session-list">
                    <For each={sessionRows() ?? []}>
                      {(session) => (
                        <li class="session-row">
                          <strong>{session.user_agent?.trim() || '未记录设备信息'}</strong>
                          <span>最近活动：<time datetime={session.last_seen_at}>{session.last_seen_at}</time></span>
                        </li>
                      )}
                    </For>
                  </ul>
                </Show>
              </Show>
              <div class="session-actions">
                <IrisButton
                  variant="outline"
                  disabled={!canRevokeOthers() || sessionLoadState() === 'loading' || revokingOthers()}
                  onClick={() => void revokeOtherSessions()}
                >
                  {revokingOthers() ? '正在退出其他设备…' : '退出其他设备'}
                </IrisButton>
                <IrisDialogClose class="session-close">关闭</IrisDialogClose>
              </div>
            </IrisDialogContent>
          </IrisDialog>
          <IrisButton variant="ghost" size="sm" onClick={() => void logoutManually()}>
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
                <Show when={recallFeedback()[currentRoomId()]}>
                  <IrisAlert
                    tone={recallFeedback()[currentRoomId()]?.type === 'info' ? 'info' : 'danger'}
                    class="inline-alert recall-feedback"
                    role="status"
                  >
                    {recallFeedback()[currentRoomId()]?.text}
                  </IrisAlert>
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
                          <MessageRow
                            message={message}
                            participant={props.participant}
                            isEditing={() => editingMessageId() === message.id}
                            editDraft={editDraft}
                            editOriginal={editOriginal}
                            editSaving={editSaving}
                            editError={editError}
                            editConflict={editConflict}
                            isRecalling={() => Boolean(recallingMessages()[message.id])}
                            isDeleting={() => Boolean(deletingMessages()[message.id])}
                            deleteFeedback={() => deleteFeedback()[message.id]}
                            reactions={() => reactionsByMessage()[message.id] ?? []}
                            isReacting={(emoji) => Boolean(reacting()[reactionKey(message.id, emoji)])}
                            reactionFeedback={() => reactionFeedback()[message.id]}
                            deliveryStatus={() => {
                              pendingRevision()
                              return message.delivery_status
                            }}
                            failureMessage={() => {
                              pendingRevision()
                              return message.failure_message
                            }}
                            persistenceWarning={() => {
                              pendingRevision()
                              return Boolean((message as Message & { persistence_warning?: boolean }).persistence_warning)
                            }}
                            clientMessageId={() => message.client_message_id}
                            replyParent={replyParent}
                            onBeginEdit={beginMessageEdit}
                            onRecall={(target) => void recallMessage(target)}
                            onDelete={(target) => void deleteMessage(target)}
                            onBeginReply={beginReply}
                            onToggleReaction={(target, emoji) => void toggleReaction(target, emoji)}
                            onRetryDelivery={(clientMessageId) => { retryPendingMessage(clientMessageId) }}
                            onEditDraftChange={setEditDraft}
                            onEditSubmit={(event, target) => void saveMessageEdit(event, target)}
                            onLoadLatest={loadLatestEditVersion}
                            onRebase={rebaseEditOnLatestVersion}
                            onCancelEdit={clearMessageEdit}
                          />
                        )}
                      </For>
                    </Show>
                  </Show>
                </section>

                <form class="composer" onSubmit={sendMessage}>
                  <Show when={replyToId()}>
                    <div class="composer-reply-context" role="status">
                      <div>
                        <strong>
                          {replyTarget()
                            ? `回复 ${participantLabel(replyTarget()!.sender_id, props.participant)}`
                            : '回复消息'}:
                        </strong>
                        <span>{replyPreview(replyTarget(), replyToId())}</span>
                      </div>
                      <IrisButton
                        type="button"
                        variant="ghost"
                        size="sm"
                        aria-label="取消回复"
                        onClick={cancelReply}
                      >取消</IrisButton>
                    </div>
                  </Show>
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
