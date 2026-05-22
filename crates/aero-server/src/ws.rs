//! WebSocket endpoint and protocol.
//!
//! Wire format is JSON; all frames are tagged with `type`. See the design spec
//! for the full message set. JWT is passed via `?token=...` query parameter
//! because browser WebSocket clients can't set custom headers.

use std::sync::Arc;

use aero_common::{
    Block, CallEvent, CallId, CallKind, CallMode, MessageId, ParticipantId, ReactionOp, RoomEvent,
    RoomId,
};
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
use tracing::{debug, info, instrument, warn};

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
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    state.hub.register(pid, tx.clone());

    let _ = tx.send(Message::Text(
        serde_json::to_string(&ServerFrame::Welcome { participant: pid }).unwrap_or_default(),
    ));

    let outgoing = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sender.send(msg).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Text(text) => {
                if let Err(e) = handle_text(&text, &state, pid, &tx).await {
                    let _ = tx.send(Message::Text(
                        serde_json::to_string(&ServerFrame::Error {
                            code: "handler",
                            msg: e.to_string(),
                        })
                        .unwrap_or_default(),
                    ));
                }
            }
            Message::Ping(p) => {
                let _ = tx.send(Message::Pong(p));
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    state.hub.unregister(pid, &tx);
    outgoing.abort();
    info!(%pid, "ws closed");
}

async fn handle_text(
    text: &str,
    state: &AppState,
    pid: ParticipantId,
    tx: &mpsc::UnboundedSender<Message>,
) -> anyhow::Result<()> {
    let frame: ClientFrame = serde_json::from_str(text)?;
    match frame {
        ClientFrame::Ping => {
            let _ = tx.send(Message::Text(
                serde_json::to_string(&ServerFrame::Pong).unwrap_or_default(),
            ));
        }
        ClientFrame::JoinRoom { room_id } => {
            if !state.rooms.is_member(room_id, pid).await? {
                let _ = tx.send(Message::Text(
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
            let _ = tx.send(Message::Text(
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
