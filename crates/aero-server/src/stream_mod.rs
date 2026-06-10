//! Live-stream chat moderation HTTP API (ban / timeout viewers from danmaku).
//!
//! Additive layer over the new [`aero_storage::StreamModRepo`]. The stream
//! *owner* (`stream.owner_id == auth.participant_id`) is the moderator: they ban
//! (permanent) or time-out (a `duration_secs` window) a viewer from posting in
//! that stream's chat, lift a ban, and list the stream's bans.
//!
//! Enforcement of the ban on the *posting* path lives next to the danmaku
//! handlers (`stream_chat_post` in [`crate::routes`] and the `StreamChat` WS
//! frame in [`crate::ws`]), which reject a banned poster with 403 before the
//! line is accepted/broadcast. Nothing here touches existing modules' code.

use std::str::FromStr;

use aero_common::{Error as AeroError, ParticipantId, Result as AeroResult};
use aero_storage::StreamModRepo;
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use time::{Duration, OffsetDateTime};
use ulid::Ulid;

use aero_auth::AuthUser;

use crate::error::ApiResult;
use crate::state::AppState;

/// Mount the stream-moderation routes. Folded into the main router by
/// [`crate::routes::build`]; kept separate so the moderation surface lives next
/// to its own storage repo, additively over the live/danmaku path.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/streams/:id/ban", post(ban_viewer))
        .route("/api/streams/:id/unban", post(unban_viewer))
        .route("/api/streams/:id/bans", get(list_bans))
}

fn parse_stream_id(s: &str) -> AeroResult<Ulid> {
    Ulid::from_str(s).map_err(|e| AeroError::Invalid(format!("stream id: {e}")))
}

fn parse_participant_id(s: &str) -> AeroResult<ParticipantId> {
    ParticipantId::from_str(s).map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
}

/// Resolve a stream and assert the caller may MODERATE its chat — the stream
/// OWNER *or* an assigned stream MODERATOR (migration 0083). Used by the
/// ban/timeout/unban authority so a moderator wields the same power as the owner.
/// Returns the stream id on success; `NotFound`/`Forbidden` otherwise.
async fn require_mod(s: &AppState, stream_str: &str, caller: ParticipantId) -> AeroResult<Ulid> {
    let stream_id = parse_stream_id(stream_str)?;
    if crate::stream_moderators::may_moderate(s, stream_id, caller).await? {
        Ok(stream_id)
    } else {
        Err(AeroError::Forbidden(
            "only the stream owner or a moderator may moderate its chat".into(),
        ))
    }
}

fn mod_repo(s: &AppState) -> StreamModRepo {
    StreamModRepo::new(s.participants.pool().clone())
}

#[derive(Deserialize)]
struct BanReq {
    participant_id: String,
    #[serde(default)]
    reason: Option<String>,
    /// Timeout length in seconds. Absent ⇒ permanent ban.
    #[serde(default)]
    duration_secs: Option<u64>,
}

/// `POST /api/streams/:id/ban` — owner bans or times-out a viewer from the chat.
async fn ban_viewer(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<BanReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream_id = require_mod(&s, &id_str, auth.participant_id).await?;
    let target = parse_participant_id(&req.participant_id)?;
    let until = match req.duration_secs {
        Some(secs) if secs > 0 => {
            let secs = i64::try_from(secs)
                .map_err(|_| AeroError::Invalid("duration_secs too large".into()))?;
            Some(OffsetDateTime::now_utc() + Duration::seconds(secs))
        }
        // Absent (or an explicit 0) ⇒ permanent ban.
        _ => None,
    };
    mod_repo(&s)
        .ban(stream_id, target, auth.participant_id, req.reason.as_deref(), until)
        .await?;
    Ok(Json(serde_json::json!({
        "banned": true,
        "participant_id": target.to_string(),
        "until": until.map(time::OffsetDateTime::unix_timestamp),
    })))
}

#[derive(Deserialize)]
struct UnbanReq {
    participant_id: String,
}

/// `POST /api/streams/:id/unban` — owner lifts a viewer's ban (idempotent).
async fn unban_viewer(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<UnbanReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream_id = require_mod(&s, &id_str, auth.participant_id).await?;
    let target = parse_participant_id(&req.participant_id)?;
    let removed = mod_repo(&s).unban(stream_id, target).await?;
    Ok(Json(serde_json::json!({ "banned": false, "removed": removed })))
}

/// `GET /api/streams/:id/bans` — owner lists the stream's bans, newest first.
async fn list_bans(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream_id = require_mod(&s, &id_str, auth.participant_id).await?;
    let bans = mod_repo(&s).list_bans(stream_id).await?;
    Ok(Json(serde_json::json!({ "bans": bans })))
}
