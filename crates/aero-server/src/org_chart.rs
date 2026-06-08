//! Org chart / manager hierarchy — reporting lines (Lark/Teams-style).
//!
//! Thin handlers over [`OrgChartRepo`](aero_storage::OrgChartRepo): set/clear a
//! participant's manager, and read their manager, direct reports, and full
//! reporting chain (walked upward to the top). No new id type; every value is a
//! [`ParticipantId`].
//!
//! ## Authorization
//!
//! Reading the chart (manager / reports / chain) is open to any authenticated
//! participant — an org chart is normally visible org-wide. **Mutating** a
//! reporting line is gated by the pure, DB-free [`authorize_manage`]: a
//! participant may always set/clear their OWN line, and an Owner/Admin of the
//! default workspace may set/clear anyone's. Self-as-own-manager is rejected
//! `400`. The guard is unit-tested offline (Postgres is absent in CI); the async
//! [`assert_can_manage`] resolves the caller's default-workspace role from the
//! shared [`WorkspaceRepo`](aero_storage::WorkspaceRepo) before applying it.
//! Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{
    Error as AeroError, ParticipantId, Result as AeroResult, WorkspaceId, WorkspaceRole,
};
use aero_storage::{OrgChartRepo, DEFAULT_CHAIN_DEPTH};
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
            "/api/participants/:id/manager",
            put(set_manager).delete(clear_manager).get(get_manager),
        )
        .route("/api/participants/:id/reports", get(list_reports))
        .route("/api/participants/:id/chain", get(get_chain))
}

/// The legacy / default workspace (all-zero UUID), whose Owner/Admin set may
/// administer the org chart for everyone. Mirrors `crate::routes`'
/// `DEFAULT_WORKSPACE_ID`.
const DEFAULT_WORKSPACE_ID: WorkspaceId = WorkspaceId(ulid::Ulid(0));

/// Build an [`OrgChartRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> OrgChartRepo {
    OrgChartRepo::new(s.pg.clone())
}

fn parse_participant(s: &str) -> Result<ParticipantId, AeroError> {
    ParticipantId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
}

/// May `caller` set or clear `target`'s reporting line? A participant may always
/// manage their OWN line (`caller == target`); otherwise the caller must be an
/// administrator (Owner/Admin) of the default workspace. Pure + DB-free so the
/// authorization matrix is unit-tested offline.
///
/// # Errors
/// [`AeroError::Forbidden`] when `caller != target` and the caller is not a
/// default-workspace administrator.
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

/// Resolve the caller's default-workspace role and assert they may manage
/// `target`'s reporting line. Self-management always passes; managing another's
/// line requires Owner/Admin of the default workspace.
async fn assert_can_manage(
    s: &AppState,
    caller: ParticipantId,
    target: ParticipantId,
) -> Result<(), AeroError> {
    // Fast path: self-management needs no role lookup.
    if caller == target {
        return Ok(());
    }
    let role = s
        .workspaces
        .member_role(DEFAULT_WORKSPACE_ID, caller)
        .await
        .map_err(AeroError::from)?;
    authorize_manage(caller, target, role)
}

#[derive(Deserialize)]
struct SetManagerReq {
    /// The participant who will be `:id`'s manager.
    manager_id: String,
}

/// `PUT /api/participants/:id/manager` — set (or re-point) `:id`'s manager.
/// Gated by [`authorize_manage`]; a participant cannot be their own manager
/// (`400`). Returns the new reporting line.
async fn set_manager(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<SetManagerReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = parse_participant(&id_str)?;
    assert_can_manage(&s, auth.participant_id, target).await?;
    let manager = parse_participant(&req.manager_id)?;
    if manager == target {
        return Err(AeroError::Invalid("a participant cannot be their own manager".into()).into());
    }
    repo(&s)
        .set_manager(target, manager, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({
        "participant_id": target,
        "manager_id": manager,
    })))
}

/// `DELETE /api/participants/:id/manager` — clear `:id`'s reporting line. Gated by
/// [`authorize_manage`]. `404` if `:id` had no manager (nothing to clear).
async fn clear_manager(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = parse_participant(&id_str)?;
    assert_can_manage(&s, auth.participant_id, target).await?;
    let removed = repo(&s)
        .clear_manager(target)
        .await
        .map_err(AeroError::from)?;
    if !removed {
        return Err(AeroError::NotFound(format!("manager for participant {target}")).into());
    }
    Ok(Json(serde_json::json!({ "cleared": true })))
}

/// `GET /api/participants/:id/manager` — `:id`'s manager, or `null` if they have
/// no reporting line. Readable by any authenticated participant.
async fn get_manager(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = parse_participant(&id_str)?;
    let manager = repo(&s)
        .manager_of(target)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "manager_id": manager })))
}

/// `GET /api/participants/:id/reports` — `:id`'s direct reports. Readable by any
/// authenticated participant.
async fn list_reports(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = parse_participant(&id_str)?;
    let reports = repo(&s)
        .direct_reports(target)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "reports": reports })))
}

/// `GET /api/participants/:id/chain` — `:id`'s reporting chain walked upward to
/// the top (nearest manager first, excluding `:id`). Bounded depth with cycle
/// detection. Readable by any authenticated participant.
async fn get_chain(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = parse_participant(&id_str)?;
    let chain = repo(&s)
        .reporting_chain(target, DEFAULT_CHAIN_DEPTH)
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
    fn managing_another_requires_default_ws_admin() {
        let caller = ParticipantId::new();
        let other = ParticipantId::new();
        // Admin and Owner may set someone else's manager.
        assert!(authorize_manage(caller, other, Some(WorkspaceRole::Owner)).is_ok());
        assert!(authorize_manage(caller, other, Some(WorkspaceRole::Admin)).is_ok());
        // Member, Guest, and non-members may not — and the denial is a 403.
        for role in [Some(WorkspaceRole::Member), Some(WorkspaceRole::Guest), None] {
            let r = authorize_manage(caller, other, role);
            assert!(r.is_err(), "role {role:?} must not manage another's line");
            assert_eq!(status_of(&r), 403, "denial is 403 for role {role:?}");
        }
    }
}
