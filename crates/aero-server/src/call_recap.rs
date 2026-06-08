//! Call-transcript + post-call AI recap reads (Wave 23).
//!
//! Teams/Zoom-style "meeting recap": the WS layer persists a call's *final*
//! caption lines into `call_transcripts` (see the `CallCaption` handler in
//! [`crate::ws`]) and, when the call ends, stores an AI summary grounded in what
//! was said onto the call session (the `CallEnd` handler). This module exposes
//! those two artifacts read-only over the persisted state owned by
//! [`CallTranscriptRepo`](aero_storage::CallTranscriptRepo) — there is no write
//! path here.
//!
//! Both handlers are room-access gated: the call's room is resolved via
//! [`CallRepo::room_id`](aero_storage::CallRepo::room_id) (`404` for an unknown
//! call), then [`assert_room_access`] authorizes the caller against that room
//! (`403` for a non-member / cross-tenant caller), exactly like
//! [`crate::call_history`]. So a call's transcript or recap can never leak to
//! someone who isn't in the call's room. Mounted via [`routes`] and `.merge`d
//! into the gateway router.
//!
//! [`assert_room_access`]: aero_im_core::ImService::assert_room_access

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{CallId, Error as AeroError};
use aero_storage::CallTranscriptRepo;
use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// Mount the call-recap routes, folded into the gateway router by
/// [`crate::routes::build`].
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/calls/:call_id/transcript", get(get_transcript))
        .route("/api/calls/:call_id/recap", get(get_recap))
}

/// Parse a `CallId` from a path segment, mapping a decode failure to `400`.
fn parse_call(s: &str) -> Result<CallId, AeroError> {
    CallId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("call id: {e}")))
}

/// Resolve the call's room and authorize `caller` against it. `404` for an
/// unknown call, `403`/`404` propagated from
/// [`assert_room_access`](aero_im_core::ImService::assert_room_access).
async fn authorize_call(s: &AppState, call: CallId, caller: AuthUser) -> Result<(), AeroError> {
    let room = s
        .calls
        .room_id(call)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("call {call}")))?;
    s.im.assert_room_access(caller.participant_id, room).await?;
    Ok(())
}

/// `GET /api/calls/:call_id/transcript` — the call's persisted transcript lines,
/// in spoken (chronological) order. Authorizes the caller against the call's room
/// (`404` for an unknown call, `403` for a non-member). The response shape is
/// `{ "lines": [ TranscriptLine, ... ] }`.
///
/// # Errors
/// - [`AeroError::Invalid`] when the path id fails to decode as a [`CallId`].
/// - `403` / `404` from [`authorize_call`].
/// - [`AeroError::Internal`] on a storage failure.
async fn get_transcript(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(call_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let call = parse_call(&call_str)?;
    authorize_call(&s, call, auth).await?;
    let lines = CallTranscriptRepo::new(s.pg.clone())
        .lines(call)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "lines": lines })))
}

/// `GET /api/calls/:call_id/recap` — the call's post-call AI recap, or `null` if
/// none was produced (no transcript, no AI backend, or an empty summary).
/// Authorizes the caller against the call's room (`404` for an unknown call,
/// `403` for a non-member). The response shape is `{ "recap": "..."|null }`.
///
/// # Errors
/// - [`AeroError::Invalid`] when the path id fails to decode as a [`CallId`].
/// - `403` / `404` from [`authorize_call`].
/// - [`AeroError::Internal`] on a storage failure.
async fn get_recap(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(call_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let call = parse_call(&call_str)?;
    authorize_call(&s, call, auth).await?;
    let recap = CallTranscriptRepo::new(s.pg.clone())
        .recap(call)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "recap": recap })))
}
