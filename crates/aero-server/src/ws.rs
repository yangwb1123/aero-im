//! WebSocket endpoint and protocol.
//!
//! Wire format is JSON; all frames are tagged with `type`. See the design spec for
//! the full message set. JWT is passed via `?token=...` query parameter because
//! browser WebSocket clients can't set custom headers.

use std::sync::Arc;

use aero_common::{Block, MessageEnvelope, MessageId, ParticipantId, RoomId};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::IntoResponse,
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
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
    JoinRoom { room_id: RoomId },
    SendMessage {
        room_id: RoomId,
        blocks: Vec<Block>,
        #[serde(default)]
        reply_to: Option<MessageId>,
    },
    Ping,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerFrame<'a> {
    Presence { room_id: RoomId, online: Vec<ParticipantId> },
    Error { code: &'a str, msg: String },
    Pong,
    Welcome { participant: ParticipantId },
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

    // Welcome
    let _ = tx.send(Message::Text(
        serde_json::to_string(&ServerFrame::Welcome { participant: pid }).unwrap_or_default(),
    ));

    // Outgoing pump
    let outgoing = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sender.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Incoming loop
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
            // Authorize: must be a member
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
            let _ = state
                .im
                .send_message(pid, room_id, blocks, reply_to)
                .await?;
            // Fan-out happens via the NATS subscriber loop, not here.
        }
    }
    Ok(())
}

/// Background loop that subscribes to `im.room.*` and pushes incoming envelopes
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
        match serde_json::from_slice::<MessageEnvelope>(sub.payload()) {
            Ok(env) => {
                let recipients = if env.recipients.is_empty() {
                    state
                        .rooms
                        .members(env.message.room_id)
                        .await
                        .unwrap_or_default()
                } else {
                    env.recipients.clone()
                };
                let payload = json!({
                    "type": "message",
                    "message": env.message,
                });
                state.hub.fan_out(&recipients, &payload);
                let _ = sub.ack().await;
            }
            Err(e) => {
                warn!(error = ?e, "bad envelope on bus");
                let _ = sub.nack().await;
            }
        }
    }
    Ok(())
}
