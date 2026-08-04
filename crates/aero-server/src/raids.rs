//! Raids HTTP API (creator sends their viewers to another stream at end).
//!
//! A "raid" is the Twitch/Kick end-of-stream send-off: the SOURCE stream's owner
//! raids a TARGET stream, redirecting their current viewers there. Thin handlers
//! over [`aero_storage::RaidRepo`].
//!
//! `POST /api/streams/:id/raid` is source-owner-gated; both streams must exist
//! (`404` otherwise). The current viewer count (cluster-correct, from the Redis
//! viewer set with the local Hub as fallback) is captured and a
//! [`StreamEvent::Raid`] is broadcast on the SOURCE stream so its watchers'
//! clients redirect to the target. `GET /api/streams/:id/raids` lists the raids a
//! stream launched, newest first.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, StreamEvent};
use aero_storage::{
    raid::{
        DEFAULT_RAID_HISTORY_LIMIT, MAX_RAID_HISTORY_LIMIT, MAX_RAID_HISTORY_OFFSET,
        MAX_RAID_MESSAGE_CHARS,
    },
    RaidRepo,
};
use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use ulid::Ulid;

use crate::error::ApiResult;
use crate::state::AppState;

/// Mount the raid routes, folded into the main router by [`crate::routes::build`].
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/streams/:id/raid", post(launch_raid))
        .route("/api/streams/:id/raids", get(list_raids))
        .route("/api/me/raid-analytics", get(raid_analytics))
}

fn parse_stream(s: &str) -> Result<Ulid, AeroError> {
    Ulid::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("stream id: {e}")))
}

fn repo(s: &AppState) -> RaidRepo {
    RaidRepo::new(s.pg.clone())
}

/// Current viewer count of a stream — cluster-correct from the Redis viewer set,
/// falling back to the local Hub count on a Redis error (mirrors the WS path).
async fn viewer_count(s: &AppState, stream: Ulid) -> u32 {
    let local = s.hub.stream_viewer_count(stream);
    match s.stream_viewers.count(stream).await {
        Ok(n) => u32::try_from(n).unwrap_or(u32::MAX).max(local),
        Err(_) => local,
    }
}

#[derive(Deserialize)]
struct RaidReq {
    target_stream_id: String,
    /// Optional custom message to show in the target channel (shoutout text).
    #[serde(default)]
    message: Option<String>,
}

/// `POST /api/streams/:id/raid` — the source stream's owner raids `target_stream_id`,
/// sending the source's current viewers there. Both streams must exist; records the
/// raid and broadcasts [`StreamEvent::Raid`] on the SOURCE stream so watchers redirect.
/// An optional `message` is persisted with the raid record.
async fn launch_raid(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<RaidReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let source = parse_stream(&id_str)?;
    let target = parse_stream(&req.target_stream_id)?;
    if source == target {
        return Err(AeroError::Invalid("cannot raid the same stream".into()).into());
    }
    let message = req
        .message
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    if message
        .as_deref()
        .is_some_and(|value| value.chars().count() > MAX_RAID_MESSAGE_CHARS)
    {
        return Err(AeroError::Invalid(format!(
            "raid message exceeds {MAX_RAID_MESSAGE_CHARS} characters"
        ))
        .into());
    }

    let count = viewer_count(&s, source).await;
    let id = repo(&s)
        .create_authorized(
            source,
            target,
            auth.participant_id,
            i32::try_from(count).unwrap_or(i32::MAX),
            message.as_deref(),
        )
        .await?;

    // `create_authorized` returns only after commit. Never publish a redirect
    // for a rejected or rolled-back raid.
    s.live
        .broadcast(&StreamEvent::Raid {
            stream_id: source,
            target_stream_id: target,
            viewer_count: count,
        })
        .await;

    Ok(Json(serde_json::json!({
        "raid_id": id.to_string(),
        "source_stream": source.to_string(),
        "target_stream": target.to_string(),
        "viewer_count": count,
        "message": message,
    })))
}

#[derive(Deserialize)]
struct RaidHistoryQuery {
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    offset: Option<i64>,
}

/// `GET /api/streams/:id/raids` — the raids this stream launched, newest first.
/// Any authenticated viewer may read the raid log.
async fn list_raids(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
    Query(query): Query<RaidHistoryQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let source = parse_stream(&id_str)?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_RAID_HISTORY_LIMIT)
        .clamp(1, MAX_RAID_HISTORY_LIMIT);
    let offset = query
        .offset
        .unwrap_or_default()
        .clamp(0, MAX_RAID_HISTORY_OFFSET);
    let raids = repo(&s)
        .list_for_stream(source, limit, offset)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "raids": raids })))
}

/// `GET /api/me/raid-analytics` — aggregate analytics for the authenticated user's
/// raids: total raids sent, total/avg/peak viewers carried.
async fn raid_analytics(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let analytics = repo(&s)
        .analytics(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(
        serde_json::to_value(analytics).map_err(AeroError::from)?,
    ))
}
