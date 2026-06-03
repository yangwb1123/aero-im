//! Channel management HTTP surface: browse public channels, join/leave, archive,
//! and edit channel metadata (topic / description / visibility).
//!
//! Thin handlers — every business invariant (workspace membership, room
//! membership, the public+non-archived join rule, event broadcast) lives in
//! [`ImService`](aero_im_core::ImService). Mounted via [`routes`] and `.merge`d
//! into the gateway router, mirroring [`crate::workspaces`].

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, RoomId, WorkspaceId};
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All channel routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/workspaces/:id/channels", get(list_channels))
        .route("/api/rooms/:id/join", post(join_channel))
        .route("/api/rooms/:id/leave", post(leave_channel))
        .route("/api/rooms/:id/archive", post(archive_channel))
        .route("/api/rooms/:id/channel", axum::routing::patch(update_channel))
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

/// `GET /api/workspaces/:id/channels` — the workspace's public, joinable
/// channels. Caller must be a member of the workspace.
async fn list_channels(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&id_str)?;
    let channels = s.im.list_workspace_channels(auth.participant_id, ws).await?;
    Ok(Json(serde_json::to_value(channels).map_err(AeroError::from)?))
}

/// `POST /api/rooms/:id/join` — join a public, non-archived channel.
async fn join_channel(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.join_channel(auth.participant_id, room).await?;
    Ok(Json(serde_json::json!({ "joined": true })))
}

/// `POST /api/rooms/:id/leave` — leave a channel.
async fn leave_channel(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.leave_channel(auth.participant_id, room).await?;
    Ok(Json(serde_json::json!({ "left": true })))
}

#[derive(Deserialize)]
struct ArchiveReq {
    archived: bool,
}

/// `POST /api/rooms/:id/archive` — archive or un-archive a channel. Caller must
/// be a member of the room.
async fn archive_channel(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<ArchiveReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.archive_channel(auth.participant_id, room, req.archived).await?;
    Ok(Json(serde_json::json!({ "archived": req.archived })))
}

#[derive(Deserialize)]
struct UpdateChannelReq {
    // `Option<Option<_>>` is deliberate (matches `update_me` in `crate::routes`):
    // outer = field present; inner = nullable on the wire, so an explicit `null`
    // clears the value while an omitted field is left untouched.
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    #[allow(clippy::option_option)]
    topic: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    #[allow(clippy::option_option)]
    description: Option<Option<String>>,
    #[serde(default)]
    is_private: Option<bool>,
}

/// Distinguish "absent" from "present and null" for a nullable JSON field, so a
/// PATCH can clear a value (`null`) vs. leave it untouched (omitted). Mirrors the
/// `update_me` avatar handling in [`crate::routes`].
#[allow(clippy::option_option)]
fn deserialize_optional_field<'de, D, T>(d: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// `PATCH /api/rooms/:id/channel` — update channel metadata (topic, description,
/// visibility). Only provided fields change. Caller must be a member of the room.
async fn update_channel(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<UpdateChannelReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    // Trim provided text fields; an empty/whitespace string clears the value.
    let topic = req
        .topic
        .map(|inner| inner.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()));
    let description = req
        .description
        .map(|inner| inner.map(|d| d.trim().to_owned()).filter(|d| !d.is_empty()));
    s.im
        .set_channel_meta(auth.participant_id, room, topic, description, req.is_private)
        .await?;
    Ok(Json(serde_json::json!({ "updated": true })))
}
