//! Admin — force-revoke a member's login sessions (offboarding).
//!
//! When a member is offboarded an admin must be able to immediately terminate
//! every device that member is signed in on, independent of the member's own
//! "sign out everywhere" control. Because sessions belong to a global
//! participant rather than a workspace, this module exposes a single
//! **owner-only** route that atomically revokes and blacklists all of a target
//! participant's active refresh sessions.
//!
//! ## Authorization
//!
//! The global impact requires an effective workspace owner. The storage mutation
//! locks the workspace and both membership rows, then rechecks active-account,
//! workspace-deactivation, mandatory-2FA, and current role hierarchy before
//! touching any session. The pure [`authorize_global_revoke`] helper mirrors the
//! role matrix for DB-free unit coverage; the database transaction is the
//! authoritative request-path guard.
//!
//! ## Refresh tokens
//!
//! The storage mutation updates `auth_sessions` and inserts every corresponding
//! hash into `revoked_tokens` in one statement, so an error cannot leave a token
//! refreshable after its inventory row was marked revoked.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{
    Error as AeroError, ParticipantId, Result as AeroResult, WorkspaceId, WorkspaceRole,
};
use aero_storage::{auth_session::AdminSessionRevokeError, role_can_manage_member, SessionRepo};
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// Admin session-revocation routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/workspaces/:id/members/:pid/revoke-sessions",
        post(revoke_sessions),
    )
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn parse_participant(s: &str) -> Result<ParticipantId, AeroError> {
    ParticipantId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
}

/// May `caller` force-revoke `target`'s participant-wide sessions?
///
/// The session inventory is global rather than workspace-scoped, so only an
/// owner may perform this operation. The ordinary workspace role hierarchy is
/// still applied to the target.
///
/// # Errors
/// [`AeroError::Forbidden`] when the caller is not an owner or cannot manage the
/// target's current role.
pub fn authorize_global_revoke(caller: WorkspaceRole, target: WorkspaceRole) -> AeroResult<()> {
    if caller == WorkspaceRole::Owner && role_can_manage_member(caller, target) {
        Ok(())
    } else {
        Err(AeroError::Forbidden(
            "global session revocation requires workspace owner".into(),
        ))
    }
}

fn map_revoke_error(error: AdminSessionRevokeError) -> AeroError {
    match error {
        AdminSessionRevokeError::WorkspaceNotFound => AeroError::NotFound("workspace".into()),
        AdminSessionRevokeError::MemberNotFound => AeroError::NotFound("workspace member".into()),
        AdminSessionRevokeError::NotAuthorized => AeroError::Forbidden(
            "global session revocation requires an effective workspace owner".into(),
        ),
        AdminSessionRevokeError::Storage(error) => AeroError::from(error),
    }
}

/// `POST /api/workspaces/:id/members/:pid/revoke-sessions` — workspace owner
/// force-revokes **all** of a target member's active login sessions
/// (offboarding). Idempotent: re-running once a member has no active sessions
/// revokes zero and still returns `200`. Returns the number of sessions revoked.
async fn revoke_sessions(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((ws_str, pid_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    let target = parse_participant(&pid_str)?;

    let revoked = SessionRepo::new(s.pg.clone())
        .revoke_workspace_member_sessions_authorized(ws, auth.participant_id, target)
        .await
        .map_err(map_revoke_error)?;
    s.hub.disconnect_participant(target);
    crate::session_control::publish_revoke_participant(&s, target).await;

    Ok(Json(serde_json::json!({ "revoked": revoked })))
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
    fn global_revoke_is_owner_only_and_respects_target_hierarchy() {
        for caller in ALL {
            for target in ALL {
                assert_eq!(
                    allowed(&authorize_global_revoke(caller, target)),
                    caller == WorkspaceRole::Owner && role_can_manage_member(caller, target),
                    "caller {caller:?}, target {target:?}"
                );
            }
        }
        assert!(allowed(&authorize_global_revoke(
            WorkspaceRole::Owner,
            WorkspaceRole::Owner,
        )));
        assert!(!allowed(&authorize_global_revoke(
            WorkspaceRole::Admin,
            WorkspaceRole::Owner,
        )));
    }

    #[test]
    fn global_revoke_non_owner_denials_are_403() {
        for caller in [
            WorkspaceRole::Guest,
            WorkspaceRole::Member,
            WorkspaceRole::Admin,
        ] {
            assert_eq!(
                status_of(&authorize_global_revoke(caller, WorkspaceRole::Member,)),
                403,
                "role {caller:?}"
            );
        }
    }
}
