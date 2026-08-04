//! Stream-moderator role HTTP API (assign MOD ROLE on a stream's chat).
//!
//! DISTINCT from [`crate::stream_mod`] (which bans/timeouts chatters): this assigns
//! a moderator ROLE to a participant on a stream. The stream OWNER adds/removes
//! moderators and lists them; a moderator then gains the same chat-ban/timeout
//! authority as the owner (the ban handler in [`crate::stream_mod`] allows owner OR
//! [`is_stream_moderator`]). Management handlers use actor-aware storage
//! transactions; the helper reads remain for other live feature gates.
//!
//! `POST`/`DELETE /api/streams/:id/moderators[/:pid]` are owner-gated; `GET` lists
//! the stream's moderators. Owner-gating resolves the stream and checks
//! `stream.owner_id == caller` (mirroring [`crate::stream_mod::routes`]).

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId};
use aero_storage::{StreamModeratorRepo, StreamRepo};
use axum::{
    extract::{Path, State},
    routing::{delete, post},
    Json, Router,
};
use serde::Deserialize;
use ulid::Ulid;

use crate::error::ApiResult;
use crate::state::AppState;

/// Mount the stream-moderator routes, folded into the main router by
/// [`crate::routes::build`].
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/streams/:id/moderators",
            post(add_moderator).get(list_moderators),
        )
        .route("/api/streams/:id/moderators/:pid", delete(remove_moderator))
}

fn parse_stream(s: &str) -> Result<Ulid, AeroError> {
    Ulid::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("stream id: {e}")))
}

fn parse_participant(s: &str) -> Result<ParticipantId, AeroError> {
    ParticipantId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
}

fn repo(s: &AppState) -> StreamModeratorRepo {
    StreamModeratorRepo::new(s.pg.clone())
}

/// Whether `participant` may moderate `stream`'s chat: they are the stream OWNER
/// or hold the moderator ROLE. The shared authority check used by the chat-ban
/// path so a moderator wields the same ban/timeout power as the owner.
///
/// Best-effort on the role lookup: a storage error degrades to "owner-only" rather
/// than granting moderation, so an outage can never widen authority.
pub async fn may_moderate(
    s: &AppState,
    stream: Ulid,
    participant: ParticipantId,
) -> Result<bool, AeroError> {
    let row = StreamRepo::new(s.pg.clone())
        .get(stream)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("stream {stream}")))?;
    if row.owner_id == participant {
        return Ok(true);
    }
    let is_mod = repo(s)
        .is_moderator(stream, participant)
        .await
        .map_err(AeroError::from)?;
    Ok(is_mod)
}

/// Whether `participant` holds the moderator ROLE on `stream` (excludes the owner).
/// A thin pass-through for callers that already know the owner.
///
/// # Errors
/// Propagates storage errors via [`AeroError`].
pub async fn is_stream_moderator(
    s: &AppState,
    stream: Ulid,
    participant: ParticipantId,
) -> Result<bool, AeroError> {
    repo(s)
        .is_moderator(stream, participant)
        .await
        .map_err(AeroError::from)
}

#[derive(Deserialize)]
struct AddModReq {
    participant_id: String,
}

/// `POST /api/streams/:id/moderators` — owner grants a participant the mod role.
async fn add_moderator(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<AddModReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&id_str)?;
    let target = parse_participant(&req.participant_id)?;
    let id = repo(&s)
        .add_authorized(stream, target, auth.participant_id)
        .await?;
    Ok(Json(serde_json::json!({
        "moderator_id": id.to_string(),
        "stream_id": stream.to_string(),
        "participant_id": target.to_string(),
    })))
}

/// `DELETE /api/streams/:id/moderators/:pid` — owner revokes a participant's mod role.
async fn remove_moderator(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((id_str, pid_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&id_str)?;
    let target = parse_participant(&pid_str)?;
    let removed = repo(&s)
        .remove_authorized(stream, target, auth.participant_id)
        .await?;
    Ok(Json(serde_json::json!({ "removed": removed })))
}

/// `GET /api/streams/:id/moderators` — the stream's moderators, newest first.
/// Owner-gated (the moderator roster is management data, like the ban list).
async fn list_moderators(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&id_str)?;
    let mods = repo(&s)
        .list_authorized(stream, auth.participant_id)
        .await?;
    Ok(Json(serde_json::json!({ "moderators": mods })))
}
