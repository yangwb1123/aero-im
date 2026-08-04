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
    Router::new()
        .route(
            "/api/workspaces/:id/security",
            get(get_security).put(set_security),
        )
        .route(
            "/api/workspaces/:id/storage-region",
            get(get_storage_region).put(set_storage_region),
        )
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

/// Assert the caller is a member of the workspace (any role), returning their role.
async fn effective_member_role(
    s: &AppState,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> Result<WorkspaceRole, AeroError> {
    s.workspaces
        .effective_member_role(workspace, caller)
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
    s.workspaces
        .set_require_2fa_authorized(ws, req.require_2fa, auth.participant_id)
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
    effective_member_role(&s, ws, auth.participant_id).await?;
    let require_2fa = s
        .workspaces
        .require_2fa(ws)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "require_2fa": require_2fa })))
}

#[derive(Deserialize)]
struct StorageRegionReq {
    storage_region: String,
}

fn canonical_requested_region(
    router: &aero_storage::RegionRouter,
    requested: &str,
) -> Result<String, AeroError> {
    let requested = requested.trim();
    if requested.is_empty() {
        return Err(AeroError::Invalid(
            "storage_region must be `default` or a configured region".into(),
        ));
    }
    router
        .canonical_region_code(Some(requested))
        .map_err(|error| AeroError::Invalid(error.to_string()))
}

fn storage_region_response(
    s: &AppState,
    configured: Option<&str>,
) -> Result<serde_json::Value, AeroError> {
    let effective = s
        .region_router
        .canonical_region_code(configured)
        .map_err(|error| {
            AeroError::Internal(anyhow::anyhow!(
                "workspace has unusable storage region: {error}"
            ))
        })?;
    Ok(serde_json::json!({
        "storage_region": effective,
        "available_regions": s.region_router.configured_region_codes(),
    }))
}

/// Any member may inspect where future blobs will be placed.
async fn get_storage_region(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    effective_member_role(&s, ws, auth.participant_id).await?;
    let configured = s
        .workspaces
        .region_code(ws)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(storage_region_response(&s, configured.as_deref())?))
}

/// Owner/Admin updates the placement policy for future blobs.
///
/// Already-reserved blobs retain their immutable `blobs.storage_region`.
async fn set_storage_region(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Json(req): Json<StorageRegionReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    let canonical = canonical_requested_region(&s.region_router, &req.storage_region)?;
    let stored = (canonical != aero_storage::DEFAULT_STORAGE_REGION).then_some(canonical.as_str());
    if !s
        .workspaces
        .set_region_code_authorized(ws, stored, auth.participant_id)
        .await
        .map_err(AeroError::from)?
    {
        return Err(AeroError::NotFound("workspace".into()).into());
    }
    Ok(Json(storage_region_response(&s, stored)?))
}

#[cfg(test)]
mod region_tests {
    use std::{collections::HashMap, sync::Arc};

    use aero_storage::{BlobStore, LocalFsBlobStore, RegionRouter};

    use super::*;

    fn router() -> RegionRouter {
        let default: Arc<dyn BlobStore> = Arc::new(
            LocalFsBlobStore::new(
                std::env::temp_dir().join(format!("aero-region-route-{}", uuid::Uuid::new_v4())),
            )
            .unwrap(),
        );
        let regional: Arc<dyn BlobStore> = Arc::new(
            LocalFsBlobStore::new(
                std::env::temp_dir().join(format!("aero-region-route-{}", uuid::Uuid::new_v4())),
            )
            .unwrap(),
        );
        RegionRouter::from_stores(default, HashMap::from([("eu-west-1".to_owned(), regional)]))
            .unwrap()
    }

    #[test]
    fn management_region_validation_accepts_only_default_or_configured() {
        let router = router();
        assert_eq!(
            canonical_requested_region(&router, " default ").unwrap(),
            "default"
        );
        assert_eq!(
            canonical_requested_region(&router, "eu-west-1").unwrap(),
            "eu-west-1"
        );
        assert!(canonical_requested_region(&router, "").is_err());
        assert!(canonical_requested_region(&router, "eu-moon-1").is_err());
    }
}
