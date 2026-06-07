//! Workspace announcements / banners — admin-posted, workspace-wide notices.
//!
//! A workspace administrator posts a short banner (e.g. "Office closed Friday");
//! every member of the workspace reads the currently *active* ones. A banner is
//! active until its optional expiry passes (`expires_in_secs` ⇒ `now + secs`),
//! and an admin can delete one early. The active-window rule itself lives in the
//! storage layer's pure [`is_active`](aero_storage::is_active) (applied in SQL by
//! [`AnnouncementRepo::list_active`](aero_storage::AnnouncementRepo)).
//!
//! Thin handlers over [`AnnouncementRepo`](aero_storage::AnnouncementRepo):
//! posting and deleting require the caller to be a workspace administrator
//! (admin/owner, via the shared [`WorkspaceRepo`](aero_storage::WorkspaceRepo),
//! mirroring [`crate::guests`]); listing requires only workspace membership.
//! Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{
    AnnouncementId, Error as AeroError, ParticipantId, WorkspaceId, WorkspaceRole,
};
use aero_storage::AnnouncementRepo;
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use time::{Duration, OffsetDateTime};

use crate::error::ApiResult;
use crate::state::AppState;

/// Maximum announcement body length (characters), matching the validation bound
/// in the spec (`1..=2000`).
const MAX_BODY_LEN: usize = 2000;

/// All workspace-announcement routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/workspaces/:id/announcements",
            post(create_announcement).get(list_announcements),
        )
        .route(
            "/api/workspaces/:id/announcements/:aid",
            axum::routing::delete(delete_announcement),
        )
}

/// Build an [`AnnouncementRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> AnnouncementRepo {
    AnnouncementRepo::new(s.pg.clone())
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn parse_announcement(s: &str) -> Result<AnnouncementId, AeroError> {
    AnnouncementId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("announcement id: {e}")))
}

/// Resolve the caller's role in a workspace, rejecting non-members with a `403`.
/// Mirrors `crate::guests::caller_role`.
async fn caller_role(
    s: &AppState,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> Result<WorkspaceRole, AeroError> {
    s.workspaces
        .member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))
}

/// Assert the caller is a workspace administrator (admin/owner). Non-members are
/// rejected `403` (not a member); members/guests are rejected `403` (not an
/// admin). Mirrors `crate::guests::authorize_manage_guests`.
async fn assert_admin(
    s: &AppState,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    let role = caller_role(s, workspace, caller).await?;
    if role.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden("posting announcements requires admin".into()))
    }
}

#[derive(Deserialize)]
struct CreateAnnouncementReq {
    /// The banner text shown to members.
    body: String,
    /// Optional relative auto-expiry, in seconds from now. Absent ⇒ the banner
    /// never expires.
    #[serde(default)]
    expires_in_secs: Option<u64>,
}

/// `POST /api/workspaces/:id/announcements` — post a workspace-wide banner. The
/// caller must be a workspace administrator (admin/owner); a blank or
/// over-length body is rejected `400`. `expires_in_secs`, when present, sets the
/// banner's expiry to `now + secs`. Returns the created row.
async fn create_announcement(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Json(req): Json<CreateAnnouncementReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_admin(&s, ws, auth.participant_id).await?;

    let body = req.body.trim();
    if body.is_empty() {
        return Err(AeroError::Invalid("body must not be empty".into()).into());
    }
    if body.chars().count() > MAX_BODY_LEN {
        return Err(AeroError::Invalid("body too long".into()).into());
    }

    let now = OffsetDateTime::now_utc();
    let expires_at = req
        .expires_in_secs
        .map(|secs| now + Duration::seconds(i64::try_from(secs).unwrap_or(i64::MAX)));

    let id = repo(&s)
        .create(ws, body, auth.participant_id, expires_at)
        .await
        .map_err(AeroError::from)?;
    // Re-read by id (unfiltered by expiry) so the response carries the full,
    // canonical row even for an already-expired banner (e.g. a 0-second TTL).
    let row = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("announcement".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `GET /api/workspaces/:id/announcements` — the active announcements in this
/// workspace, newest first. Workspace members only; expired banners are filtered
/// out at the SQL layer.
async fn list_announcements(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    caller_role(&s, ws, auth.participant_id).await?;
    let active = repo(&s)
        .list_active(ws, OffsetDateTime::now_utc())
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(active).map_err(AeroError::from)?))
}

/// `DELETE /api/workspaces/:id/announcements/:aid` — remove a banner early. The
/// caller must be a workspace administrator (admin/owner). Workspace-scoped: a
/// `404` if the banner isn't in this workspace (another tenant's or unknown).
async fn delete_announcement(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((ws_str, aid_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    let id = parse_announcement(&aid_str)?;
    assert_admin(&s, ws, auth.participant_id).await?;
    let removed = repo(&s).delete(id, ws).await.map_err(AeroError::from)?;
    if !removed {
        return Err(AeroError::NotFound(format!("announcement {id}")).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}
