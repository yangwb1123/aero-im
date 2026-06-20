// livecards.js — P11 live interactivity (弹幕 + 礼物) card plumbing.
//
// Extracted from app.js. Owns the bridge between render.js's in-message stream
// cards and the realtime backend: watch/unwatch, gift backlog hydration, live
// stream-event fan-in, and interactive-block submission. Self-contained — only
// touches the shared spine (state / ws), the api client, and render's `toast`.
//
// Public surface (imported by app.js):
//   • liveHooks()                       — hooks object handed to render.js cards
//   • submitBlockInteraction(id, a, v)  — interactive-block click → API
//   • registerLiveCard(streamId, ctrl)  — (kept internal; used via liveHooks)
//   • clearLiveCards()                  — unwatch all on room switch
//   • handleStreamEvent(ev)             — route a `msg:stream_event` frame

import { api } from './api.js';
import { state, ws } from './context.js';
import { toast } from './render.js';

// Hooks handed to render.js's stream cards. Built fresh per render so the gift
// catalog + my id are always current.
export function liveHooks() {
  return {
    gifts: state.giftCatalog,
    meId: state.me?.id || null,
    register: (streamId, controller) => registerLiveCard(streamId, controller),
    chat: (streamId, body) => ws.streamChat(streamId, body),
    gift: (streamId, giftId, qty) => ws.streamGift(streamId, giftId, qty),
    end: (streamId) => endStream(streamId),
  };
}

// Submit an interactive-block interaction (Button click / Select choice) to
// `POST /api/messages/:id/interact`. Returns the promise so render.js can give
// optimistic feedback (disable/mark-done) on success and re-enable on failure.
// The server broadcasts RoomEvent::Interaction so the poster (bot/webhook) sees
// the click live.
export function submitBlockInteraction(messageId, actionId, value) {
  return api
    .interactBlock(messageId, actionId, value)
    .then((res) => {
      toast('已提交', 'ok');
      return res;
    })
    .catch((err) => {
      toast(`提交失败:${err.message || err}`, 'error');
      throw err;
    });
}

// A stream card just mounted: remember its controller, start watching, and
// hydrate a little recent-gift backlog (chat backlog is pushed by the server).
function registerLiveCard(streamId, controller) {
  if (!streamId) return;
  state.liveCards.set(streamId, controller);
  ws.watchStream(streamId);
  state.watchedStreams.add(streamId);
  api.streamGiftList(streamId, 5)
    .then((r) => { for (const g of (r?.gifts || [])) controller.addGift(g); })
    .catch(() => {});
}

// Stop watching every stream and forget controllers (called on room switch,
// before the message list is rebuilt with the new room's cards).
export function clearLiveCards() {
  for (const sid of state.watchedStreams) ws.unwatchStream(sid);
  state.watchedStreams.clear();
  state.liveCards.clear();
}

export function handleStreamEvent(ev) {
  if (!ev || !ev.kind || !ev.stream_id) return;
  const ctrl = state.liveCards.get(ev.stream_id);
  if (!ctrl) return;
  switch (ev.kind) {
    case 'chat': ctrl.addChat(ev); break;
    case 'gift': ctrl.addGift(ev); break;
    case 'viewers': ctrl.setViewers(ev.count); break;
    case 'status': ctrl.setStatus(ev.status); break;
    default: break;
  }
}

async function endStream(streamId) {
  if (!streamId) return;
  if (!window.confirm('确定结束这场直播?')) return;
  try {
    await api.endStream(streamId);
    toast('直播已结束', 'info');
  } catch (err) {
    toast(`结束失败:${err.message}`, 'error');
  }
}
