//! "Mark all as read" — clear a participant's unread badge for a single room or
//! across every room they belong to.
//!
//! Both endpoints are thin wrappers over the EXISTING read-receipt machinery —
//! there is no new storage table or repo. Marking a room read finds the newest
//! message currently in it and advances the caller's read cursor to that id via
//! [`ImService::mark_read`](aero_im_core::ImService::mark_read), which persists
//! the receipt AND broadcasts the `Read` room event (so other members' "seen"
//! indicators update) — identical to what `POST /api/rooms/:id/read` does, only
//! the target message is resolved server-side instead of supplied by the client.
//!
//! - `POST /api/rooms/:id/read-all` clears one room (membership asserted first).
//! - `POST /api/read-all` clears every room the caller belongs to, best-effort
//!   per room (a single room's failure is logged and skipped, never aborting the
//!   sweep).
//!
//! Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, RoomId};
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// Both "mark all as read" routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/rooms/:id/read-all", post(mark_room_read_all))
        .route("/api/read-all", post(mark_all_read))
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

/// `POST /api/rooms/:id/read-all` — mark the room read up to its newest message.
///
/// Asserts the caller may access the room (workspace + room membership), then
/// resolves the room's latest message id ([`MessageRepo::list_recent`] with a
/// limit of 1, which orders newest-first) and advances the caller's read cursor
/// there via [`ImService::mark_read`] (persists the receipt and broadcasts the
/// `Read` event). Returns `{read: false}` when the room has no messages (nothing
/// to mark), otherwise `{read: true, last_message_id: <id>}`.
///
/// # Errors
/// [`AeroError::Invalid`] for a malformed room id, [`AeroError::Forbidden`] /
/// [`AeroError::NotFound`] when the caller may not access the room (surfaced by
/// `assert_room_access`), or a storage error reading the latest message /
/// recording the receipt.
async fn mark_room_read_all(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    // Tenant guard (workspace + room membership) before touching the room.
    s.im.assert_room_access(auth.participant_id, room).await?;

    // `list_recent` returns newest-first, so the first row is the latest message.
    let latest = s
        .messages
        .list_recent(room, None, 1)
        .await
        .map_err(AeroError::from)?
        .into_iter()
        .next();

    let Some(msg) = latest else {
        // Empty room — nothing to mark, and `mark_read` has no valid cursor.
        return Ok(Json(serde_json::json!({ "read": false })));
    };

    s.im.mark_read(auth.participant_id, room, msg.id).await?;
    Ok(Json(serde_json::json!({
        "read": true,
        "last_message_id": msg.id,
    })))
}

/// `POST /api/read-all` — mark EVERY room the caller belongs to read.
///
/// Lists the caller's rooms ([`ImService::list_my_rooms`]) and, for each one with
/// at least one message, advances their read cursor to that room's newest message
/// via [`ImService::mark_read`]. Best-effort per room: a room that fails to read
/// its latest message or record the receipt is logged and skipped so a single bad
/// room never aborts the whole sweep. Returns `{rooms_marked: <count>}` — the
/// number of rooms actually advanced (rooms with no messages are not counted).
///
/// # Errors
/// [`AeroError`] only if listing the caller's rooms itself fails; per-room errors
/// are swallowed (logged) and do not propagate.
async fn mark_all_read(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let rooms = s.im.list_my_rooms(auth.participant_id).await?;

    let mut rooms_marked: u32 = 0;
    for room in rooms {
        // Resolve the room's newest message (newest-first, limit 1).
        let latest = match s.messages.list_recent(room.id, None, 1).await {
            Ok(msgs) => msgs.into_iter().next(),
            Err(e) => {
                tracing::warn!(error = ?e, room = %room.id, "read-all: list_recent failed; skipping");
                continue;
            }
        };
        let Some(msg) = latest else { continue };

        match s.im.mark_read(auth.participant_id, room.id, msg.id).await {
            Ok(_) => rooms_marked += 1,
            Err(e) => {
                tracing::warn!(error = ?e, room = %room.id, "read-all: mark_read failed; skipping");
            }
        }
    }

    Ok(Json(serde_json::json!({ "rooms_marked": rooms_marked })))
}
