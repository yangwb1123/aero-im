//! Stream key rotation / reset HTTP API.
//!
//! A `stream_key` is a secret the publisher embeds in their RTMP/WHIP/SRT ingest
//! URL; anyone who learns it can hijack the broadcast. This module lets the
//! stream *owner* (`stream.owner_id == auth.participant_id`) rotate a leaked key
//! to a freshly generated one without recreating the stream — the standard
//! Twitch/YouTube "reset stream key" control.
//!
//! Thin handler over [`StreamRepo::rotate_key`](aero_storage::StreamRepo), which
//! is owner-scoped at the SQL layer (`WHERE id = $1 AND owner_id = $2`): a
//! non-owner (or unknown stream) matches no row and resolves to `None` ⇒ the
//! handler maps that to `404`, never leaking whether the stream exists. Mounted
//! via [`routes`] and `.merge`d into the main router; nothing else is touched.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, Result as AeroResult};
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};
use ulid::Ulid;

use crate::error::ApiResult;
use crate::state::AppState;

/// Stream-key routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/streams/:id/rotate-key", post(rotate_key))
}

fn parse_stream_id(s: &str) -> AeroResult<Ulid> {
    Ulid::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("stream id: {e}")))
}

/// `POST /api/streams/:id/rotate-key` — owner-only. Generate a new secret
/// `stream_key`, invalidating the old one, and return it.
///
/// Returns `404` when the stream does not exist *or* the caller is not its owner
/// (the owner-scoped SQL update matches no row in either case), so a stranger
/// cannot probe for or rotate another creator's key.
async fn rotate_key(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_stream_id(&id_str)?;
    let new_key = s
        .streams
        .rotate_key(id, auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("stream {id}")))?;
    Ok(Json(serde_json::json!({ "stream_key": new_key })))
}
