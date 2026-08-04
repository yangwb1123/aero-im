//! Workspace custom emoji HTTP API — UUID-primary-key variant (migration 0115).
//!
//! Backs the `workspace_emoji` table introduced in migration 0115. This module
//! runs ALONGSIDE the existing `crate::emoji` routes which operate on the
//! `custom_emoji` (ULID-PK) table from migration 0021. The two stores coexist;
//! clients may use either. Creation and deletion recheck the current effective
//! Owner/Admin under the workspace lock; listing is member-readable.
//!
//! Routes:
//!   POST   /api/workspaces/:id/custom-emoji       — admin creates a custom emoji
//!   GET    /api/workspaces/:id/custom-emoji       — member lists custom emoji
//!   DELETE /api/workspaces/:id/custom-emoji/:eid  — admin deletes a custom emoji

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, WorkspaceId};
use aero_storage::{WorkspaceEmojiRepo, WorkspaceRepo};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::error::ApiResult;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/workspaces/:id/custom-emoji",
            get(list_custom_emoji).post(create_custom_emoji),
        )
        .route(
            "/api/workspaces/:id/custom-emoji/:eid",
            axum::routing::delete(delete_custom_emoji),
        )
}

fn parse_workspace_id(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn parse_emoji_uuid(s: &str) -> Result<Uuid, AeroError> {
    Uuid::from_str(s).map_err(|e| AeroError::Invalid(format!("emoji id: {e}")))
}

/// Resolve the caller's workspace role (and reject non-members with 403).
async fn caller_role(
    repo: &WorkspaceRepo,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> Result<aero_common::WorkspaceRole, AeroError> {
    repo.effective_member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))
}

/// `GET /api/workspaces/:id/custom-emoji` — list workspace custom emoji.
/// Any workspace member may list.
async fn list_custom_emoji(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace_id(&id_str)?;
    caller_role(&s.workspaces, ws, auth.participant_id).await?;
    let repo = WorkspaceEmojiRepo::new(s.pg.clone());
    let emoji = repo.list_for_workspace(ws).await?;
    Ok(Json(serde_json::to_value(emoji).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct CreateCustomEmojiReq {
    name: String,
    blob_id: String,
}

/// `POST /api/workspaces/:id/custom-emoji` — create a custom emoji (admin only).
async fn create_custom_emoji(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<CreateCustomEmojiReq>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let ws = parse_workspace_id(&id_str)?;
    let blob_uuid = parse_emoji_uuid(req.blob_id.trim())?;
    let repo = WorkspaceEmojiRepo::new(s.pg.clone());
    let created = repo
        .create_authorized(ws, req.name.trim(), blob_uuid, auth.participant_id)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(created).map_err(AeroError::from)?),
    ))
}

/// `DELETE /api/workspaces/:id/custom-emoji/:eid` — delete a custom emoji (admin only).
async fn delete_custom_emoji(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((id_str, eid_str)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let ws = parse_workspace_id(&id_str)?;
    let eid = parse_emoji_uuid(&eid_str)?;
    let repo = WorkspaceEmojiRepo::new(s.pg.clone());
    repo.delete_authorized(eid, ws, auth.participant_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
