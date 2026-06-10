//! Saved items / bookmarks HTTP surface (personal save-for-later).
//!
//! A user privately saves messages to revisit later (Slack "Saved items"). The
//! saved list is per-user and crosses rooms. Thin handlers — saving first
//! resolves the message to find its room and asserts the caller's access to that
//! room (so you can only save a message you can see), then delegates to
//! [`BookmarkRepo`](aero_storage::BookmarkRepo). Mounted via [`routes`] and
//! `.merge`d into the main router, mirroring the other feature modules.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{BookmarkCollectionId, Error as AeroError, MessageId};
use aero_storage::BookmarkRepo;
use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All bookmark routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/messages/:id/save",
            post(save_message).delete(unsave_message),
        )
        .route("/api/saved", get(list_saved))
}

fn parse_message(s: &str) -> Result<MessageId, AeroError> {
    MessageId::from_str(s).map_err(|e| AeroError::Invalid(format!("message id: {e}")))
}

/// The bookmarks repo over the shared pool. `AppState` does not carry a dedicated
/// field for it, so we construct it inline (cheap — a clone of an `Arc<PgPool>`),
/// keeping this feature self-contained and `AppState` untouched.
fn repo(s: &AppState) -> BookmarkRepo {
    BookmarkRepo::new(s.participants.pool().clone())
}

#[derive(Deserialize)]
struct SaveReq {
    /// Optional free-text note attached to the saved item.
    #[serde(default)]
    note: Option<String>,
}

/// `POST /api/messages/:id/save` — save a message for later. Resolves the
/// message to find its room, asserts the caller may access that room, then saves
/// (idempotent). 404 if the message does not exist.
async fn save_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<SaveReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let message = parse_message(&id_str)?;
    let msg = s
        .messages
        .get(message)
        .await?
        .ok_or_else(|| AeroError::NotFound(format!("message {message}")))?;
    s.im.assert_room_access(auth.participant_id, msg.room_id).await?;
    let note = req.note.as_deref().map(str::trim).filter(|n| !n.is_empty());
    let created = repo(&s)
        .save(auth.participant_id, message, msg.room_id, note)
        .await?;
    Ok(Json(serde_json::json!({ "saved": true, "created": created })))
}

/// `DELETE /api/messages/:id/save` — remove a saved message. Always scoped to the
/// caller, so they can never touch another user's saved items.
async fn unsave_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let message = parse_message(&id_str)?;
    let removed = repo(&s).unsave(auth.participant_id, message).await?;
    Ok(Json(serde_json::json!({ "saved": false, "removed": removed })))
}

#[derive(Deserialize)]
struct SavedQuery {
    #[serde(default)]
    limit: Option<i64>,
    /// Optional folder filter: only saved items filed in this collection. Omit
    /// (or leave empty) for the whole saved list.
    #[serde(default)]
    collection_id: Option<String>,
}

/// `GET /api/saved` — the caller's saved messages, newest-saved first, across all
/// rooms. Soft-deleted messages are omitted by the repository. An optional
/// `?collection_id=` filters to one bookmark collection (folder); when absent the
/// whole saved list is returned.
async fn list_saved(
    State(s): State<AppState>,
    auth: AuthUser,
    Query(q): Query<SavedQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let collection = match q.collection_id.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        Some(c) => Some(
            BookmarkCollectionId::from_str(c)
                .map_err(|e| AeroError::Invalid(format!("collection id: {e}")))?,
        ),
        None => None,
    };
    let saved = repo(&s)
        .list_in_collection(auth.participant_id, collection, q.limit)
        .await?;
    Ok(Json(serde_json::to_value(saved).map_err(AeroError::from)?))
}
