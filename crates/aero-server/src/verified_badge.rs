//! Creator verified badge HTTP API (migration 0116).
//!
//! Lets a platform admin grant or revoke a verified badge on any participant,
//! and surfaces `is_verified` / `verified_at` in the participant GET response.
//!
//! Routes:
//!   PATCH /api/admin/participants/:id/verify  — admin only; grant/revoke badge

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, WorkspaceId};
use axum::{
    extract::{Path, State},
    routing::patch,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/admin/participants/:id/verify", patch(set_verified))
}

fn parse_participant_id(s: &str) -> Result<ParticipantId, AeroError> {
    ParticipantId::from_str(s).map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
}

/// Resolve the default workspace id so we can check the actor is an admin.
/// Uses the all-zero UUID (the "default workspace") as the platform-admin anchor,
/// mirroring the pattern used in `admin_sessions.rs` and `workspace_security.rs`.
const DEFAULT_WORKSPACE: WorkspaceId = WorkspaceId(ulid::Ulid(0));

#[derive(Deserialize)]
struct VerifyReq {
    verified: bool,
}

/// `PATCH /api/admin/participants/:id/verify` — grant or revoke the verified
/// badge on a participant. Requires the caller to be an admin/owner of the
/// platform's default workspace.
async fn set_verified(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<VerifyReq>,
) -> ApiResult<Json<serde_json::Value>> {
    // Platform-admin gate: caller must be Admin or Owner of the default workspace.
    let role = s
        .workspaces
        .effective_member_role(DEFAULT_WORKSPACE, auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("platform admin required".into()))?;
    if !role.can_administer() {
        return Err(AeroError::Forbidden("platform admin required".into()).into());
    }
    let target = parse_participant_id(&id_str)?;
    s.participants
        .set_verified_authorized(DEFAULT_WORKSPACE, auth.participant_id, target, req.verified)
        .await
        .map_err(map_verified_write_error)?;
    s.participant_cache.invalidate(&target);
    Ok(Json(serde_json::json!({
        "participant_id": target,
        "is_verified": req.verified,
    })))
}

/// `GET /api/participants/:id/verified` — check whether a participant is verified.
/// Any authenticated user may query this. Surfaced as a thin supplemental endpoint;
/// the main participant GET already returns the full profile but does not yet
/// include is_verified (the column is new). This endpoint bridges that gap.
pub async fn get_verified_status(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = parse_participant_id(&id_str)?;
    let row = s
        .participant_cache
        .get_or_fetch(target, &s.participants)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("participant".into()))?;
    // is_verified is not yet on the Participant struct — query directly.
    let is_verified: bool =
        sqlx::query_scalar("SELECT is_verified FROM participants WHERE id = $1")
            .bind(target.to_uuid())
            .fetch_optional(&s.pg)
            .await
            .map_err(AeroError::from)?
            .unwrap_or(false);
    let verified_at: Option<time::OffsetDateTime> =
        sqlx::query_scalar("SELECT verified_at FROM participants WHERE id = $1")
            .bind(target.to_uuid())
            .fetch_optional(&s.pg)
            .await
            .map_err(AeroError::from)?
            .flatten();
    let _ = row; // participant exists check done
    Ok(Json(serde_json::json!({
        "participant_id": target,
        "is_verified": is_verified,
        "verified_at": verified_at.map(|t| t.to_string()),
    })))
}

fn map_verified_write_error(error: AeroError) -> AeroError {
    match error {
        AeroError::Forbidden(_) => AeroError::Forbidden("platform admin required".into()),
        AeroError::NotFound(_) => AeroError::NotFound("active participant".into()),
        other => other,
    }
}

pub fn get_routes() -> Router<AppState> {
    Router::new().route(
        "/api/participants/:id/verified",
        axum::routing::get(get_verified_status),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_workspace_is_all_zero_ulid() {
        // The all-zero ulid is the documented DEFAULT_WORKSPACE_ID sentinel in routes.rs.
        assert_eq!(
            DEFAULT_WORKSPACE.0.to_string(),
            "00000000000000000000000000"
        );
    }

    #[test]
    fn verified_write_errors_have_stable_public_shapes() {
        let revoked = map_verified_write_error(AeroError::Forbidden("demoted".into()));
        assert_eq!(revoked.status_code(), 403);
        assert_eq!(revoked.to_string(), "forbidden: platform admin required");
        let missing = map_verified_write_error(AeroError::NotFound("deleted target id".into()));
        assert_eq!(missing.status_code(), 404);
        assert_eq!(missing.to_string(), "not found: active participant");
    }
}
