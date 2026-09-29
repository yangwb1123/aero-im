import { createEffect, createSignal, on, onCleanup } from 'solid-js'
import type { Accessor, JSX } from 'solid-js'
import {
  bindCanvasRealtime,
  CANVAS_OP_PAGE_SIZE,
  CanvasReducer,
} from '../canvas_model.js'
import type { CanvasReducerSnapshot } from '../canvas_model.js'
import type { WsClient } from '../ws.js'
import type { ApiError, Canvas } from './api'
import { api } from './api'
import {
  clearCanvasCreateIntent,
  loadCanvasCreateIntent,
  saveCanvasCreateIntent,
} from './canvas_create_intents.js'
import { createCanvasEditor } from './canvas_editor'
import type { CanvasEditContext, RetainedCanvasDraft } from './canvas_editor'
import { CanvasPanelView } from './CanvasPanelView'
import { errorText } from './canvas_panel_utils'

interface CanvasPanelProps {
  roomId: Accessor<string>
  ws: WsClient
  participantId: string
  onDraftStateChange: (dirty: boolean) => void
}

type ListStatus = 'idle' | 'loading' | 'ready' | 'error'
type RecoveryAction = 'none' | 'list' | 'canvas' | 'sync'

interface UncertainCreate {
  title: string
  clientCreateId: string
  listGenerationAtFailure: number
}

function newClientCreateId(): string {
  // UUIDv7 keeps the timestamp in the high bits. The server maps those bits
  // directly to CanvasId's ULID value, preserving the Canvas sort invariant.
  const bytes = globalThis.crypto.getRandomValues(new Uint8Array(16))
  let timestamp = Date.now()
  for (let index = 5; index >= 0; index -= 1) {
    bytes[index] = timestamp % 256
    timestamp = Math.floor(timestamp / 256)
  }
  bytes[6] = (bytes[6] & 0x0f) | 0x70
  bytes[8] = (bytes[8] & 0x3f) | 0x80
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('')
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`
}

function canvasCreateIntentStorage(): Storage | null {
  try {
    return window.sessionStorage
  } catch {
    return null
  }
}

function createOutcomeMayBeUnknown(reason: unknown): boolean {
  if (!reason || typeof reason !== 'object' || !('status' in reason)) return true
  const status = Number((reason as { status?: unknown }).status)
  return status === 0 || status === 408 || status >= 500
}

export function CanvasPanel(props: CanvasPanelProps): JSX.Element {
  const [opened, setOpened] = createSignal(false)
  const [canvases, setCanvases] = createSignal<Canvas[]>([])
  const [listStatus, setListStatus] = createSignal<ListStatus>('idle')
  const [activeId, setActiveId] = createSignal('')
  const [canvasLoading, setCanvasLoading] = createSignal(false)
  const [snapshot, setSnapshot] = createSignal<CanvasReducerSnapshot>()
  const [error, setError] = createSignal('')
  const [recoveryAction, setRecoveryAction] = createSignal<RecoveryAction>('none')
  const [accessDenied, setAccessDenied] = createSignal(false)
  const [createTitle, setCreateTitle] = createSignal('')
  const [creatingRooms, setCreatingRooms] = createSignal<Record<string, boolean>>({})
  const [uncertainCreates, setUncertainCreates] = createSignal<Record<string, UncertainCreate>>({})
  const [saveStatus, setSaveStatus] = createSignal('')
  const creating = (): boolean => Boolean(creatingRooms()[currentRoomId()])
  const creationUncertain = (): boolean => Boolean(uncertainCreates()[currentRoomId()])
  const inFlightCreateRooms = new Set<string>()
  let reducer: CanvasReducer | null = null
  let disposed = false
  // The context generation invalidates requests on room changes and panel close/reopen.
  let roomGeneration = 0
  let listGeneration = 0
  let selectionGeneration = 0
  let activeCatchup: { key: string; promise: Promise<void> } | null = null

  const currentRoomId = (): string => props.roomId()
  const isCurrentRoom = (roomId: string, generation: number): boolean => (
    roomId === currentRoomId() && generation === roomGeneration
  )
  const isCurrentSelection = (
    roomId: string,
    roomToken: number,
    canvasId: string,
    selectionToken: number,
  ): boolean => isCurrentRoom(roomId, roomToken)
    && activeId() === canvasId && selectionToken === selectionGeneration

  const getEditContext = (): CanvasEditContext | null => {
    const roomId = currentRoomId()
    const canvasId = activeId()
    if (!roomId || !canvasId || !reducer) return null
    return {
      roomId,
      canvasId,
      roomToken: roomGeneration,
      selectionToken: selectionGeneration,
      reducer,
    }
  }

  const editor = createCanvasEditor({
    participantId: props.participantId,
    getContext: getEditContext,
    isCurrent: (context) => isCurrentSelection(
      context.roomId,
      context.roomToken,
      context.canvasId,
      context.selectionToken,
    ) && reducer === context.reducer,
    applyReducer: () => applyReducer(),
    catchUp: (verifySnapshot) => catchUp(verifySnapshot),
    onError: (message, action) => {
      setError(message)
      setRecoveryAction(action)
    },
    onForbidden: () => clearForbiddenContent(),
    onStatus: setSaveStatus,
  })

  const clearCanvas = (clearDrafts = true): void => {
    selectionGeneration += 1
    reducer = null
    setActiveId('')
    setCanvasLoading(false)
    setSnapshot(undefined)
    if (clearDrafts) editor.clearDrafts()
    else editor.invalidateContext()
    setSaveStatus('')
  }

  const clearForbiddenContent = (): void => {
    // Remove protected server content while retaining only user-entered drafts.
    clearCanvas(false)
    setCanvases([])
    setListStatus('error')
    setRecoveryAction('none')
    setAccessDenied(true)
  }

  const applyReducer = (): void => {
    if (!reducer) return
    const next = reducer.snapshot()
    setSnapshot(next)
    editor.syncTextDraft(next)
  }

  const fetchOps = async (
    roomId: string,
    roomToken: number,
    canvasId: string,
    selectionToken: number,
    target: CanvasReducer,
  ): Promise<void> => {
    for (;;) {
      const before = target.cursor
      const payload = await api.listCanvasOps(roomId, canvasId, {
        since: before,
        limit: CANVAS_OP_PAGE_SIZE,
      })
      if (!isCurrentSelection(roomId, roomToken, canvasId, selectionToken)
        || reducer !== target) return
      const rows = Array.isArray(payload?.ops) ? payload.ops : []
      const result = target.ingestMany(rows)
      if (result.conflict) throw new Error('操作序号冲突，请重新打开 Canvas。')
      if (rows.length > 0 && target.cursor <= before) {
        throw new Error('操作日志没有前进，请重新打开 Canvas。')
      }
      if (rows.length < CANVAS_OP_PAGE_SIZE) break
    }
    if (target.pending.size > 0 || target.pendingUnsequenced.size > 0) {
      throw new Error('操作日志仍有缺口，请稍后重试。')
    }
  }

  const selectCanvas = async (
    canvasId: string,
    roomToken = roomGeneration,
    forceReload = false,
  ): Promise<void> => {
    const roomId = currentRoomId()
    if (!roomId || !canvasId || !isCurrentRoom(roomId, roomToken)) return
    if (!forceReload && canvasId === activeId() && reducer) return
    if (editor.hasUnsavedEdits()
      && !window.confirm('切换 Canvas 会放弃尚未保存的编辑，是否继续？')) return
    const selectionToken = ++selectionGeneration
    reducer = null
    setActiveId(canvasId)
    setSnapshot(undefined)
    setCanvasLoading(true)
    setError('')
    setRecoveryAction('none')
    setSaveStatus('')
    editor.clearDrafts()
    try {
      const canvas = await api.getCanvas(roomId, canvasId)
      if (!isCurrentSelection(roomId, roomToken, canvasId, selectionToken)) return
      const nextReducer = new CanvasReducer(canvas as unknown as Record<string, unknown>)
      reducer = nextReducer
      applyReducer()
      await fetchOps(roomId, roomToken, canvasId, selectionToken, nextReducer)
      if (!isCurrentSelection(roomId, roomToken, canvasId, selectionToken)) return
      applyReducer()
    } catch (reason) {
      if (!isCurrentSelection(roomId, roomToken, canvasId, selectionToken)) return
      setError(errorText(reason, 'Canvas 加载失败'))
      setRecoveryAction((reason as Partial<ApiError> | null)?.status === 404 ? 'list' : 'canvas')
      if ((reason as Partial<ApiError> | null)?.status === 403) clearForbiddenContent()
    } finally {
      if (isCurrentSelection(roomId, roomToken, canvasId, selectionToken)) {
        setCanvasLoading(false)
      }
    }
  }

  const loadList = async (
    roomId: string,
    roomToken: number,
    preferredId?: string,
  ): Promise<void> => {
    if (!roomId || !isCurrentRoom(roomId, roomToken)) return
    const listToken = ++listGeneration
    selectionGeneration += 1
    const isCurrentList = (): boolean => isCurrentRoom(roomId, roomToken)
      && listToken === listGeneration
    setListStatus('loading')
    setError('')
    setRecoveryAction('none')
    try {
      const rows = await api.listCanvases(roomId)
      if (!isCurrentList()) return
      const list = Array.isArray(rows) ? rows : []
      const uncertain = uncertainCreates()[roomId]
      // A list request already in flight when the create became uncertain cannot
      // prove whether the server committed that create. Ignore it entirely.
      if (uncertain && listToken <= uncertain.listGenerationAtFailure) return
      setCanvases(list)
      setAccessDenied(false)
      setListStatus('ready')
      // Titles are not identities: a same-titled Canvas may belong to another
      // creation. While uncertain, only keep an explicitly selected existing
      // Canvas; never auto-select a possible match or clear the uncertainty.
      const preferred = preferredId && list.some((row) => row.id === preferredId)
        ? preferredId
        : uncertain ? undefined : list[0]?.id
      if (preferred) await selectCanvas(preferred, roomToken)
      else if (!uncertain) clearCanvas()
      if (!isCurrentList()) return
      if (uncertain) {
        setError('创建结果仍无法确认；列表中的同名 Canvas 不能证明是本次创建。请安全重试以复用原请求 ID。')
        setRecoveryAction('list')
      }
    } catch (reason) {
      if (!isCurrentList()) return
      setListStatus('error')
      setError(errorText(reason, 'Canvas 列表加载失败'))
      setRecoveryAction('list')
      if ((reason as Partial<ApiError> | null)?.status === 403) clearForbiddenContent()
    }
  }

  const openPanel = (): void => {
    if (opened()) {
      if (editor.hasUnsavedEdits()
        && !window.confirm('关闭面板会放弃尚未保存的编辑，是否继续？')) return
      roomGeneration += 1
      listGeneration += 1
      setOpened(false)
      clearCanvas()
      setError('')
      setRecoveryAction('none')
      return
    }
    const roomId = currentRoomId()
    if (!roomId) return
    setOpened(true)
    const roomToken = ++roomGeneration
    setAccessDenied(false)
    clearCanvas()
    void loadList(roomId, roomToken)
  }

  const performCreate = async (
    roomId: string,
    roomToken: number,
    title: string,
    clientCreateId: string,
  ): Promise<void> => {
    if (!roomId || accessDenied() || creatingRooms()[roomId]
      || inFlightCreateRooms.has(roomId)) return
    // Persist before sending: a reload between the server commit and response
    // must retry with the same operation identity.
    const intentSaved = saveCanvasCreateIntent(canvasCreateIntentStorage(), props.participantId, roomId, {
      title,
      clientCreateId,
    })
    if (!intentSaved) {
      if (isCurrentRoom(roomId, roomToken)) {
        setError('浏览器会话存储不可用，创建请求尚未发送。启用会话存储后再安全重试。')
        setRecoveryAction('none')
      }
      return
    }
    inFlightCreateRooms.add(roomId)
    setCreatingRooms((current) => ({ ...current, [roomId]: true }))
    setError('')
    setRecoveryAction('none')
    try {
      const created = await api.createCanvas(roomId, {
        title,
        clientCreateId,
        blocks: [{ type: 'text', content: '' }],
      })
      clearCanvasCreateIntent(
        canvasCreateIntentStorage(),
        props.participantId,
        roomId,
        clientCreateId,
      )
      setUncertainCreates((current) => {
        if (current[roomId]?.clientCreateId !== clientCreateId) return current
        const next = { ...current }
        delete next[roomId]
        return next
      })
      if (!isCurrentRoom(roomId, roomToken)) return
      // Invalidate any list started before the create completed so it cannot
      // replace the newly created item after this response.
      listGeneration += 1
      setCreateTitle('')
      setCanvases((current) => [created, ...current.filter((item) => item.id !== created.id)])
      setAccessDenied(false)
      setListStatus('ready')
      await selectCanvas(created.id, roomToken)
    } catch (reason) {
      if (createOutcomeMayBeUnknown(reason)) {
        saveCanvasCreateIntent(canvasCreateIntentStorage(), props.participantId, roomId, {
          title,
          clientCreateId,
        })
        setUncertainCreates((current) => {
          const existing = current[roomId]
          if (existing && existing.clientCreateId !== clientCreateId) return current
          return {
            ...current,
            [roomId]: { title, clientCreateId, listGenerationAtFailure: listGeneration },
          }
        })
        if (isCurrentRoom(roomId, roomToken) && listStatus() === 'loading') {
          setListStatus('error')
        }
      } else if (uncertainCreates()[roomId]?.clientCreateId !== clientCreateId) {
        clearCanvasCreateIntent(
          canvasCreateIntentStorage(),
          props.participantId,
          roomId,
          clientCreateId,
        )
      }
      if (!isCurrentRoom(roomId, roomToken)) return
      setError(errorText(reason, 'Canvas 创建失败'))
      setRecoveryAction('list')
      if ((reason as Partial<ApiError> | null)?.status === 403) clearForbiddenContent()
    } finally {
      inFlightCreateRooms.delete(roomId)
      if (!disposed) {
        setCreatingRooms((current) => {
          const next = { ...current }
          delete next[roomId]
          return next
        })
      }
    }
  }

  const createCanvas = async (event: SubmitEvent): Promise<void> => {
    event.preventDefault()
    const roomId = currentRoomId()
    const roomToken = roomGeneration
    const title = createTitle().trim()
    if (!roomId || accessDenied() || creatingRooms()[roomId]
      || inFlightCreateRooms.has(roomId) || uncertainCreates()[roomId]) return
    if (!title) {
      setError('请输入 Canvas 标题。')
      setRecoveryAction('none')
      return
    }
    let clientCreateId: string
    try {
      clientCreateId = newClientCreateId()
    } catch {
      setError('无法生成安全的 Canvas 创建标识，请检查浏览器加密支持后重试。')
      setRecoveryAction('none')
      return
    }
    await performCreate(roomId, roomToken, title, clientCreateId)
  }

  const retryUncertainCreate = async (): Promise<void> => {
    const roomId = currentRoomId()
    const pending = uncertainCreates()[roomId]
    if (!roomId || !pending || accessDenied() || creatingRooms()[roomId]
      || inFlightCreateRooms.has(roomId)) return
    await performCreate(roomId, roomGeneration, pending.title, pending.clientCreateId)
  }

  const catchUp = async (verifySnapshot = true): Promise<void> => {
    const roomId = currentRoomId()
    const roomToken = roomGeneration
    const canvasId = activeId()
    const target = reducer
    const selectionToken = selectionGeneration
    if (!opened() || !roomId || !canvasId || !target) return
    const key = `${roomToken}:${selectionToken}:${canvasId}`
    if (activeCatchup?.key === key) return activeCatchup.promise
    const previousSaveStatus = saveStatus()
    const promise = (async () => {
      setSaveStatus('正在同步…')
      try {
        let current = target
        if (verifySnapshot) {
          const latest = await api.getCanvas(roomId, canvasId)
          if (!isCurrentSelection(roomId, roomToken, canvasId, selectionToken)) return
          if (Number(latest.version) !== current.version
            || Number(latest.snapshot_op_seq ?? 0) > current.cursor) {
            current = new CanvasReducer(latest as unknown as Record<string, unknown>)
            reducer = current
            applyReducer()
          }
        }
        await fetchOps(roomId, roomToken, canvasId, selectionToken, current)
        if (!isCurrentSelection(roomId, roomToken, canvasId, selectionToken)) return
        applyReducer()
        if (editor.hasPendingSubmission()) {
          setSaveStatus(previousSaveStatus)
        } else if (editor.hasUnsavedEdits()) {
          setSaveStatus('有未保存草稿')
          setError('')
          setRecoveryAction('none')
        } else {
          setSaveStatus('已同步')
          setError('')
          setRecoveryAction('none')
        }
      } catch (reason) {
        if (!isCurrentSelection(roomId, roomToken, canvasId, selectionToken)) return
        setSaveStatus('同步失败')
        setError(errorText(reason, 'Canvas 同步失败'))
        setRecoveryAction('sync')
        if ((reason as Partial<ApiError> | null)?.status === 403) clearForbiddenContent()
      }
    })()
    activeCatchup = { key, promise }
    try {
      await promise
    } finally {
      if (activeCatchup?.key === key) activeCatchup = null
    }
  }

  createEffect(() => {
    props.onDraftStateChange(editor.hasUnsavedEdits())
  })

  createEffect(on(props.roomId, (roomId) => {
    roomGeneration += 1
    selectionGeneration += 1
    setAccessDenied(false)
    reducer = null
    activeCatchup = null
    setCanvases([])
    setListStatus('idle')
    setError('')
    setRecoveryAction('none')
    clearCanvas()
    if (roomId) {
      const pendingCreate = loadCanvasCreateIntent(
        canvasCreateIntentStorage(),
        props.participantId,
        roomId,
      )
      if (pendingCreate) {
        setUncertainCreates((current) => current[roomId]
          ? current
          : {
            ...current,
            [roomId]: { ...pendingCreate, listGenerationAtFailure: -1 },
          })
      }
    }
    if (opened() && roomId) void loadList(roomId, roomGeneration)
  }))

  const unbindCanvasRealtime = bindCanvasRealtime(props.ws, {
    onCanvasOp: (frame) => {
      const roomId = currentRoomId()
      if (!roomId || frame?.room_id !== roomId || !opened()) return
      const canvasId = String(frame.canvas_id ?? '')
      if (!canvasId || canvasId !== activeId() || !reducer) return
      const result = reducer.ingest(frame)
      if (result.status === 'conflict') {
        setError('Canvas 操作序号冲突，正在重新同步。')
        setRecoveryAction('sync')
        void catchUp(true)
        return
      }
      if (result.applied.length > 0 || result.status === 'queued') applyReducer()
      if (result.gap) {
        setSaveStatus('正在补齐操作…')
        void catchUp(false)
      }
    },
    onReconnect: () => {
      if (opened() && activeId() && reducer) void catchUp(true)
    },
  })

  onCleanup(() => {
    disposed = true
    props.onDraftStateChange(false)
    roomGeneration += 1
    listGeneration += 1
    selectionGeneration += 1
    unbindCanvasRealtime()
  })

  return (
    <CanvasPanelView
      opened={opened}
      currentRoomId={currentRoomId}
      error={error}
      recoveryAction={recoveryAction}
      accessDenied={accessDenied}
      retainedDrafts={editor.retainedDrafts}
      creationUncertain={creationUncertain}
      createTitle={createTitle}
      creating={creating}
      canvases={canvases}
      listStatus={listStatus}
      activeId={activeId}
      canvasLoading={canvasLoading}
      snapshot={snapshot}
      saveStatus={saveStatus}
      textDraft={editor.textDraft}
      noteDraft={editor.noteDraft}
      noteDrafts={editor.noteDrafts}
      notePendingDeletion={editor.notePendingDeletion}
      structuredDraft={editor.structuredDraft}
      saving={editor.saving}
      onToggle={openPanel}
      onRetryList={() => void loadList(currentRoomId(), roomGeneration, activeId() || undefined)}
      onRetryCreate={() => void retryUncertainCreate()}
      onRetryCanvas={() => void selectCanvas(activeId(), roomGeneration, true)}
      onRetrySync={() => void catchUp(true)}
      onCreate={(submitEvent) => void createCanvas(submitEvent)}
      onCreateTitleInput={setCreateTitle}
      onSelectCanvas={(canvasId) => void selectCanvas(canvasId)}
      onSaveText={(submitEvent) => void editor.saveText(submitEvent)}
      onTextInput={(value) => {
        editor.setTextDirty(true)
        editor.setTextDraft(value)
      }}
      onUpdateNote={(id, existing) => void editor.updateNote(id, existing)}
      onRequestDeleteNote={editor.setNotePendingDeletion}
      onConfirmDeleteNote={(id) => void editor.deleteNote(id)}
      onCancelDeleteNote={() => editor.setNotePendingDeletion('')}
      onNoteInput={(id, original, value) => editor.setNoteDraftValue(id, original, value)}
      onNoteCreateInput={editor.setNoteDraft}
      onAddNote={(submitEvent) => void editor.addNote(submitEvent)}
      onStructuredInput={editor.setStructuredDraft}
      onSubmitStructured={(submitEvent) => void editor.submitStructured(submitEvent)}
    />
  )
}
