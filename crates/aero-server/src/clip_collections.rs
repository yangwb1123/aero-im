//! Clip collections / playlists — per-user named, ordered groups of stream
//! clips.
//!
//! A viewer can create a named collection (like a YouTube playlist), add or
//! remove their own clips, and share the ordered list with others. The
//! collection is creator-owned: only the creator may delete it. Adding/removing
//! clips is also creator-scoped here (the handler checks the caller owns the
//! collection). Thin handlers over
//! [`ClipCollectionRepo`](aero_storage::ClipCollectionRepo).

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{ClipCollectionId, ClipId, Error as AeroError};
use aero_storage::ClipCollectionRepo;
use axum::{
    extract::{Path, State},
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All clip-collection routes, ready to `.merge` into the gateway router.
pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/api/clip-collections",
            post(create_collection).get(list_collections),
        )
        .route(
            "/api/clip-collections/:id",
            get(get_collection).delete(delete_collection),
        )
        .route(
            "/api/clip-collections/:id/clips",
            post(add_clip).get(list_clips),
        )
        .route(
            "/api/clip-collections/:id/clips/:clip_id",
            delete(remove_clip),
        )
        .with_state(state)
}

fn repo(s: &AppState) -> ClipCollectionRepo {
    ClipCollectionRepo::new(s.pg.clone())
}

fn parse_collection(s: &str) -> Result<ClipCollectionId, AeroError> {
    ClipCollectionId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("collection id: {e}")))
}

fn parse_clip(s: &str) -> Result<ClipId, AeroError> {
    ClipId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("clip id: {e}")))
}

#[derive(Deserialize)]
struct CreateCollectionReq {
    title: String,
    #[serde(default)]
    description: Option<String>,
}

/// `POST /api/clip-collections` — create a new collection for the caller.
async fn create_collection(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateCollectionReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let title = req.title.trim();
    if title.is_empty() {
        return Err(AeroError::Invalid("title must not be empty".into()).into());
    }
    if title.len() > 256 {
        return Err(AeroError::Invalid("title too long".into()).into());
    }
    let desc = req
        .description
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let id = repo(&s)
        .create(auth.participant_id, title, desc)
        .await
        .map_err(AeroError::from)?;
    // Re-read to return the full canonical row (created_at).
    let row = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("collection".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `GET /api/clip-collections` — the caller's collections, newest first.
async fn list_collections(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let rows = repo(&s)
        .list_for_creator(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "collections": rows })))
}

/// `GET /api/clip-collections/:id` — get one collection by id.
async fn get_collection(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_collection(&id_str)?;
    let row = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("collection {id}")))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `DELETE /api/clip-collections/:id` — delete one of the caller's collections.
/// Creator-scoped: `404` if it is not the caller's collection or does not exist.
async fn delete_collection(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_collection(&id_str)?;
    let removed = repo(&s)
        .delete(id, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    if !removed {
        return Err(AeroError::NotFound(format!("collection {id}")).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[derive(Deserialize)]
struct AddClipReq {
    clip_id: String,
    #[serde(default)]
    position: i32,
}

/// `POST /api/clip-collections/:id/clips` — add a clip to the collection.
/// The caller must own the collection (`403` otherwise).
async fn add_clip(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<AddClipReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_collection(&id_str)?;
    let clip = parse_clip(&req.clip_id)?;

    // Guard: caller must own the collection.
    let row = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("collection {id}")))?;
    if row.creator_id != auth.participant_id.to_uuid() {
        return Err(AeroError::Forbidden("only the collection owner may add clips".into()).into());
    }

    repo(&s)
        .add_clip(id, clip, req.position)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "added": true })))
}

/// `DELETE /api/clip-collections/:id/clips/:clip_id` — remove a clip from the
/// collection. Caller must own the collection.
async fn remove_clip(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((id_str, clip_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_collection(&id_str)?;
    let clip = parse_clip(&clip_str)?;

    // Guard: caller must own the collection.
    let row = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("collection {id}")))?;
    if row.creator_id != auth.participant_id.to_uuid() {
        return Err(
            AeroError::Forbidden("only the collection owner may remove clips".into()).into(),
        );
    }

    let removed = repo(&s)
        .remove_clip(id, clip)
        .await
        .map_err(AeroError::from)?;
    if !removed {
        return Err(AeroError::NotFound(format!("clip {clip} in collection {id}")).into());
    }
    Ok(Json(serde_json::json!({ "removed": true })))
}

/// `GET /api/clip-collections/:id/clips` — ordered list of clip ids in the
/// collection. Any authenticated caller may read a collection's clips.
async fn list_clips(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_collection(&id_str)?;
    // Verify the collection exists before returning its clips.
    if repo(&s).get(id).await.map_err(AeroError::from)?.is_none() {
        return Err(AeroError::NotFound(format!("collection {id}")).into());
    }
    let clips = repo(&s).list_clips(id).await.map_err(AeroError::from)?;
    let clip_strs: Vec<String> = clips.iter().map(|c| c.to_string()).collect();
    Ok(Json(serde_json::json!({ "clips": clip_strs })))
}
