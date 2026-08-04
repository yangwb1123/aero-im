// Access-token rotation used by the WebSocket reconnect path. Dependencies are
// injected to keep this leaf module independent from the shared UI context.

export async function refreshWsAccessToken(api, auth, state) {
  const expectedRefresh = auth.getRefresh();
  const expectedParticipant = state.me?.id;
  if (!expectedRefresh || !expectedParticipant) throw new Error('refresh session unavailable');

  const session = await api.refresh(expectedRefresh);
  // A logout/account switch may race the request. Never let an old account's
  // rotation response overwrite the new account's local session.
  if (auth.getRefresh() !== expectedRefresh || state.me?.id !== expectedParticipant) {
    throw new Error('refresh session changed');
  }
  if (!session?.access_token || !session?.refresh_token
    || session?.participant?.id !== expectedParticipant) {
    throw new Error('invalid refresh response');
  }
  auth.setSession(session.access_token, session.refresh_token, expectedParticipant);
  return session.access_token;
}
