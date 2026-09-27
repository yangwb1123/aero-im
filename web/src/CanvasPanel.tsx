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
  knownIds: string[]
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
      setCanvases(list)
      setAccessDenied(false)
      setListStatus('ready')
      const uncertain = uncertainCreates()[roomId]
      const reconciled = uncertain && list.find((row) =>
        row.title.trim() === uncertain.title && !uncertain.knownIds.includes(row.id))
      if (uncertain) {
        setUncertainCreates((current) => {
          const next = { ...current }
          delete next[roomId]
          return next
        })
        if (reconciled) setCreateTitle('')
      }
      const listPreferredId = reconciled?.id ?? preferredId
      const preferred = listPreferredId && list.some((row) => row.id === listPreferredId)
        ? listPreferredId
        : list[0]?.id
      if (preferred) await selectCanvas(preferred, roomToken)
      else clearCanvas()
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
    const knownIds = canvases().map((item) => item.id)
    inFlightCreateRooms.add(roomId)
    setCreatingRooms((current) => ({ ...current, [roomId]: true }))
    setError('')
    setRecoveryAction('none')
    try {
      const created = await api.createCanvas(roomId, {
        title,
        blocks: [{ type: 'text', content: '' }],
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
        setUncertainCreates((current) => ({
          ...current,
          [roomId]: { title, knownIds },
        }))
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
