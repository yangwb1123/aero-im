//! Stream categories & discovery HTTP API (Twitch-style browse-by-category/tag).
//!
//! Additive layer over the new [`aero_storage::StreamCategoryRepo`]. Viewers list
//! the category catalog and browse the **live** streams filed under a category;
//! the stream *owner* (`stream.owner_id == auth.participant_id`) files their
//! stream under a category and tags it. Nothing here touches the existing
//! `streams` table or its handlers — categories/tags live in disjoint
//! association tables keyed by the stream's uuid id.
//!
//! Discovery resolves a category by slug, asks the repo for the ids of the
//! currently-live streams under it (the repo applies the `status = 'live'`
//! predicate), then re-hydrates each via [`StreamRepo`](aero_storage::StreamRepo)
//! so the response carries full stream rows. Mounted via [`routes`] and `.merge`d
//! into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, Result as AeroResult};
use aero_storage::StreamCategoryRepo;
use axum::{
    extract::{Path, State},
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use ulid::Ulid;

use crate::error::ApiResult;
use crate::state::AppState;

/// All stream-discovery routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/live/categories", get(list_categories))
        .route("/api/live/categories/:slug/streams", get(category_streams))
        .route(
            "/api/streams/:id/category",
            post(assign_category).delete(clear_category),
        )
        .route("/api/streams/:id/tags", get(list_tags).post(add_tag))
        .route("/api/streams/:id/tags/:tag", delete(remove_tag))
}

/// Default number of live streams a category browse returns.
const CATEGORY_STREAM_LIMIT: i64 = 50;

/// Build a [`StreamCategoryRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> StreamCategoryRepo {
    StreamCategoryRepo::new(s.pg.clone())
}

fn parse_stream_id(s: &str) -> AeroResult<Ulid> {
    Ulid::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("stream id: {e}")))
}

/// The streams table persists the stream's ULID id as a PG `uuid`; the discovery
/// tables key on that same uuid. Convert here so the repo stays uuid-typed.
fn stream_uuid(id: Ulid) -> uuid::Uuid {
    uuid::Uuid::from_u128(id.0)
}

/// Resolve a stream from the path and assert the caller owns it. Returns the
/// stream's uuid id (the association-table key) on success; `NotFound` /
/// `Forbidden` otherwise. Mirrors `crate::stream_mod::require_owner`.
async fn require_owner(s: &AppState, id_str: &str, caller: ParticipantId) -> AeroResult<uuid::Uuid> {
    let stream_id = parse_stream_id(id_str)?;
    let stream = s
        .streams
        .get(stream_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("stream {stream_id}")))?;
    if stream.owner_id != caller {
        return Err(AeroError::Forbidden(
            "only the stream owner may set its category or tags".into(),
        ));
    }
    Ok(stream_uuid(stream_id))
}

/// `GET /api/live/categories` — the whole category catalog, ordered for display.
async fn list_categories(
    State(s): State<AppState>,
    _auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let cats = repo(&s).list_categories().await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "categories": cats })))
}

/// `GET /api/live/categories/:slug/streams` — the live streams filed under the
/// category, fully hydrated. `404` if the slug is unknown.
async fn category_streams(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(slug): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let cat = repo(&s)
        .category_by_slug(slug.trim())
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("category {slug}")))?;
    let ids = repo(&s)
        .streams_in_category(cat.id, CATEGORY_STREAM_LIMIT)
        .await
        .map_err(AeroError::from)?;
    // Re-read each via StreamRepo so the response carries the canonical stream
    // rows (title/owner/status/…), not just ids.
    let mut streams = Vec::with_capacity(ids.len());
    for raw in ids {
        if let Some(stream) = s
            .streams
            .get(Ulid(raw.as_u128()))
            .await
            .map_err(AeroError::from)?
        {
            streams.push(stream);
        }
    }
    Ok(Json(serde_json::json!({
        "category": cat,
        "streams": streams,
    })))
}

#[derive(Deserialize)]
struct AssignCategoryReq {
    /// Slug of the category to file the stream under.
    slug: String,
}

/// `POST /api/streams/:id/category` — owner files their stream under a category
/// (upserts: a stream has at most one category). `404` if the slug is unknown.
async fn assign_category(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<AssignCategoryReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = require_owner(&s, &id_str, auth.participant_id).await?;
    let cat = repo(&s)
        .category_by_slug(req.slug.trim())
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("category {}", req.slug)))?;
    repo(&s).assign(stream, cat.id).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "assigned": true, "category": cat })))
}

/// `DELETE /api/streams/:id/category` — owner clears the stream's category
/// (idempotent).
async fn clear_category(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = require_owner(&s, &id_str, auth.participant_id).await?;
    repo(&s).clear_assignment(stream).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "assigned": false })))
}

#[derive(Deserialize)]
struct AddTagReq {
    /// The tag to attach to the stream.
    tag: String,
}

/// Validate + normalize a tag: trimmed, non-empty, length-bounded, lowercased.
fn normalize_tag(raw: &str) -> AeroResult<String> {
    let tag = raw.trim().to_lowercase();
    if tag.is_empty() {
        return Err(AeroError::Invalid("tag must not be empty".into()));
    }
    if tag.len() > 64 {
        return Err(AeroError::Invalid("tag too long".into()));
    }
    Ok(tag)
}

/// `POST /api/streams/:id/tags` — owner adds a tag to their stream (idempotent).
async fn add_tag(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<AddTagReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = require_owner(&s, &id_str, auth.participant_id).await?;
    let tag = normalize_tag(&req.tag)?;
    repo(&s).add_tag(stream, &tag).await.map_err(AeroError::from)?;
    let tags = repo(&s).tags_for(stream).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "tags": tags })))
}

/// `GET /api/streams/:id/tags` — list a stream's tags. No ownership check: tags
/// are public discovery metadata on a stream.
async fn list_tags(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = stream_uuid(parse_stream_id(&id_str)?);
    let tags = repo(&s).tags_for(stream).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "tags": tags })))
}

/// `DELETE /api/streams/:id/tags/:tag` — owner removes a tag. `404` if the tag
/// wasn't set on the stream.
async fn remove_tag(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((id_str, tag)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = require_owner(&s, &id_str, auth.participant_id).await?;
    let tag = normalize_tag(&tag)?;
    let removed = repo(&s).remove_tag(stream, &tag).await.map_err(AeroError::from)?;
    if !removed {
        return Err(AeroError::NotFound(format!("tag {tag}")).into());
    }
    Ok(Json(serde_json::json!({ "removed": true, "tag": tag })))
}
