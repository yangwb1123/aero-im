//! Bookmark collections / folders HTTP surface (per-user saved-items folders).
//!
//! The flat per-user saved list ([`crate::bookmarks`]) gains named, ordered
//! folders (Slack/Lark "Saved items" collections). Everything here is
//! OWNER-SCOPED — a collection belongs to the caller, and a saved message can
//! only be filed into one of the caller's own collections — so no room-access
//! check applies (saved items are private). Thin handlers delegating to
//! [`BookmarkCollectionRepo`](aero_storage::BookmarkCollectionRepo). Mounted via
//! [`routes`] and `.merge`d into the main router, mirroring [`crate::bookmarks`].
//!
//! The collection-FILTERED listing of saved items lives on `GET /api/saved`
//! (extended with `?collection_id=` in [`crate::bookmarks`]); this module owns
//! collection CRUD and the per-item assign/clear endpoint.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{BookmarkCollectionId, Error as AeroError, MessageId};
use aero_storage::BookmarkCollectionRepo;
use axum::{
    extract::{Path, State},
    routing::{get, put},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All bookmark-collection routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/bookmark-collections",
            get(list_collections).post(create_collection),
        )
        .route(
            "/api/bookmark-collections/:cid",
            axum::routing::patch(rename_collection).delete(delete_collection),
        )
        .route("/api/saved/:message_id/collection", put(set_item_collection))
}

/// The collections repo over the shared pool, built inline (cheap `PgPool`
/// clone) so `AppState` stays untouched.
fn repo(s: &AppState) -> BookmarkCollectionRepo {
    BookmarkCollectionRepo::new(s.participants.pool().clone())
}

fn parse_collection(s: &str) -> Result<BookmarkCollectionId, AeroError> {
    BookmarkCollectionId::from_str(s)
        .map_err(|e| AeroError::Invalid(format!("collection id: {e}")))
}

fn parse_message(s: &str) -> Result<MessageId, AeroError> {
    MessageId::from_str(s).map_err(|e| AeroError::Invalid(format!("message id: {e}")))
}

#[derive(Deserialize)]
struct CreateReq {
    name: String,
    #[serde(default)]
    position: Option<i32>,
}

/// `POST /api/bookmark-collections` — create a new collection for the caller.
/// Rejects an empty/whitespace name.
async fn create_collection(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(AeroError::Invalid("name must not be empty".into()).into());
    }
    let id = repo(&s)
        .create(auth.participant_id, name, req.position.unwrap_or(0))
        .await?;
    Ok(Json(serde_json::json!({ "id": id, "name": name })))
}

/// `GET /api/bookmark-collections` — the caller's collections in sidebar order.
async fn list_collections(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let cols = repo(&s).list_for(auth.participant_id).await?;
    Ok(Json(serde_json::to_value(cols).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct RenameReq {
    name: String,
    #[serde(default)]
    position: Option<i32>,
}

/// `PATCH /api/bookmark-collections/:cid` — rename (and optionally reposition) one
/// of the caller's collections. 404 if it isn't the caller's. Rejects an empty
/// name.
async fn rename_collection(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(cid_str): Path<String>,
    Json(req): Json<RenameReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let cid = parse_collection(&cid_str)?;
    let name = req.name.trim();
    if name.is_empty() {
        return Err(AeroError::Invalid("name must not be empty".into()).into());
    }
    let updated = repo(&s)
        .rename(cid, auth.participant_id, name, req.position)
        .await?;
    if !updated {
        return Err(AeroError::NotFound(format!("bookmark collection {cid}")).into());
    }
    Ok(Json(serde_json::json!({ "updated": true })))
}

/// `DELETE /api/bookmark-collections/:cid` — delete one of the caller's
/// collections. Its saved items are NOT removed — the DB `ON DELETE SET NULL`
/// unfiles them back into the flat list. 404 if it isn't the caller's.
async fn delete_collection(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(cid_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let cid = parse_collection(&cid_str)?;
    let deleted = repo(&s).delete(cid, auth.participant_id).await?;
    if !deleted {
        return Err(AeroError::NotFound(format!("bookmark collection {cid}")).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[derive(Deserialize)]
struct SetCollectionReq {
    /// Target collection, or `null`/absent to clear the assignment (move the
    /// saved item back to the flat list).
    #[serde(default)]
    collection_id: Option<String>,
}

/// `PUT /api/saved/:message_id/collection` — file the caller's saved message into
/// a collection (or clear its assignment with a null/absent `collection_id`).
/// Owner-scoped on both sides: 404 if the message isn't saved by the caller, or
/// the target collection isn't the caller's.
async fn set_item_collection(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(msg_str): Path<String>,
    Json(req): Json<SetCollectionReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let message = parse_message(&msg_str)?;
    let r = repo(&s);
    if let Some(c) = req.collection_id.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        let cid = parse_collection(c)?;
        let assigned = r.assign(auth.participant_id, message, cid).await?;
        if !assigned {
            // Either the bookmark isn't the caller's or the collection isn't.
            return Err(AeroError::NotFound(format!(
                "saved message {message} or collection {cid}"
            ))
            .into());
        }
        Ok(Json(serde_json::json!({ "collection_id": cid })))
    } else {
        // Clear: a no-op if the bookmark doesn't exist, surfaced as 404 so the
        // caller learns the message isn't saved.
        let cleared = r.clear(auth.participant_id, message).await?;
        if !cleared {
            return Err(AeroError::NotFound(format!("saved message {message}")).into());
        }
        Ok(Json(serde_json::json!({ "collection_id": serde_json::Value::Null })))
    }
}
