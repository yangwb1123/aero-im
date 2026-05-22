//! HTTP route definitions. WS upgrade lives in `ws::handler`.

use std::str::FromStr;

use aero_auth::{AuthUser, LoginRequest, RegisterRequest};
use aero_common::{Error as AeroError, ParticipantId, Result as AeroResult, RoomId, RoomKind};
use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;
use crate::ws;

pub fn build(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/auth/register", post(auth_register))
        .route("/api/auth/login", post(auth_login))
        .route("/api/me", get(me))
        .route("/api/rooms", post(create_room).get(list_rooms))
        .route("/api/rooms/:id/members", post(add_member))
        .route("/api/rooms/:id/messages", get(room_history))
        .route("/ws", get(ws::handler))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

// ----- Auth -----

async fn auth_register(
    State(s): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let out = s.auth.register(req).await?;
    Ok(Json(serde_json::json!({
        "access_token": out.access_token,
        "refresh_token": out.refresh_token,
        "participant": out.participant,
    })))
}

async fn auth_login(
    State(s): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let out = s.auth.login(req).await?;
    Ok(Json(serde_json::json!({
        "access_token": out.access_token,
        "refresh_token": out.refresh_token,
        "participant": out.participant,
    })))
}

async fn me(State(s): State<AppState>, auth: AuthUser) -> ApiResult<Json<serde_json::Value>> {
    let p = s
        .participants
        .get(auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("participant".into()))?;
    Ok(Json(serde_json::to_value(p).map_err(AeroError::from)?))
}

// ----- Rooms -----

#[derive(Deserialize)]
struct CreateRoomReq {
    kind: String,
    name: Option<String>,
}

async fn create_room(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateRoomReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let kind = parse_room_kind(&req.kind)?;
    let room = s.im.create_room(auth.participant_id, kind, req.name).await?;
    Ok(Json(serde_json::to_value(room).map_err(AeroError::from)?))
}

async fn list_rooms(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let rooms = s.im.list_my_rooms(auth.participant_id).await?;
    Ok(Json(serde_json::to_value(rooms).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct AddMemberReq {
    participant_id: String,
}

async fn add_member(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<AddMemberReq>,
) -> ApiResult<axum::http::StatusCode> {
    let room = RoomId::from_str(&room_str)
        .map_err(|e| AeroError::Invalid(format!("room id: {e}")))?;
    let member = ParticipantId::from_str(&req.participant_id)
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))?;
    s.im.add_member(auth.participant_id, room, member).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct HistoryQuery {
    before: Option<String>,
    limit: Option<i64>,
}

async fn room_history(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = RoomId::from_str(&room_str)
        .map_err(|e| AeroError::Invalid(format!("room id: {e}")))?;
    let before = q
        .before
        .as_deref()
        .map(aero_common::MessageId::from_str)
        .transpose()
        .map_err(|e| AeroError::Invalid(format!("before id: {e}")))?;
    let limit = q.limit.unwrap_or(100);
    let msgs = s.im.history(auth.participant_id, room, before, limit).await?;
    Ok(Json(serde_json::to_value(msgs).map_err(AeroError::from)?))
}

fn parse_room_kind(s: &str) -> AeroResult<RoomKind> {
    match s {
        "direct" => Ok(RoomKind::Direct),
        "group" => Ok(RoomKind::Group),
        "channel" => Ok(RoomKind::Channel),
        _ => Err(AeroError::Invalid(format!("unknown room kind: {s}"))),
    }
}

// Note: `From<E: Into<Error>> for ApiError` in `error.rs` covers conversion from
// `aero_common::Error`, `sqlx::Error`, `serde_json::Error`, and `anyhow::Error`.
