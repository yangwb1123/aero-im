import { messageText, type Message, type Participant } from './api'

export const QUICK_REACTIONS = ['👍', '❤️', '😂', '🎉', '👀'] as const

export function participantLabel(id: string, current: Participant): string {
  if (id === current.id) return current.display_name?.trim() || current.email || '我'
  return id.slice(0, 10)
}

export function messageTime(value?: string): string {
  if (!value) return ''
  const date = new Date(value)
  return Number.isNaN(date.getTime())
    ? ''
    : date.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit' })
}

export function replyPreview(message: Message | undefined, fallbackId: string): string {
  if (!message) return `消息 ${fallbackId.slice(0, 8)}`
  if (message.deleted_at) return '原消息已删除'
  if (message.recalled_at) return '原消息已撤回'
  const text = messageText(message).trim()
  return text ? (text.length > 120 ? `${text.slice(0, 120)}…` : text) : '[非文本消息]'
}

export function messageVersion(message: Message): number | undefined {
  return Number.isInteger(message.version) && (message.version ?? 0) > 0
    ? message.version
    : undefined
}

export function editableMessageText(message: Message): string {
  return (message.blocks ?? [])
    .filter((block) => block.type === 'text' || !block.type)
    .map((block) => typeof block.content === 'string' ? block.content : '')
    .join('\n')
}

export function isPlainTextMessage(message: Message): boolean {
  return (message.blocks ?? []).every((block) => {
    if (block.type === 'text') {
      return block.spans === undefined || (Array.isArray(block.spans) && block.spans.length === 0)
    }
    return !block.type && typeof block.content === 'string'
      && Object.keys(block).every((key) => key === 'content')
  })
}

export { messageText }
