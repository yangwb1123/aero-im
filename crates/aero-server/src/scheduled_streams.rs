//! Scheduled streams — live-event announcements.
//!
//! A workspace member announces an upcoming live stream ahead of time (title,
//! optional description, optional associated room, scheduled-for time); members
//! list the upcoming announcements and the creator can cancel one. This is purely
//! the announcement/lifecycle record — actually going live still uses the
//! existing `/api/streams` ingest path, so nothing here touches media transport.
//!
//! Thin handlers over [`ScheduledStreamRepo`](aero_storage::ScheduledStreamRepo):
//! the create/list routes assert the caller is a member of the workspace (via the
//! shared [`WorkspaceRepo`], mirroring [`crate::workspaces`]); cancel is
//! creator-scoped at the SQL layer. Mounted via [`routes`] and `.merge`d into the
//! main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, RoomId, ScheduledStreamId, WorkspaceId};
use aero_storage::ScheduledStreamRepo;
use axum::{
    extract::{Path, State},
    routing::{delete, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All scheduled-stream routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/workspaces/:id/scheduled-streams",
            post(create_scheduled_stream).get(list_scheduled_streams),
        )
        .route(
            "/api/scheduled-streams/:id",
            delete(cancel_scheduled_stream),
        )
}

/// Build a [`ScheduledStreamRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> ScheduledStreamRepo {
    ScheduledStreamRepo::new(s.pg.clone())
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

/// Assert the caller is a member of the workspace, rejecting non-members with a
/// `403`. Mirrors `crate::workspaces::caller_role`'s membership gate.
async fn assert_member(
    s: &AppState,
    workspace: WorkspaceId,
    caller: aero_common::ParticipantId,
) -> Result<(), AeroError> {
    s.workspaces
        .member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))?;
    Ok(())
}

#[derive(Deserialize)]
struct CreateScheduledStreamReq {
    /// Human-readable title of the upcoming stream.
    title: String,
    /// Optional longer description / agenda.
    #[serde(default)]
    description: Option<String>,
    /// Optional room id the upcoming stream is associated with.
    #[serde(default)]
    room_id: Option<String>,
    /// When the stream is expected to go live, RFC 3339 (e.g.
    /// `2026-06-10T18:30:00Z`). A malformed value is a 400 at body-deserialization
    /// time.
    #[serde(with = "time::serde::rfc3339")]
    scheduled_for: time::OffsetDateTime,
}

/// `POST /api/workspaces/:id/scheduled-streams` — announce an upcoming live
/// stream. The caller must be a member of the workspace; a blank title or a past
/// `scheduled_for` is rejected `400`. Returns the created row.
async fn create_scheduled_stream(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Json(req): Json<CreateScheduledStreamReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_member(&s, ws, auth.participant_id).await?;

    let title = req.title.trim();
    if title.is_empty() {
        return Err(AeroError::Invalid("title must not be empty".into()).into());
    }
    if title.len() > 256 {
        return Err(AeroError::Invalid("title too long".into()).into());
    }
    if req.scheduled_for <= time::OffsetDateTime::now_utc() {
        return Err(AeroError::Invalid("scheduled_for must be in the future".into()).into());
    }
    let room = match req.room_id.as_deref() {
        Some(r) => Some(
            RoomId::from_str(r.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))?,
        ),
        None => None,
    };
    let description = req
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(ToOwned::to_owned);

    let id = repo(&s)
        .create(ws, room, title, description, req.scheduled_for, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    // Re-read so the response carries the full, canonical row (status, created_at).
    let row = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("scheduled stream".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `GET /api/workspaces/:id/scheduled-streams` — the workspace's upcoming
/// announcements (still-scheduled, not yet past), soonest first. Members only.
async fn list_scheduled_streams(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_member(&s, ws, auth.participant_id).await?;
    let upcoming = repo(&s)
        .list_upcoming(ws, time::OffsetDateTime::now_utc())
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(upcoming).map_err(AeroError::from)?))
}

/// `DELETE /api/scheduled-streams/:id` — cancel one of the caller's own
/// still-scheduled announcements. Creator-scoped: a `404` if it isn't the
/// caller's still-scheduled row (already live/ended/canceled, someone else's, or
/// unknown).
async fn cancel_scheduled_stream(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = ScheduledStreamId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("scheduled stream id: {e}")))?;
    let canceled = repo(&s)
        .cancel(id, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    if !canceled {
        return Err(AeroError::NotFound(format!("scheduled stream {id}")).into());
    }
    Ok(Json(serde_json::json!({ "canceled": true })))
}
