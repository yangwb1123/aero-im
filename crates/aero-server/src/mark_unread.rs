//! "Mark as unread" — the triage primitive that rolls a participant's read
//! cursor *backwards* so a channel re-badges as unread (Slack/Teams/Lark).
//!
//! The existing read machinery is strictly forward-only
//! ([`ReceiptRepo::mark_read`](aero_storage::ReceiptRepo::mark_read) refuses to
//! roll the cursor back). This endpoint is its deliberate inverse: it parks the
//! caller's cursor *just before* a chosen message, so that message and
//! everything after it count as unread again.
//!
//! `POST /api/messages/:id/mark-unread` resolves the target's canonical room,
//! locks and rechecks the caller's effective access, finds the immediate visible
//! predecessor, and changes the cursor in one storage transaction via
//! [`ReceiptRepo::mark_unread_authorized`](aero_storage::ReceiptRepo::mark_unread_authorized).
//! `None` when the target is the room's first message clears the receipt so the
//! whole room re-badges unread. It then best-effort broadcasts a
//! [`RoomEvent::Read`] on `im.room.{room}` (mirroring
//! [`ImService::mark_read`](aero_im_core::ImService::mark_read)) so other
//! members' "seen" indicators stay in sync.
//!
//! Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, MessageId, RoomEvent};
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// The mark-as-unread route, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/messages/:id/mark-unread", post(mark_unread))
}

fn parse_message(s: &str) -> Result<MessageId, AeroError> {
    MessageId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("message id: {e}")))
}

/// `POST /api/messages/:id/mark-unread` — roll the caller's read cursor back to
/// *before* the given message, so it (and everything newer) re-badges as unread.
///
/// Resolves the message (`404` if unknown), then transactionally rechecks room
/// access and parks the caller's read cursor on the message's predecessor (or
/// clears it entirely when the message is the room's first). Best-effort
/// broadcasts the new read state to the room and returns the new cursor.
///
/// # Errors
/// - `404` when the message does not exist.
/// - `403` when the caller may not access the message's room at commit.
/// - `500` if the storage layer errors.
async fn mark_unread(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = parse_message(&id_str)?;
    let (room, predecessor, updated_at) = s
        .receipts
        .mark_unread_authorized(auth.participant_id, target)
        .await?;

    // Best-effort: tell the room the caller's read cursor moved (so other
    // members' "seen" indicators stay live), mirroring `ImService::mark_read`.
    // Only meaningful when a predecessor exists — a cleared cursor has no
    // `last_message_id` to report (the `Read` event's field is non-optional), so
    // that case is intentionally not broadcast.
    if let Some(last) = predecessor {
        // Through the stamped seam (ROADMAP3 方向一) so this Read carries a
        // `seq` like ImService::mark_read's own broadcast; best-effort.
        s.im.broadcast_room_event(
            room,
            RoomEvent::Read {
                room_id: room,
                participant: auth.participant_id,
                last_message_id: last,
                at: updated_at,
            },
        )
        .await;
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "room_id": room,
        "last_read_message_id": predecessor,
    })))
}
