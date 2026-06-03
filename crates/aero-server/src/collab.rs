//! Collaboration HTTP surface: threads, notifications/unread, and pinned messages.
//!
//! Thin handlers — every business invariant (room access, message ownership,
//! event broadcast) lives in [`ImService`](aero_im_core::ImService) or the
//! repositories. Mounted via [`routes`] and `.merge`d into the main router,
//! mirroring [`crate::workspaces`].

use std::collections::BTreeMap;
use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{
    Error as AeroError, MessageId, NotificationId, RoomId, RoomUnread,
};
use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All collaboration routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        // Threads
        .route("/api/messages/:id/thread", get(thread))
        // Notification inbox + unread badges
        .route("/api/notifications", get(list_notifications))
        .route("/api/notifications/count", get(notification_count))
        .route("/api/notifications/read", post(mark_notifications_read))
        .route("/api/unread", get(list_unread))
        // Pinned messages
        .route("/api/rooms/:id/pins", get(list_pins).post(pin_message))
        .route("/api/rooms/:id/pins/:message_id", axum::routing::delete(unpin_message))
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

fn parse_message(s: &str) -> Result<MessageId, AeroError> {
    MessageId::from_str(s).map_err(|e| AeroError::Invalid(format!("message id: {e}")))
}

// ------------------------------------------------------------------ Threads

#[derive(Deserialize)]
struct ThreadQuery {
    /// Exclusive lower-bound cursor (newest reply id already seen) for forward paging.
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

/// `GET /api/messages/:id/thread` — the replies hanging off a root message plus
/// a summary (count, repliers, last reply). Access is gated on the root's room.
async fn thread(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<ThreadQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let root = parse_message(&id_str)?;
    let root_msg = s
        .messages
        .get(root)
        .await?
        .ok_or_else(|| AeroError::NotFound(format!("message {root}")))?;
    s.im.assert_room_access(auth.participant_id, root_msg.room_id).await?;

    let after = match q.after.as_deref() {
        Some(c) => Some(parse_message(c)?),
        None => None,
    };
    let limit = q.limit.unwrap_or(50);
    let summary = s.messages.thread_summary(root).await?;
    let replies = s.messages.thread_replies(root, after, limit).await?;
    Ok(Json(serde_json::json!({
        "summary": summary,
        "replies": replies,
    })))
}

// ------------------------------------------------------- Notification inbox

#[derive(Deserialize)]
struct NotifQuery {
    #[serde(default)]
    unread: bool,
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

/// `GET /api/notifications` — the caller's own inbox, newest first.
async fn list_notifications(
    State(s): State<AppState>,
    auth: AuthUser,
    Query(q): Query<NotifQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let before = match q.before.as_deref() {
        Some(c) => Some(
            NotificationId::from_str(c)
                .map_err(|e| AeroError::Invalid(format!("notification id: {e}")))?,
        ),
        None => None,
    };
    let list = s
        .notifications
        .list(auth.participant_id, before, q.unread, q.limit)
        .await?;
    Ok(Json(serde_json::to_value(list).map_err(AeroError::from)?))
}

/// `GET /api/notifications/count` — unread notification count (the mention badge).
async fn notification_count(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let count = s.notifications.unread_count(auth.participant_id).await?;
    Ok(Json(serde_json::json!({ "unread": count })))
}

#[derive(Deserialize)]
struct MarkReadReq {
    /// Specific notification ids to mark read.
    #[serde(default)]
    ids: Vec<String>,
    /// Mark every (optionally one room's) unread notification read.
    #[serde(default)]
    all: bool,
    /// When `all`, restrict to this room.
    #[serde(default)]
    room_id: Option<String>,
}

/// `POST /api/notifications/read` — mark notifications read. Always scoped to the
/// caller, so they can never flip another user's inbox.
async fn mark_notifications_read(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<MarkReadReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let updated = if req.all {
        let room = match req.room_id.as_deref() {
            Some(r) => Some(parse_room(r)?),
            None => None,
        };
        s.notifications.mark_all_read(auth.participant_id, room).await?
    } else {
        let ids: Vec<NotificationId> = req
            .ids
            .iter()
            .map(|s| NotificationId::from_str(s))
            .collect::<Result<_, _>>()
            .map_err(|e| AeroError::Invalid(format!("notification id: {e}")))?;
        s.notifications.mark_read(auth.participant_id, &ids).await?
    };
    Ok(Json(serde_json::json!({ "updated": updated })))
}

/// `GET /api/unread` — per-room unread tallies (messages + mentions) for the
/// caller. Drives the sidebar unread + mention badges in one round-trip.
async fn list_unread(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<Vec<RoomUnread>>> {
    let p = auth.participant_id;
    let msgs = s.messages.unread_counts_by_room(p).await?;
    let mentions = s.notifications.unread_counts_by_room(p).await?;

    let mut map: BTreeMap<RoomId, RoomUnread> = BTreeMap::new();
    for (room_id, unread) in msgs {
        map.entry(room_id)
            .or_insert(RoomUnread { room_id, unread: 0, mentions: 0 })
            .unread = unread;
    }
    for (room_id, m) in mentions {
        map.entry(room_id)
            .or_insert(RoomUnread { room_id, unread: 0, mentions: 0 })
            .mentions = m;
    }
    Ok(Json(map.into_values().collect()))
}

// ----------------------------------------------------------- Pinned messages

/// `GET /api/rooms/:id/pins` — the room's pinned messages, newest first.
async fn list_pins(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    let pins = s.im.list_pins(auth.participant_id, room).await?;
    Ok(Json(serde_json::to_value(pins).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct PinReq {
    message_id: String,
}

/// `POST /api/rooms/:id/pins` — pin a message (idempotent).
async fn pin_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<PinReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    let message = parse_message(&req.message_id)?;
    let created = s.im.pin_message(auth.participant_id, room, message).await?;
    Ok(Json(serde_json::json!({ "pinned": true, "created": created })))
}

/// `DELETE /api/rooms/:id/pins/:message_id` — unpin a message.
async fn unpin_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((room_str, message_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    let message = parse_message(&message_str)?;
    let removed = s.im.unpin_message(auth.participant_id, room, message).await?;
    Ok(Json(serde_json::json!({ "pinned": false, "removed": removed })))
}
