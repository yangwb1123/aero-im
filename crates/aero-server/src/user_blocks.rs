//! User-level blocking / ignoring HTTP surface.
//!
//! Routes:
//!
//! * `POST /api/users/:user_id/block` — block another participant. Idempotent.
//! * `DELETE /api/users/:user_id/block` — unblock. Idempotent.
//! * `GET /api/me/blocks` — list all participants the caller has blocked.

use std::str::FromStr as _;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// All user-block routes, ready to `.merge` into the gateway router.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/users/:user_id/block",
            post(block_user).delete(unblock_user),
        )
        .route("/api/me/blocks", get(list_blocks))
}

fn parse_participant(s: &str) -> Result<ParticipantId, AeroError> {
    ParticipantId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
}

/// `POST /api/users/:user_id/block` — block another participant.
///
/// Blocking yourself is rejected with 400. Otherwise idempotent: re-blocking is a
/// no-op (returns 200 immediately after the first block).
async fn block_user(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(target_str): Path<String>,
) -> ApiResult<StatusCode> {
    let target = parse_participant(&target_str)?;
    if target == auth.participant_id {
        return Err(AeroError::Invalid("cannot block yourself".into()).into());
    }
    s.blocks
        .block(auth.participant_id, target)
        .await?;
    Ok(StatusCode::OK)
}

/// `DELETE /api/users/:user_id/block` — unblock a previously-blocked participant.
///
/// Idempotent: unblocking someone who was never blocked is a no-op (returns 200).
async fn unblock_user(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(target_str): Path<String>,
) -> ApiResult<StatusCode> {
    let target = parse_participant(&target_str)?;
    s.blocks
        .unblock(auth.participant_id, target)
        .await?;
    Ok(StatusCode::OK)
}

/// `GET /api/me/blocks` — list all participants the caller has blocked.
///
/// Returns `{ "blocks": ["<uuid>", ...] }` — a JSON array of participant-id
/// strings, newest block first.
async fn list_blocks(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let blocked = s
        .blocks
        .blocks_for(auth.participant_id)
        .await?;
    let ids: Vec<String> = blocked.into_iter().map(|p| p.to_string()).collect();
    Ok(Json(serde_json::json!({ "blocks": ids })))
}
