//! Workspace custom-emoji HTTP API.
//!
//! Additive layer over the new [`aero_storage::EmojiRepo`]: a workspace defines
//! named custom emoji (`:shipit:`) backed by an already-uploaded image blob
//! (clients upload via the existing `POST /api/blobs`, then register the emoji
//! by `blob_id`). Members reference emoji by name in messages/reactions; the
//! client resolves name → image URL, served by the existing `GET /api/blobs/:id`.
//!
//! ## Authorization
//!
//! Everything is tenant-scoped. Listing requires effective workspace
//! membership. Creation and deletion require a current effective Owner/Admin,
//! rechecked under the workspace governance lock in the same storage
//! transaction as the mutation. The image blob is likewise locked and must be
//! finalized, available, and scoped to that workspace.

use std::str::FromStr;

use aero_common::{
    BlobId, EmojiId, Error as AeroError, ParticipantId, Result as AeroResult, WorkspaceId,
    WorkspaceRole,
};
use aero_storage::{is_valid_emoji_name, EmojiRepo, WorkspaceRepo};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use aero_auth::AuthUser;

use crate::error::ApiResult;
use crate::state::AppState;

// ---------- Router ----------

/// Mount the custom-emoji routes. Folded into the main router by
/// [`crate::routes::build`]; kept separate so the route table lives next to its
/// tenant-bound handlers.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/workspaces/:id/emoji",
            get(list_emoji).post(create_emoji),
        )
        .route("/api/emoji/:id", axum::routing::delete(delete_emoji))
}

// ---------- Shared helpers ----------

fn parse_workspace_id(s: &str) -> AeroResult<WorkspaceId> {
    WorkspaceId::from_str(s).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn parse_emoji_id(s: &str) -> AeroResult<EmojiId> {
    EmojiId::from_str(s).map_err(|e| AeroError::Invalid(format!("emoji id: {e}")))
}

fn parse_blob_id(s: &str) -> AeroResult<BlobId> {
    BlobId::from_str(s).map_err(|e| AeroError::Invalid(format!("blob id: {e}")))
}

/// Resolve the caller's role in a workspace, rejecting non-members with `403`.
/// Mirrors `crate::workspaces::caller_role` so the "must be a member" check is
/// consistent across feature modules.
async fn caller_role(
    repo: &WorkspaceRepo,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> AeroResult<WorkspaceRole> {
    repo.effective_member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))
}

// ---------- Handlers ----------

/// `GET /api/workspaces/:id/emoji` — list a workspace's custom emoji. Caller
/// must be a workspace member.
async fn list_emoji(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace_id(&id_str)?;
    // Membership gate: resolving the caller's role rejects non-members with 403.
    caller_role(&s.workspaces, ws, auth.participant_id).await?;
    let repo = EmojiRepo::new(s.participants.pool().clone());
    let emoji = repo.list(ws).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(emoji).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct CreateEmojiReq {
    name: String,
    /// The id of an already-uploaded image blob (via `POST /api/blobs`).
    blob_id: String,
}

/// `POST /api/workspaces/:id/emoji` — register a custom emoji. The storage
/// transaction requires a current workspace Owner/Admin and validates the blob
/// tenant/lifecycle boundary. Duplicate names return `409`.
async fn create_emoji(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<CreateEmojiReq>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let ws = parse_workspace_id(&id_str)?;
    let name = req.name.trim();
    if !is_valid_emoji_name(name) {
        return Err(AeroError::Invalid(
            "emoji name must be 1-64 chars of lowercase [a-z0-9_-]".into(),
        )
        .into());
    }
    let blob_id = parse_blob_id(req.blob_id.trim())?;
    let repo = EmojiRepo::new(s.participants.pool().clone());
    let emoji = repo
        .create_authorized(ws, name, blob_id, auth.participant_id)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(emoji).map_err(AeroError::from)?),
    ))
}

/// `DELETE /api/emoji/:id` — delete a custom emoji under a transactionally
/// current workspace Owner/Admin decision.
async fn delete_emoji(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<StatusCode> {
    let id = parse_emoji_id(&id_str)?;
    let repo = EmojiRepo::new(s.participants.pool().clone());
    repo.delete_authorized(id, auth.participant_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
