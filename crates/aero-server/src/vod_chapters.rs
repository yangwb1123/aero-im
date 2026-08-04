//! VOD chapters / markers HTTP API (timestamped table-of-contents on a recording).
//!
//! A VOD (recording) chapter is a `start_secs` + title marker the creator adds so
//! viewers can jump to a section; playback reuses the VOD's existing HLS playlist
//! with a client-side seek — no media is processed. Thin handlers over
//! [`aero_storage::VodChapterRepo`].
//!
//! `POST /api/vods/:id/chapters` is gated to the VOD owner (the stream owner who
//! created the recording); the VOD must exist (`404` otherwise). `start_secs` must
//! be `>= 0` and `title` non-empty (`400`). `GET /api/vods/:id/chapters` is public
//! to any viewer, ordered by position. `DELETE /api/vod-chapters/:cid` is
//! creator-scoped at the SQL layer (a non-creator's or unknown id ⇒ `404`).

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, VodChapterId, VodId};
use aero_storage::{VodChapterRepo, VodRepo};
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// Max chapter title length, matching the clip title bound.
const MAX_TITLE_LEN: usize = 256;

/// Mount the VOD-chapter routes, folded into the main router by
/// [`crate::routes::build`].
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/vods/:id/chapters",
            post(create_chapter).get(list_chapters),
        )
        .route(
            "/api/vod-chapters/:cid",
            axum::routing::delete(delete_chapter),
        )
}

fn parse_vod(s: &str) -> Result<VodId, AeroError> {
    VodId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("vod id: {e}")))
}

fn parse_chapter(s: &str) -> Result<VodChapterId, AeroError> {
    VodChapterId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("chapter id: {e}")))
}

fn repo(s: &AppState) -> VodChapterRepo {
    VodChapterRepo::new(s.pg.clone())
}

#[derive(Deserialize)]
struct CreateChapterReq {
    start_secs: i64,
    title: String,
}

/// `POST /api/vods/:id/chapters` — the VOD owner adds a `{start_secs, title}`
/// chapter. The VOD must exist (`404`); only its owner may add chapters (`403`);
/// `start_secs >= 0` and a non-empty title are required (`400`). Returns the row.
async fn create_chapter(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<CreateChapterReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let vod_id = parse_vod(&id_str)?;
    // The VOD must exist and be owned by the caller (the stream owner who recorded
    // it) — chapters are an owner annotation over the recording.
    let vod = VodRepo::new(s.pg.clone())
        .get(vod_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("vod {vod_id}")))?;
    if vod.owner_id != auth.participant_id {
        return Err(AeroError::Forbidden("only the VOD owner may add chapters".into()).into());
    }

    if req.start_secs < 0 {
        return Err(AeroError::Invalid("start_secs must be >= 0".into()).into());
    }
    let start = i32::try_from(req.start_secs)
        .map_err(|_| AeroError::Invalid("start_secs too large".into()))?;
    let title = req.title.trim();
    if title.is_empty() {
        return Err(AeroError::Invalid("title must not be empty".into()).into());
    }
    if title.len() > MAX_TITLE_LEN {
        return Err(AeroError::Invalid("title too long".into()).into());
    }

    let id = repo(&s)
        .create(vod_id, start, title, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    let row = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("chapter".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `GET /api/vods/:id/chapters` — the VOD's chapters, ordered by `start_secs`. Any
/// authenticated viewer may list them.
async fn list_chapters(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let vod_id = parse_vod(&id_str)?;
    let chapters = repo(&s)
        .list_for_vod(vod_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "chapters": chapters })))
}

/// `DELETE /api/vod-chapters/:cid` — delete one of the caller's own chapters.
/// Creator-scoped: `404` if it isn't the caller's chapter (someone else's or unknown).
async fn delete_chapter(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(cid_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_chapter(&cid_str)?;
    let removed = repo(&s)
        .delete(id, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    if !removed {
        return Err(AeroError::NotFound(format!("chapter {id}")).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}
