// room_sync.js — hydrate the room sidebar and persisted unread counts together.

import { api, ApiError } from './api.js';
import { state } from './context.js';

export async function syncRoomSidebar(onUnauthorized, onUpdated) {
  const [roomsResult, unreadResult] = await Promise.allSettled([
    api.listRooms(),
    api.unread(),
  ]);

  if (roomsResult.status === 'fulfilled' && Array.isArray(roomsResult.value)) {
    state.rooms.clear();
    for (const room of roomsResult.value) state.rooms.set(room.id, room);
  }
  if (unreadResult.status === 'fulfilled' && Array.isArray(unreadResult.value)) {
    state.unreadByRoom.clear();
    for (const row of unreadResult.value) {
      const count = Number(row?.unread);
      if (row?.room_id && Number.isFinite(count) && count > 0) {
        state.unreadByRoom.set(row.room_id, count);
      }
    }
  }

  const unauthorized = [roomsResult, unreadResult].some(
    (result) => result.status === 'rejected' &&
      result.reason instanceof ApiError &&
      result.reason.status === 401,
  );
  if (unauthorized) {
    onUnauthorized();
    return;
  }
  onUpdated();
}
