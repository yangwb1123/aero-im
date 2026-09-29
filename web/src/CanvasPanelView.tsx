import { For, Show } from 'solid-js'
import type { Accessor, JSX } from 'solid-js'
import type { CanvasReducerSnapshot } from '../canvas_model.js'
import type { Canvas } from './api'
import type { RetainedCanvasDraft } from './canvas_editor'
import { isRecord, saveStatusMark, saveStatusTone } from './canvas_panel_utils'

type ListStatus = 'idle' | 'loading' | 'ready' | 'error'
type RecoveryAction = 'none' | 'list' | 'canvas' | 'sync'

interface CanvasPanelViewProps {
  opened: Accessor<boolean>
  currentRoomId: Accessor<string>
  error: Accessor<string>
  recoveryAction: Accessor<RecoveryAction>
  accessDenied: Accessor<boolean>
  retainedDrafts: Accessor<RetainedCanvasDraft[]>
  creationUncertain: Accessor<boolean>
  createTitle: Accessor<string>
  creating: Accessor<boolean>
  canvases: Accessor<Canvas[]>
  listStatus: Accessor<ListStatus>
  activeId: Accessor<string>
  canvasLoading: Accessor<boolean>
  snapshot: Accessor<CanvasReducerSnapshot | undefined>
  saveStatus: Accessor<string>
  textDraft: Accessor<string>
  noteDraft: Accessor<string>
  noteDrafts: Accessor<Record<string, string>>
  notePendingDeletion: Accessor<string>
  structuredDraft: Accessor<string>
  saving: Accessor<boolean>
  onToggle: () => void
  onRetryList: () => void
  onRetryCreate: () => void
  onRetryCanvas: () => void
  onRetrySync: () => void
  onCreate: (event: SubmitEvent) => void
  onCreateTitleInput: (value: string) => void
  onSelectCanvas: (canvasId: string) => void
  onSaveText: (event: SubmitEvent) => void
  onTextInput: (value: string) => void
  onUpdateNote: (id: string, existing: string) => void
  onRequestDeleteNote: (id: string) => void
  onConfirmDeleteNote: (id: string) => void
  onCancelDeleteNote: () => void
  onNoteInput: (id: string, original: string, value: string) => void
  onNoteCreateInput: (value: string) => void
  onAddNote: (event: SubmitEvent) => void
  onStructuredInput: (value: string) => void
  onSubmitStructured: (event: SubmitEvent) => void
}

export function CanvasPanelView(props: CanvasPanelViewProps): JSX.Element {
  return (
    <section class="canvas-workspace">
      <button
        type="button"
        class="canvas-toggle"
        aria-expanded={props.opened()}
        onClick={props.onToggle}
      >
        {props.opened() ? '关闭 Canvas' : '协作 Canvas'}
      </button>
      <Show when={props.opened()}>
        <div class="canvas-panel" aria-label="房间协作 Canvas">
          <header class="canvas-panel-header">
            <div>
              <span class="eyebrow">COLLABORATION</span>
              <h2>房间 Canvas</h2>
            </div>
            <span class="canvas-room-label">当前房间 · {props.currentRoomId().slice(0, 12)}</span>
          </header>

          <Show when={props.error() || props.creationUncertain()}>
            <div class="canvas-error" role="alert">
              <span aria-hidden="true">⚠</span>{' '}
              <Show when={props.error()} fallback="创建结果仍未确认；请使用安全重试创建，避免重复提交。">
                {props.error()}
              </Show>
              <Show when={props.recoveryAction() === 'list'}>
                <button type="button" onClick={props.onRetryList}>刷新 Canvas 列表</button>
              </Show>
              <Show when={props.creationUncertain()}>
                <button
                  type="button"
                  data-testid="canvas-safe-create-retry"
                  disabled={props.creating() || props.accessDenied()}
                  onClick={props.onRetryCreate}
                >
                  安全重试创建
                </button>
              </Show>
              <Show when={props.recoveryAction() === 'canvas' && props.activeId()}>
                <button type="button" onClick={props.onRetryCanvas}>重新加载此 Canvas</button>
              </Show>
              <Show when={props.recoveryAction() === 'sync'}>
                <button type="button" onClick={props.onRetrySync}>加载最新状态</button>
              </Show>
            </div>
          </Show>

          <Show when={props.accessDenied() && props.retainedDrafts().length > 0}>
            <section class="canvas-preserved-drafts" aria-label="保留的未保存草稿">
              <h3>未保存的编辑草稿已保留</h3>
              <p>已隐藏服务端 Canvas 内容；以下仅显示本地输入的草稿。</p>
              <For each={props.retainedDrafts()}>
                {(draft) => (
                  <label>
                    {draft.label}
                    <textarea aria-label={`保留草稿：${draft.label}`} readOnly rows="3" value={draft.value} />
                  </label>
                )}
              </For>
            </section>
          </Show>

          <form class="canvas-create-form" data-testid="canvas-create-form" onSubmit={props.onCreate}>
            <input
              aria-label="新 Canvas 标题"
              disabled={props.accessDenied() || props.creationUncertain()}
              maxlength={512}
              placeholder="新 Canvas 标题"
              value={props.createTitle()}
              onInput={(event) => props.onCreateTitleInput(event.currentTarget.value)}
            />
            <button type="submit" disabled={props.creating() || props.accessDenied() || props.creationUncertain()}>
              {props.creating() ? '创建中…' : props.creationUncertain() ? '创建结果待确认' : '创建 Canvas'}
            </button>
          </form>

          <div class="canvas-columns">
            <aside class="canvas-list" aria-label="Canvas 列表">
              <Show when={props.listStatus() === 'loading'}>
                <p class="canvas-state">正在加载 Canvas 列表…</p>
              </Show>
              <Show when={props.listStatus() === 'error'}>
                <p class="canvas-state">
                  {props.creationUncertain() ? '创建结果未确认；可安全重试。' : 'Canvas 列表加载失败。'}
                </p>
              </Show>
              <Show when={props.listStatus() === 'ready' && props.canvases().length === 0}>
                <p class="canvas-state">
                  此房间还没有 Canvas。可在上方输入标题，创建第一个协作空间。
                </p>
              </Show>
              <For each={props.canvases()}>
                {(canvas) => (
                  <button
                    type="button"
                    class="canvas-list-item"
                    classList={{ active: canvas.id === props.activeId() }}
                    aria-current={canvas.id === props.activeId() ? 'true' : undefined}
                    onClick={() => props.onSelectCanvas(canvas.id)}
                  >
                    <strong>{canvas.title || '未命名 Canvas'}</strong>
                    <small>快照 v{canvas.version ?? 0}</small>
                  </button>
                )}
              </For>
            </aside>

            <main class="canvas-editor">
              <Show when={props.canvasLoading()}>
                <p class="canvas-state">正在读取 Canvas 快照与操作日志…</p>
              </Show>
              <Show when={!props.canvasLoading() && props.snapshot()} fallback={null}>
                <>
                  <header class="canvas-document-header">
                    <div>
                      <h3>{props.snapshot()?.title || '未命名 Canvas'}</h3>
                      <p>
                        快照 v{props.snapshot()?.version} · 操作 #{props.snapshot()?.op_seq}
                        {(props.snapshot()?.unknown_ops ?? 0) > 0
                          ? ` · ${props.snapshot()?.unknown_ops} 个未渲染操作（日志游标已前进）`
                          : ''}
                        {(props.snapshot()?.pending_unsequenced ?? 0) > 0
                          ? ` · ${props.snapshot()?.pending_unsequenced} 个等待持久序号的实时操作`
                          : ''}
                      </p>
                    </div>
                    <span
                      class="canvas-save-status"
                      data-state={saveStatusTone(props.saveStatus())}
                      aria-live="polite"
                    >
                      <Show when={props.saveStatus()}>
                        {saveStatusMark(props.saveStatus())} {props.saveStatus()}
                      </Show>
                    </span>
                  </header>

                  <form class="canvas-edit-form" onSubmit={props.onSaveText}>
                    <label for="canvas-text-draft">正文</label>
                    <textarea
                      id="canvas-text-draft"
                      aria-label="Canvas 正文"
                      rows="5"
                      value={props.textDraft()}
                      onInput={(event) => props.onTextInput(event.currentTarget.value)}
                    />
                    <button type="submit" disabled={props.saving()}>
                      {props.saveStatus() === '保存失败' ? '重试保存' : '保存正文'}
                    </button>
                  </form>

                  <section class="canvas-notes" aria-label="便笺">
                    <h4>便笺</h4>
                    <For each={props.snapshot()?.blocks.filter((block) => isRecord(block) && block.type === 'note') ?? []}>
                      {(block) => {
                        const id = String((block as Record<string, unknown>).note_id ?? '')
                        const original = String((block as Record<string, unknown>).content
                          ?? (block as Record<string, unknown>).text ?? '')
                        return (
                          <article class="canvas-note">
                            <textarea
                              aria-label={`便笺 ${id}`}
                              value={props.noteDrafts()[id] ?? original}
                              onInput={(event) => props.onNoteInput(id, original, event.currentTarget.value)}
                            />
                            <div class="canvas-note-actions">
                              <button type="button" disabled={props.saving()} onClick={() => props.onUpdateNote(id, original)}>
                                更新便笺
                              </button>
                              <Show when={props.notePendingDeletion() === id} fallback={
                                <button type="button" disabled={props.saving()} onClick={() => props.onRequestDeleteNote(id)}>
                                  删除便笺
                                </button>
                              }>
                                <div class="canvas-delete-confirm" role="group" aria-label={`确认删除便笺 ${id}`}>
                                  <span>此操作会从共享 Canvas 移除此便笺。</span>
                                  <button
                                    type="button"
                                    data-danger="true"
                                    disabled={props.saving()}
                                    onClick={() => props.onConfirmDeleteNote(id)}
                                  >
                                    确认删除
                                  </button>
                                  <button type="button" disabled={props.saving()} onClick={props.onCancelDeleteNote}>
                                    取消
                                  </button>
                                </div>
                              </Show>
                            </div>
                          </article>
                        )
                      }}
                    </For>
                    <Show when={(props.snapshot()?.blocks.filter((block) => isRecord(block) && block.type === 'note').length ?? 0) === 0}>
                      <p class="canvas-state">暂无便笺。</p>
                    </Show>
                    <form class="canvas-note-create" onSubmit={props.onAddNote}>
                      <input
                        aria-label="新便笺内容"
                        placeholder="添加便笺"
                        value={props.noteDraft()}
                        onInput={(event) => props.onNoteCreateInput(event.currentTarget.value)}
                      />
                      <button type="submit" disabled={props.saving()}>添加便笺</button>
                    </form>
                  </section>

                  <section class="canvas-structured" aria-label="结构化内容">
                    <h4>结构化内容</h4>
                    <For each={props.snapshot()?.blocks.filter((block) => !(
                      isRecord(block) && (block.type === 'text' || block.type === 'note')
                    )) ?? []}>
                      {(block) => <pre>{JSON.stringify(block, null, 2)}</pre>}
                    </For>
                    <form class="canvas-structured-form" onSubmit={props.onSubmitStructured}>
                      <label for="canvas-structured-draft">
                        JSON 操作（set_blocks / upsert_block / delete_block 等）
                      </label>
                      <textarea
                        id="canvas-structured-draft"
                        aria-label="结构化操作 JSON"
                        rows="4"
                        value={props.structuredDraft()}
                        placeholder={'{"type":"upsert_block","block":{"id":"status","type":"status","value":"ready"}}'}
                        onInput={(event) => props.onStructuredInput(event.currentTarget.value)}
                      />
                      <button type="submit" disabled={props.saving()}>提交操作</button>
                    </form>
                  </section>
                </>
              </Show>
              <Show when={!props.canvasLoading() && props.listStatus() === 'ready' && !props.activeId()}>
                <p class="canvas-state">选择或新建一个 Canvas。</p>
              </Show>
            </main>
          </div>
        </div>
      </Show>
    </section>
  )
}
