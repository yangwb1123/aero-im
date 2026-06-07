//! Channel join requests — request-to-join with owner/admin approval.
//!
//! A workspace member asks to join a channel they are not yet in; the channel's
//! creator (its `created_by`) or a workspace admin of the room's workspace then
//! approves or denies the request. Request lifecycle (create / list / decide) is
//! owned by [`JoinRequestRepo`](aero_storage::JoinRequestRepo); *granting*
//! membership on approval reuses the EXISTING
//! [`RoomRepo::add_member`](aero_storage::RoomRepo) path, so no membership SQL is
//! duplicated here.
//!
//! The approval gate is a single shared check: the caller passes iff they are the
//! room's creator OR an Admin/Owner of the room's workspace (resolved from
//! `room_workspace` + `member_role`); everyone else gets `403`. Creating a
//! request is open to any authenticated caller, but a caller who is already a
//! member is rejected `409` (there is nothing to request). Mounted via [`routes`]
//! and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, JoinRequestId, ParticipantId, RoomId};
use aero_storage::JoinRequestRepo;
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
        .route("/api/rooms/:id/join-requests", axum::routing::get(list_requests))
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
    if creator == caller {
        return Ok(());
    }
    // Not the creator — fall back to a workspace-admin check on the room's tenant.
    let workspace = s
        .rooms
        .room_workspace(room)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("room".into()))?;
    let is_admin = s
        .workspaces
        .member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .is_some_and(aero_common::WorkspaceRole::can_administer);
    if is_admin {
        Ok(())
    } else {
        Err(AeroError::Forbidden("not the channel owner or a workspace admin".into()))
    }
}

/// `POST /api/rooms/:id/join-request` — ask to join the channel. Open to any
/// authenticated caller, but a caller who is already a member is rejected `409`
/// (there is nothing to request). Idempotent for an outstanding ask: a repeated
/// request while one is still pending resolves to the same row. Returns the
/// created (or existing pending) request.
async fn request_join(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    // Already a member ⇒ nothing to request.
    if s.rooms
        .is_member(room, auth.participant_id)
        .await
        .map_err(AeroError::from)?
    {
        return Err(AeroError::Conflict("already a member of this channel".into()).into());
    }

    let id = repo(&s)
        .create(room, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
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

/// `POST /api/join-requests/:rid/approve` — approve a pending request: enroll the
/// requester as a member (via the existing `add_member` path) THEN flip the
/// request to `approved`. The caller must be the request's room's creator or a
/// workspace admin (`403` otherwise); an unknown request id is `404`.
async fn approve_request(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_request(&id_str)?;
    let request = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("join request {id}")))?;
    assert_can_decide(&s, request.room_id, auth.participant_id).await?;

    // Grant membership first (idempotent `ON CONFLICT DO NOTHING`), then record
    // the decision — so a successful flip always implies the requester is in.
    s.rooms
        .add_member(request.room_id, request.requester_id)
        .await
        .map_err(AeroError::from)?;
    repo(&s)
        .decide(id, "approved", auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "approved": true })))
}

/// `POST /api/join-requests/:rid/deny` — deny a pending request: flip it to
/// `denied` without granting membership. The caller must be the request's room's
/// creator or a workspace admin (`403` otherwise); an unknown request id is `404`.
async fn deny_request(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_request(&id_str)?;
    let request = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("join request {id}")))?;
    assert_can_decide(&s, request.room_id, auth.participant_id).await?;

    repo(&s)
        .decide(id, "denied", auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "denied": true })))
}
