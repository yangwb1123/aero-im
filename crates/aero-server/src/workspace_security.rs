//! Workspace security policy — workspace-wide two-factor enforcement (Wave 24).
//!
//! An admin (Owner/Admin) toggles whether the workspace mandates 2FA; any member
//! may read the current policy. The toggle only flips
//! [`WorkspaceRepo::set_require_2fa`](aero_storage::WorkspaceRepo::set_require_2fa);
//! the actual enforcement lives at the room-data choke point
//! [`ImService::assert_room_access`](aero_im_core::ImService::assert_room_access)
//! (mirroring the deactivation gate): a member of a `require_2fa` workspace who
//! has not activated TOTP is locked out of that workspace's room data until they
//! enroll via the (non-room-gated) `/api/me/2fa/*` routes.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, WorkspaceId, WorkspaceRole};
use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// Workspace-security routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/workspaces/:id/security",
        get(get_security).put(set_security),
    )
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

/// Assert the caller is a member of the workspace (any role), returning their role.
async fn member_role(
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

#[derive(Deserialize)]
struct SecurityReq {
    /// Whether the workspace mandates 2FA for its members.
    require_2fa: bool,
}

/// `PUT /api/workspaces/:id/security` `{require_2fa}` — admin (Owner/Admin) sets the
/// workspace's 2FA mandate. Enforcement is in `assert_room_access`.
async fn set_security(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Json(req): Json<SecurityReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    let role = member_role(&s, ws, auth.participant_id).await?;
    if !role.can_administer() {
        return Err(AeroError::Forbidden("workspace admin required".into()).into());
    }
    s.workspaces
        .set_require_2fa(ws, req.require_2fa)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "require_2fa": req.require_2fa })))
}

/// `GET /api/workspaces/:id/security` — the workspace's current 2FA policy. Any
/// member may read it (so a client can prompt enrollment proactively).
async fn get_security(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    member_role(&s, ws, auth.participant_id).await?;
    let require_2fa = s.workspaces.require_2fa(ws).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "require_2fa": require_2fa })))
}
