import { For, Show, type Accessor, type JSX } from 'solid-js'
import { IrisAlert, IrisAvatar, IrisButton } from '@iris-ui-kit/solid'
import type { Message, Participant, ReactionSummary } from './api'
import {
  editableMessageText,
  isPlainTextMessage,
  messageText,
  messageTime,
  participantLabel,
  QUICK_REACTIONS,
  replyPreview,
} from './message_format'

export interface MessageRowProps {
  message: Message
  participant: Participant
  isEditing: Accessor<boolean>
  editDraft: Accessor<string>
  editOriginal: Accessor<string>
  editSaving: Accessor<boolean>
  editError: Accessor<string>
  editConflict: Accessor<Message | null>
  isRecalling: Accessor<boolean>
  isDeleting: Accessor<boolean>
  deleteFeedback: Accessor<string | undefined>
  reactions: Accessor<ReactionSummary[]>
  isReacting: (emoji: string) => boolean
  reactionFeedback: Accessor<string | undefined>
  deliveryStatus: Accessor<Message['delivery_status']>
  failureMessage: Accessor<string | null | undefined>
  persistenceWarning: Accessor<boolean>
  clientMessageId: Accessor<string | undefined>
  replyParent: (message: Message) => Message | undefined
  onBeginEdit: (message: Message) => void
  onRecall: (message: Message) => void
  onDelete: (message: Message) => void
  onBeginReply: (message: Message) => void
  onToggleReaction: (message: Message, emoji: string) => void
  onRetryDelivery: (clientMessageId: string) => void
  onEditDraftChange: (value: string) => void
  onEditSubmit: (event: Event, message: Message) => void
  onLoadLatest: (message: Message) => void
  onRebase: (message: Message) => void
  onCancelEdit: () => void
}

export function MessageRow(props: MessageRowProps): JSX.Element {
  const message = (): Message => props.message

  return (
    <article class="message-row" classList={{ mine: message().sender_id === props.participant.id }}>
      <IrisAvatar name={participantLabel(message().sender_id, props.participant)} size={32} />
      <div class="message-body">
        <div class="message-meta">
          <strong>{participantLabel(message().sender_id, props.participant)}</strong>
          <time>{messageTime(message().created_at)}</time>
        </div>
        <Show when={props.deliveryStatus()}>
          <div
            class="message-delivery-status"
            classList={{ failed: props.deliveryStatus() === 'failed' }}
            role="status"
          >
            <span>{props.deliveryStatus() === 'waiting' ? '等待连接，已加入待发送队列'
              : props.deliveryStatus() === 'retrying' ? '正在重试发送…'
                : props.deliveryStatus() === 'failed' ? `发送失败：${props.failureMessage() ?? '未知错误'}`
                  : '发送中…'}</span>
            <Show when={props.persistenceWarning()}>
              <span class="message-persistence-warning">本地存储不可用；刷新可能丢失此消息</span>
            </Show>
            <Show when={props.deliveryStatus() === 'failed' && props.clientMessageId()}>
              <button
                type="button"
                class="message-retry-button"
                data-testid="message-retry"
                onClick={() => {
                  const clientMessageId = props.clientMessageId()
                  if (clientMessageId) props.onRetryDelivery(clientMessageId)
                }}
              >重试发送</button>
            </Show>
          </div>
        </Show>
        <Show
          when={props.isEditing()}
          fallback={
            <>
              <Show when={message().reply_to}>
                <div class="message-reply-context">
                  <strong>
                    {(() => {
                      const parent = props.replyParent(message())
                      return parent
                        ? `回复 ${participantLabel(parent.sender_id, props.participant)}`
                        : '回复消息'
                    })()}:
                  </strong>
                  <span>{replyPreview(props.replyParent(message()), message().reply_to ?? '')}</span>
                </div>
              </Show>
              <div class="message-bubble">
                <Show
                  when={!message().deleted_at && !message().recalled_at}
                  fallback={<em>{message().deleted_at ? '消息已删除' : '消息已撤回'}</em>}
                >
                  {messageText(message()) || '[非文本消息]'}
                </Show>
              </div>
              <Show when={!message().deleted_at && !message().recalled_at && !props.deliveryStatus()}>
                <div class="message-reply-actions">
                  <IrisButton
                    type="button"
                    variant="ghost"
                    size="sm"
                    data-testid="message-reply"
                    aria-label={`回复消息 ${message().id}`}
                    onClick={() => props.onBeginReply(message())}
                  >回复</IrisButton>
                </div>
              </Show>
              <Show when={message().sender_id === props.participant.id
                && !message().deleted_at && !message().recalled_at && !props.deliveryStatus()}
              >
                <div class="message-actions">
                  <Show when={isPlainTextMessage(message())}>
                    <IrisButton
                      type="button"
                      variant="ghost"
                      size="sm"
                      class="message-edit-button"
                      data-testid="message-edit"
                      aria-label={`编辑消息 ${message().id}`}
                      disabled={props.editSaving() || props.isRecalling() || props.isDeleting()}
                      onClick={() => props.onBeginEdit(message())}
                    >编辑</IrisButton>
                  </Show>
                  <IrisButton
                    type="button"
                    variant="ghost"
                    size="sm"
                    class="message-recall-button"
                    data-testid="message-recall"
                    aria-label={`撤回消息 ${message().id}`}
                    disabled={props.isRecalling() || props.isDeleting()}
                    onClick={() => props.onRecall(message())}
                  >
                    {props.isRecalling() ? '撤回中…' : '撤回'}
                  </IrisButton>
                  <IrisButton
                    type="button"
                    variant="ghost"
                    size="sm"
                    class="message-delete-button"
                    data-testid="message-delete"
                    aria-label={`删除消息 ${message().id}`}
                    disabled={props.isDeleting() || props.isRecalling()}
                    onClick={() => props.onDelete(message())}
                  >
                    {props.isDeleting() ? '删除中…' : '删除'}
                  </IrisButton>
                  <Show when={props.deleteFeedback()}>
                    <span class="message-delete-feedback" role="status">
                      {props.deleteFeedback()}
                    </span>
                  </Show>
                </div>
              </Show>
              <Show when={!message().deleted_at && !message().recalled_at && !props.deliveryStatus()}>
                <div class="message-reaction-row">
                  <div class="message-reactions" aria-label="消息表情回应">
                    <For each={props.reactions()}>
                      {(summary) => {
                        const selected = (): boolean => summary.participants.includes(props.participant.id)
                        const pending = (): boolean => props.isReacting(summary.emoji)
                        return (
                          <button
                            type="button"
                            class="reaction-pill"
                            classList={{ selected: selected() }}
                            aria-label={`${summary.emoji}，${summary.count} 个回应`}
                            aria-pressed={selected()}
                            disabled={pending()}
                            onClick={() => props.onToggleReaction(message(), summary.emoji)}
                          >
                            <span>{summary.emoji}</span>
                            <span>{summary.count}</span>
                          </button>
                        )
                      }}
                    </For>
                    <details class="reaction-picker">
                      <summary aria-label={`添加表情回应到消息 ${message().id}`} title="添加回应">+</summary>
                      <div class="reaction-picker-options" role="group" aria-label="快速表情回应">
                        <For each={[...QUICK_REACTIONS]}>
                          {(emoji) => (
                            <button
                              type="button"
                              aria-label={`回应 ${emoji}`}
                              data-testid="reaction-option"
                              disabled={props.isReacting(emoji)}
                              onClick={(event) => {
                                event.currentTarget.closest('details')?.removeAttribute('open')
                                props.onToggleReaction(message(), emoji)
                              }}
                            >{emoji}</button>
                          )}
                        </For>
                      </div>
                    </details>
                  </div>
                  <Show when={props.reactionFeedback()}>
                    <span class="message-reaction-feedback" role="status">
                      {props.reactionFeedback()}
                    </span>
                  </Show>
                </div>
              </Show>
            </>
          }
        >
          <form class="message-edit-form" onSubmit={(event) => props.onEditSubmit(event, message())}>
            <textarea
              aria-label={`编辑消息内容 ${message().id}`}
              rows="3"
              maxlength="8000"
              value={props.editDraft()}
              onInput={(event) => props.onEditDraftChange(event.currentTarget.value)}
            />
            <Show when={props.editError()}>
              <IrisAlert tone="danger" class="message-edit-error" role="alert">
                {props.editError()}
              </IrisAlert>
            </Show>
            <Show when={props.editConflict()?.id === message().id}>
              <div class="message-edit-conflict" role="group" aria-label="编辑冲突处理">
                <Show when={isPlainTextMessage(props.editConflict()!)} fallback={
                  <p>服务器最新版本包含非文本内容，当前编辑器无法安全合并；本地草稿仍保留。</p>
                }>
                  <p>服务器最新版本：{editableMessageText(props.editConflict()!)}</p>
                </Show>
                <Show when={!props.editConflict()?.deleted_at && !props.editConflict()?.recalled_at
                  && isPlainTextMessage(props.editConflict()!)}>
                  <IrisButton
                    type="button"
                    variant="outline"
                    size="sm"
                    onClick={() => props.onLoadLatest(props.editConflict()!)}
                  >放弃草稿并载入最新版本</IrisButton>
                  <IrisButton
                    type="button"
                    variant="outline"
                    size="sm"
                    onClick={() => props.onRebase(props.editConflict()!)}
                  >确认覆盖最新版本</IrisButton>
                </Show>
              </div>
            </Show>
            <Show when={message().deleted_at || message().recalled_at}>
              <p class="message-edit-unavailable" role="status">
                消息已删除或撤回；本地编辑草稿仍保留。
              </p>
            </Show>
            <div class="message-edit-actions">
              <IrisButton
                type="submit"
                variant="solid"
                size="sm"
                disabled={props.editSaving() || !props.editDraft().trim()
                  || props.editDraft() === props.editOriginal()
                  || Boolean(message().deleted_at || message().recalled_at)}
              >{props.editSaving() ? '保存中…' : '保存编辑'}</IrisButton>
              <IrisButton
                type="button"
                variant="ghost"
                size="sm"
                disabled={props.editSaving()}
                onClick={props.onCancelEdit}
              >取消</IrisButton>
            </div>
          </form>
        </Show>
      </div>
    </article>
  )
}
