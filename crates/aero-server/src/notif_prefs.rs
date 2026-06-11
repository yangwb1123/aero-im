//! Notification-preferences HTTP surface: per-channel mute + per-user
//! Do-Not-Disturb (DND).
//!
//! These preferences are consulted by `ImService::dispatch_notifications` (the
//! suppression seam) before a mention/reply notification is persisted or pushed,
//! so a muted room or an active DND window silences the recipient. Thin handlers
//! over [`NotificationPrefsRepo`](aero_storage::NotificationPrefsRepo); mounted
//! via [`routes`] and `.merge`d into the gateway router, mirroring
//! [`crate::collab`].

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, RoomId};
use aero_storage::NotificationPrefsRepo;
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/rooms/:id/mute",
            post(mute_room).delete(unmute_room),
        )
        .route(
            "/api/rooms/:id/notification-level",
            axum::routing::put(set_notification_level).get(get_notification_level),
        )
        .route("/api/notifications/prefs", get(get_prefs))
        .route(
            "/api/notifications/prefs/dnd",
            axum::routing::put(set_dnd),
        )
}

/// The three permitted per-room notification levels (the DB CHECK enforces the
/// same set). Validated at the edge so a bad value is a clean `400` rather than a
/// constraint violation surfaced as a `500`.
const NOTIFICATION_LEVELS: [&str; 3] = ["all", "mentions", "none"];

fn repo(s: &AppState) -> NotificationPrefsRepo {
    NotificationPrefsRepo::new(s.pg.clone())
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

/// `POST /api/rooms/:id/mute` — silence mention/reply notifications from a room
/// the caller belongs to. Idempotent.
async fn mute_room(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    repo(&s).mute(auth.participant_id, room).await?;
    Ok(Json(serde_json::json!({ "room_id": room, "muted": true })))
}

/// `DELETE /api/rooms/:id/mute` — un-silence a room.
async fn unmute_room(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let removed = repo(&s).unmute(auth.participant_id, room).await?;
    Ok(Json(serde_json::json!({ "room_id": room, "muted": false, "removed": removed })))
}

#[derive(Deserialize)]
struct LevelReq {
    /// One of `all` / `mentions` / `none`.
    level: String,
}

/// `PUT /api/rooms/:id/notification-level` — set the caller's per-room
/// notification level (`all` | `mentions` | `none`) for a room they belong to.
/// `all` notifies on every message, `mentions` only on @-mentions, `none` never.
/// Room-access gated like [`mute_room`]. An unrecognized level is a `400`.
async fn set_notification_level(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<LevelReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    let level = req.level.trim().to_ascii_lowercase();
    if !NOTIFICATION_LEVELS.contains(&level.as_str()) {
        return Err(AeroError::Invalid(
            "level must be one of: all, mentions, none".into(),
        )
        .into());
    }
    s.im.assert_room_access(auth.participant_id, room).await?;
    repo(&s).set_level(auth.participant_id, room, &level).await?;
    Ok(Json(serde_json::json!({ "room_id": room, "level": level })))
}

/// `GET /api/rooms/:id/notification-level` — the caller's effective per-room
/// notification level for a room they belong to. Returns the explicit level if
/// set, else the mute-derived default (`none` when the room is muted, else
/// `all`). Room-access gated like [`mute_room`].
async fn get_notification_level(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let r = repo(&s);
    let explicit = r.get_level(auth.participant_id, room).await?;
    // Effective level mirrors the dispatcher: explicit row wins; else a muted
    // room reads as `none`, else `all`.
    let effective = match explicit.as_deref() {
        Some(l) => l.to_owned(),
        None if r.is_muted(auth.participant_id, room).await? => "none".to_owned(),
        None => "all".to_owned(),
    };
    Ok(Json(serde_json::json!({
        "room_id": room,
        "level": effective,
        "explicit": explicit,
    })))
}

/// `GET /api/notifications/prefs` — the caller's muted rooms + DND window.
async fn get_prefs(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let r = repo(&s);
    let muted = r.muted_rooms(auth.participant_id).await?;
    let dnd = r.get_dnd(auth.participant_id).await?;
    Ok(Json(serde_json::json!({
        "muted_rooms": muted,
        "dnd": dnd.map(|(start, end)| serde_json::json!({
            "start_minute": start,
            "end_minute": end,
        })),
    })))
}

#[derive(Deserialize)]
struct DndReq {
    /// Minute-of-day [0, 1440) for the window start; `null`/absent clears DND.
    #[serde(default)]
    start_minute: Option<i32>,
    #[serde(default)]
    end_minute: Option<i32>,
}

/// `PUT /api/notifications/prefs/dnd` — set or clear the caller's daily DND
/// window (minutes-of-day, UTC). Both bounds required to arm; omit/null to clear.
/// A window where start > end is treated as overnight (wraps past midnight).
async fn set_dnd(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<DndReq>,
) -> ApiResult<Json<serde_json::Value>> {
    // Validate bounds when arming; clearing (both None) is always allowed.
    for m in [req.start_minute, req.end_minute].into_iter().flatten() {
        if !(0..aero_storage::notification_prefs::MINUTES_PER_DAY).contains(&m) {
            return Err(AeroError::Invalid("minute-of-day must be in [0, 1440)".into()).into());
        }
    }
    // Arming requires both bounds; a single bound is ambiguous.
    if req.start_minute.is_some() != req.end_minute.is_some() {
        return Err(
            AeroError::Invalid("provide both start_minute and end_minute, or neither".into()).into(),
        );
    }
    repo(&s)
        .set_dnd(auth.participant_id, req.start_minute, req.end_minute)
        .await?;
    Ok(Json(serde_json::json!({
        "dnd": req.start_minute.zip(req.end_minute).map(|(start, end)| serde_json::json!({
            "start_minute": start,
            "end_minute": end,
        })),
    })))
}
