//! Read receipts route handlers.
use std::str::FromStr;
use axum::{extract::{Path, Query, State}, routing::{get, post}, Json, Router};
use aero_auth::AuthUser;
use aero_common::{Error as AeroError, MessageId, ParticipantId, RoomId, Result as AeroResult};
use serde::Deserialize;
use crate::error::ApiResult;
use crate::routes::helpers::parse_room_id;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/rooms/:id/read", post(mark_read))
        .route("/api/rooms/:id/receipts", get(list_receipts))
}

// ----- Read receipts -----

#[derive(Deserialize)]
pub(crate) struct MarkReadReq {
    last_message_id: String,
}

pub(crate) async fn mark_read(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<MarkReadReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&room_str)?;
    // Tenant guard (workspace + room membership) before recording a receipt.
    s.im.assert_room_access(auth.participant_id, room).await?;
    let mid = MessageId::from_str(&req.last_message_id)
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let r = s.im.mark_read(auth.participant_id, room, mid).await?;
    Ok(Json(serde_json::to_value(r).map_err(AeroError::from)?))
}

pub(crate) async fn list_receipts(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&room_str)?;
    // Tenant guard (workspace + room membership) — supersedes the prior bare
    // room-membership check.
    s.im.assert_room_access(auth.participant_id, room).await?;
    let rs = s.im.receipts_for(room).await?;
    Ok(Json(serde_json::to_value(rs).map_err(AeroError::from)?))
}

/// `GET /api/rooms/:id/delivery-cursor` — the caller's persisted DELIVERY cursor
/// for this room (ROADMAP 方向三·A): the Last-Known-Good `(message_id, seq)` the
/// client has ACKed receiving. Lets a client fetch its LKG over REST (e.g. an
/// offline-first client priming before opening the socket). Returns `null` when
/// the caller has never ACKed in the room. Member-gated via `assert_room_access`.
pub(crate) async fn get_delivery_cursor(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let cur = s
        .delivery_cursors
        .get(auth.participant_id, room)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(cur).map_err(AeroError::from)?))
}

