import { createSignal, onCleanup } from 'solid-js'
import type { CanvasReducer } from '../canvas_model.js'
import { isSupportedCanvasOp } from '../canvas_model.js'
import { api, type CanvasOperation } from './api'
import {
  appendCanvasOperationWithRetry,
  createCanvasClientOpId,
} from './canvas_submission.js'
import { errorText, isRecord, textFrom } from './canvas_panel_utils'
import type { CanvasReducerSnapshot } from '../canvas_model.js'

export interface RetainedCanvasDraft {
  label: string
  value: string
}

export interface CanvasEditContext {
  roomId: string
  canvasId: string
  roomToken: number
  selectionToken: number
  reducer: CanvasReducer
}

interface CanvasEditorOptions {
  participantId: string
  getContext: () => CanvasEditContext | null
  isCurrent: (context: CanvasEditContext) => boolean
  applyReducer: () => void
  catchUp: (verifySnapshot?: boolean) => Promise<void>
  onError: (message: string, recovery: 'none' | 'sync') => void
  onForbidden: () => void
  onStatus: (status: string) => void
}

interface PendingSubmission {
  key: string
  id: string
}

function noteId(): string {
  if (globalThis.crypto?.randomUUID) return globalThis.crypto.randomUUID()
  return `note-${Date.now()}-${Math.random().toString(16).slice(2)}`
}

export function createCanvasEditor(options: CanvasEditorOptions) {
  const [textDraft, setTextDraft] = createSignal('')
  const [textDirty, setTextDirty] = createSignal(false)
  const [noteDraft, setNoteDraft] = createSignal('')
  const [pendingNoteId, setPendingNoteId] = createSignal('')
  const [noteDrafts, setNoteDrafts] = createSignal<Record<string, string>>({})
  const [notePendingDeletion, setNotePendingDeletion] = createSignal('')
  const [structuredDraft, setStructuredDraft] = createSignal('')
  const [saving, setSaving] = createSignal(false)
  let pendingSubmission: PendingSubmission | null = null
  let operationGeneration = 0
  let disposed = false

  const hasUnsavedEdits = (): boolean => textDirty()
    || noteDraft().trim().length > 0
    || Object.keys(noteDrafts()).length > 0
    || structuredDraft().trim().length > 0

  const retainedDrafts = (): RetainedCanvasDraft[] => {
    const drafts: RetainedCanvasDraft[] = []
    if (textDirty()) drafts.push({ label: '正文', value: textDraft() })
    if (noteDraft().trim()) drafts.push({ label: '新便笺', value: noteDraft() })
    for (const [id, value] of Object.entries(noteDrafts())) {
      drafts.push({ label: `便笺 ${id}`, value })
    }
    if (structuredDraft().trim()) drafts.push({ label: '结构化操作', value: structuredDraft() })
    return drafts
  }

  const invalidateContext = (): void => {
    operationGeneration += 1
    setSaving(false)
  }

  const clearDrafts = (): void => {
    pendingSubmission = null
    invalidateContext()
    setTextDraft('')
    setTextDirty(false)
    setNoteDraft('')
    setPendingNoteId('')
    setNoteDrafts({})
    setNotePendingDeletion('')
    setStructuredDraft('')
  }

  const syncTextDraft = (snapshot: CanvasReducerSnapshot): void => {
    if (!textDirty()) setTextDraft(textFrom(snapshot))
  }

  const hasPendingSubmission = (): boolean => pendingSubmission !== null

  const setNoteDraftValue = (id: string, original: string, value: string): void => {
    setNoteDrafts((current) => {
      const next = { ...current }
      if (value === original) delete next[id]
      else next[id] = value
      return next
    })
  }

  const submitOperation = async (op: Record<string, unknown>): Promise<boolean> => {
    const context = options.getContext()
    if (!context || saving()) return false
    const attempt = ++operationGeneration
    const key = JSON.stringify({ roomId: context.roomId, canvasId: context.canvasId, op })
    if (!pendingSubmission || pendingSubmission.key !== key) {
      pendingSubmission = { key, id: createCanvasClientOpId() }
    }
    const operationId = pendingSubmission.id
    setSaving(true)
    options.onError('', 'none')
    options.onStatus('保存中…')
    try {
      const result = await appendCanvasOperationWithRetry(
        api,
        context.roomId,
        context.canvasId,
        op,
        operationId,
      )
      if (!options.isCurrent(context)) return false
      const applied = context.reducer.ingest(result.operation as CanvasOperation)
      if (applied.status === 'conflict') throw new Error('操作序号冲突，请重新同步。')
      pendingSubmission = null
      options.onStatus(!isSupportedCanvasOp(op)
        ? `已记录未渲染操作：${String(op.type ?? '(无类型)')}`
        : '已保存')
      options.applyReducer()
      if (applied.gap || context.reducer.pending.size > 0) {
        await options.catchUp(false)
        if (!options.isCurrent(context)) return false
      }
      return true
    } catch (reason) {
      if (!options.isCurrent(context)) return false
      options.onStatus('保存失败')
      options.onError(
        errorText(reason, '保存失败'),
        (reason as { status?: number } | null)?.status === 409
          || (reason instanceof Error && reason.message.includes('冲突'))
          ? 'sync'
          : 'none',
      )
      if ((reason as { status?: number } | null)?.status === 403) options.onForbidden()
      return false
    } finally {
      if (!disposed && attempt === operationGeneration) setSaving(false)
    }
  }

  const saveText = async (event: SubmitEvent): Promise<void> => {
    event.preventDefault()
    const context = options.getContext()
    if (!context) return
    const submittedText = textDraft()
    if (await submitOperation({ type: 'set_text', text: submittedText })
      && options.isCurrent(context) && textDraft() === submittedText) {
      setTextDirty(false)
      setTextDraft(textFrom(context.reducer.snapshot()))
    }
  }

  const addNote = async (event: SubmitEvent): Promise<void> => {
    event.preventDefault()
    const draft = noteDraft()
    const text = draft.trim()
    if (!text) {
      options.onError('请输入便笺内容。', 'none')
      return
    }
    const context = options.getContext()
    if (!context) return
    const id = pendingNoteId() || noteId()
    setPendingNoteId(id)
    const ok = await submitOperation({
      type: 'add_note',
      note_id: id,
      text,
      author_id: options.participantId,
    })
    if (ok && options.isCurrent(context)) {
      if (noteDraft() === draft) setNoteDraft('')
      setPendingNoteId('')
    }
  }

  const updateNote = async (id: string, existing: string): Promise<void> => {
    const context = options.getContext()
    if (!context) return
    const text = noteDrafts()[id] ?? existing
    if (await submitOperation({ type: 'update_note', note_id: id, text })
      && options.isCurrent(context)) {
      setNoteDrafts((current) => {
        const next = { ...current }
        delete next[id]
        return next
      })
    }
  }

  const deleteNote = async (id: string): Promise<void> => {
    const context = options.getContext()
    if (!context) return
    if (await submitOperation({ type: 'delete_note', note_id: id })
      && options.isCurrent(context)) {
      setNotePendingDeletion('')
      setNoteDrafts((current) => {
        const next = { ...current }
        delete next[id]
        return next
      })
    }
  }

  const submitStructured = async (event: SubmitEvent): Promise<void> => {
    event.preventDefault()
    const context = options.getContext()
    if (!context) return
    const submittedDraft = structuredDraft()
    let op: unknown
    try {
      op = JSON.parse(submittedDraft)
      if (!isRecord(op)) throw new Error('操作必须是 JSON 对象。')
    } catch (reason) {
      options.onError(errorText(reason, 'JSON 无效'), 'none')
      return
    }
    if (await submitOperation(op) && options.isCurrent(context)
      && structuredDraft() === submittedDraft) setStructuredDraft('')
  }

  onCleanup(() => {
    disposed = true
  })

  return {
    textDraft,
    setTextDraft,
    textDirty,
    setTextDirty,
    noteDraft,
    setNoteDraft,
    noteDrafts,
    setNoteDrafts,
    notePendingDeletion,
    setNotePendingDeletion,
    structuredDraft,
    setStructuredDraft,
    saving,
    hasUnsavedEdits,
    hasPendingSubmission,
    retainedDrafts,
    invalidateContext,
    clearDrafts,
    setNoteDraftValue,
    syncTextDraft,
    saveText,
    addNote,
    updateNote,
    deleteNote,
    submitStructured,
  }
}
