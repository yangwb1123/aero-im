//! Per-user channel-sidebar sections HTTP surface.
//!
//! Slack/Teams "sections": a user organizes their channel sidebar into named,
//! ordered sections and assigns channels (rooms) to them. Sections are PRIVATE to
//! the caller and scoped to a workspace — pure organizational metadata over
//! existing rooms. Every route is scoped to `auth.participant_id`, so one user can
//! never read or touch another's sections.
//!
//! Thin handlers over [`ChannelSectionRepo`](aero_storage::ChannelSectionRepo):
//! the workspace-scoped create/list routes assert the caller is a member of the
//! workspace (via the shared [`WorkspaceRepo`], mirroring [`crate::workspaces`]);
//! the section-scoped routes (rename / delete / channel add+remove) are
//! owner-scoped at the SQL layer, so a section id the caller doesn't own simply
//! `404`s. Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{ChannelSectionId, Error as AeroError, RoomId, WorkspaceId};
use aero_storage::ChannelSectionRepo;
use axum::{
    extract::{Path, State},
    routing::{get, patch, put},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All channel-section routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/workspaces/:id/sections",
            get(list_sections).post(create_section),
        )
        .route(
            "/api/sections/:sid",
            patch(rename_section).delete(delete_section),
        )
        .route(
            "/api/sections/:sid/channels/:rid",
            put(add_channel).delete(remove_channel),
        )
}

/// Build a [`ChannelSectionRepo`] from shared state, over the shared pool. Cheap
/// (a clone of an `Arc<PgPool>`), keeping this feature self-contained.
fn repo(s: &AppState) -> ChannelSectionRepo {
    ChannelSectionRepo::new(s.pg.clone())
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn parse_section(s: &str) -> Result<ChannelSectionId, AeroError> {
    ChannelSectionId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("section id: {e}")))
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

/// Assert the caller is a member of the workspace, rejecting non-members with a
/// `403`. Mirrors `crate::scheduled_streams::assert_member`.
async fn assert_member(
    s: &AppState,
    workspace: WorkspaceId,
    caller: aero_common::ParticipantId,
) -> Result<(), AeroError> {
    s.workspaces
        .member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))?;
    Ok(())
}

#[derive(Deserialize)]
struct CreateSectionReq {
    /// The section's display name.
    name: String,
}

/// `POST /api/workspaces/:id/sections` — create a new (empty) section for the
/// caller in the workspace. The caller must be a member of the workspace; a blank
/// name is rejected `400`. Returns the created section's id.
async fn create_section(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Json(req): Json<CreateSectionReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_member(&s, ws, auth.participant_id).await?;
    let name = req.name.trim();
    if name.is_empty() {
        return Err(AeroError::Invalid("name must not be empty".into()).into());
    }
    if name.len() > 128 {
        return Err(AeroError::Invalid("name too long".into()).into());
    }
    let id = repo(&s)
        .create(auth.participant_id, ws, name)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "id": id })))
}

/// `GET /api/workspaces/:id/sections` — the caller's sections in the workspace,
/// ordered by position, each carrying its assigned `room_ids`. Members only;
/// always scoped to the caller.
async fn list_sections(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_member(&s, ws, auth.participant_id).await?;
    let sections = repo(&s)
        .list_for(auth.participant_id, ws)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(sections).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct RenameSectionReq {
    /// The section's new display name.
    name: String,
}

/// `PATCH /api/sections/:sid` — rename one of the caller's own sections.
/// Owner-scoped: a `404` if it isn't the caller's section (someone else's or
/// unknown). A blank name is rejected `400`.
async fn rename_section(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(sid_str): Path<String>,
    Json(req): Json<RenameSectionReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_section(&sid_str)?;
    let name = req.name.trim();
    if name.is_empty() {
        return Err(AeroError::Invalid("name must not be empty".into()).into());
    }
    if name.len() > 128 {
        return Err(AeroError::Invalid("name too long".into()).into());
    }
    let renamed = repo(&s)
        .rename(id, auth.participant_id, name)
        .await
        .map_err(AeroError::from)?;
    if !renamed {
        return Err(AeroError::NotFound(format!("section {id}")).into());
    }
    Ok(Json(serde_json::json!({ "renamed": true })))
}

/// `DELETE /api/sections/:sid` — delete one of the caller's own sections (its
/// channel assignments cascade away). Owner-scoped: a `404` if it isn't the
/// caller's section.
async fn delete_section(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(sid_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_section(&sid_str)?;
    let deleted = repo(&s)
        .delete(id, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    if !deleted {
        return Err(AeroError::NotFound(format!("section {id}")).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// `PUT /api/sections/:sid/channels/:rid` — assign a channel to one of the
/// caller's own sections. Idempotent and owner-scoped at the SQL layer (the
/// insert resolves the section only when the caller owns it), so targeting a
/// section the caller doesn't own affects nothing. `created` reports whether a
/// new assignment was added (vs. the channel already being present, or the
/// section not being the caller's).
async fn add_channel(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((sid_str, rid_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_section(&sid_str)?;
    let room = parse_room(&rid_str)?;
    let created = repo(&s)
        .add_channel(id, auth.participant_id, room)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "added": true, "created": created })))
}

/// `DELETE /api/sections/:sid/channels/:rid` — remove a channel from one of the
/// caller's own sections. Owner-scoped at the SQL layer. `removed` reports whether
/// an assignment was actually dropped.
async fn remove_channel(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((sid_str, rid_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_section(&sid_str)?;
    let room = parse_room(&rid_str)?;
    let removed = repo(&s)
        .remove_channel(id, auth.participant_id, room)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "removed": removed })))
}
