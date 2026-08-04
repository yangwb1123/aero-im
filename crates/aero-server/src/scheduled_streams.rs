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
//! creator-scoped and rechecks effective tenant access while holding the row
//! lock. Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, RoomId, ScheduledStreamId, WorkspaceId};
use aero_storage::{
    ScheduledStreamRepo, ScheduledStreamWriteError, MAX_SCHEDULED_STREAM_DESCRIPTION_CHARS,
    MAX_SCHEDULED_STREAM_TITLE_CHARS, MAX_UPCOMING_SCHEDULED_STREAMS_PAGE,
};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
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
        .route(
            "/api/workspaces/:id/streams/schedule.ics",
            get(workspace_schedule_ics),
        )
}

/// Build a [`ScheduledStreamRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> ScheduledStreamRepo {
    ScheduledStreamRepo::new(s.pg.clone())
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

/// Assert the caller currently passes every workspace access gate.
async fn assert_member(
    s: &AppState,
    workspace: WorkspaceId,
    caller: aero_common::ParticipantId,
) -> Result<(), AeroError> {
    s.workspaces
        .effective_member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))?;
    Ok(())
}

async fn assert_room_in_workspace(
    s: &AppState,
    workspace: WorkspaceId,
    room: RoomId,
    caller: aero_common::ParticipantId,
) -> Result<(), AeroError> {
    s.im.assert_room_access(caller, room).await?;
    let actual_workspace = s
        .rooms
        .room_workspace(room)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("room".into()))?;
    if actual_workspace != workspace {
        return Err(AeroError::Forbidden(
            "scheduled stream room is outside the requested workspace".into(),
        ));
    }
    Ok(())
}

fn map_write_error(error: ScheduledStreamWriteError) -> AeroError {
    match error {
        ScheduledStreamWriteError::Database(error) => AeroError::from(error),
        ScheduledStreamWriteError::CreatorNotMember => {
            AeroError::Forbidden("scheduled stream workspace is not accessible".into())
        }
        ScheduledStreamWriteError::RoomNotAccessible => {
            AeroError::Forbidden("scheduled stream room is not accessible".into())
        }
        ScheduledStreamWriteError::NotFound => AeroError::NotFound("scheduled stream".into()),
        ScheduledStreamWriteError::InvalidInput(message) => AeroError::Invalid(message),
        ScheduledStreamWriteError::LimitReached => {
            AeroError::Invalid("scheduled stream limit reached".into())
        }
    }
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
    if title.chars().count() > MAX_SCHEDULED_STREAM_TITLE_CHARS {
        return Err(AeroError::Invalid(format!(
            "title is too long (max {MAX_SCHEDULED_STREAM_TITLE_CHARS} chars)"
        ))
        .into());
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
    if let Some(room) = room {
        assert_room_in_workspace(&s, ws, room, auth.participant_id).await?;
    }
    let description = req
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(ToOwned::to_owned);
    if description
        .as_deref()
        .is_some_and(|value| value.chars().count() > MAX_SCHEDULED_STREAM_DESCRIPTION_CHARS)
    {
        return Err(AeroError::Invalid(format!(
            "description is too long (max {MAX_SCHEDULED_STREAM_DESCRIPTION_CHARS} chars)"
        ))
        .into());
    }

    let id = repo(&s)
        .create(
            ws,
            room,
            title,
            description,
            req.scheduled_for,
            auth.participant_id,
        )
        .await
        .map_err(map_write_error)?;
    // Re-read so the response carries the full, canonical row (status, created_at).
    let row = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("scheduled stream".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct ListScheduledStreamsQuery {
    #[serde(default = "default_scheduled_stream_limit")]
    limit: i64,
}

fn default_scheduled_stream_limit() -> i64 {
    100
}

/// `GET /api/workspaces/:id/scheduled-streams` — the workspace's upcoming
/// announcements (still-scheduled, not yet past), soonest first. Members only.
async fn list_scheduled_streams(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Query(query): Query<ListScheduledStreamsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_member(&s, ws, auth.participant_id).await?;
    let upcoming = repo(&s)
        .list_upcoming(ws, time::OffsetDateTime::now_utc(), query.limit)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(
        serde_json::to_value(upcoming).map_err(AeroError::from)?,
    ))
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
    let existing = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("scheduled stream {id}")))?;
    if existing.created_by != auth.participant_id {
        return Err(AeroError::NotFound(format!("scheduled stream {id}")).into());
    }
    assert_member(&s, existing.workspace_id, auth.participant_id).await?;
    if let Some(room) = existing.room_id {
        assert_room_in_workspace(&s, existing.workspace_id, room, auth.participant_id).await?;
    }

    repo(&s)
        .cancel(id, auth.participant_id)
        .await
        .map_err(map_write_error)?;
    Ok(Json(serde_json::json!({ "canceled": true })))
}

/// Format a [`time::OffsetDateTime`] as the iCalendar UTC timestamp form:
/// `YYYYMMDDTHHmmssZ`.
fn fmt_ics_dt(dt: time::OffsetDateTime) -> String {
    let dt = dt.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
    )
}

/// Escape special characters in iCalendar TEXT values (SUMMARY, DESCRIPTION).
fn ics_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace('\n', "\\n")
}

/// `GET /api/workspaces/:id/streams/schedule.ics` — the workspace's upcoming
/// scheduled streams serialised as RFC 5545 iCalendar. Members only.
async fn workspace_schedule_ics(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
) -> Result<Response, crate::error::ApiError> {
    let ws = parse_workspace(&ws_str)?;
    assert_member(&s, ws, auth.participant_id).await?;
    let streams = repo(&s)
        .list_upcoming(
            ws,
            time::OffsetDateTime::now_utc(),
            MAX_UPCOMING_SCHEDULED_STREAMS_PAGE,
        )
        .await
        .map_err(AeroError::from)?;

    let mut ics = String::from("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Aero//Aero IM//EN\r\n");
    for stream in &streams {
        let dtstart = fmt_ics_dt(stream.scheduled_for);
        // Default duration: 1 hour.
        let dtend = fmt_ics_dt(stream.scheduled_for + time::Duration::hours(1));
        ics.push_str("BEGIN:VEVENT\r\n");
        ics.push_str(&format!("UID:{}@aero\r\n", stream.id));
        ics.push_str(&format!("SUMMARY:{}\r\n", ics_escape(&stream.title)));
        ics.push_str(&format!("DTSTART:{dtstart}\r\n"));
        ics.push_str(&format!("DTEND:{dtend}\r\n"));
        if let Some(desc) = &stream.description {
            if !desc.is_empty() {
                ics.push_str(&format!("DESCRIPTION:{}\r\n", ics_escape(desc)));
            }
        }
        ics.push_str("END:VEVENT\r\n");
    }
    ics.push_str("END:VCALENDAR\r\n");

    let response = (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/calendar; charset=utf-8"),
            ),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static("attachment; filename=\"schedule.ics\""),
            ),
        ],
        ics,
    )
        .into_response();
    Ok(response)
}
