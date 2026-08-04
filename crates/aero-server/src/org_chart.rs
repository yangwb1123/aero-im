//! Org chart / manager hierarchy — reporting lines (Lark/Teams-style).
//!
//! Thin handlers over [`OrgChartRepo`](aero_storage::OrgChartRepo): set/clear a
//! participant's manager, and read their manager, direct reports, and full
//! reporting chain (walked upward to the top). No new id type; every value is a
//! [`ParticipantId`].
//!
//! ## Authorization
//!
//! Reading is visible only to effective members of the explicitly selected
//! workspace, and every target/manager must be an effective member of that same
//! workspace. **Mutating** a reporting line is gated by [`authorize_manage`]: a
//! participant may set/clear their own line, while an Owner/Admin may manage
//! another member. Storage repeats those checks transactionally.
//! Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{
    Error as AeroError, ParticipantId, Result as AeroResult, WorkspaceId, WorkspaceRole,
};
use aero_storage::{OrgChartRepo, OrgChartWriteError, DEFAULT_CHAIN_DEPTH};
use axum::{
    extract::{Path, State},
    routing::{get, put},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All org-chart routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/workspaces/:wid/participants/:id/manager",
            put(set_manager).delete(clear_manager).get(get_manager),
        )
        .route(
            "/api/workspaces/:wid/participants/:id/reports",
            get(list_reports),
        )
        .route(
            "/api/workspaces/:wid/participants/:id/chain",
            get(get_chain),
        )
}

/// Build an [`OrgChartRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> OrgChartRepo {
    OrgChartRepo::new(s.pg.clone())
}

fn parse_participant(s: &str) -> Result<ParticipantId, AeroError> {
    ParticipantId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn map_org_write_error(error: OrgChartWriteError) -> AeroError {
    match error {
        OrgChartWriteError::Database(error) => AeroError::from(error),
        OrgChartWriteError::ActorNotMember | OrgChartWriteError::Forbidden => {
            AeroError::Forbidden("not allowed to manage this reporting line".into())
        }
        OrgChartWriteError::TargetNotMember => {
            AeroError::NotFound("participant in workspace".into())
        }
        OrgChartWriteError::ManagerNotMember => {
            AeroError::Invalid("manager is not an active workspace member".into())
        }
        OrgChartWriteError::SelfManager => {
            AeroError::Invalid("a participant cannot be their own manager".into())
        }
        OrgChartWriteError::Cycle => {
            AeroError::Conflict("reporting line would create a cycle".into())
        }
    }
}

/// May `caller` set or clear `target`'s reporting line? A participant may always
/// manage their OWN line (`caller == target`); otherwise the caller must be an
/// administrator (Owner/Admin) of the selected workspace. Pure + DB-free so the
/// authorization matrix is unit-tested offline.
///
/// # Errors
/// [`AeroError::Forbidden`] when `caller != target` and the caller is not a
/// workspace administrator.
pub fn authorize_manage(
    caller: ParticipantId,
    target: ParticipantId,
    caller_role: Option<WorkspaceRole>,
) -> AeroResult<()> {
    if caller == target {
        return Ok(());
    }
    match caller_role {
        Some(role) if role.can_administer() => Ok(()),
        _ => Err(AeroError::Forbidden(
            "setting another participant's manager requires admin".into(),
        )),
    }
}

async fn effective_role(
    s: &AppState,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Result<Option<WorkspaceRole>, AeroError> {
    s.workspaces
        .effective_member_role(workspace, participant)
        .await
        .map_err(AeroError::from)
}

async fn assert_effective_member(
    s: &AppState,
    workspace: WorkspaceId,
    participant: ParticipantId,
    missing: AeroError,
) -> Result<WorkspaceRole, AeroError> {
    effective_role(s, workspace, participant)
        .await?
        .ok_or(missing)
}

/// Resolve the caller's selected-workspace role and assert they may manage
/// `target`'s reporting line.
async fn assert_can_manage(
    s: &AppState,
    workspace: WorkspaceId,
    caller: ParticipantId,
    target: ParticipantId,
) -> Result<(), AeroError> {
    let role = assert_effective_member(
        s,
        workspace,
        caller,
        AeroError::Forbidden("not an active workspace member".into()),
    )
    .await?;
    authorize_manage(caller, target, Some(role))
}

#[derive(Deserialize)]
struct SetManagerReq {
    /// The participant who will be `:id`'s manager.
    manager_id: String,
}

/// Set (or re-point) `:id`'s manager in the selected workspace.
/// Gated by [`authorize_manage`]; a participant cannot be their own manager
/// (`400`). Returns the new reporting line.
async fn set_manager(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((ws_str, id_str)): Path<(String, String)>,
    Json(req): Json<SetManagerReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let workspace = parse_workspace(&ws_str)?;
    let target = parse_participant(&id_str)?;
    assert_can_manage(&s, workspace, auth.participant_id, target).await?;
    assert_effective_member(
        &s,
        workspace,
        target,
        AeroError::NotFound("participant in workspace".into()),
    )
    .await?;
    let manager = parse_participant(&req.manager_id)?;
    assert_effective_member(
        &s,
        workspace,
        manager,
        AeroError::Invalid("manager is not an active workspace member".into()),
    )
    .await?;
    repo(&s)
        .set_manager(workspace, target, manager, auth.participant_id)
        .await
        .map_err(map_org_write_error)?;
    Ok(Json(serde_json::json!({
        "workspace_id": workspace,
        "participant_id": target,
        "manager_id": manager,
    })))
}

/// Clear `:id`'s reporting line in the selected workspace. Gated by
/// [`authorize_manage`]. Returns `404` if `:id` had no manager.
async fn clear_manager(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((ws_str, id_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let workspace = parse_workspace(&ws_str)?;
    let target = parse_participant(&id_str)?;
    assert_can_manage(&s, workspace, auth.participant_id, target).await?;
    let removed = repo(&s)
        .clear_manager(workspace, target, auth.participant_id)
        .await
        .map_err(map_org_write_error)?;
    if !removed {
        return Err(AeroError::NotFound(format!("manager for participant {target}")).into());
    }
    Ok(Json(serde_json::json!({ "cleared": true })))
}

/// Return `:id`'s manager in the selected workspace, or `null` if absent.
async fn get_manager(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((ws_str, id_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let workspace = parse_workspace(&ws_str)?;
    let target = parse_participant(&id_str)?;
    assert_effective_member(
        &s,
        workspace,
        auth.participant_id,
        AeroError::Forbidden("not an active workspace member".into()),
    )
    .await?;
    assert_effective_member(
        &s,
        workspace,
        target,
        AeroError::NotFound("participant in workspace".into()),
    )
    .await?;
    let manager = repo(&s)
        .manager_of(workspace, target)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "manager_id": manager })))
}

/// Return `:id`'s direct reports in the selected workspace.
async fn list_reports(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((ws_str, id_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let workspace = parse_workspace(&ws_str)?;
    let target = parse_participant(&id_str)?;
    assert_effective_member(
        &s,
        workspace,
        auth.participant_id,
        AeroError::Forbidden("not an active workspace member".into()),
    )
    .await?;
    assert_effective_member(
        &s,
        workspace,
        target,
        AeroError::NotFound("participant in workspace".into()),
    )
    .await?;
    let reports = repo(&s)
        .direct_reports(workspace, target)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "reports": reports })))
}

/// Return `:id`'s tenant-local reporting chain, nearest manager first.
async fn get_chain(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((ws_str, id_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let workspace = parse_workspace(&ws_str)?;
    let target = parse_participant(&id_str)?;
    assert_effective_member(
        &s,
        workspace,
        auth.participant_id,
        AeroError::Forbidden("not an active workspace member".into()),
    )
    .await?;
    assert_effective_member(
        &s,
        workspace,
        target,
        AeroError::NotFound("participant in workspace".into()),
    )
    .await?;
    let chain = repo(&s)
        .reporting_chain(workspace, target, DEFAULT_CHAIN_DEPTH)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "chain": chain })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HTTP status a guard's error maps to (or 200 on `Ok`).
    fn status_of(r: &AeroResult<()>) -> u16 {
        r.as_ref().err().map_or(200, AeroError::status_code)
    }

    #[test]
    fn self_management_always_allowed_regardless_of_role() {
        let me = ParticipantId::new();
        // No role at all (not even a member) still passes for one's own line.
        assert!(authorize_manage(me, me, None).is_ok());
        for role in [
            WorkspaceRole::Guest,
            WorkspaceRole::Member,
            WorkspaceRole::Admin,
            WorkspaceRole::Owner,
        ] {
            assert!(authorize_manage(me, me, Some(role)).is_ok());
        }
    }

    #[test]
    fn managing_another_requires_selected_workspace_admin() {
        let caller = ParticipantId::new();
        let other = ParticipantId::new();
        // Admin and Owner may set someone else's manager.
        assert!(authorize_manage(caller, other, Some(WorkspaceRole::Owner)).is_ok());
        assert!(authorize_manage(caller, other, Some(WorkspaceRole::Admin)).is_ok());
        // Member, Guest, and non-members may not — and the denial is a 403.
        for role in [
            Some(WorkspaceRole::Member),
            Some(WorkspaceRole::Guest),
            None,
        ] {
            let r = authorize_manage(caller, other, role);
            assert!(r.is_err(), "role {role:?} must not manage another's line");
            assert_eq!(status_of(&r), 403, "denial is 403 for role {role:?}");
        }
    }
}
