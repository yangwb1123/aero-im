//! Channel join requests — request-to-join with owner/admin approval.
//!
//! A workspace member asks to join a channel they are not yet in; the channel's
//! creator (its `created_by`) or a workspace admin of the room's workspace then
//! approves or denies the request. Request lifecycle and approval membership
//! grant are owned by [`JoinRequestRepo`](aero_storage::JoinRequestRepo) in one
//! authorized transaction.
//!
//! The approval gate is a single shared check: the caller passes iff they are the
//! room's creator OR an Admin/Owner of the room's workspace (resolved from
//! `room_workspace` + effective membership); everyone else gets `403`. Creating
//! requires effective workspace membership and rejects an existing room member.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, JoinRequestId, ParticipantId, RoomId};
use aero_storage::{JoinRequestRepo, JoinRequestWriteError};
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// All channel join-request routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/rooms/:id/join-request", post(request_join))
        .route(
            "/api/rooms/:id/join-requests",
            axum::routing::get(list_requests),
        )
        .route("/api/join-requests/:rid/approve", post(approve_request))
        .route("/api/join-requests/:rid/deny", post(deny_request))
}

/// Build a [`JoinRequestRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> JoinRequestRepo {
    JoinRequestRepo::new(s.pg.clone())
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

fn parse_request(s: &str) -> Result<JoinRequestId, AeroError> {
    JoinRequestId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("join request id: {e}")))
}

fn map_write_error(error: JoinRequestWriteError) -> AeroError {
    match error {
        JoinRequestWriteError::Database(error) => AeroError::from(error),
        JoinRequestWriteError::NotFound => AeroError::NotFound("join request or channel".into()),
        JoinRequestWriteError::RequesterNotMember => {
            AeroError::Forbidden("requester is not an active workspace member".into())
        }
        JoinRequestWriteError::AlreadyMember => {
            AeroError::Conflict("already a member of this channel".into())
        }
        JoinRequestWriteError::Forbidden => {
            AeroError::Forbidden("not allowed to decide this join request".into())
        }
        JoinRequestWriteError::InvalidStatus => {
            AeroError::Invalid("invalid join-request decision".into())
        }
    }
}

/// Assert the caller may approve/deny join requests for `room`: they must be the
/// room's creator (`rooms.created_by`) OR an Admin/Owner of the room's workspace.
/// A missing room is `404`; an authenticated non-owner / non-admin is `403`.
async fn assert_can_decide(
    s: &AppState,
    room: RoomId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    let creator = s
        .rooms
        .created_by(room)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("room".into()))?;
    let workspace = s
        .rooms
        .room_workspace(room)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("room".into()))?;
    let role = s
        .workspaces
        .effective_member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?;
    if role.is_some_and(|role| creator == caller || role.can_administer()) {
        Ok(())
    } else {
        Err(AeroError::Forbidden(
            "not the channel owner or a workspace admin".into(),
        ))
    }
}

/// Ask to join the channel. The caller must be an effective member of its
/// workspace but not yet a room member. Storage checks both transactionally.
async fn request_join(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    let id = repo(&s)
        .create_authorized(room, auth.participant_id)
        .await
        .map_err(map_write_error)?;
    // Re-read so the response carries the full, canonical row (status/created_at).
    let row = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("join request".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `GET /api/rooms/:id/join-requests` — the channel's pending join requests (the
/// approval queue), newest first. The caller must be the room's creator or a
/// workspace admin (`403` otherwise).
async fn list_requests(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    assert_can_decide(&s, room, auth.participant_id).await?;
    let requests = repo(&s)
        .list_for_room(room, Some("pending"))
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "requests": requests })))
}

/// Approve a pending request and enroll its requester atomically.
async fn approve_request(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_request(&id_str)?;
    let request = repo(&s)
        .decide_authorized(id, "approved", auth.participant_id)
        .await
        .map_err(map_write_error)?;
    Ok(Json(
        serde_json::json!({ "approved": true, "request": request }),
    ))
}

/// Deny a pending request without granting membership.
async fn deny_request(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_request(&id_str)?;
    let request = repo(&s)
        .decide_authorized(id, "denied", auth.participant_id)
        .await
        .map_err(map_write_error)?;
    Ok(Json(
        serde_json::json!({ "denied": true, "request": request }),
    ))
}
