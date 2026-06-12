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
use aero_common::{Error as AeroError, ParticipantId, StreamEvent};
use aero_storage::{RaidRepo, StreamRepo};
use axum::{
    extract::{Path, State},
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

/// Resolve `stream` and assert `caller` owns it. Returns the resolved owner check;
/// `NotFound` if the stream is unknown, `Forbidden` if the caller is not its owner.
async fn require_owner(s: &AppState, stream: Ulid, caller: ParticipantId) -> Result<(), AeroError> {
    let row = StreamRepo::new(s.pg.clone())
        .get(stream)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("stream {stream}")))?;
    if row.owner_id != caller {
        return Err(AeroError::Forbidden(
            "only the source stream's owner may raid".into(),
        ));
    }
    Ok(())
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
}

/// `POST /api/streams/:id/raid` — the source stream's owner raids `target_stream_id`,
/// sending the source's current viewers there. Both streams must exist; records the
/// raid and broadcasts [`StreamEvent::Raid`] on the SOURCE stream so watchers redirect.
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
    // Source-owner only.
    require_owner(&s, source, auth.participant_id).await?;
    // The target must exist too — a raid to a phantom stream could never resolve.
    if StreamRepo::new(s.pg.clone())
        .get(target)
        .await
        .map_err(AeroError::from)?
        .is_none()
    {
        return Err(AeroError::NotFound(format!("target stream {target}")).into());
    }

    let count = viewer_count(&s, source).await;
    let id = repo(&s)
        .create(source, target, auth.participant_id, i32::try_from(count).unwrap_or(i32::MAX))
        .await?;

    // Tell the source stream's watchers to redirect to the target.
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
    })))
}

/// `GET /api/streams/:id/raids` — the raids this stream launched, newest first.
/// Any authenticated viewer may read the raid log.
async fn list_raids(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let source = parse_stream(&id_str)?;
    let raids = repo(&s)
        .list_for_stream(source)
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
    Ok(Json(serde_json::to_value(analytics).map_err(AeroError::from)?))
}
