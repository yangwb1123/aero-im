//! Admin — force-revoke a member's login sessions (offboarding).
//!
//! When a member is offboarded an admin must be able to immediately terminate
//! every device that member is signed in on, independent of the member's own
//! "sign out everywhere" control. This module exposes a single admin/owner-gated
//! route that revokes **all** of a target participant's active `auth_sessions`
//! rows via [`SessionRepo::revoke_all_for_participant`](aero_storage::SessionRepo),
//! invalidating the session inventory.
//!
//! ## Authorization
//!
//! The route is admin/owner-gated. The privilege decision is the pure, DB-free
//! [`authorize_admin`] (mirroring [`crate::deactivation`] / [`crate::ai_dlq`]), so
//! the role matrix is unit-tested offline (Postgres is absent in CI); the async
//! handler resolves the caller's role from the shared
//! [`WorkspaceRepo`](aero_storage::WorkspaceRepo) and then calls the guard.
//!
//! ## Refresh tokens
//!
//! Revoking `auth_sessions` rows invalidates the session inventory (and the
//! session listing / global-sign-out machinery). It does **not** by itself add
//! the underlying refresh tokens to `revoked_tokens`
//! ([`RevokedTokenRepo`](aero_storage::RevokedTokenRepo)); the returned token
//! hashes would have to be blacklisted there to also make an in-flight
//! `POST /api/auth/refresh` fail `401`. This handler returns the revoked count;
//! adding the hashes to the revocation list is a follow-up wiring concern.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{
    Error as AeroError, ParticipantId, Result as AeroResult, WorkspaceId, WorkspaceRole,
};
use aero_storage::{SessionRepo, WorkspaceRepo};
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

/// May `caller` force-revoke a member's sessions? **Admin or owner only** —
/// offboarding a member is workspace administration, the same bar as
/// deactivation and the audit trail. Gates on [`WorkspaceRole::can_administer`].
///
/// # Errors
/// [`AeroError::Forbidden`] for non-administrators (members / guests).
pub fn authorize_admin(caller: WorkspaceRole) -> AeroResult<()> {
    if caller.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden("session revocation requires admin".into()))
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

/// `POST /api/workspaces/:id/members/:pid/revoke-sessions` — admin/owner
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
    assert_admin(&s.workspaces, ws, auth.participant_id).await?;

    let revoked = SessionRepo::new(s.pg.clone())
        .revoke_all_for_participant(target)
        .await
        .map_err(AeroError::from)?;

    // Blacklist every revoked refresh-token hash, mirroring the password-change /
    // delete-account / sign-out-others paths (sessions.rs). Marking `auth_sessions`
    // alone does NOT stop a refresh: `POST /api/auth/refresh` only consults
    // `revoked_tokens`, so an offboarded member's in-flight refresh token would keep
    // minting fresh access tokens until it expired. This closes that gap.
    let blacklist = aero_storage::revoked_token::RevokedTokenRepo::new(s.pg.clone());
    for hash in &revoked {
        if let Err(e) = blacklist.revoke(hash, Some(target)).await {
            // Best-effort: a blacklist hiccup must not fail a successful
            // session-revocation, but it IS the security-critical half, so warn loudly.
            tracing::warn!(error = ?e, %target, "offboarding: failed to blacklist a revoked refresh token");
        }
    }

    // Best-effort audit append (the trail is observability, not a transactional
    // invariant, so a logging hiccup must not fail a successful offboarding).
    if let Err(e) = s
        .audit
        .append(
            ws,
            Some(auth.participant_id),
            "session.revoked",
            Some(&target.to_string()),
            serde_json::json!({ "revoked": revoked.len() }),
        )
        .await
    {
        tracing::warn!(error = ?e, %ws, target = %target, "session.revoked audit append failed");
    }

    Ok(Json(serde_json::json!({ "revoked": revoked.len() })))
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
    fn revoke_sessions_is_admin_and_owner_only_over_all_roles() {
        // Exhaustive over the 4 roles: admin + owner may force-revoke a member's
        // sessions; member and guest may not. Tracks `can_administer` exactly,
        // like the deactivation / audit-view gates.
        for r in ALL {
            assert_eq!(
                allowed(&authorize_admin(r)),
                r.can_administer(),
                "session revocation allowed only for admin/owner, role {r:?}"
            );
        }
        assert!(allowed(&authorize_admin(WorkspaceRole::Owner)));
        assert!(allowed(&authorize_admin(WorkspaceRole::Admin)));
        assert!(!allowed(&authorize_admin(WorkspaceRole::Member)));
        assert!(!allowed(&authorize_admin(WorkspaceRole::Guest)));
    }

    #[test]
    fn revoke_sessions_non_admin_denials_are_403() {
        // Member and guest denials surface as 403 (authorization), not 400/404.
        for r in [WorkspaceRole::Guest, WorkspaceRole::Member] {
            assert_eq!(status_of(&authorize_admin(r)), 403, "role {r:?}");
        }
    }
}
