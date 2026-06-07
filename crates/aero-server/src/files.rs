//! Per-channel Files tab — list the file/media attachments shared in a room.
//!
//! A thin handler over [`FileIndexRepo`](aero_storage::FileIndexRepo), which
//! projects `File` blocks out of the existing `messages` table (no new storage).
//! The single route asserts room access via `ImService::assert_room_access`
//! (workspace + room membership) BEFORE reading — so a caller can only list the
//! files of a room they belong to — then returns them newest-first with an
//! optional `before` keyset cursor and `limit`. Mounted via [`routes`] and
//! `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, MessageId, RoomId};
use aero_storage::FileIndexRepo;
use axum::{
    extract::{Path, Query, State},
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All files-tab routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/rooms/:id/files", get(list_room_files))
}

/// Default number of files a listing returns when the client omits `limit`. The
/// storage repo applies the hard ceiling.
const DEFAULT_FILES_LIMIT: i64 = 50;

/// Build a [`FileIndexRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> FileIndexRepo {
    FileIndexRepo::new(s.pg.clone())
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

#[derive(Deserialize)]
struct FilesQuery {
    /// Exclusive keyset cursor: page toward older files (a [`MessageId`]).
    before: Option<String>,
    /// Page size; the storage repo clamps it into `[1, 200]`.
    limit: Option<i64>,
}

/// `GET /api/rooms/:id/files?before=&limit=` — the file/media attachments shared
/// in the room, newest-first. The caller must be able to access the room
/// (workspace + room membership), else `403`/`404`. A malformed room or `before`
/// id is `400`.
async fn list_room_files(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Query(q): Query<FilesQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let before = q
        .before
        .as_deref()
        .map(|c| MessageId::from_str(c.trim()))
        .transpose()
        .map_err(|e| AeroError::Invalid(format!("before id: {e}")))?;
    let limit = q.limit.unwrap_or(DEFAULT_FILES_LIMIT);
    let files = repo(&s)
        .list_for_room(room, before, limit)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "files": files })))
}
