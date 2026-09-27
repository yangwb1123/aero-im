export interface RefreshApi {
  refresh(refreshToken: string): Promise<{
    access_token: string
    refresh_token?: string
    participant: { id: string }
  }>
}

export interface RefreshAuth {
  getRefresh(): string | null
  setSession(access: string, refresh: string, participantId: string): void
}

export function refreshWsAccessToken(
  api: RefreshApi,
  auth: RefreshAuth,
  state: { me: { id: string } | null | undefined },
): Promise<string>
