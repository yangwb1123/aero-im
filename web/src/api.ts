export interface Participant {
  id: string
  display_name?: string
  email?: string
  avatar_url?: string | null
}

export interface SessionResponse {
  access_token: string
  refresh_token?: string
  participant: Participant
}

export interface Room {
  id: string
  name?: string | null
  kind?: string
  workspace_id?: string | null
  topic?: string | null
}

export interface MessageBlock {
  type?: string
  content?: string
  [key: string]: unknown
}

export interface Message {
  id: string
  room_id: string
  sender_id: string
  blocks?: MessageBlock[]
  created_at?: string
  edited_at?: string | null
  deleted_at?: string | null
  recalled_at?: string | null
  delivery_ordinal?: number
}

export interface PresenceFrame {
  type: 'presence'
  room_id?: string
  online?: string[]
}

export class ApiError extends Error {
  readonly status: number
  readonly body: unknown

  constructor(status: number, body: unknown, message: string) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.body = body
  }
}

const TOKEN_KEY = 'aero_token'
const REFRESH_KEY = 'aero_refresh'
const PID_KEY = 'aero_pid'

export const sessionStorage = {
  get token(): string | null {
    return window.localStorage.getItem(TOKEN_KEY)
  },
  get refresh(): string | null {
    return window.localStorage.getItem(REFRESH_KEY)
  },
  get participantId(): string | null {
    return window.localStorage.getItem(PID_KEY)
  },
  set(value: SessionResponse): void {
    window.localStorage.setItem(TOKEN_KEY, value.access_token)
    if (value.refresh_token) window.localStorage.setItem(REFRESH_KEY, value.refresh_token)
    window.localStorage.setItem(PID_KEY, value.participant.id)
  },
  clear(): void {
    window.localStorage.removeItem(TOKEN_KEY)
    window.localStorage.removeItem(REFRESH_KEY)
    window.localStorage.removeItem(PID_KEY)
  },
}

async function request<T>(
  method: string,
  path: string,
  options: { body?: unknown; authenticated?: boolean } = {},
): Promise<T> {
  const headers: Record<string, string> = { Accept: 'application/json' }
  if (options.body !== undefined) headers['Content-Type'] = 'application/json'
  if (options.authenticated !== false && sessionStorage.token) {
    headers.Authorization = `Bearer ${sessionStorage.token}`
  }

  let response: Response
  try {
    response = await fetch(path, {
      method,
      headers,
      body: options.body === undefined ? undefined : JSON.stringify(options.body),
    })
  } catch (error) {
    const message = error instanceof Error ? error.message : '网络请求失败'
    throw new ApiError(0, null, `网络错误：${message}`)
  }

  const contentType = response.headers.get('content-type') ?? ''
  const body = contentType.includes('application/json')
    ? await response.json().catch(() => null)
    : await response.text()
  if (!response.ok) {
    const value = body && typeof body === 'object' ? body as Record<string, unknown> : null
    const message = typeof value?.msg === 'string'
      ? value.msg
      : typeof value?.message === 'string'
        ? value.message
        : typeof value?.error === 'string'
          ? value.error
          : `HTTP ${response.status}`
    throw new ApiError(response.status, body, message)
  }
  return body as T
}

export const api = {
  login(email: string, password: string, secondFactor = ''): Promise<SessionResponse> {
    const body: Record<string, string> = { email, password }
    if (/^\d{6}$/.test(secondFactor)) body.totp = secondFactor
    else if (secondFactor) body.recovery_code = secondFactor
    return request<SessionResponse>('POST', '/api/auth/login', { body, authenticated: false })
  },

  register(email: string, displayName: string, password: string): Promise<SessionResponse> {
    return request<SessionResponse>('POST', '/api/auth/register', {
      body: { email, display_name: displayName, password },
      authenticated: false,
    })
  },

  me(): Promise<Participant> {
    return request<Participant>('GET', '/api/me')
  },

  logout(): Promise<unknown> {
    const refresh = sessionStorage.refresh
    return request('POST', '/api/auth/logout', {
      body: { refresh_token: refresh ?? '' },
      authenticated: false,
    })
  },

  listRooms(): Promise<Room[]> {
    return request<Room[]>('GET', '/api/rooms')
  },

  createRoom(kind: 'group' | 'channel', name: string): Promise<Room> {
    return request<Room>('POST', '/api/rooms', {
      body: { kind, ...(name.trim() ? { name: name.trim() } : {}) },
    })
  },

  listMessages(roomId: string): Promise<Message[]> {
    return request<Message[]>(
      'GET',
      `/api/rooms/${encodeURIComponent(roomId)}/messages?limit=100`,
    )
  },
}

export function messageText(message: Message): string {
  return (message.blocks ?? [])
    .filter((block) => block.type === 'text' || !block.type)
    .map((block) => typeof block.content === 'string' ? block.content : '')
    .join('')
}
