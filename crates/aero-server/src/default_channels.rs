//! Workspace default channels — an admin-curated set new members auto-join.
//!
//! An administrator marks channels (rooms) in a workspace as "defaults"; the
//! register/enroll path then auto-joins every new member into them (the enroll
//! hook reads [`DefaultChannelRepo::list`](aero_storage::DefaultChannelRepo),
//! wired by the orchestrator). Each default is a single `(workspace, room)` pair
//! — marking is idempotent.
//!
//! Thin handlers over [`DefaultChannelRepo`](aero_storage::DefaultChannelRepo):
//! marking/unmarking require the caller to be a workspace administrator
//! (admin/owner, via the shared [`WorkspaceRepo`](aero_storage::WorkspaceRepo),
//! mirroring [`crate::announcements`]) AND the room to belong to the workspace
//! (verified via [`RoomRepo::room_workspace`](aero_storage::RoomRepo), else
//! `400`); listing requires only workspace membership. Mounted via [`routes`]
//! and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, RoomId, WorkspaceId, WorkspaceRole};
use aero_storage::DefaultChannelRepo;
use axum::{
    extract::{Path, State},
    routing::put,
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// All workspace default-channel routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/workspaces/:id/default-channels/:rid",
            put(add_default_channel).delete(remove_default_channel),
        )
        .route(
            "/api/workspaces/:id/default-channels",
            axum::routing::get(list_default_channels),
        )
}

/// Auto-join `participant` into every default channel of `workspace` (best-effort).
/// Called from the enroll paths — registration (the default workspace) and
/// workspace member-add — so a new member lands in the curated channels. A
/// per-room failure is logged and skipped: onboarding convenience must never fail
/// the enrollment itself. `add_member` is an idempotent `ON CONFLICT DO NOTHING`.
pub async fn auto_join_defaults(s: &AppState, workspace: WorkspaceId, participant: ParticipantId) {
    let rooms = match repo(s).list(workspace).await {
        Ok(rooms) => rooms,
        Err(err) => {
            tracing::warn!(error = ?err, %workspace, "default-channel lookup failed");
            return;
        }
    };
    for room in rooms {
        if let Err(err) = s.rooms.add_member(room, participant).await {
            tracing::warn!(error = ?err, %room, %participant, "auto-join default channel failed");
        }
    }
}

/// Build a [`DefaultChannelRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> DefaultChannelRepo {
    DefaultChannelRepo::new(s.pg.clone())
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

/// Resolve the caller's role in a workspace, rejecting non-members with a `403`.
/// Mirrors `crate::announcements::caller_role`.
async fn caller_role(
    s: &AppState,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> Result<WorkspaceRole, AeroError> {
    s.workspaces
        .member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))
}

/// Assert the caller is a workspace administrator (admin/owner). Non-members are
/// rejected `403` (not a member); members/guests are rejected `403` (not an
/// admin). Mirrors `crate::announcements::assert_admin`.
async fn assert_admin(
    s: &AppState,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    let role = caller_role(s, workspace, caller).await?;
    if role.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden("managing default channels requires admin".into()))
    }
}

/// Assert `room` belongs to `workspace`, rejecting a mismatch (or unknown room)
/// with a `400`. Reuses [`RoomRepo::room_workspace`](aero_storage::RoomRepo) so a
/// default can never point at another tenant's (or a non-existent) room.
async fn assert_room_in_workspace(
    s: &AppState,
    workspace: WorkspaceId,
    room: RoomId,
) -> Result<(), AeroError> {
    let owner = s.rooms.room_workspace(room).await.map_err(AeroError::from)?;
    if owner == Some(workspace) {
        Ok(())
    } else {
        Err(AeroError::Invalid("room does not belong to this workspace".into()))
    }
}

/// `PUT /api/workspaces/:id/default-channels/:rid` — mark a channel as a default
/// new members auto-join. The caller must be a workspace administrator
/// (admin/owner), and the room must belong to the workspace (`400` otherwise).
/// Idempotent: re-marking is a no-op.
async fn add_default_channel(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((ws_str, rid_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    let room = parse_room(&rid_str)?;
    assert_admin(&s, ws, auth.participant_id).await?;
    assert_room_in_workspace(&s, ws, room).await?;
    repo(&s).add(ws, room).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "default": true })))
}

/// `DELETE /api/workspaces/:id/default-channels/:rid` — unmark a channel as a
/// default. The caller must be a workspace administrator (admin/owner), and the
/// room must belong to the workspace (`400` otherwise). Idempotent: unmarking a
/// room that was never a default is a no-op.
async fn remove_default_channel(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((ws_str, rid_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    let room = parse_room(&rid_str)?;
    assert_admin(&s, ws, auth.participant_id).await?;
    assert_room_in_workspace(&s, ws, room).await?;
    repo(&s).remove(ws, room).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "default": false })))
}

/// `GET /api/workspaces/:id/default-channels` — the workspace's default channels.
/// Workspace members only.
async fn list_default_channels(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    caller_role(&s, ws, auth.participant_id).await?;
    let rooms = repo(&s).list(ws).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "rooms": rooms })))
}
