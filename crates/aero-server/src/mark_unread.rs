//! "Mark as unread" — the triage primitive that rolls a participant's read
//! cursor *backwards* so a channel re-badges as unread (Slack/Teams/Lark).
//!
//! The existing read machinery is strictly forward-only
//! ([`ReceiptRepo::mark_read`](aero_storage::ReceiptRepo::mark_read) refuses to
//! roll the cursor back). This endpoint is its deliberate inverse: it parks the
//! caller's cursor *just before* a chosen message, so that message and
//! everything after it count as unread again.
//!
//! `POST /api/messages/:id/mark-unread` resolves the target message (`404` if it
//! does not exist), asserts the caller may access its room, finds the message's
//! immediate predecessor in that room
//! ([`MessageRepo::list_recent`](aero_storage::MessageRepo::list_recent) with
//! `before = target, limit = 1`, which orders newest-first), and sets the
//! caller's read cursor there via
//! [`ReceiptRepo::set_cursor`](aero_storage::ReceiptRepo::set_cursor) — `None`
//! when the target is the room's first message, which clears the receipt so the
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
/// Resolves the message (`404` if unknown), asserts room access, then parks the
/// caller's read cursor on the message's predecessor (or clears it entirely when
/// the message is the room's first), and best-effort broadcasts the new read
/// state to the room. Returns the new cursor.
///
/// # Errors
/// - `404` when the message does not exist.
/// - `403`/`404` from [`ImService::assert_room_access`] when the caller may not
///   access the room.
/// - `500` if the storage layer errors.
async fn mark_unread(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = parse_message(&id_str)?;
    let msg = s
        .messages
        .get(target)
        .await?
        .ok_or_else(|| AeroError::NotFound(format!("message {target}")))?;
    let room = msg.room_id;
    s.im.assert_room_access(auth.participant_id, room).await?;

    // The predecessor is the newest message strictly before `target` in this
    // room. `None` ⇒ `target` is the room's first message, so the cursor clears
    // and the whole room re-badges unread.
    let predecessor = s
        .messages
        .list_recent(room, Some(target), 1)
        .await?
        .first()
        .map(|m| m.id);

    s.receipts
        .set_cursor(room, auth.participant_id, predecessor)
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
                at: time::OffsetDateTime::now_utc(),
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
