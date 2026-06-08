//! Information barriers / ethical walls — admin-managed barred user-group pairs.
//!
//! Microsoft Purview-style: an admin defines barred PAIRS of user-groups
//! (migration 0033); members across a barred pair may not DM each other or share a
//! channel. This module owns the admin-facing CRUD over
//! [`BarrierRepo`](aero_storage::BarrierRepo); the actual ENFORCEMENT lives at the
//! conversation-creation seams ([`crate::dm`] / [`crate::group_dm`]), which call
//! [`BarrierRepo::barred`](aero_storage::BarrierRepo::barred) before creating a
//! room.
//!
//! ## Authorization
//!
//! Every route is admin/owner-gated (the same bar as legal holds / analytics /
//! deactivation). The privilege decision is the pure, DB-free [`authorize_admin`]
//! (mirroring [`crate::legal_holds`]), so the role matrix is unit-tested offline
//! (Postgres is absent in CI); the async [`assert_admin`] resolves the caller's
//! role from the shared [`WorkspaceRepo`](aero_storage::WorkspaceRepo) and applies
//! the guard. The delete endpoint re-resolves the barrier's own workspace and
//! re-checks admin against *that* tenant, so a barrier can only ever be lifted by
//! an admin of the workspace it belongs to. Mounted via [`routes`] and `.merge`d
//! into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{
    BarrierId, Error as AeroError, ParticipantId, Result as AeroResult, UserGroupId, WorkspaceId,
    WorkspaceRole,
};
use aero_storage::{BarrierRepo, UserGroupRepo};
use axum::{
    extract::{Path, State},
    routing::{delete, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All information-barrier routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/workspaces/:id/barriers",
            post(create_barrier).get(list_barriers),
        )
        .route("/api/barriers/:bid", delete(delete_barrier))
}

/// Build a [`BarrierRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> BarrierRepo {
    BarrierRepo::new(s.pg.clone())
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn parse_barrier(s: &str) -> Result<BarrierId, AeroError> {
    BarrierId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("barrier id: {e}")))
}

fn parse_group(s: &str, field: &str) -> Result<UserGroupId, AeroError> {
    UserGroupId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("{field}: {e}")))
}

/// May `caller` manage a workspace's information barriers? **Admin or owner only**
/// — defining ethical walls is a compliance-grade administrative action, the same
/// bar as legal holds / the audit trail / deactivation. Gates on
/// [`WorkspaceRole::can_administer`].
///
/// # Errors
/// [`AeroError::Forbidden`] for non-administrators (members / guests).
pub fn authorize_admin(caller: WorkspaceRole) -> AeroResult<()> {
    if caller.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden("information barriers require admin".into()))
    }
}

/// Resolve the caller's workspace role and assert they may administer it. Mirrors
/// [`crate::legal_holds::assert_admin`]: requires Owner/Admin, rejecting
/// non-members (`403`) and non-admins (`403`).
async fn assert_admin(
    s: &AppState,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    let role = s
        .workspaces
        .member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))?;
    authorize_admin(role)
}

#[derive(Deserialize)]
struct CreateBarrierReq {
    /// One side of the barred pair (a user-group id in this workspace).
    group_a: String,
    /// The other side of the barred pair (a user-group id in this workspace).
    group_b: String,
}

/// `POST /api/workspaces/:id/barriers` — bar a pair of user-groups. Admin/owner
/// only. Both groups must exist and belong to this workspace (else `404`), and the
/// two must be distinct (`400`). Returns the created barrier.
async fn create_barrier(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Json(req): Json<CreateBarrierReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_admin(&s, ws, auth.participant_id).await?;

    let group_a = parse_group(&req.group_a, "group_a")?;
    let group_b = parse_group(&req.group_b, "group_b")?;
    if group_a == group_b {
        return Err(AeroError::Invalid("a group cannot be barred from itself".into()).into());
    }

    // Both groups must live in *this* workspace — a barrier can never reference
    // another tenant's group.
    let groups = UserGroupRepo::new(s.pg.clone());
    for (id, field) in [(group_a, "group_a"), (group_b, "group_b")] {
        let g = groups
            .get(id)
            .await
            .map_err(AeroError::from)?
            .ok_or_else(|| AeroError::NotFound(format!("{field} in this workspace")))?;
        if g.workspace_id != ws {
            return Err(AeroError::NotFound(format!("{field} in this workspace")).into());
        }
    }

    let id = repo(&s)
        .create(ws, group_a, group_b, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    // Re-read so the response carries the full, canonical row (created_at etc.).
    let row = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("barrier".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `GET /api/workspaces/:id/barriers` — the barriers defined in this workspace,
/// newest first. Admin/owner only.
async fn list_barriers(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_admin(&s, ws, auth.participant_id).await?;
    let barriers = repo(&s).list(ws).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "barriers": barriers })))
}

/// `DELETE /api/barriers/:bid` — remove a barrier. The barrier's own workspace is
/// resolved first, then admin is re-checked against *that* tenant. `404` if the
/// barrier is unknown or already removed.
async fn delete_barrier(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_barrier(&id_str)?;
    let barrier = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("barrier {id}")))?;
    // Re-check admin on the barrier's OWN workspace (the path carries no workspace).
    assert_admin(&s, barrier.workspace_id, auth.participant_id).await?;
    let removed = repo(&s)
        .delete(id, barrier.workspace_id)
        .await
        .map_err(AeroError::from)?;
    if !removed {
        return Err(AeroError::NotFound(format!("barrier {id}")).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
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

    #[test]
    fn barriers_are_admin_and_owner_only_over_all_roles() {
        // Exhaustive over the 4 roles: admin + owner may manage barriers; member and
        // guest may not. Tracks `can_administer` exactly, like the legal-hold /
        // analytics / deactivation gates.
        for r in ALL {
            assert_eq!(
                authorize_admin(r).is_ok(),
                r.can_administer(),
                "information barriers allowed only for admin/owner, role {r:?}"
            );
        }
    }

    #[test]
    fn non_admin_denials_are_403() {
        for r in [WorkspaceRole::Guest, WorkspaceRole::Member] {
            assert_eq!(status_of(&authorize_admin(r)), 403, "role {r:?}");
        }
    }
}
