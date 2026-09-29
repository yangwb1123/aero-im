export interface RecallErrorNotice {
  text: string
  type: 'error' | 'info'
}

export function recallErrorToast(err?: unknown): RecallErrorNotice | null
