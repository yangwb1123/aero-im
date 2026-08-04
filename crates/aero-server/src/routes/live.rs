//! Live streaming route handlers — stream CRUD, chat, gifts, leaderboard.
//! Extracted from the monolithic `routes.rs`.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, Result as AeroResult, Stream, StreamProtocol};
use aero_storage::StreamWriteError;
use axum::{
    extract::{Path, Query, State},
    http::header,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use ulid::Ulid;

use crate::error::ApiResult;
use crate::routes::helpers::parse_room_id;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/streams", post(stream_create).get(stream_list))
        .route("/api/streams/:id", get(stream_get))
        .route("/api/streams/:id/end", post(stream_end))
        .route("/api/live/gifts", get(live_gift_catalog))
        .route(
            "/api/streams/:id/chat",
            get(stream_chat_list).post(stream_chat_post),
        )
        .route(
            "/api/streams/:id/gifts",
            get(stream_gift_list).post(stream_gift_send),
        )
        .route("/api/streams/:id/leaderboard", get(stream_leaderboard))
}

#[derive(Deserialize)]
struct CreateStreamReq {
    title: String,
    #[serde(default)]
    room_id: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
}

#[derive(Deserialize)]
struct ChatPostReq {
    body: String,
}

#[derive(Deserialize)]
struct GiftSendReq {
    gift_id: String,
    #[serde(default)]
    qty: Option<u32>,
}

#[derive(serde::Deserialize)]
struct CursorLimitQuery {
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    since: Option<String>,
}

fn parse_stream_id(s: &str) -> AeroResult<ulid::Ulid> {
    ulid::Ulid::from_str(s).map_err(|e| AeroError::Invalid(format!("stream id: {e}")))
}

fn map_stream_write_error(error: StreamWriteError) -> AeroError {
    match error {
        StreamWriteError::Database(error) => AeroError::from(error),
        StreamWriteError::RoomNotAccessible => {
            AeroError::Forbidden("stream room is not accessible".into())
        }
    }
}

/// Viewer-safe stream metadata. The ingest key is a credential and must only be
/// returned by owner-authorized create/rotate-key endpoints.
fn public_stream_json(stream: Stream) -> serde_json::Value {
    let Stream {
        id,
        owner_id,
        room_id,
        title,
        stream_key: _,
        status,
        hls_path,
        protocol,
        started_at,
        ended_at,
        created_at,
    } = stream;
    serde_json::json!({
        "id": id,
        "owner_id": owner_id,
        "room_id": room_id,
        "title": title,
        "status": status,
        "hls_path": hls_path,
        "protocol": protocol,
        "started_at": started_at,
        "ended_at": ended_at,
        "created_at": created_at,
    })
}

// --- Handlers ---

async fn stream_create(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateStreamReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let proto = match req.protocol.as_deref().unwrap_or("rtmp") {
        "whip" => StreamProtocol::Whip,
        "srt" => StreamProtocol::Srt,
        _ => StreamProtocol::Rtmp,
    };
    let room_id = req.room_id.as_deref().map(parse_room_id).transpose()?;
    if let Some(room) = room_id {
        s.im.assert_room_access(auth.participant_id, room).await?;
    }
    let title = crate::stream_meta::validate_title(&req.title)?;
    let stream = s
        .streams
        .create(aero_storage::stream::NewStream {
            owner_id: auth.participant_id,
            room_id,
            title,
            protocol: proto,
            stream_key: None,
        })
        .await
        .map_err(map_stream_write_error)?;
    let ingest_url = s
        .live_ingest_urls
        .for_stream(stream.protocol, &stream.stream_key);
    Ok(Json(serde_json::json!({
        "id": stream.id, "stream_key": stream.stream_key, "title": stream.title,
        "protocol": stream.protocol, "ingest_url": ingest_url,
        "hls_url": stream.hls_path.map(|p| format!("{}/hls/{}/index.m3u8", s.public_base_url, p)),
    })))
}

async fn stream_list(
    State(s): State<AppState>,
    _auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let live = s.streams.list_live().await.map_err(AeroError::from)?;
    Ok(Json(serde_json::Value::Array(
        live.into_iter().map(public_stream_json).collect(),
    )))
}

async fn stream_get(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = Ulid::from_str(&id_str).map_err(|e| AeroError::Invalid(format!("stream id: {e}")))?;
    let stream = s
        .streams
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("stream".into()))?;
    Ok(Json(public_stream_json(stream)))
}

async fn live_gift_catalog(_auth: AuthUser) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "gifts": aero_common::gift_catalog() }))
}

async fn stream_chat_list(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<CursorLimitQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_stream_id(&id_str)?;
    let since = q
        .since
        .as_deref()
        .and_then(|c| ulid::Ulid::from_string(c.trim()).ok());
    let chat = s
        .live
        .recent_chat_since(id, since, q.limit.unwrap_or(50))
        .await?;
    Ok(Json(serde_json::json!({ "chat": chat })))
}

async fn stream_chat_post(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<ChatPostReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_stream_id(&id_str)?;
    crate::stream_chat_modes::enforce_chat_post(&s, id, auth.participant_id).await?;
    let is_sub = s.live.subscriber_flag(id, auth.participant_id).await;
    let line = s
        .live
        .post_chat(auth.participant_id, id, req.body, is_sub)
        .await?;
    Ok(Json(serde_json::json!(line)))
}

async fn stream_gift_list(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<CursorLimitQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_stream_id(&id_str)?;
    let since = q
        .since
        .as_deref()
        .and_then(|c| ulid::Ulid::from_string(c.trim()).ok());
    let gifts = s
        .live
        .recent_gifts_since(id, since, q.limit.unwrap_or(30))
        .await?;
    Ok(Json(serde_json::json!({ "gifts": gifts })))
}

async fn stream_gift_send(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    headers: header::HeaderMap,
    Json(req): Json<GiftSendReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_stream_id(&id_str)?;
    let qty = req.qty.unwrap_or(1);
    let idem = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let (line, _inserted) = s
        .live
        .send_gift(auth.participant_id, id, &req.gift_id, qty, idem)
        .await?;
    Ok(Json(serde_json::json!({ "gift": line })))
}

async fn stream_leaderboard(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<CursorLimitQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_stream_id(&id_str)?;
    let rows = s.live.leaderboard(id, q.limit.unwrap_or(10)).await?;
    Ok(Json(serde_json::json!({ "leaderboard": rows })))
}

async fn stream_end(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_stream_id(&id_str)?;
    s.live.end_stream(auth.participant_id, id).await?;
    let mut recorded_vod: Option<serde_json::Value> = None;
    if let Ok(Some(stream)) = s.streams.get(id).await {
        let flagged = aero_storage::VodRepo::new(s.participants.pool().clone())
            .is_recording(id)
            .await
            .unwrap_or(Some(false))
            .unwrap_or(false);
        if flagged {
            match crate::vod::finalize_recording(&s, &stream).await {
                Ok(vod) => recorded_vod = serde_json::to_value(&vod).ok(),
                Err(e) => tracing::warn!(error = ?e, stream = %id, "auto-VOD finalize failed"),
            }
        }
    }
    Ok(Json(serde_json::json!({ "ok": true, "vod": recorded_vod })))
}

#[cfg(test)]
mod tests {
    use aero_common::{ParticipantId, StreamStatus};

    use super::*;

    #[test]
    fn public_stream_metadata_never_serializes_ingest_key() {
        let stream = Stream {
            id: Ulid::new(),
            owner_id: ParticipantId::new(),
            room_id: None,
            title: "Live".into(),
            stream_key: "top-secret-ingest-key".into(),
            status: StreamStatus::Live,
            hls_path: Some("stream/path".into()),
            protocol: StreamProtocol::Rtmp,
            started_at: None,
            ended_at: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        };

        let public = public_stream_json(stream);
        assert!(public.get("stream_key").is_none());
        assert_eq!(public["title"], "Live");
        assert_eq!(public["hls_path"], "stream/path");
    }
}
