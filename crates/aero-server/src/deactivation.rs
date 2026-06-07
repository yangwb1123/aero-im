//! Workspace user deactivation — admin revokes a member's access to a workspace.
//!
//! An admin/owner deactivates a member within a workspace, revoking that member's
//! access to the workspace's rooms. The access-enforcement itself is wired in the
//! service layer (`ImService::assert_room_access` consults
//! [`DeactivationRepo::is_deactivated`](aero_storage::DeactivationRepo)); this
//! module owns only the admin-facing CRUD: deactivate, reactivate, and list.
//!
//! ## Authorization
//!
//! Every route is admin/owner-gated. The privilege decision is the pure,
//! DB-free [`authorize_admin`] (mirroring `crate::workspaces`), so the role
//! matrix is unit-tested offline (Postgres is absent in CI); the async handlers
//! resolve the caller's role from the shared [`WorkspaceRepo`](aero_storage::WorkspaceRepo)
//! and then call the guard. Deactivating yourself is rejected `400` — an admin
//! locking themselves out of the tenant is almost certainly a mistake.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{
    Error as AeroError, ParticipantId, Result as AeroResult, WorkspaceId, WorkspaceRole,
};
use aero_storage::{DeactivationRepo, WorkspaceRepo};
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// All deactivation routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/workspaces/:id/members/:pid/deactivate",
            post(deactivate_member),
        )
        .route(
            "/api/workspaces/:id/members/:pid/reactivate",
            post(reactivate_member),
        )
        .route("/api/workspaces/:id/deactivated", get(list_deactivated))
}

/// Build a [`DeactivationRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> DeactivationRepo {
    DeactivationRepo::new(s.pg.clone())
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn parse_participant(s: &str) -> Result<ParticipantId, AeroError> {
    ParticipantId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
}

/// May `caller` deactivate / reactivate / list deactivated members? **Admin or
/// owner only** — managing who has access to a tenant is workspace
/// administration, the same bar as the audit trail and retention policy. Gates on
/// [`WorkspaceRole::can_administer`].
///
/// # Errors
/// [`AeroError::Forbidden`] for non-administrators (members / guests).
pub fn authorize_admin(caller: WorkspaceRole) -> AeroResult<()> {
    if caller.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden("deactivation requires admin".into()))
    }
}

/// Resolve the caller's role and assert they may administer the workspace.
/// Rejects non-members (`403`) and non-admins (`403`).
async fn assert_admin(
    repo: &WorkspaceRepo,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    let role = repo
        .member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))?;
    authorize_admin(role)
}

/// `POST /api/workspaces/:id/members/:pid/deactivate` — admin/owner deactivates a
/// member, revoking their access to the workspace's rooms. Idempotent. An admin
/// deactivating themselves is rejected `400`.
async fn deactivate_member(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((ws_str, pid_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    let target = parse_participant(&pid_str)?;
    assert_admin(&s.workspaces, ws, auth.participant_id).await?;
    if target == auth.participant_id {
        return Err(AeroError::Invalid("cannot deactivate yourself".into()).into());
    }
    repo(&s)
        .deactivate(ws, target, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "deactivated": true })))
}

/// `POST /api/workspaces/:id/members/:pid/reactivate` — admin/owner restores a
/// previously-deactivated member's access. Idempotent.
async fn reactivate_member(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((ws_str, pid_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    let target = parse_participant(&pid_str)?;
    assert_admin(&s.workspaces, ws, auth.participant_id).await?;
    repo(&s)
        .reactivate(ws, target)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "reactivated": true })))
}

/// `GET /api/workspaces/:id/deactivated` — admin/owner lists the workspace's
/// deactivated members, newest first.
async fn list_deactivated(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_admin(&s.workspaces, ws, auth.participant_id).await?;
    let members = repo(&s).list(ws).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "members": members })))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [WorkspaceRole; 4] = [
        WorkspaceRole::Guest,
        WorkspaceRole::Member,
        WorkspaceRole::Admin,
        WorkspaceRole::Owner,
    ];

    /// HTTP status a guard's error maps to (or 200 on `Ok`).
    fn status_of(r: &AeroResult<()>) -> u16 {
        r.as_ref().err().map_or(200, AeroError::status_code)
    }

    fn allowed(r: &AeroResult<()>) -> bool {
        r.is_ok()
    }

    #[test]
    fn deactivation_is_admin_and_owner_only_over_all_roles() {
        // Exhaustive over the 4 roles: admin + owner may manage deactivations;
        // member and guest may not. Tracks `can_administer` exactly, like the
        // audit-view / retention gates in `crate::workspaces`.
        for r in ALL {
            assert_eq!(
                allowed(&authorize_admin(r)),
                r.can_administer(),
                "deactivation allowed only for admin/owner, role {r:?}"
            );
        }
        assert!(allowed(&authorize_admin(WorkspaceRole::Owner)));
        assert!(allowed(&authorize_admin(WorkspaceRole::Admin)));
        assert!(!allowed(&authorize_admin(WorkspaceRole::Member)));
        assert!(!allowed(&authorize_admin(WorkspaceRole::Guest)));
    }

    #[test]
    fn deactivation_non_admin_denials_are_403() {
        // Member and guest denials surface as 403 (authorization), not 400/404.
        for r in [WorkspaceRole::Guest, WorkspaceRole::Member] {
            assert_eq!(status_of(&authorize_admin(r)), 403, "role {r:?}");
        }
    }
}
