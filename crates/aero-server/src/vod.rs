//! Stream VOD / recording HTTP surface (save a finished live stream for replay).
//!
//! A streamer flags a stream to be recorded (`POST .../record`); when the stream
//! ends, its written HLS playlist is retained as a VOD that members can list and
//! play back later. Two ways a VOD is created:
//!
//!   * **Explicit finalize** — `POST /api/streams/:id/vod` (owner): read the
//!     stream, and if it has an `hls_path`, snapshot it into a VOD. Always
//!     available, so the lifecycle is verifiable without a real publisher.
//!   * **Auto on end** — when a stream flagged `recording` ends, the `stream_end`
//!     handler in [`crate::routes`] best-effort calls [`finalize_recording`] so a
//!     VOD is created without a second request.
//!
//! Thin handlers — ownership is `stream.owner_id == caller` (matching the existing
//! stream routes); room listings gate on [`ImService::assert_room_access`]. The
//! repo over the shared pool is built inline (cheap `Arc<PgPool>` clone), keeping
//! `AppState` untouched, mirroring the other feature modules.
//!
//! SCOPE: real segment capture rides the EXISTING HLS writer (the documented
//! seam); this module owns only the recording lifecycle + VOD metadata + the
//! playback-URL exposure. It does not move media bytes.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, RoomId, Stream, StreamStatus, Vod, VodId};
use aero_storage::{playback_url, VodRepo};
use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All VOD routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        // Flag a stream for recording / finalize a stream into a VOD.
        .route("/api/streams/:id/record", post(set_record))
        .route("/api/streams/:id/vod", post(finalize_vod))
        // List a stream's / a room's recordings (each with a playback URL).
        .route("/api/streams/:id/vods", get(list_stream_vods))
        .route("/api/rooms/:id/vods", get(list_room_vods))
        // A single VOD + playback URL; owner-scoped delete.
        .route("/api/vods/:id", get(get_vod).delete(delete_vod))
}

/// The VOD repo over the shared pool. `AppState` carries no dedicated field for
/// it, so we build it inline (cheap — a clone of an `Arc<PgPool>`), keeping this
/// feature self-contained and `AppState` untouched.
fn repo(s: &AppState) -> VodRepo {
    VodRepo::new(s.participants.pool().clone())
}

fn parse_stream_id(s: &str) -> Result<ulid::Ulid, AeroError> {
    ulid::Ulid::from_str(s).map_err(|e| AeroError::Invalid(format!("stream id: {e}")))
}

fn parse_room_id(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

fn parse_vod_id(s: &str) -> Result<VodId, AeroError> {
    VodId::from_str(s).map_err(|e| AeroError::Invalid(format!("vod id: {e}")))
}

/// Load a stream the caller owns, or fail with the right HTTP status: 404 when it
/// does not exist, 403 when it exists but the caller is not its owner. Mirrors the
/// ownership rule the live/stream routes use (`stream.owner_id == caller`).
async fn owned_stream(
    s: &AppState,
    stream_id: ulid::Ulid,
    caller: aero_common::ParticipantId,
) -> Result<Stream, AeroError> {
    let stream = s
        .streams
        .get(stream_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("stream {stream_id}")))?;
    if stream.owner_id != caller {
        return Err(AeroError::Forbidden("only the stream owner may do that".into()));
    }
    Ok(stream)
}

/// Whole-second duration between a stream's start and end, when both are known and
/// the end is not before the start. Pure (no DB), so the derivation is testable.
#[must_use]
fn stream_duration_secs(stream: &Stream) -> Option<u32> {
    let start = stream.started_at?;
    let end = stream.ended_at?;
    let secs = (end - start).whole_seconds();
    u32::try_from(secs).ok()
}

/// Finalize a stream into a VOD: snapshot its written HLS playlist into a new
/// recording row and return it. Shared by the explicit `POST .../vod` route and
/// the auto-on-end hook in [`crate::routes::stream_end`].
///
/// The stream must be `Live` or `Ended` (an `Idle` stream has produced no
/// playlist) and must carry an `hls_path` (set once ingest marks it live). Returns
/// `Conflict` / `Invalid` otherwise. The caller is responsible for the owner check.
///
/// # Errors
/// [`AeroError::Conflict`] if the stream never went live, [`AeroError::Invalid`]
/// if it has no playlist yet, or a database error from the insert.
pub async fn finalize_recording(s: &AppState, stream: &Stream) -> Result<Vod, AeroError> {
    if matches!(stream.status, StreamStatus::Idle) {
        return Err(AeroError::Conflict(
            "stream has not gone live; nothing to record".into(),
        ));
    }
    let hls_path = stream
        .hls_path
        .as_deref()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| AeroError::Invalid("stream has no HLS playlist to record".into()))?;
    let duration = stream_duration_secs(stream);
    let id = repo(s)
        .create(
            stream.id,
            stream.owner_id,
            stream.room_id,
            &stream.title,
            hls_path,
            duration,
        )
        .await
        .map_err(AeroError::from)?;
    Ok(Vod {
        id,
        stream_id: stream.id,
        owner_id: stream.owner_id,
        room_id: stream.room_id,
        title: stream.title.clone(),
        hls_path: hls_path.to_owned(),
        duration_secs: duration,
        created_at: time::OffsetDateTime::now_utc(),
    })
}

/// Render `{ vod, playback_url }` for one VOD, resolving the absolute playlist URL
/// from the configured public base.
fn vod_json(s: &AppState, vod: &Vod) -> serde_json::Value {
    serde_json::json!({
        "vod": vod,
        "playback_url": playback_url(&s.public_base_url, &vod.hls_path),
    })
}

#[derive(Deserialize)]
struct RecordReq {
    on: bool,
}

/// `POST /api/streams/:id/record` — owner: flag (or unflag) a stream for
/// recording. A flagged stream is auto-finalized into a VOD when it ends.
async fn set_record(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<RecordReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_stream_id(&id_str)?;
    // Ownership gate (existence + owner) before flipping the flag.
    owned_stream(&s, id, auth.participant_id).await?;
    repo(&s).set_recording(id, req.on).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "stream_id": id.to_string(), "recording": req.on })))
}

/// `POST /api/streams/:id/vod` — owner: explicitly finalize the stream into a VOD
/// from its written playlist. Returns the created VOD + playback URL.
async fn finalize_vod(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_stream_id(&id_str)?;
    let stream = owned_stream(&s, id, auth.participant_id).await?;
    let vod = finalize_recording(&s, &stream).await?;
    Ok(Json(vod_json(&s, &vod)))
}

#[derive(Deserialize)]
struct LimitQuery {
    #[serde(default)]
    limit: Option<i64>,
}

/// `GET /api/streams/:id/vods` — recordings finalized from a stream, newest first,
/// each with a playback URL. Any authenticated caller may read (a stream's VODs
/// are as public as the stream itself).
async fn list_stream_vods(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<LimitQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_stream_id(&id_str)?;
    // A stream's recordings share its owner; filter the owner's list by stream id.
    let stream = s
        .streams
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("stream {id}")))?;
    let vods: Vec<Vod> = repo(&s)
        .list_for_owner(stream.owner_id, q.limit)
        .await
        .map_err(AeroError::from)?
        .into_iter()
        .filter(|v| v.stream_id == id)
        .collect();
    let out: Vec<serde_json::Value> = vods.iter().map(|v| vod_json(&s, v)).collect();
    Ok(Json(serde_json::json!({ "vods": out })))
}

/// `GET /api/rooms/:id/vods` — a room's recordings, newest first, each with a
/// playback URL. Gated on room access (workspace + room membership).
async fn list_room_vods(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<LimitQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&id_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let vods = repo(&s).list_for_room(room, q.limit).await.map_err(AeroError::from)?;
    let out: Vec<serde_json::Value> = vods.iter().map(|v| vod_json(&s, v)).collect();
    Ok(Json(serde_json::json!({ "vods": out })))
}

/// `GET /api/vods/:id` — one VOD + playback URL.
async fn get_vod(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_vod_id(&id_str)?;
    let vod = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("vod".into()))?;
    Ok(Json(vod_json(&s, &vod)))
}

/// `DELETE /api/vods/:id` — owner: delete a recording. Always owner-scoped, so a
/// caller can never delete another user's VOD (a non-owner gets 404, not 403, so
/// VOD existence is not leaked).
async fn delete_vod(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_vod_id(&id_str)?;
    let removed = repo(&s).delete(id, auth.participant_id).await.map_err(AeroError::from)?;
    if !removed {
        return Err(AeroError::NotFound("vod".into()).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::{ParticipantId, StreamProtocol};
    use time::OffsetDateTime;

    fn stream_with(
        status: StreamStatus,
        hls_path: Option<&str>,
        started: Option<OffsetDateTime>,
        ended: Option<OffsetDateTime>,
    ) -> Stream {
        Stream {
            id: ulid::Ulid::new(),
            owner_id: ParticipantId::new(),
            room_id: None,
            title: "t".into(),
            stream_key: "k".into(),
            status,
            hls_path: hls_path.map(str::to_owned),
            protocol: StreamProtocol::Whip,
            started_at: started,
            ended_at: ended,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn duration_is_whole_seconds_when_both_timestamps_present() {
        let start = OffsetDateTime::UNIX_EPOCH;
        let end = start + time::Duration::seconds(3661);
        let s = stream_with(StreamStatus::Ended, Some("/hls/x/index.m3u8"), Some(start), Some(end));
        assert_eq!(stream_duration_secs(&s), Some(3661));
    }

    #[test]
    fn duration_is_none_when_a_timestamp_is_missing() {
        let start = OffsetDateTime::UNIX_EPOCH;
        assert_eq!(
            stream_duration_secs(&stream_with(StreamStatus::Live, Some("/hls/x"), Some(start), None)),
            None,
            "live stream has no end yet"
        );
        assert_eq!(
            stream_duration_secs(&stream_with(StreamStatus::Ended, Some("/hls/x"), None, Some(start))),
            None,
            "missing start"
        );
    }

    #[test]
    fn duration_is_none_when_end_precedes_start() {
        // A clock-skew / bad-data end-before-start yields None rather than a huge
        // wrapped value (u32::try_from of a negative fails).
        let start = OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(100);
        let end = OffsetDateTime::UNIX_EPOCH;
        let s = stream_with(StreamStatus::Ended, Some("/hls/x"), Some(start), Some(end));
        assert_eq!(stream_duration_secs(&s), None);
    }
}
