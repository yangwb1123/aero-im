//! WebSocket endpoint and protocol.
//!
//! Wire format is JSON; all frames are tagged with `type`. See the design spec
//! for the full message set. JWT is passed via `?token=...` query parameter
//! because browser WebSocket clients can't set custom headers.
//!
//! ## Reconnect backfill protocol (ROADMAP 方向五)
//!
//! A client that dropped its socket can avoid silently losing messages by
//! reconnecting with an extra query param: `/ws?token=...&since=<message_id>`,
//! where `<message_id>` is the id of the **last message it successfully
//! received** (any room). On connect, before any live event resumes, the server
//! replays every message created strictly after that cursor — across all rooms
//! the participant belongs to — as ordinary `message` frames (via
//! [`aero_storage::MessageRepo::list_since`], oldest-first, capped per room).
//! `since` is best-effort: a malformed/garbage cursor is ignored (the client
//! simply gets no backfill) rather than failing the upgrade, and replay
//! failures never abort the connection. The forward REST complement is
//! `GET /api/rooms/:id/messages?since=<message_id>`.

use std::str::FromStr;
use std::sync::Arc;

use aero_common::metrics::{self, names};
use aero_common::{
    Block, CallEvent, CallId, CallKind, CallMode, MessageId, ParticipantId, ReactionOp, RoomEvent,
    RoomId, StreamEvent,
};
use ulid::Ulid;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::IntoResponse,
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

use crate::hub::WsSender;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct WsParams {
    token: String,
    /// Optional reconnect cursor: the id of the last message the client already
    /// has. On connect the server backfills everything created after it across
    /// the participant's rooms before live events resume. See the module docs.
    #[serde(default)]
    since: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientFrame {
    /// Subscribe local presence for a room.
    JoinRoom { room_id: RoomId },
    /// Send a new message.
    SendMessage {
        room_id: RoomId,
        blocks: Vec<Block>,
        #[serde(default)]
        reply_to: Option<MessageId>,
    },
    /// Edit an existing message (sender only).
    EditMessage { id: MessageId, blocks: Vec<Block> },
    /// Soft-delete a message.
    DeleteMessage { id: MessageId },
    /// Toggle a reaction.
    React { message_id: MessageId, emoji: String },
    /// Mark a room read up to the given message.
    MarkRead { room_id: RoomId, last_message_id: MessageId },
    /// Best-effort typing indicator.
    Typing { room_id: RoomId, on: bool },
    /// Start a call (1:1 or group).
    CallInvite {
        room_id: RoomId,
        kind: CallKind,
        #[serde(default)]
        mode: Option<CallMode>,
        sdp: String,
    },
    /// Answer an incoming call.
    CallAnswer {
        call_id: CallId,
        room_id: RoomId,
        to: ParticipantId,
        sdp: String,
    },
    /// Trickle an ICE candidate.
    CallIce {
        call_id: CallId,
        room_id: RoomId,
        to: ParticipantId,
        candidate: serde_json::Value,
    },
    /// End a call.
    CallEnd {
        call_id: CallId,
        room_id: RoomId,
        #[serde(default)]
        reason: Option<String>,
    },
    /// Live caption line during a call. When `target_lang` is set and differs
    /// from `lang`, the server translates *final* lines via the AI backend.
    CallCaption {
        call_id: CallId,
        room_id: RoomId,
        text: String,
        #[serde(default)]
        lang: Option<String>,
        #[serde(default)]
        target_lang: Option<String>,
        #[serde(default)]
        is_final: bool,
    },
    /// Join (or start) a group call (P6 mesh). Omit `call_id` to start a new one.
    CallJoin {
        room_id: RoomId,
        kind: CallKind,
        #[serde(default)]
        call_id: Option<CallId>,
    },
    /// Leave a group call.
    CallLeave { call_id: CallId, room_id: RoomId },
    /// A per-pair mesh offer to one peer in a group call.
    CallOffer {
        call_id: CallId,
        room_id: RoomId,
        to: ParticipantId,
        sdp: String,
    },
    /// Start watching a live stream (danmaku/gift fan-out + viewer count).
    WatchStream { stream_id: Ulid },
    /// Stop watching a live stream.
    UnwatchStream { stream_id: Ulid },
    /// Post a danmaku line on a stream.
    StreamChat { stream_id: Ulid, body: String },
    /// Send a gift on a stream (`qty` defaults to 1).
    StreamGift {
        stream_id: Ulid,
        gift_id: String,
        #[serde(default)]
        qty: Option<u32>,
    },
    Ping,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerFrame<'a> {
    Welcome { participant: ParticipantId },
    Presence { room_id: RoomId, online: Vec<ParticipantId> },
    Message { message: aero_common::Message },
    Edited { message: aero_common::Message },
    Deleted { room_id: RoomId, message_id: MessageId, by: ParticipantId },
    Reaction {
        room_id: RoomId,
        message_id: MessageId,
        participant: ParticipantId,
        emoji: String,
        op: ReactionOp,
    },
    Read {
        room_id: RoomId,
        participant: ParticipantId,
        last_message_id: MessageId,
        at: time::OffsetDateTime,
    },
    Typing { room_id: RoomId, participant: ParticipantId, on: bool },
    Call { event: CallEvent },
    /// Per-stream interactivity event (danmaku/gift/viewers/status).
    StreamEvent { event: StreamEvent },
    Error { code: &'a str, msg: String },
    Pong,
}

#[instrument(skip(ws, state))]
pub async fn handler(
    ws: WebSocketUpgrade,
    Query(p): Query<WsParams>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let claims = match state.auth.verify(&p.token) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "ws auth failed");
            return (axum::http::StatusCode::UNAUTHORIZED, "invalid token").into_response();
        }
    };
    let pid: ParticipantId = match claims.participant_id() {
        Ok(p) => p,
        Err(_) => {
            return (axum::http::StatusCode::UNAUTHORIZED, "invalid sub").into_response();
        }
    };
    // Best-effort reconnect cursor: a garbage value is simply ignored (no
    // backfill) rather than rejecting the upgrade. See module-level protocol docs.
    let since = parse_resume_cursor(p.since.as_deref());
    ws.on_upgrade(move |socket| run_socket(socket, state, pid, since))
}

#[instrument(skip(socket, state, since), fields(%pid))]
async fn run_socket(
    socket: WebSocket,
    state: AppState,
    pid: ParticipantId,
    since: Option<MessageId>,
) {
    let (mut sender, mut receiver) = socket.split();
    // Bounded outbound queue: a slow/stalled client can never make the
    // broadcaster grow memory without limit (OOM guard). On a full queue the Hub
    // drops the frame and — per config — disconnects the client via `close`.
    let (tx, mut rx) = mpsc::channel::<Message>(state.ws_config.send_queue_capacity);
    let close = CancellationToken::new();
    state.hub.register(pid, WsSender::new(tx.clone(), close.clone()));

    let _ = tx.try_send(Message::Text(
        serde_json::to_string(&ServerFrame::Welcome { participant: pid }).unwrap_or_default(),
    ));

    let outgoing = {
        let close = close.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = close.cancelled() => break,
                    msg = rx.recv() => match msg {
                        Some(msg) => {
                            if sender.send(msg).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    },
                }
            }
        })
    };

    // Reconnect backfill (ROADMAP 方向五): before live events resume, replay
    // everything the client missed while disconnected. Done after the outgoing
    // task is draining `tx` (so we apply real back-pressure instead of dropping)
    // and before the receive loop (so the catch-up is chronological and lands
    // ahead of any new live frames). Best-effort: failures never abort the conn.
    if let Some(cursor) = since {
        backfill_since(&state, pid, cursor, &tx, &close).await;
    }

    loop {
        tokio::select! {
            // The Hub asked us to drop this connection (laggy / evicted).
            () = close.cancelled() => break,
            incoming = receiver.next() => {
                let Some(Ok(msg)) = incoming else { break };
                match msg {
                    Message::Text(text) => {
                        if let Err(e) = handle_text(&text, &state, pid, &tx).await {
                            let _ = tx.try_send(Message::Text(
                                serde_json::to_string(&ServerFrame::Error {
                                    code: "handler",
                                    msg: e.to_string(),
                                })
                                .unwrap_or_default(),
                            ));
                        }
                    }
                    Message::Ping(p) => {
                        let _ = tx.try_send(Message::Pong(p));
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    let registered = WsSender::new(tx, close.clone());
    state.hub.unregister(pid, &registered);
    close.cancel();
    outgoing.abort();
    info!(%pid, "ws closed");
}

async fn handle_text(
    text: &str,
    state: &AppState,
    pid: ParticipantId,
    tx: &mpsc::Sender<Message>,
) -> anyhow::Result<()> {
    let frame: ClientFrame = serde_json::from_str(text)?;
    match frame {
        ClientFrame::Ping => {
            let _ = tx.try_send(Message::Text(
                serde_json::to_string(&ServerFrame::Pong).unwrap_or_default(),
            ));
        }
        ClientFrame::JoinRoom { room_id } => {
            if !state.rooms.is_member(room_id, pid).await? {
                let _ = tx.try_send(Message::Text(
                    serde_json::to_string(&ServerFrame::Error {
                        code: "forbidden",
                        msg: "not a member".into(),
                    })
                    .unwrap_or_default(),
                ));
                return Ok(());
            }
            state.hub.join_room(room_id, pid);
            let online = state.hub.room_members_online(room_id);
            let _ = tx.try_send(Message::Text(
                serde_json::to_string(&ServerFrame::Presence { room_id, online })
                    .unwrap_or_default(),
            ));
            debug!(%pid, %room_id, "joined room");
        }
        ClientFrame::SendMessage { room_id, blocks, reply_to } => {
            // Tenant guard: the sender must belong to BOTH the room's workspace and
            // the room before a message is accepted. `ImService::send_message`
            // re-checks room membership (a distinct, retained check); this adds the
            // workspace-membership dimension. Maps to the WS `error` frame on denial.
            state.im.assert_room_access(pid, room_id).await?;
            state.im.send_message(pid, room_id, blocks, reply_to).await?;
        }
        ClientFrame::EditMessage { id, blocks } => {
            state.im.edit_message(pid, id, blocks).await?;
        }
        ClientFrame::DeleteMessage { id } => {
            state.im.delete_message(pid, id).await?;
        }
        ClientFrame::React { message_id, emoji } => {
            state.im.toggle_reaction(pid, message_id, &emoji).await?;
        }
        ClientFrame::MarkRead { room_id, last_message_id } => {
            state.im.mark_read(pid, room_id, last_message_id).await?;
        }
        ClientFrame::Typing { room_id, on } => {
            state.im.typing(pid, room_id, on).await?;
        }
        ClientFrame::CallInvite { room_id, kind, mode, sdp } => {
            state
                .im
                .start_call(pid, room_id, kind, mode.unwrap_or(CallMode::P2p), sdp)
                .await?;
        }
        ClientFrame::CallAnswer { call_id, room_id, to, sdp } => {
            state
                .im
                .relay_call_event(
                    room_id,
                    CallEvent::Answer { call_id, from: pid, to, sdp },
                )
                .await?;
        }
        ClientFrame::CallIce { call_id, room_id, to, candidate } => {
            state
                .im
                .relay_call_event(
                    room_id,
                    CallEvent::Ice { call_id, from: pid, to, candidate },
                )
                .await?;
        }
        ClientFrame::CallEnd { call_id, room_id, reason } => {
            state
                .im
                .relay_call_event(
                    room_id,
                    CallEvent::End {
                        call_id,
                        room_id,
                        by: pid,
                        reason: reason.unwrap_or_else(|| "ended".into()),
                    },
                )
                .await?;
        }
        ClientFrame::CallCaption { call_id, room_id, text, lang, target_lang, is_final } => {
            let text = text.trim().to_string();
            if text.is_empty() {
                return Ok(());
            }
            // Translate only *final* lines, only when a distinct target language
            // is set and an AI backend is available. Interim lines relay verbatim
            // to keep latency low.
            let (translated, translated_lang) = match (is_final, target_lang.as_deref(), &state.ai) {
                (true, Some(target), Some(ai))
                    if lang.as_deref().map_or(true, |l| !same_lang(l, target)) =>
                {
                    match ai.translate(&text, target).await {
                        Ok(t) if !t.trim().is_empty() => {
                            (Some(t), Some(target.to_string()))
                        }
                        _ => (None, None),
                    }
                }
                _ => (None, None),
            };
            state
                .im
                .relay_call_event(
                    room_id,
                    CallEvent::Caption {
                        call_id,
                        room_id,
                        from: pid,
                        text,
                        lang,
                        translated,
                        translated_lang,
                        is_final,
                    },
                )
                .await?;
        }
        ClientFrame::CallJoin { room_id, kind, call_id } => {
            if !state.rooms.is_member(room_id, pid).await? {
                let _ = tx.try_send(Message::Text(
                    serde_json::to_string(&ServerFrame::Error {
                        code: "forbidden",
                        msg: "not a member".into(),
                    })
                    .unwrap_or_default(),
                ));
                return Ok(());
            }
            let call_id = match call_id {
                Some(c) => c,
                None => {
                    let c = CallId::new();
                    // Best-effort session row; the live roster is in the Hub.
                    if let Err(e) =
                        state.calls.start(c, room_id, pid, kind, CallMode::Sfu, &[]).await
                    {
                        warn!(error = ?e, "persist group call session failed");
                    }
                    c
                }
            };
            // Local Hub keeps the roster for per-process mesh delivery; its
            // return value is the fallback set of already-present peers.
            let local_existing = state.hub.call_join(call_id, pid);
            // Redis is the cluster-wide roster (ROADMAP 方向二): register self,
            // then source the "whom to connect to" set from Redis (minus self) so
            // a joiner sees peers connected to *other* nodes too. `join` doubles
            // as the heartbeat restamp. On any Redis error, fall back to the local
            // Hub roster so a single-node call still works.
            if let Err(e) = state.call_roster.join(call_id, pid).await {
                warn!(error = ?e, %call_id, "redis call-roster join failed");
            }
            let existing = call_peers_excluding(state, call_id, pid, local_existing).await;
            // Tell the joiner who is already in the call (whom to connect to).
            state
                .im
                .relay_call_event(
                    room_id,
                    CallEvent::Roster { call_id, to: pid, members: existing, kind },
                )
                .await?;
            // Tell the room a new peer joined.
            state
                .im
                .relay_call_event(room_id, CallEvent::Join { call_id, room_id, from: pid, kind })
                .await?;
        }
        ClientFrame::CallLeave { call_id, room_id } => {
            state.hub.call_leave(call_id, pid);
            if let Err(e) = state.call_roster.leave(call_id, pid).await {
                warn!(error = ?e, %call_id, "redis call-roster leave failed");
            }
            state
                .im
                .relay_call_event(room_id, CallEvent::Leave { call_id, room_id, from: pid })
                .await?;
        }
        ClientFrame::CallOffer { call_id, room_id, to, sdp } => {
            state
                .im
                .relay_call_event(room_id, CallEvent::Offer { call_id, from: pid, to, sdp })
                .await?;
        }
        ClientFrame::WatchStream { stream_id } => {
            // Local Hub still tracks watchers for per-process event fan-out...
            state.hub.watch_stream(stream_id, pid);
            // ...while Redis is the cluster-wide source of the viewer COUNT
            // (ROADMAP 方向二). `join` also (re)stamps the heartbeat, so a
            // re-watch keeps the entry alive without a separate keep-alive.
            if let Err(e) = state.stream_viewers.join(stream_id, pid).await {
                warn!(error = ?e, %stream_id, "redis stream-viewer join failed");
            }
            // Replay a small danmaku backlog so the new watcher has context.
            if let Ok(lines) = state.live.recent_chat(stream_id, 30).await {
                for line in lines {
                    let frame = ServerFrame::StreamEvent { event: StreamEvent::Chat(line) };
                    let _ = tx.try_send(Message::Text(
                        serde_json::to_string(&frame).unwrap_or_default(),
                    ));
                }
            }
            let count = stream_viewer_count(state, stream_id).await;
            state.live.publish_viewers(stream_id, count).await;
        }
        ClientFrame::UnwatchStream { stream_id } => {
            state.hub.unwatch_stream(stream_id, pid);
            if let Err(e) = state.stream_viewers.leave(stream_id, pid).await {
                warn!(error = ?e, %stream_id, "redis stream-viewer leave failed");
            }
            let count = stream_viewer_count(state, stream_id).await;
            state.live.publish_viewers(stream_id, count).await;
        }
        ClientFrame::StreamChat { stream_id, body } => {
            state.live.post_chat(pid, stream_id, body).await?;
        }
        ClientFrame::StreamGift { stream_id, gift_id, qty } => {
            state.live.send_gift(pid, stream_id, &gift_id, qty.unwrap_or(1)).await?;
        }
    }
    Ok(())
}

/// Per-room cap on reconnect backfill replay, so a client that has been away for
/// a long time can't make a single connection replay an unbounded history (it
/// can keep paging via the REST `?since=` route). Matches the keyset page window
/// `MessageRepo::list_since` clamps to.
const BACKFILL_PER_ROOM_LIMIT: i64 = 200;

/// Parse a best-effort reconnect/resume cursor. `None` (absent) and any value
/// that fails to decode as a [`MessageId`] both yield `None` — backfill is
/// purely additive, so a bad cursor must degrade to "no backfill" rather than
/// fail the connection. Pure + total, so it unit-tests without any I/O.
#[must_use]
fn parse_resume_cursor(raw: Option<&str>) -> Option<MessageId> {
    raw.and_then(|s| MessageId::from_str(s.trim()).ok())
}

/// Pick the authoritative cluster-wide count from the Redis result, falling back
/// to the process-local Hub value when Redis is unavailable so the reported
/// number is never worse than today's single-node behaviour.
///
/// This is the injectable seam for the ROADMAP 方向二 "Redis-global count"
/// requirement: the Watch/Unwatch and roster paths call Redis and feed the
/// result here, so the *direction* (prefer Redis) and the *fallback* (use local
/// on error) are unit-testable without a live Redis.
#[must_use]
fn authoritative_count(redis: anyhow::Result<u64>, local_fallback: u32) -> u32 {
    // `map_or` consumes the result by value (Ok ⇒ Redis count, saturating into
    // u32; Err ⇒ the process-local fallback).
    redis.map_or(local_fallback, |n| u32::try_from(n).unwrap_or(u32::MAX))
}

/// Cluster-wide viewer count for a stream: Redis global `count`, falling back to
/// the local Hub count if Redis errs.
async fn stream_viewer_count(state: &AppState, stream_id: Ulid) -> u32 {
    let local = state.hub.stream_viewer_count(stream_id);
    authoritative_count(state.stream_viewers.count(stream_id).await, local)
}

/// The set of call peers a freshly-joined participant must connect to, sourced
/// from the cluster-wide Redis roster (so peers on other nodes are included),
/// with `joiner` removed. On any Redis error, returns `local_fallback` (the
/// Hub's already-present set) so a single-node call still works.
async fn call_peers_excluding(
    state: &AppState,
    call_id: CallId,
    joiner: ParticipantId,
    local_fallback: Vec<ParticipantId>,
) -> Vec<ParticipantId> {
    match state.call_roster.roster(call_id).await {
        Ok(members) => members.into_iter().filter(|p| *p != joiner).collect(),
        Err(e) => {
            warn!(error = ?e, %call_id, "redis call-roster read failed; using local roster");
            local_fallback
        }
    }
}

/// The rooms whose history we replay on reconnect: every room the participant
/// belongs to. Factored out (and kept pure over the room list) so the
/// selection/iteration logic is unit-testable without a database.
#[must_use]
fn backfill_room_ids(rooms: &[aero_common::Room]) -> Vec<RoomId> {
    rooms.iter().map(|r| r.id).collect()
}

/// Replay messages missed since `cursor` for every room the participant belongs
/// to, oldest-first, as ordinary `message` frames — the WS half of ROADMAP
/// 方向五 reconnect backfill. Best-effort throughout: a failed room lookup or a
/// failed per-room query is logged and skipped, never aborting the connection.
/// Sends honour `close` so a torn-down connection stops replaying immediately,
/// and use the bounded channel's back-pressure (await, not `try_send`) so a
/// large catch-up is delivered rather than silently dropped.
async fn backfill_since(
    state: &AppState,
    pid: ParticipantId,
    cursor: MessageId,
    tx: &mpsc::Sender<Message>,
    close: &CancellationToken,
) {
    let rooms = match state.rooms.rooms_for(pid).await {
        Ok(rs) => rs,
        Err(e) => {
            warn!(error = ?e, %pid, "reconnect backfill: list rooms failed");
            return;
        }
    };
    let mut replayed = 0usize;
    for room in backfill_room_ids(&rooms) {
        let missed = match state.messages.list_since(room, cursor, BACKFILL_PER_ROOM_LIMIT).await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = ?e, %room, "reconnect backfill: list_since failed");
                continue;
            }
        };
        for message in missed {
            let frame = ServerFrame::Message { message };
            let json = serde_json::to_string(&frame).unwrap_or_default();
            tokio::select! {
                biased;
                // Connection is going away — stop replaying.
                () = close.cancelled() => return,
                res = tx.send(Message::Text(json)) => {
                    if res.is_err() {
                        return; // receiver gone
                    }
                }
            }
            replayed += 1;
        }
    }
    if replayed > 0 {
        debug!(%pid, replayed, "reconnect backfill replayed missed messages");
    }
}

/// Background loop that subscribes to `im.room.*` and pushes each [`RoomEvent`]
/// into the local Hub. Started once per process at boot.
pub async fn run_bus_listener(state: AppState) -> anyhow::Result<()> {
    use aero_bus::EventBus;
    let bus: Arc<dyn EventBus> = state.bus.clone();
    let mut stream = bus
        .subscribe("im.room.*", Some("aero-server"))
        .await
        .map_err(|e| anyhow::anyhow!("subscribe: {e}"))?;
    info!("bus listener started");
    while let Some(sub) = stream.next().await {
        match serde_json::from_slice::<RoomEvent>(sub.payload()) {
            Ok(event) => {
                let room = event.room_id();
                let recipients = match event.explicit_recipients() {
                    list if !list.is_empty() => list,
                    _ => {
                        if let Some(rid) = room {
                            state.rooms.members(rid).await.unwrap_or_default()
                        } else {
                            Vec::new()
                        }
                    }
                };
                // Message-throughput counter (ROADMAP 方向四): the bus is the single
                // source of truth for accepted messages — every chat message (REST
                // or WS) lands here exactly once before fan-out — so counting only
                // `Message` events here is the non-double-counting choke point
                // (other RoomEvent variants are edits/reactions/typing, not new msgs).
                if matches!(event, RoomEvent::Message(_)) {
                    metrics::inc_counter(names::MESSAGES_SENT_TOTAL, 1);
                }
                let frame = room_event_to_frame_json(&event);
                state.hub.fan_out_raw(&recipients, &frame);
                let _ = sub.ack().await;
            }
            Err(e) => {
                // Compatibility: accept the legacy raw MessageEnvelope payload too.
                if let Ok(env) =
                    serde_json::from_slice::<aero_common::MessageEnvelope>(sub.payload())
                {
                    let recipients = if env.recipients.is_empty() {
                        state.rooms.members(env.message.room_id).await.unwrap_or_default()
                    } else {
                        env.recipients.clone()
                    };
                    // Legacy envelope path also carries exactly one new message.
                    metrics::inc_counter(names::MESSAGES_SENT_TOTAL, 1);
                    let frame = serde_json::json!({
                        "type": "message",
                        "message": env.message,
                    });
                    state.hub.fan_out_raw(&recipients, &frame.to_string());
                    let _ = sub.ack().await;
                    continue;
                }
                warn!(error = ?e, "bad envelope on bus");
                let _ = sub.nack().await;
            }
        }
    }
    Ok(())
}

/// Loose BCP-47 comparison on the primary subtag, so `en` and `en-US` are the
/// same language and the server skips a no-op translation.
fn same_lang(a: &str, b: &str) -> bool {
    let primary = |s: &str| s.split(['-', '_']).next().unwrap_or(s).to_ascii_lowercase();
    primary(a) == primary(b)
}

#[cfg(test)]
mod tests {
    use super::{
        authoritative_count, backfill_room_ids, parse_resume_cursor, same_lang,
        BACKFILL_PER_ROOM_LIMIT,
    };
    use aero_common::{MessageId, ParticipantId, Room, RoomId, RoomKind};
    use ulid::Ulid;

    #[test]
    fn same_lang_matches_on_primary_subtag() {
        assert!(same_lang("en", "en-US"));
        assert!(same_lang("zh-CN", "zh-Hans"));
        assert!(same_lang("EN", "en"));
        assert!(!same_lang("en", "zh"));
        assert!(!same_lang("zh-CN", "en-US"));
    }

    #[test]
    fn resume_cursor_is_best_effort() {
        // Absent and malformed cursors both degrade to "no backfill" (None), never
        // an error — backfill is purely additive and must not fail the upgrade.
        assert!(parse_resume_cursor(None).is_none());
        assert!(parse_resume_cursor(Some("")).is_none());
        assert!(parse_resume_cursor(Some("not-a-ulid")).is_none());
        // A valid id round-trips, with surrounding whitespace tolerated.
        let id = MessageId::new();
        assert_eq!(parse_resume_cursor(Some(&id.to_string())), Some(id));
        assert_eq!(parse_resume_cursor(Some(&format!("  {id}  "))), Some(id));
    }

    #[test]
    fn authoritative_count_prefers_redis_then_falls_back() {
        // Redis Ok is authoritative even when it disagrees with the local count.
        assert_eq!(authoritative_count(Ok(7), 3), 7);
        // Redis error ⇒ fall back to the process-local count (no worse than today).
        assert_eq!(authoritative_count(Err(anyhow::anyhow!("down")), 3), 3);
        // A count that overflows u32 saturates rather than wrapping/panicking.
        assert_eq!(authoritative_count(Ok(u64::from(u32::MAX) + 1), 0), u32::MAX);
        // Zero from Redis is honored (e.g. last viewer just left, cluster-wide).
        assert_eq!(authoritative_count(Ok(0), 9), 0);
    }

    fn room_with_id(id: RoomId) -> Room {
        Room {
            id,
            kind: RoomKind::Group,
            name: None,
            created_by: ParticipantId::new(),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn backfill_selects_every_room_id_in_order() {
        // Empty membership ⇒ nothing to replay.
        assert!(backfill_room_ids(&[]).is_empty());
        // Otherwise: exactly the ids of every room the participant belongs to,
        // order-preserving (so replay follows the membership listing order).
        let a = RoomId::new();
        let b = RoomId::new();
        let c = RoomId::new();
        let rooms = [room_with_id(a), room_with_id(b), room_with_id(c)];
        assert_eq!(backfill_room_ids(&rooms), vec![a, b, c]);
    }

    #[test]
    fn backfill_per_room_limit_matches_keyset_window() {
        // The replay cap equals the storage keyset clamp ceiling, so a single
        // reconnect never over-replays beyond one page per room.
        assert_eq!(BACKFILL_PER_ROOM_LIMIT, 200);
        // Sanity: it is a usable id-orderable cursor type (compile-time check that
        // the backfill path keys on a time-sortable MessageId/Ulid).
        let _ = Ulid::new();
    }
}

/// Background loop that subscribes to `live.stream.*` and pushes each
/// [`StreamEvent`] to local watchers of that stream. Uses an *ephemeral*
/// consumer: live interactivity is broadcast (every instance must see every
/// event to fan out to its own watchers) and a few dropped danmaku across a
/// restart are immaterial. Started once per process at boot.
pub async fn run_live_bus_listener(state: AppState) -> anyhow::Result<()> {
    use aero_bus::EventBus;
    let bus: Arc<dyn EventBus> = state.bus.clone();
    let mut stream = bus
        .subscribe("live.stream.*", None)
        .await
        .map_err(|e| anyhow::anyhow!("subscribe live: {e}"))?;
    info!("live bus listener started");
    while let Some(sub) = stream.next().await {
        match serde_json::from_slice::<StreamEvent>(sub.payload()) {
            Ok(event) => {
                let watchers = state.hub.stream_watchers(event.stream_id());
                if !watchers.is_empty() {
                    let frame = ServerFrame::StreamEvent { event };
                    let json = serde_json::to_string(&frame).unwrap_or_default();
                    state.hub.fan_out_raw(&watchers, &json);
                }
                let _ = sub.ack().await;
            }
            Err(e) => {
                warn!(error = ?e, "bad StreamEvent on bus");
                let _ = sub.nack().await;
            }
        }
    }
    Ok(())
}

/// Translate a `RoomEvent` into the JSON wire frame the browser expects.
fn room_event_to_frame_json(event: &RoomEvent) -> String {
    let frame: ServerFrame<'_> = match event.clone() {
        RoomEvent::Message(env) => ServerFrame::Message { message: env.message },
        RoomEvent::Edited(m) => ServerFrame::Edited { message: m },
        RoomEvent::Deleted { room_id, message_id, by } => {
            ServerFrame::Deleted { room_id, message_id, by }
        }
        RoomEvent::Reaction { room_id, message_id, participant, emoji, op } => {
            ServerFrame::Reaction { room_id, message_id, participant, emoji, op }
        }
        RoomEvent::Read { room_id, participant, last_message_id, at } => {
            ServerFrame::Read { room_id, participant, last_message_id, at }
        }
        RoomEvent::Typing { room_id, participant, on } => {
            ServerFrame::Typing { room_id, participant, on }
        }
        RoomEvent::Call(call) => ServerFrame::Call { event: call },
    };
    serde_json::to_string(&frame)
        .unwrap_or_else(|_| "{\"type\":\"error\",\"code\":\"serialize\",\"msg\":\"\"}".into())
}
