//! WebSocket endpoint and protocol.
//!
//! Wire format is JSON; all frames are tagged with `type`. See the design spec
//! for the full message set. JWT is passed via `?token=...` query parameter
//! because browser WebSocket clients can't set custom headers.

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
    ws.on_upgrade(move |socket| run_socket(socket, state, pid))
}

#[instrument(skip(socket, state), fields(%pid))]
async fn run_socket(socket: WebSocket, state: AppState, pid: ParticipantId) {
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
            let existing = state.hub.call_join(call_id, pid);
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
            state.hub.watch_stream(stream_id, pid);
            // Replay a small danmaku backlog so the new watcher has context.
            if let Ok(lines) = state.live.recent_chat(stream_id, 30).await {
                for line in lines {
                    let frame = ServerFrame::StreamEvent { event: StreamEvent::Chat(line) };
                    let _ = tx.try_send(Message::Text(
                        serde_json::to_string(&frame).unwrap_or_default(),
                    ));
                }
            }
            let count = state.hub.stream_viewer_count(stream_id);
            state.live.publish_viewers(stream_id, count).await;
        }
        ClientFrame::UnwatchStream { stream_id } => {
            state.hub.unwatch_stream(stream_id, pid);
            let count = state.hub.stream_viewer_count(stream_id);
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
    use super::same_lang;

    #[test]
    fn same_lang_matches_on_primary_subtag() {
        assert!(same_lang("en", "en-US"));
        assert!(same_lang("zh-CN", "zh-Hans"));
        assert!(same_lang("EN", "en"));
        assert!(!same_lang("en", "zh"));
        assert!(!same_lang("zh-CN", "en-US"));
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
