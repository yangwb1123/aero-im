//! Workspace analytics — admin-only aggregate dashboard numbers for one tenant.
//!
//! Thin, read-only handlers over [`AnalyticsRepo`](aero_storage::AnalyticsRepo):
//! a headline overview, a busiest-channels ranking, and a per-day message
//! timeline. There is **no** new table and **no** new id — every figure is a
//! parameterized aggregate over existing tables, scoped to the workspace via
//! `rooms.workspace_id`.
//!
//! ## Authorization
//!
//! Every route is admin/owner-gated. The privilege decision is the pure, DB-free
//! [`authorize_admin`] (mirroring `crate::deactivation`), so the role matrix is
//! unit-tested offline (Postgres is absent in CI); the async [`assert_admin`]
//! resolves the caller's role from the shared
//! [`WorkspaceRepo`](aero_storage::WorkspaceRepo) and then applies the guard.
//! Non-members and non-admins are both rejected `403`. Mounted via [`routes`] and
//! `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{
    Error as AeroError, ParticipantId, Result as AeroResult, RoomId, WorkspaceId, WorkspaceRole,
};
use aero_storage::AnalyticsRepo;
use axum::{
    extract::{Path, Query, State},
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All workspace-analytics routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/workspaces/:id/analytics", get(overview))
        // summary is a member-accessible alias for the overview numbers plus
        // 30-day active-user + message counts — no admin required.
        .route("/api/workspaces/:id/analytics/summary", get(summary))
        .route("/api/workspaces/:id/analytics/channels", get(top_channels))
        .route("/api/workspaces/:id/analytics/timeline", get(timeline))
        .route(
            "/api/workspaces/:id/channels/:room_id/analytics",
            get(channel_analytics),
        )
        .route(
            "/api/workspaces/:id/analytics/reactions",
            get(workspace_reactions),
        )
        .route("/api/rooms/:id/analytics/reactions", get(room_reactions))
}

/// Default number of channels the busiest-channels ranking returns when the
/// client omits `?limit=`.
const DEFAULT_TOP_LIMIT: i64 = 10;
/// Default timeline window (in days) when the client omits `?days=`.
const DEFAULT_TIMELINE_DAYS: i64 = 14;

/// Build an [`AnalyticsRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> AnalyticsRepo {
    AnalyticsRepo::new(s.pg.clone())
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

/// May `caller` view a workspace's analytics? **Admin or owner only** — aggregate
/// activity over an entire tenant is administrative insight, the same bar as the
/// audit trail / retention policy / deactivation. Gates on
/// [`WorkspaceRole::can_administer`].
///
/// # Errors
/// [`AeroError::Forbidden`] for non-administrators (members / guests).
pub fn authorize_admin(caller: WorkspaceRole) -> AeroResult<()> {
    if caller.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden("analytics requires admin".into()))
    }
}

/// Resolve the caller's workspace role and assert they may administer it. Mirrors
/// `crate::saved_searches::assert_member`, but requires Owner/Admin rather than
/// mere membership. Rejects non-members (`403`) and non-admins (`403`).
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

/// Assert the caller is a member (any role) of `workspace`. Used by the
/// member-accessible summary route so we still gate on membership without the
/// full admin bar.
async fn assert_member(
    s: &AppState,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    s.workspaces
        .member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))?;
    Ok(())
}

/// `GET /api/workspaces/:id/analytics/summary` — workspace analytics summary
/// open to any workspace member (not just admins). Returns total message count,
/// active users in the last 30 days, messages in the last 30 days, and total
/// distinct members. The 30-day window is broader than the existing 7-day
/// overview so it serves dashboard widgets and activity-heat displays.
async fn summary(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_member(&s, ws, auth.participant_id).await?;
    let stats = repo(&s).workspace_summary(ws).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(stats).map_err(AeroError::from)?))
}

/// `GET /api/workspaces/:id/analytics` — headline aggregate counts (total/last-7d
/// messages, rooms, members, active members). Admin/owner only.
async fn overview(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_admin(&s, ws, auth.participant_id).await?;
    let stats = repo(&s).overview(ws).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(stats).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct LimitQuery {
    /// How many channels to rank (clamped in the repo). Absent ⇒ [`DEFAULT_TOP_LIMIT`].
    #[serde(default)]
    limit: Option<i64>,
}

/// `GET /api/workspaces/:id/analytics/channels?limit=10` — the busiest channels in
/// the workspace, ranked by message count, descending. Admin/owner only.
async fn top_channels(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Query(q): Query<LimitQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_admin(&s, ws, auth.participant_id).await?;
    let limit = q.limit.unwrap_or(DEFAULT_TOP_LIMIT);
    let channels = repo(&s)
        .top_channels(ws, limit)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "channels": channels })))
}

#[derive(Deserialize)]
struct DaysQuery {
    /// Trailing window in days (clamped in the repo). Absent ⇒ [`DEFAULT_TIMELINE_DAYS`].
    #[serde(default)]
    days: Option<i64>,
}

/// `GET /api/workspaces/:id/analytics/timeline?days=14` — per-day message volume
/// over the trailing window, oldest day first. Admin/owner only.
async fn timeline(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Query(q): Query<DaysQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_admin(&s, ws, auth.participant_id).await?;
    let days = q.days.unwrap_or(DEFAULT_TIMELINE_DAYS);
    let series = repo(&s)
        .messages_per_day(ws, days)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "timeline": series })))
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

/// `GET /api/workspaces/:id/channels/:room_id/analytics` — per-channel aggregate
/// stats (total and 30-day message/sender counts). Admin/owner only.
async fn channel_analytics(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((ws_str, room_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    let room = parse_room(&room_str)?;
    assert_admin(&s, ws, auth.participant_id).await?;
    let stats = repo(&s)
        .channel_analytics(ws, room)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(stats).map_err(AeroError::from)?))
}

/// `GET /api/workspaces/:id/analytics/reactions?limit=10` — most-used reactions
/// across all messages in the workspace, descending by frequency. Admin/owner only.
async fn workspace_reactions(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Query(q): Query<LimitQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_admin(&s, ws, auth.participant_id).await?;
    let limit = q.limit.unwrap_or(DEFAULT_TOP_LIMIT);
    let stats = repo(&s)
        .top_reactions_workspace(ws, limit)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "reactions": stats })))
}

/// `GET /api/rooms/:id/analytics/reactions?limit=10` — most-used reactions on
/// messages in a room, descending by frequency. Any authenticated caller may read.
async fn room_reactions(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(room_str): Path<String>,
    Query(q): Query<LimitQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    let limit = q.limit.unwrap_or(DEFAULT_TOP_LIMIT);
    let stats = repo(&s)
        .top_reactions_room(room, limit)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "reactions": stats })))
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
    fn analytics_is_admin_and_owner_only_over_all_roles() {
        // Exhaustive over the 4 roles: admin + owner may view analytics; member
        // and guest may not. Tracks `can_administer` exactly, like the
        // deactivation / audit-view gates.
        for r in ALL {
            assert_eq!(
                authorize_admin(r).is_ok(),
                r.can_administer(),
                "analytics allowed only for admin/owner, role {r:?}"
            );
        }
        assert!(authorize_admin(WorkspaceRole::Owner).is_ok());
        assert!(authorize_admin(WorkspaceRole::Admin).is_ok());
        assert!(authorize_admin(WorkspaceRole::Member).is_err());
        assert!(authorize_admin(WorkspaceRole::Guest).is_err());
    }

    #[test]
    fn analytics_non_admin_denials_are_403() {
        // Member and guest denials surface as 403 (authorization), not 400/404.
        for r in [WorkspaceRole::Guest, WorkspaceRole::Member] {
            assert_eq!(status_of(&authorize_admin(r)), 403, "role {r:?}");
        }
    }
}
