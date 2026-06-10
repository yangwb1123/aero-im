//! Per-tenant usage report — admin observability of one workspace's resource
//! consumption (operability).
//!
//! A thin, read-only handler over
//! [`UsageReportRepo`](aero_storage::UsageReportRepo): messages (total + 30d), AI
//! jobs by kind with token totals, referenced-blob count + bytes, and member
//! counts (total + active 30d). There is **no** new table and **no** new id —
//! every figure is a parameterized aggregate scoped to the workspace.
//!
//! ## Authorization
//!
//! Admin/owner-gated, mirroring [`crate::analytics`]: aggregate resource use over
//! an entire tenant is administrative insight. The privilege decision is the pure,
//! DB-free [`authorize_admin`] (unit-tested offline); the async [`assert_admin`]
//! resolves the caller's role from the shared
//! [`WorkspaceRepo`](aero_storage::WorkspaceRepo). Non-members and non-admins are
//! both rejected `403`.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, Result as AeroResult, WorkspaceId, WorkspaceRole};
use aero_storage::UsageReportRepo;
use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// Mount the usage-report route, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/workspaces/:id/admin/usage", get(usage))
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

/// May `caller` view a workspace's usage report? **Admin or owner only** —
/// aggregate resource consumption over an entire tenant is administrative insight,
/// the same bar as analytics / the audit trail. Gates on
/// [`WorkspaceRole::can_administer`].
///
/// # Errors
/// [`AeroError::Forbidden`] for non-administrators (members / guests).
pub fn authorize_admin(caller: WorkspaceRole) -> AeroResult<()> {
    if caller.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden("usage report requires admin".into()))
    }
}

/// Resolve the caller's workspace role and assert they may administer it. Rejects
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

/// `GET /api/workspaces/:id/admin/usage` — per-tenant resource-consumption report
/// (messages, AI jobs + tokens by kind, referenced-blob storage, members).
/// Admin/owner only.
async fn usage(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_admin(&s, ws, auth.participant_id).await?;
    let report = UsageReportRepo::new(s.pg.clone())
        .report(ws)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(report).map_err(AeroError::from)?))
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

    fn status_of(r: &AeroResult<()>) -> u16 {
        r.as_ref().err().map_or(200, AeroError::status_code)
    }

    #[test]
    fn usage_is_admin_and_owner_only_over_all_roles() {
        for r in ALL {
            assert_eq!(
                authorize_admin(r).is_ok(),
                r.can_administer(),
                "usage allowed only for admin/owner, role {r:?}"
            );
        }
        assert!(authorize_admin(WorkspaceRole::Owner).is_ok());
        assert!(authorize_admin(WorkspaceRole::Admin).is_ok());
        assert!(authorize_admin(WorkspaceRole::Member).is_err());
        assert!(authorize_admin(WorkspaceRole::Guest).is_err());
    }

    #[test]
    fn usage_non_admin_denials_are_403() {
        for r in [WorkspaceRole::Guest, WorkspaceRole::Member] {
            assert_eq!(status_of(&authorize_admin(r)), 403, "role {r:?}");
        }
    }
}
