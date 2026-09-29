import type { ApiError } from './api'
import type { CanvasReducerSnapshot } from '../canvas_model.js'

export function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

export function errorText(reason: unknown, action: string): string {
  const error = reason as Partial<ApiError> | null
  if (action.includes('创建')
    && (error?.status === 0 || (typeof error?.status === 'number' && error.status >= 500))) {
    return `${action}，结果暂时无法确认。可安全重试；系统会复用原创建标识，避免重复创建。`
  }
  if (error?.status === 0) return `${action}遇到网络问题。检查连接后可安全重试。`
  if (error?.status === 401) return '登录已过期，请重新登录后继续。'
  if (error?.status === 403) return '没有权限访问此房间的 Canvas；请联系房间管理员。'
  if (error?.status === 404) return `${action}：Canvas 已不存在，请刷新列表后重新选择。`
  if (error?.status === 409) return `${action}：Canvas 已有并发更新。先加载最新状态，检查草稿后再决定是否保存。`
  if (error?.status === 422) return `${action}：输入未通过校验，请修正内容后重试。`
  if (error?.status === 429) return `${action}：请求过于频繁，请稍后重试。`
  if (typeof error?.status === 'number' && error.status >= 500) {
    return `${action}：服务暂时不可用，输入已保留，请稍后重试。`
  }
  const message = reason instanceof Error ? reason.message : String(reason ?? '未知错误')
  return `${action}：${message}`
}

export function saveStatusTone(status: string): string {
  if (status.includes('失败')) return 'error'
  if (status.includes('未渲染')) return 'warning'
  if (status.includes('已保存') || status.includes('已同步')) return 'success'
  if (status) return 'pending'
  return 'idle'
}

export function saveStatusMark(status: string): string {
  const tone = saveStatusTone(status)
  if (tone === 'error') return '⚠'
  if (tone === 'success') return '✓'
  if (tone === 'warning') return 'ⓘ'
  return tone === 'pending' ? '↻' : ''
}

export function textFrom(snapshot: CanvasReducerSnapshot | undefined): string {
  const block = snapshot?.blocks.find((value) => isRecord(value) && value.type === 'text')
  return isRecord(block) ? String(block.content ?? block.text ?? '') : ''
}
