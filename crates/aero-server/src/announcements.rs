//! Workspace announcements / banners — admin-posted, workspace-wide notices.
//!
//! A workspace administrator posts a short banner (e.g. "Office closed Friday");
//! every member of the workspace reads the currently *active* ones. A banner is
//! active until its optional expiry passes (`expires_in_secs` ⇒ `now + secs`),
//! and an admin can delete one early. The active-window rule itself lives in the
//! storage layer's pure [`is_active`](aero_storage::is_active) (applied in SQL by
//! [`AnnouncementRepo::list_active_authorized`](aero_storage::AnnouncementRepo)).
//!
//! Handlers retain a cheap effective-role preflight for predictable errors, then
//! call transaction-owned repository methods that repeat authorization under
//! the workspace lock. Posting/deleting and their audit records therefore share
//! one commit; listing cannot race a membership revocation.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{AnnouncementId, Error as AeroError, ParticipantId, WorkspaceId, WorkspaceRole};
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

fn resolve_expiry(
    expires_in_secs: Option<u64>,
    now: OffsetDateTime,
) -> Result<Option<OffsetDateTime>, AeroError> {
    expires_in_secs
        .map(|secs| {
            let secs = i64::try_from(secs)
                .map_err(|_| AeroError::Invalid("expires_in_secs too large".into()))?;
            if secs == 0 {
                return Err(AeroError::Invalid(
                    "expires_in_secs must be greater than zero".into(),
                ));
            }
            now.checked_add(Duration::seconds(secs))
                .ok_or_else(|| AeroError::Invalid("expires_in_secs too large".into()))
        })
        .transpose()
}

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
        .effective_member_role(workspace, caller)
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
        Err(AeroError::Forbidden(
            "posting announcements requires admin".into(),
        ))
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

    let row = repo(&s)
        .create_authorized(
            ws,
            body,
            auth.participant_id,
            resolve_expiry(req.expires_in_secs, OffsetDateTime::now_utc())?,
        )
        .await
        .map_err(AeroError::from)?;
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
        .list_active_authorized(ws, auth.participant_id, OffsetDateTime::now_utc())
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
    repo(&s)
        .delete_authorized(id, ws, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[cfg(test)]
mod tests {
    use super::resolve_expiry;
    use aero_common::Error;
    use time::OffsetDateTime;

    #[test]
    fn expiry_must_be_representable_and_strictly_future() {
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            resolve_expiry(None, now).unwrap(),
            None,
            "missing TTL means no expiry"
        );
        assert!(resolve_expiry(Some(1), now).unwrap().is_some());
        assert!(matches!(
            resolve_expiry(Some(0), now),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            resolve_expiry(Some(u64::MAX), now),
            Err(Error::Invalid(_))
        ));
    }
}
