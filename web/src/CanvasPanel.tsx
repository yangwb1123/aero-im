import { createEffect, createSignal, For, on, onCleanup, Show } from 'solid-js'
import type { Accessor, JSX } from 'solid-js'
import {
  bindCanvasRealtime,
  CANVAS_OP_PAGE_SIZE,
  CanvasReducer,
  isSupportedCanvasOp,
} from '../canvas_model.js'
import type { WsClient } from '../ws.js'
import type { ApiError, Canvas, CanvasOperation } from './api'
import { api } from './api'
import {
  appendCanvasOperationWithRetry,
  createCanvasClientOpId,
} from './canvas_submission.js'
import type { CanvasReducerSnapshot } from '../canvas_model.js'

interface CanvasPanelProps {
  roomId: Accessor<string>
  ws: WsClient
  participantId: string
}

type ListStatus = 'idle' | 'loading' | 'ready' | 'error'

interface PendingSubmission {
  key: string
  id: string
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function noteId(): string {
  if (globalThis.crypto?.randomUUID) return globalThis.crypto.randomUUID()
  return `note-${Date.now()}-${Math.random().toString(16).slice(2)}`
}

function errorText(reason: unknown, action: string): string {
  const error = reason as Partial<ApiError> | null
  if (error?.status === 401) return '登录已过期，请重新登录后继续。'
  if (error?.status === 403) return '没有权限访问此房间的 Canvas。'
  const message = reason instanceof Error ? reason.message : String(reason ?? '未知错误')
  return `${action}：${message}`
}

function textFrom(snapshot: CanvasReducerSnapshot | undefined): string {
  const block = snapshot?.blocks.find((value) => isRecord(value) && value.type === 'text')
  return isRecord(block) ? String(block.content ?? block.text ?? '') : ''
}

export function CanvasPanel(props: CanvasPanelProps): JSX.Element {
  const [opened, setOpened] = createSignal(false)
  const [canvases, setCanvases] = createSignal<Canvas[]>([])
  const [listStatus, setListStatus] = createSignal<ListStatus>('idle')
  const [activeId, setActiveId] = createSignal('')
  const [canvasLoading, setCanvasLoading] = createSignal(false)
  const [snapshot, setSnapshot] = createSignal<CanvasReducerSnapshot>()
  const [error, setError] = createSignal('')
  const [createTitle, setCreateTitle] = createSignal('')
  const [creating, setCreating] = createSignal(false)
  const [textDraft, setTextDraft] = createSignal('')
  const [textDirty, setTextDirty] = createSignal(false)
  const [noteDraft, setNoteDraft] = createSignal('')
  const [pendingNoteId, setPendingNoteId] = createSignal('')
  const [noteDrafts, setNoteDrafts] = createSignal<Record<string, string>>({})
  const [structuredDraft, setStructuredDraft] = createSignal('')
  const [saving, setSaving] = createSignal(false)
  const [saveStatus, setSaveStatus] = createSignal('')
  let reducer: CanvasReducer | null = null
  let roomGeneration = 0
  let selectionGeneration = 0
  let pendingSubmission: PendingSubmission | null = null
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

  const clearCanvas = (): void => {
    selectionGeneration += 1
    reducer = null
    pendingSubmission = null
    setActiveId('')
    setCanvasLoading(false)
    setSnapshot(undefined)
    setTextDraft('')
    setTextDirty(false)
    setNoteDraft('')
    setPendingNoteId('')
    setNoteDrafts({})
    setStructuredDraft('')
    setSaveStatus('')
  }

  const clearForbiddenContent = (): void => {
    clearCanvas()
    setCanvases([])
  }

  const applyReducer = (): void => {
    if (!reducer) return
    const next = reducer.snapshot()
    setSnapshot(next)
    if (!textDirty()) setTextDraft(textFrom(next))
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

  const selectCanvas = async (canvasId: string, roomToken = roomGeneration): Promise<void> => {
    const roomId = currentRoomId()
    if (!roomId || !canvasId || !isCurrentRoom(roomId, roomToken)) return
    const selectionToken = ++selectionGeneration
    reducer = null
    pendingSubmission = null
    setActiveId(canvasId)
    setSnapshot(undefined)
    setCanvasLoading(true)
    setError('')
    setSaveStatus('')
    setTextDraft('')
    setTextDirty(false)
    setNoteDraft('')
    setPendingNoteId('')
    setNoteDrafts({})
    setStructuredDraft('')
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
    setListStatus('loading')
    setError('')
    try {
      const rows = await api.listCanvases(roomId)
      if (!isCurrentRoom(roomId, roomToken)) return
      const list = Array.isArray(rows) ? rows : []
      setCanvases(list)
      setListStatus('ready')
      const preferred = preferredId && list.some((row) => row.id === preferredId)
        ? preferredId
        : list[0]?.id
      if (preferred) await selectCanvas(preferred, roomToken)
      else clearCanvas()
    } catch (reason) {
      if (!isCurrentRoom(roomId, roomToken)) return
      setListStatus('error')
      setError(errorText(reason, 'Canvas 列表加载失败'))
      if ((reason as Partial<ApiError> | null)?.status === 403) clearForbiddenContent()
    }
  }

  const openPanel = (): void => {
    if (opened()) {
      setOpened(false)
      return
    }
    const roomId = currentRoomId()
    if (!roomId) return
    setOpened(true)
    const roomToken = roomGeneration
    clearCanvas()
    void loadList(roomId, roomToken)
  }

  const createCanvas = async (event: SubmitEvent): Promise<void> => {
    event.preventDefault()
    const roomId = currentRoomId()
    const roomToken = roomGeneration
    const title = createTitle().trim()
    if (!roomId) return
    if (!title) {
      setError('请输入 Canvas 标题。')
      return
    }
    setCreating(true)
    setError('')
    try {
      const created = await api.createCanvas(roomId, {
        title,
        blocks: [{ type: 'text', content: '' }],
      })
      if (!isCurrentRoom(roomId, roomToken)) return
      setCreateTitle('')
      setCanvases((current) => [created, ...current.filter((item) => item.id !== created.id)])
      setListStatus('ready')
      await selectCanvas(created.id, roomToken)
    } catch (reason) {
      if (!isCurrentRoom(roomId, roomToken)) return
      setError(errorText(reason, 'Canvas 创建失败'))
      if ((reason as Partial<ApiError> | null)?.status === 403) clearForbiddenContent()
    } finally {
      if (isCurrentRoom(roomId, roomToken)) setCreating(false)
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
        setSaveStatus('已同步')
      } catch (reason) {
        if (!isCurrentSelection(roomId, roomToken, canvasId, selectionToken)) return
        setSaveStatus('同步失败')
        setError(errorText(reason, 'Canvas 同步失败'))
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

  const submitOperation = async (op: Record<string, unknown>): Promise<boolean> => {
    const roomId = currentRoomId()
    const canvasId = activeId()
    const target = reducer
    const roomToken = roomGeneration
    const selectionToken = selectionGeneration
    if (!roomId || !canvasId || !target || saving()) return false
    const key = JSON.stringify({ roomId, canvasId, op })
    if (!pendingSubmission || pendingSubmission.key !== key) {
      pendingSubmission = { key, id: createCanvasClientOpId() }
    }
    const operationId = pendingSubmission.id
    setSaving(true)
    setError('')
    setSaveStatus('保存中…')
    try {
      const result = await appendCanvasOperationWithRetry(
        api,
        roomId,
        canvasId,
        op,
        operationId,
      )
      if (!isCurrentSelection(roomId, roomToken, canvasId, selectionToken)
        || reducer !== target) return false
      const applied = target.ingest(result.operation as CanvasOperation)
      if (applied.status === 'conflict') throw new Error('操作序号冲突，请重新同步。')
      pendingSubmission = null
      if (!isSupportedCanvasOp(op)) {
        setSaveStatus(`已记录未渲染操作：${String(op.type ?? '(无类型)')}`)
      } else {
        setSaveStatus('已保存')
      }
      applyReducer()
      if (applied.gap || target.pending.size > 0) await catchUp(false)
      return true
    } catch (reason) {
      if (!isCurrentSelection(roomId, roomToken, canvasId, selectionToken)) return false
      setSaveStatus('保存失败')
      setError(errorText(reason, '保存失败'))
      if ((reason as Partial<ApiError> | null)?.status === 403) clearForbiddenContent()
      return false
    } finally {
      if (isCurrentSelection(roomId, roomToken, canvasId, selectionToken)) setSaving(false)
    }
  }

  const saveText = async (event: SubmitEvent): Promise<void> => {
    event.preventDefault()
    const submittedText = textDraft()
    if (await submitOperation({ type: 'set_text', text: submittedText })
      && textDraft() === submittedText) {
      setTextDirty(false)
      setTextDraft(textFrom(snapshot()))
    }
  }

  const addNote = async (event: SubmitEvent): Promise<void> => {
    event.preventDefault()
    const draft = noteDraft()
    const text = draft.trim()
    if (!text) {
      setError('请输入便笺内容。')
      return
    }
    const id = pendingNoteId() || noteId()
    setPendingNoteId(id)
    const ok = await submitOperation({
      type: 'add_note',
      note_id: id,
      text,
      author_id: props.participantId,
    })
    if (ok) {
      if (noteDraft() === draft) setNoteDraft('')
      setPendingNoteId('')
    }
  }

  const updateNote = async (id: string, existing: string): Promise<void> => {
    const text = noteDrafts()[id] ?? existing
    if (await submitOperation({ type: 'update_note', note_id: id, text })) {
      setNoteDrafts((current) => {
        const next = { ...current }
        delete next[id]
        return next
      })
    }
  }

  const deleteNote = async (id: string): Promise<void> => {
    if (await submitOperation({ type: 'delete_note', note_id: id })) {
      setNoteDrafts((current) => {
        const next = { ...current }
        delete next[id]
        return next
      })
    }
  }

  const submitStructured = async (event: SubmitEvent): Promise<void> => {
    event.preventDefault()
    const submittedDraft = structuredDraft()
    let op: unknown
    try {
      op = JSON.parse(submittedDraft)
      if (!isRecord(op)) throw new Error('操作必须是 JSON 对象。')
    } catch (reason) {
      setError(errorText(reason, 'JSON 无效'))
      return
    }
    if (await submitOperation(op) && structuredDraft() === submittedDraft) setStructuredDraft('')
  }

  createEffect(on(props.roomId, (roomId) => {
    roomGeneration += 1
    selectionGeneration += 1
    reducer = null
    pendingSubmission = null
    activeCatchup = null
    setCanvases([])
    setListStatus('idle')
    setError('')
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
  onCleanup(unbindCanvasRealtime)

  return (
    <section class="canvas-workspace">
      <button
        type="button"
        class="canvas-toggle"
        aria-expanded={opened()}
        onClick={openPanel}
      >
        {opened() ? '关闭 Canvas' : '协作 Canvas'}
      </button>
      <Show when={opened()}>
        <div class="canvas-panel" aria-label="房间协作 Canvas">
          <header class="canvas-panel-header">
            <div>
              <span class="eyebrow">COLLABORATION</span>
              <h2>房间 Canvas</h2>
            </div>
            <span class="canvas-room-label">当前房间 · {currentRoomId().slice(0, 12)}</span>
          </header>

          <Show when={error()}>
            <div class="canvas-error" role="alert">{error()}</div>
          </Show>

          <form class="canvas-create-form" data-testid="canvas-create-form" onSubmit={createCanvas}>
            <input
              aria-label="新 Canvas 标题"
              maxlength={512}
              placeholder="新 Canvas 标题"
              value={createTitle()}
              onInput={(event) => setCreateTitle(event.currentTarget.value)}
            />
            <button type="submit" disabled={creating()}>{creating() ? '创建中…' : '创建 Canvas'}</button>
          </form>

          <div class="canvas-columns">
            <aside class="canvas-list" aria-label="Canvas 列表">
              <Show when={listStatus() === 'loading'}>
                <p class="canvas-state">正在加载 Canvas 列表…</p>
              </Show>
              <Show when={listStatus() === 'error'}>
                <p class="canvas-state">Canvas 列表加载失败。</p>
              </Show>
              <Show when={listStatus() === 'ready' && canvases().length === 0}>
                <p class="canvas-state">此房间还没有 Canvas。</p>
              </Show>
              <For each={canvases()}>
                {(canvas) => (
                  <button
                    type="button"
                    class="canvas-list-item"
                    classList={{ active: canvas.id === activeId() }}
                    aria-current={canvas.id === activeId() ? 'true' : undefined}
                    onClick={() => void selectCanvas(canvas.id)}
                  >
                    <strong>{canvas.title || '未命名 Canvas'}</strong>
                    <small>快照 v{canvas.version ?? 0}</small>
                  </button>
                )}
              </For>
            </aside>

            <main class="canvas-editor">
              <Show when={canvasLoading()}>
                <p class="canvas-state">正在读取 Canvas 快照与操作日志…</p>
              </Show>
              <Show when={!canvasLoading() && snapshot()} fallback={null}>
                <>
                  <header class="canvas-document-header">
                    <div>
                      <h3>{snapshot()?.title || '未命名 Canvas'}</h3>
                      <p>
                        快照 v{snapshot()?.version} · 操作 #{snapshot()?.op_seq}
                        {(snapshot()?.unknown_ops ?? 0) > 0
                          ? ` · ${snapshot()?.unknown_ops} 个未渲染操作（日志游标已前进）`
                          : ''}
                        {(snapshot()?.pending_unsequenced ?? 0) > 0
                          ? ` · ${snapshot()?.pending_unsequenced} 个等待持久序号的实时操作`
                          : ''}
                      </p>
                    </div>
                    <span class="canvas-save-status" aria-live="polite">{saveStatus()}</span>
                  </header>

                  <form class="canvas-edit-form" onSubmit={saveText}>
                    <label for="canvas-text-draft">正文</label>
                    <textarea
                      id="canvas-text-draft"
                      aria-label="Canvas 正文"
                      rows="5"
                      value={textDraft()}
                      onInput={(event) => {
                        setTextDirty(true)
                        setTextDraft(event.currentTarget.value)
                      }}
                    />
                    <button type="submit" disabled={saving()}>保存正文</button>
                  </form>

                  <section class="canvas-notes" aria-label="便笺">
                    <h4>便笺</h4>
                    <For each={snapshot()?.blocks.filter((block) => isRecord(block) && block.type === 'note') ?? []}>
                      {(block) => {
                        const id = String((block as Record<string, unknown>).note_id ?? '')
                        const original = String((block as Record<string, unknown>).content
                          ?? (block as Record<string, unknown>).text ?? '')
                        return (
                          <article class="canvas-note">
                            <textarea
                              aria-label={`便笺 ${id}`}
                              value={noteDrafts()[id] ?? original}
                              onInput={(event) => setNoteDrafts((current) => ({
                                ...current,
                                [id]: event.currentTarget.value,
                              }))}
                            />
                            <div class="canvas-note-actions">
                              <button type="button" disabled={saving()} onClick={() => void updateNote(id, original)}>
                                更新便笺
                              </button>
                              <button type="button" disabled={saving()} onClick={() => void deleteNote(id)}>
                                删除便笺
                              </button>
                            </div>
                          </article>
                        )
                      }}
                    </For>
                    <Show when={(snapshot()?.blocks.filter((block) => isRecord(block) && block.type === 'note').length ?? 0) === 0}>
                      <p class="canvas-state">暂无便笺。</p>
                    </Show>
                    <form class="canvas-note-create" onSubmit={addNote}>
                      <input
                        aria-label="新便笺内容"
                        placeholder="添加便笺"
                        value={noteDraft()}
                        onInput={(event) => setNoteDraft(event.currentTarget.value)}
                      />
                      <button type="submit" disabled={saving()}>添加便笺</button>
                    </form>
                  </section>

                  <section class="canvas-structured" aria-label="结构化内容">
                    <h4>结构化内容</h4>
                    <For each={snapshot()?.blocks.filter((block) => !(
                      isRecord(block) && (block.type === 'text' || block.type === 'note')
                    )) ?? []}>
                      {(block) => <pre>{JSON.stringify(block, null, 2)}</pre>}
                    </For>
                    <form class="canvas-structured-form" onSubmit={submitStructured}>
                      <label for="canvas-structured-draft">
                        JSON 操作（set_blocks / upsert_block / delete_block 等）
                      </label>
                      <textarea
                        id="canvas-structured-draft"
                        aria-label="结构化操作 JSON"
                        rows="4"
                        value={structuredDraft()}
                        placeholder={'{"type":"upsert_block","block":{"id":"status","type":"status","value":"ready"}}'}
                        onInput={(event) => setStructuredDraft(event.currentTarget.value)}
                      />
                      <button type="submit" disabled={saving()}>提交操作</button>
                    </form>
                  </section>
                </>
              </Show>
              <Show when={!canvasLoading() && listStatus() === 'ready' && !activeId()}>
                <p class="canvas-state">选择或新建一个 Canvas。</p>
              </Show>
            </main>
          </div>
        </div>
      </Show>
    </section>
  )
}
