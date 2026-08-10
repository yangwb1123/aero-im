//! Legal holds — admin-placed retention exemptions for eDiscovery preservation.
//!
//! An admin places a room — or, with a `NULL` `room_id`, a whole workspace —
//! under a legal hold; while the hold is active the periodic retention sweep
//! ([`WorkspaceRepo::sweep_expired_messages`](aero_storage::WorkspaceRepo::sweep_expired_messages))
//! must NOT soft-delete the covered messages. Thin handlers over
//! [`LegalHoldRepo`](aero_storage::LegalHoldRepo); the sweep's own SQL carries the
//! matching exclusion.
//!
//! ## Authorization
//!
//! Every route is admin/owner-gated (the same bar as retention policy / audit /
//! deactivation). The privilege decision is the pure, DB-free [`authorize_admin`]
//! (mirroring `crate::analytics`), so the role matrix is unit-tested offline
//! (Postgres is absent in CI); the async [`assert_admin`] resolves the caller's
//! role from the shared [`WorkspaceRepo`](aero_storage::WorkspaceRepo) and applies
//! the guard. The release endpoint re-resolves the hold's own workspace and
//! re-checks admin against *that* tenant, so a hold can only ever be lifted by an
//! admin of the workspace it belongs to. Mounted via [`routes`] and `.merge`d into
//! the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{
    Error as AeroError, LegalHoldId, ParticipantId, Result as AeroResult, RoomId, WorkspaceId,
    WorkspaceRole,
};
use aero_storage::LegalHoldRepo;
use axum::{
    extract::{Path, State},
    routing::{delete, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All legal-hold routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/workspaces/:id/legal-holds",
            post(create_hold).get(list_holds),
        )
        .route("/api/legal-holds/:hid", delete(release_hold))
}

/// Build a [`LegalHoldRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> LegalHoldRepo {
    LegalHoldRepo::new(s.pg.clone())
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn parse_hold(s: &str) -> Result<LegalHoldId, AeroError> {
    LegalHoldId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("legal hold id: {e}")))
}

/// May `caller` manage a workspace's legal holds? **Admin or owner only** —
/// placing and lifting preservation orders is an administrative, compliance-grade
/// action, the same bar as the retention policy / audit trail / deactivation.
/// Gates on [`WorkspaceRole::can_administer`].
///
/// # Errors
/// [`AeroError::Forbidden`] for non-administrators (members / guests).
pub fn authorize_admin(caller: WorkspaceRole) -> AeroResult<()> {
    if caller.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden("legal holds require admin".into()))
    }
}

/// Resolve the caller's workspace role and assert they may administer it. Mirrors
/// `crate::analytics::assert_admin`: requires Owner/Admin, rejecting non-members
/// (`403`) and non-admins (`403`).
async fn assert_admin(
    s: &AppState,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    let role = s
        .workspaces
        .effective_member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))?;
    authorize_admin(role)
}

#[derive(Deserialize)]
struct CreateHoldReq {
    /// The single room to hold. Absent / `null` ⇒ a workspace-wide hold.
    #[serde(default)]
    room_id: Option<String>,
    /// Human-readable justification recorded with the hold.
    reason: String,
}

/// `POST /api/workspaces/:id/legal-holds` — place a legal hold over a room
/// (`room_id`) or the whole workspace (omit `room_id`). Admin/owner only; a blank
/// reason is rejected `400`. When a `room_id` is given it must belong to this
/// workspace (else `404`), so a hold can never be scoped to another tenant's room.
/// Returns the created hold.
async fn create_hold(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Json(req): Json<CreateHoldReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;

    let reason = req.reason.trim();
    if reason.is_empty() {
        return Err(AeroError::Invalid("reason must not be empty".into()).into());
    }
    if reason.len() > 1024 {
        return Err(AeroError::Invalid("reason too long".into()).into());
    }

    let room = match req
        .room_id
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        Some(raw) => {
            let room =
                RoomId::from_str(raw).map_err(|e| AeroError::Invalid(format!("room id: {e}")))?;
            Some(room)
        }
        None => None,
    };

    let id = repo(&s)
        .create_authorized(ws, room, reason, auth.participant_id)
        .await?;
    // Re-read so the response carries the full, canonical row (created_at etc.).
    let row = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("legal hold".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `GET /api/workspaces/:id/legal-holds` — the active legal holds in this
/// workspace, newest first. Admin/owner only.
async fn list_holds(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_admin(&s, ws, auth.participant_id).await?;
    let holds = repo(&s).list_active(ws).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "holds": holds })))
}

/// `DELETE /api/legal-holds/:hid` — release (lift) an active legal hold. The
/// hold's own workspace is resolved first, then admin is re-checked against *that*
/// tenant. `404` if the hold is unknown or already released.
async fn release_hold(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_hold(&id_str)?;
    let released = repo(&s)
        .release_authorized(id, auth.participant_id)
        .await?;
    if !released {
        return Err(AeroError::NotFound(format!("active legal hold {id}")).into());
    }
    Ok(Json(serde_json::json!({ "released": true })))
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
    fn legal_holds_are_admin_and_owner_only_over_all_roles() {
        // Exhaustive over the 4 roles: admin + owner may manage holds; member and
        // guest may not. Tracks `can_administer` exactly, like the analytics /
        // deactivation gates.
        for r in ALL {
            assert_eq!(
                authorize_admin(r).is_ok(),
                r.can_administer(),
                "legal holds allowed only for admin/owner, role {r:?}"
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
