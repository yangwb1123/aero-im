//! Per-message read receipts HTTP surface ("Seen by …").
//!
//! Today read state is per-ROOM only (a single monotonic cursor per room powers
//! unread badges). This surface adds the Slack/Teams/Lark "Seen by …" affordance:
//! a participant explicitly acknowledges an INDIVIDUAL message, and any member
//! can read the list of who has seen it. Thin handlers — marking first resolves
//! the message to find its room and asserts the caller's room access (so you can
//! only mark / read a message you can see), then delegates to
//! [`MessageReceiptRepo`](aero_storage::MessageReceiptRepo). A successful first
//! mark broadcasts a [`RoomEvent::MessageSeen`](aero_common::RoomEvent) so other
//! members' per-message read indicators update live. Mounted via [`routes`] and
//! `.merge`d into the main router, mirroring [`crate::bookmarks`].

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, MessageId, RoomEvent};
use aero_storage::MessageReceiptRepo;
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// All per-message read-receipt routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/messages/:id/seen", post(mark_seen))
        .route("/api/messages/:id/seen", get(list_seen))
}

fn parse_message(s: &str) -> Result<MessageId, AeroError> {
    MessageId::from_str(s).map_err(|e| AeroError::Invalid(format!("message id: {e}")))
}

/// The per-message receipt repo over the shared pool. `AppState` carries no
/// dedicated field for it, so we build it inline (cheap — a `PgPool` clone),
/// keeping this feature self-contained and `AppState` untouched.
fn repo(s: &AppState) -> MessageReceiptRepo {
    MessageReceiptRepo::new(s.participants.pool().clone())
}

/// `POST /api/messages/:id/seen` — record the caller as having seen the message.
/// Resolves the message to find its room, asserts the caller may access that
/// room, then records the receipt (idempotent). On the FIRST acknowledgement a
/// [`RoomEvent::MessageSeen`] is broadcast so other members' UI updates; a repeat
/// mark is a silent no-op. 404 if the message does not exist or is soft-deleted.
async fn mark_seen(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let message = parse_message(&id_str)?;
    let msg = s
        .messages
        .get(message)
        .await?
        .filter(|m| m.deleted_at.is_none())
        .ok_or_else(|| AeroError::NotFound(format!("message {message}")))?;
    s.im.assert_room_access(auth.participant_id, msg.room_id).await?;

    let created = repo(&s).mark_read(message, auth.participant_id).await?;
    // Only broadcast on a genuine first-seen, so a client re-marking on every
    // scroll doesn't spam the room's bus.
    if created {
        s.im
            .broadcast_room_event(
                msg.room_id,
                RoomEvent::MessageSeen {
                    room_id: msg.room_id,
                    message_id: message,
                    participant: auth.participant_id,
                },
            )
            .await;
    }
    Ok(Json(serde_json::json!({ "seen": true, "created": created })))
}

/// `GET /api/messages/:id/seen` — the reader list for a message: every
/// participant who has acknowledged it, with the time they first saw it, oldest
/// first. Room-access gated (you can only read the "Seen by" list of a message
/// you can see). 404 if the message does not exist or is soft-deleted.
async fn list_seen(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let message = parse_message(&id_str)?;
    let msg = s
        .messages
        .get(message)
        .await?
        .filter(|m| m.deleted_at.is_none())
        .ok_or_else(|| AeroError::NotFound(format!("message {message}")))?;
    s.im.assert_room_access(auth.participant_id, msg.room_id).await?;

    let readers = repo(&s).list_readers(message).await?;
    let body: Vec<serde_json::Value> = readers
        .into_iter()
        .map(|(participant, read_at)| {
            serde_json::json!({
                "participant_id": participant,
                "read_at": read_at
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "message_id": message, "readers": body })))
}
