//! Tasks / to-do tracker (Lark Tasks / Teams Tasks-lite).
//!
//! Lightweight, durable task tracking attached to a room: a member creates a task
//! (optionally from a message), assigns it, sets a due date, and marks it done.
//! This is deliberately distinct from the AI action-item EXTRACTION endpoint
//! ([`crate::action_items`], which only summarizes a channel) — these are durable,
//! assignable, stateful rows.
//!
//! Thin handlers over [`TaskRepo`](aero_storage::TaskRepo). Every route is gated
//! on the SAME tenant + membership guard the rest of the app uses
//! ([`assert_room_access`](aero_im_core::ImService::assert_room_access)): for the
//! room-scoped routes the room id is in the path; for the task-scoped routes we
//! first `get` the task to learn its `room_id` (`404` if missing), then assert
//! access against that room. The `/api/me/tasks` route is caller-scoped (the
//! assignee is the authenticated participant). Mounted via [`routes`] and `.merge`d
//! into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, MessageId, ParticipantId, RoomId, TaskId};
use aero_storage::{validate_status, Task, TaskRepo};
use axum::{
    extract::{Path, Query, State},
    routing::{get, patch},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// Max length of a task title, validated at the edge (`400` when exceeded).
const MAX_TITLE_LEN: usize = 512;

/// All task routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/rooms/:id/tasks", get(list_room_tasks).post(create_task))
        .route("/api/me/tasks", get(list_my_tasks))
        .route("/api/tasks/:tid", patch(update_task).delete(delete_task))
}

/// Build a [`TaskRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> TaskRepo {
    TaskRepo::new(s.pg.clone())
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

fn parse_task(s: &str) -> Result<TaskId, AeroError> {
    TaskId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("task id: {e}")))
}

/// Validate a (trimmed) title: non-empty and within [`MAX_TITLE_LEN`].
fn clean_title(raw: &str) -> Result<&str, AeroError> {
    let title = raw.trim();
    if title.is_empty() {
        return Err(AeroError::Invalid("title must not be empty".into()));
    }
    if title.len() > MAX_TITLE_LEN {
        return Err(AeroError::Invalid("title too long".into()));
    }
    Ok(title)
}

/// Load a task by id (`404` if missing), then assert the caller may access the
/// room it lives in (the shared tenant + membership guard). Returns the task so
/// the handler can reuse its fields.
async fn load_with_access(
    s: &AppState,
    id: TaskId,
    caller: ParticipantId,
) -> Result<Task, AeroError> {
    let task = repo(s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("task {id}")))?;
    s.im.assert_room_access(caller, task.room_id).await?;
    Ok(task)
}

/// Re-read a task after a mutation so the response carries the full canonical row.
async fn reread(s: &AppState, id: TaskId) -> Result<Json<serde_json::Value>, AeroError> {
    let row = repo(s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("task {id}")))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct CreateTaskReq {
    /// Human-readable summary of the work to do.
    title: String,
    /// Optional participant to assign the task to.
    #[serde(default)]
    assignee_id: Option<String>,
    /// Optional message the task is created from.
    #[serde(default)]
    source_message_id: Option<String>,
    /// Optional due date, RFC 3339 (e.g. `2026-06-10T18:30:00Z`).
    #[serde(default, with = "time::serde::rfc3339::option")]
    due_at: Option<time::OffsetDateTime>,
}

/// `POST /api/rooms/:id/tasks` — create a task in the room. Room-access gated; the
/// creator is the caller. A blank/over-long title is `400`, and a malformed
/// `assignee_id` / `source_message_id` / `due_at` is `400`. Returns the created task.
async fn create_task(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<CreateTaskReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;

    let title = clean_title(&req.title)?;
    let assignee = match req.assignee_id.as_deref() {
        Some(a) => Some(
            ParticipantId::from_str(a.trim())
                .map_err(|e| AeroError::Invalid(format!("assignee id: {e}")))?,
        ),
        None => None,
    };
    let source_message = match req.source_message_id.as_deref() {
        Some(m) => Some(
            MessageId::from_str(m.trim())
                .map_err(|e| AeroError::Invalid(format!("source message id: {e}")))?,
        ),
        None => None,
    };

    let id = repo(&s)
        .create(room, auth.participant_id, title, assignee, source_message, req.due_at)
        .await
        .map_err(AeroError::from)?;

    // Best-effort mobile push to a freshly-assigned assignee (ROADMAP 方向二).
    // Skip when the assignee is the actor (no self-notify) or when no push gateway
    // is configured. Out-of-band + best-effort, so a failed send never fails the
    // create. No DND/snooze gate (a task assignment is worth surfacing regardless).
    if let Some(assignee) = assignee {
        if assignee != auth.participant_id && s.push.any_enabled() {
            let payload = aero_push::PushPayload {
                title: format!("New task assigned: {title}"),
                body: String::new(),
                room_id: Some(room.to_string()),
                message_id: None,
                badge: None,
                collapse_key: None,
            };
            crate::push_bot::push_to_participant(&s, assignee, &payload).await;
        }
    }

    Ok(reread(&s, id).await?)
}

#[derive(Deserialize)]
struct ListTasksQuery {
    /// Optional status filter (`open` | `in_progress` | `done`); absent ⇒ all.
    #[serde(default)]
    status: Option<String>,
}

/// `GET /api/rooms/:id/tasks?status=` — the room's tasks, newest first. Room-access
/// gated. An unrecognized `status` filter is rejected `400`.
async fn list_room_tasks(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Query(q): Query<ListTasksQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let status = q.status.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(st) = status {
        if !validate_status(st) {
            return Err(AeroError::Invalid(format!("unknown status '{st}'")).into());
        }
    }
    let tasks = repo(&s)
        .list_for_room(room, status)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(tasks).map_err(AeroError::from)?))
}

/// `GET /api/me/tasks` — the tasks assigned to the caller across all rooms,
/// unfinished first. Caller-scoped (the assignee is the authenticated participant),
/// so no per-room access check is needed.
async fn list_my_tasks(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let tasks = repo(&s)
        .list_for_assignee(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(tasks).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct UpdateTaskReq {
    /// New title; absent ⇒ keep the current one.
    #[serde(default)]
    title: Option<String>,
    /// New assignee; absent ⇒ keep the current one.
    #[serde(default)]
    assignee_id: Option<String>,
    /// New due date, RFC 3339; absent ⇒ keep the current one.
    #[serde(default, with = "time::serde::rfc3339::option")]
    due_at: Option<time::OffsetDateTime>,
    /// New status (`open` | `in_progress` | `done`); absent ⇒ keep the current one.
    #[serde(default)]
    status: Option<String>,
}

/// `PATCH /api/tasks/:tid` — edit a task (any member with room access may edit).
/// Resolves the task's room and asserts access first (`404` if unknown, `403` if
/// not a member). `title` / `assignee_id` / `due_at` / `status` are each optional;
/// an omitted field keeps its current value. A blank/over-long title, malformed
/// `assignee_id` / `due_at`, or an unrecognized `status` is `400`. Returns the
/// updated task.
async fn update_task(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<UpdateTaskReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_task(&id_str)?;
    // Establishes existence (`404`) and access (`403`) before mutating.
    load_with_access(&s, id, auth.participant_id).await?;

    let title = match req.title.as_deref() {
        Some(raw) => Some(clean_title(raw)?),
        None => None,
    };
    let assignee = match req.assignee_id.as_deref() {
        Some(a) => Some(
            ParticipantId::from_str(a.trim())
                .map_err(|e| AeroError::Invalid(format!("assignee id: {e}")))?,
        ),
        None => None,
    };

    // Apply the field edits (title/assignee/due_at) when any are present.
    if title.is_some() || assignee.is_some() || req.due_at.is_some() {
        repo(&s)
            .update(id, title, assignee, req.due_at)
            .await
            .map_err(AeroError::from)?;
    }

    // Apply the status transition when present (separately, so updated_at is set
    // even for a status-only PATCH).
    if let Some(st) = req.status.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if !validate_status(st) {
            return Err(AeroError::Invalid(format!("unknown status '{st}'")).into());
        }
        repo(&s).set_status(id, st).await.map_err(AeroError::from)?;
    }

    Ok(reread(&s, id).await?)
}

/// `DELETE /api/tasks/:tid` — delete a task (any member with room access may
/// delete): resolves the task's room and asserts access, then removes it. `404`
/// if unknown, `403` if not a member.
async fn delete_task(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_task(&id_str)?;
    // Establishes existence (`404`) and access (`403`) before deleting.
    load_with_access(&s, id, auth.participant_id).await?;
    let removed = repo(&s).delete(id).await.map_err(AeroError::from)?;
    if !removed {
        return Err(AeroError::NotFound(format!("task {id}")).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}
