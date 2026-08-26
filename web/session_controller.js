// Session/bootstrap wiring for the SPA.
//
// The entry module keeps message and composer behavior local, while this
// controller owns the account-scoped WebSocket lifecycle and initial data
// loading. Dependencies are injected to avoid importing the app entry point
// back into this module.
export function createSessionController({
  api,
  auth,
  state,
  ws,
  els,
  showChat,
  forceReauth,
  refreshRoomList,
  updateTitleBadge,
  rerenderCurrentRoom,
  replayChanges,
  handleIncomingMessage,
  handleEdited,
  handleRecalled,
  handleDeleted,
  handleReaction,
  handleReadReceipt,
  handleTyping,
  handleNotify,
  handlePin,
  handlePresence,
  handleMembership,
  handleCall,
  handleStreamEvent,
  handleBackfillTruncated,
  handleResync,
  restorePendingDelivery,
  refreshWsAccessToken,
  syncRoomSidebar,
  initMessageActivity,
  initReliableDelivery,
  getPendingMap,
  refreshNotifBadge,
  toast,
  initialOf,
  avatarStyleFromId,
}) {
  let wsHooksInstalled = false;

  function hookWs() {
    if (wsHooksInstalled) return;
    wsHooksInstalled = true;
    ws.on('auth_expired', forceReauth);
    ws.on('status', (s) => {
      els.wsDot.classList.remove('ws-up', 'ws-down', 'ws-wait');
      if (s === 'up') els.wsDot.classList.add('ws-up');
      else if (s === 'wait' || s === 'connecting') els.wsDot.classList.add('ws-wait');
      else els.wsDot.classList.add('ws-down');
      els.wsDot.title = `WS: ${s}`;
    });
    ws.on('open', () => {
      if (!state.currentRoomId) return;
      ws.joinRoom(state.currentRoomId);
      // The server drops stream subscriptions with the socket. Re-watch them
      // after reconnect so live cards do not silently stop receiving events.
      for (const streamId of state.watchedStreams) ws.watchStream(streamId);
      replayChanges(state.currentRoomId);
    });
    ws.on('msg:message', (frame) => handleIncomingMessage(frame.message, frame.client_message_id));
    ws.on('msg:edited', (frame) => handleEdited(frame.message));
    ws.on('msg:recalled', (frame) => handleRecalled(frame.message));
    ws.on('msg:deleted', handleDeleted);
    ws.on('msg:reaction', handleReaction);
    ws.on('msg:read', handleReadReceipt);
    ws.on('msg:typing', handleTyping);
    ws.on('msg:notify', handleNotify);
    ws.on('msg:pin', handlePin);
    ws.on('msg:presence', handlePresence);
    ws.on('msg:membership', handleMembership);
    ws.on('msg:call', (frame) => handleCall(frame.event));
    ws.on('msg:stream_event', (frame) => handleStreamEvent(frame.event));
    ws.on('msg:backfill', handleBackfillTruncated);
    ws.on('msg:resync', handleResync);
    ws.on('msg:error', (frame) => toast(
      frame.code === 'rate_limited'
        ? '操作太频繁,请稍后重试'
        : `服务端:${frame.msg || frame.code || 'error'}`,
      'error',
    ));
    ws.on('msg:pong', () => {});
    initMessageActivity();
    initReliableDelivery({
      ws,
      getPendingMap,
      onCanonical: handleIncomingMessage,
      onRestore: restorePendingDelivery,
      onPendingChanged: rerenderCurrentRoom,
    });
  }

  function enterChat() {
    showChat();
    const me = state.me;
    els.meName.textContent = me.display_name || '—';
    els.meEmail.textContent = me.email || me.id || '';
    els.meAvatar.textContent = initialOf(me.display_name || me.email);
    els.meAvatar.setAttribute('style', avatarStyleFromId(me.id));
    hookWs();
    ws.connect(auth.getToken(), me.id, {
      refreshAccessToken: () => refreshWsAccessToken(api, auth, state),
    });
    syncRoomSidebar(forceReauth, () => {
      refreshRoomList();
      updateTitleBadge();
    });
    api.rtcConfig().then((config) => { state.rtcConfig = config; }).catch(() => {});
    api.liveGifts().then((result) => {
      state.giftCatalog = result?.gifts || [];
      if (state.currentRoomId) rerenderCurrentRoom();
    }).catch(() => {});
    if ('Notification' in window && Notification.permission === 'default') {
      Notification.requestPermission().catch(() => {});
    }
    refreshNotifBadge();
  }

  return { enterChat, hookWs };
}
