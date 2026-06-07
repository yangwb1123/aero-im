//! Per-user channel favorites (starred channels) HTTP surface.
//!
//! A user stars a channel (room) as a favorite, then lists or unstars it.
//! Favorites are PRIVATE to the caller and are pure organizational metadata over
//! existing rooms. Every route is scoped to `auth.participant_id`, so one user can
//! never read or touch another's favorites.
//!
//! Thin handlers over [`ChannelFavoriteRepo`](aero_storage::ChannelFavoriteRepo):
//! starring a room first asserts the caller may access it
//! ([`ImService::assert_room_access`](aero_im_core::ImService::assert_room_access),
//! mirroring [`crate::routes`]), so a user can only favorite a room they belong to;
//! unstarring and listing are owner-scoped at the SQL layer and need no
//! room-access check (they only ever touch the caller's own favorites). Mounted
//! via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, RoomId};
use aero_storage::ChannelFavoriteRepo;
use axum::{
    extract::{Path, State},
    routing::put,
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// All channel-favorite routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/rooms/:id/favorite",
            put(add_favorite).delete(remove_favorite),
        )
        .route("/api/favorites", axum::routing::get(list_favorites))
}

/// Build a [`ChannelFavoriteRepo`] from shared state, over the shared pool. Cheap
/// (a clone of an `Arc<PgPool>`), keeping this feature self-contained.
fn repo(s: &AppState) -> ChannelFavoriteRepo {
    ChannelFavoriteRepo::new(s.pg.clone())
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

/// `PUT /api/rooms/:id/favorite` — star a channel as one of the caller's
/// favorites. The caller must be able to access the room (workspace + room
/// membership), else `403`/`404`. Idempotent: re-starring is a no-op. Always
/// reports `favorited: true`.
async fn add_favorite(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    // Room access guard (workspace + room membership) before recording a favorite,
    // so a caller can only ever favorite a room they belong to.
    s.im.assert_room_access(auth.participant_id, room).await?;
    repo(&s)
        .add(auth.participant_id, room)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "favorited": true })))
}

/// `DELETE /api/rooms/:id/favorite` — unstar a channel. Owner-scoped at the SQL
/// layer (only the caller's own favorite is ever touched), so no room-access check
/// is needed; unstarring a room that was never favorited is a no-op. Always reports
/// `favorited: false`.
async fn remove_favorite(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    repo(&s)
        .remove(auth.participant_id, room)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "favorited": false })))
}

/// `GET /api/favorites` — the caller's favorited room ids, newest first. Always
/// scoped to the caller; no room-access check is needed since they are the
/// caller's own favorites.
async fn list_favorites(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let rooms = repo(&s)
        .list(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "rooms": rooms })))
}
