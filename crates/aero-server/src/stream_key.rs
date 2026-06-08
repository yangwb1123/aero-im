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
    // Best-effort privileged-operation audit (ROADMAP 方向四). The key has already
    // rotated; a logging failure must only warn, never fail the request. Resolve
    // the tenant from the stream's room (a roomless stream has no workspace to
    // attribute, so it is skipped).
    audit_rotated(&s, id, auth.participant_id).await;
    Ok(Json(serde_json::json!({ "stream_key": new_key })))
}

/// Record a `stream_key.rotated` audit event, attributing it to `actor` against
/// the workspace of the stream's room. Best-effort throughout: any failure to
/// resolve the room/workspace or to append the event is warn-logged and swallowed
/// (the rotation already succeeded), and a stream with no room is silently skipped.
async fn audit_rotated(s: &AppState, stream: Ulid, actor: aero_common::ParticipantId) {
    let room = match s.streams.get(stream).await {
        Ok(Some(st)) => st.room_id,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(error = ?e, %stream, "stream_key audit: load stream failed");
            return;
        }
    };
    let Some(room) = room else { return };
    let workspace = match s.rooms.room_workspace(room).await {
        Ok(Some(ws)) => ws,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(error = ?e, %stream, "stream_key audit: resolve workspace failed");
            return;
        }
    };
    if let Err(e) = s
        .audit
        .append(
            workspace,
            Some(actor),
            "stream_key.rotated",
            Some(&stream.to_string()),
            serde_json::json!({ "stream_id": stream.to_string() }),
        )
        .await
    {
        tracing::warn!(error = ?e, %workspace, "stream_key.rotated audit append failed");
    }
}
