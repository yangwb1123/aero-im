//! Per-room composer drafts HTTP surface (server-persisted message drafts).
//!
//! A user's in-progress composer message for a room is saved server-side so it
//! follows them across devices/reloads (Slack drafts). Drafts are PRIVATE to the
//! authoring user — every route is scoped to `auth.participant_id`, so one user
//! can never read or touch another's draft. Thin handlers: room-scoped routes
//! first [`assert_room_access`](aero_im_core::ImService::assert_room_access) (you
//! may only draft for a room you can post to), then delegate to
//! [`DraftRepo`](aero_storage::DraftRepo). Mounted via [`routes`] and `.merge`d
//! into the main router, mirroring the other feature modules.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Block, Error as AeroError, MessageId, RoomId};
use aero_storage::DraftRepo;
use axum::{
    extract::{Path, State},
    routing::{get, put},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All draft routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/rooms/:id/draft",
            put(save_draft).get(get_draft).delete(delete_draft),
        )
        .route("/api/drafts", get(list_drafts))
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

/// The drafts repo over the shared pool. `AppState` carries no dedicated field
/// for it, so we construct it inline (cheap — a clone of an `Arc<PgPool>`),
/// keeping this feature self-contained and `AppState` untouched.
fn repo(s: &AppState) -> DraftRepo {
    DraftRepo::new(s.pg.clone())
}

#[derive(Deserialize)]
struct SaveDraftReq {
    /// The composer content to stage.
    blocks: Vec<Block>,
    /// Optional message this draft replies to.
    #[serde(default)]
    reply_to: Option<String>,
}

/// `PUT /api/rooms/:id/draft` — save (or replace) the caller's draft for the
/// room. Upsert: there is exactly one draft per `(participant, room)`, so this
/// overwrites any previous draft. Requires room access.
async fn save_draft(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<SaveDraftReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    // Tenant + room-membership guard before staging a draft for the room.
    s.im.assert_room_access(auth.participant_id, room).await?;
    let reply_to = match req.reply_to.as_deref() {
        Some(r) => Some(
            MessageId::from_str(r).map_err(|e| AeroError::Invalid(format!("reply_to id: {e}")))?,
        ),
        None => None,
    };
    repo(&s)
        .upsert(auth.participant_id, room, &req.blocks, reply_to)
        .await?;
    Ok(Json(serde_json::json!({ "saved": true })))
}

/// `GET /api/rooms/:id/draft` — the caller's saved draft for the room, or `null`
/// if none. Requires room access; always scoped to the caller.
async fn get_draft(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let draft = repo(&s).get(auth.participant_id, room).await?;
    Ok(Json(serde_json::json!({
        "draft": serde_json::to_value(draft).map_err(AeroError::from)?,
    })))
}

/// `DELETE /api/rooms/:id/draft` — discard the caller's draft for the room (e.g.
/// after sending or clearing the composer). Requires room access; always scoped
/// to the caller.
async fn delete_draft(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let deleted = repo(&s).delete(auth.participant_id, room).await?;
    Ok(Json(serde_json::json!({ "deleted": deleted })))
}

/// `GET /api/drafts` — all of the caller's drafts across rooms, most-recently-
/// edited first. Per-user, so it only ever returns the caller's own drafts.
async fn list_drafts(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let drafts = repo(&s).list_for(auth.participant_id).await?;
    Ok(Json(serde_json::json!({
        "drafts": serde_json::to_value(drafts).map_err(AeroError::from)?,
    })))
}
