//! HTTP route definitions. WS upgrade lives in `ws::handler`.

use std::str::FromStr;

use aero_auth::{AuthUser, LoginRequest, RegisterRequest};
use aero_common::{
    BlobId, Error as AeroError, FileKind, MessageId, ParticipantId, Result as AeroResult, RoomId,
    RoomKind, StreamProtocol,
};
use aero_live_whip::{accept_whip_offer, WhipError};
use aero_storage::{blob::NewBlob, stream::NewStream};
use axum::{
    extract::{Multipart, Path, Query, State},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;
use crate::ws;

pub fn build(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        // Auth
        .route("/api/auth/register", post(auth_register))
        .route("/api/auth/login", post(auth_login))
        .route("/api/me", get(me))
        // Rooms
        .route("/api/rooms", post(create_room).get(list_rooms))
        .route("/api/rooms/:id/members", post(add_member))
        .route("/api/rooms/:id/messages", get(room_history))
        .route("/api/rooms/:id/read", post(mark_read))
        .route("/api/rooms/:id/receipts", get(list_receipts))
        .route("/api/rooms/:id/search", post(room_search))
        // Messages
        .route("/api/messages/:id", axum::routing::patch(edit_message).delete(delete_message))
        .route("/api/messages/:id/reactions", post(toggle_reaction))
        .route("/api/messages/reactions", post(reactions_batch))
        // Blobs
        .route("/api/blobs", post(blob_upload))
        .route("/api/blobs/:id", get(blob_download))
        // AI
        .route("/api/ai/summarize", post(ai_summarize))
        .route("/api/ai/ask", post(ai_ask))
        // Live streams
        .route("/api/streams", post(stream_create).get(stream_list))
        .route("/api/streams/:id", get(stream_get))
        // WHIP / WHEP — body is SDP text, response is SDP text
        .route("/whip/:stream_key", post(whip_post))
        .route("/whip/resource/:stream_id", axum::routing::delete(whip_delete))
        .route("/whep/:stream_id", post(whep_post))
        // Agents (Bot/Agent participants)
        .route("/api/agents", post(create_agent))
        .route("/api/participants/:id", get(get_participant))
        // MLS E2E (server is opaque relay; clients run openmls)
        .route("/api/mls/key-packages", post(mls_publish_kp))
        .route("/api/mls/key-packages/:participant", get(mls_consume_kp))
        .route("/api/mls/groups", post(mls_upsert_group))
        .route("/api/mls/groups/:gid", get(mls_get_group))
        // RTC config
        .route("/api/rtc/config", get(rtc_config))
        // WebSocket
        .route("/ws", get(ws::handler))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

// ----- Auth -----

async fn auth_register(
    State(s): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let out = s.auth.register(req).await?;
    Ok(Json(serde_json::json!({
        "access_token": out.access_token,
        "refresh_token": out.refresh_token,
        "participant": out.participant,
    })))
}

async fn auth_login(
    State(s): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let out = s.auth.login(req).await?;
    Ok(Json(serde_json::json!({
        "access_token": out.access_token,
        "refresh_token": out.refresh_token,
        "participant": out.participant,
    })))
}

async fn me(State(s): State<AppState>, auth: AuthUser) -> ApiResult<Json<serde_json::Value>> {
    let p = s
        .participants
        .get(auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("participant".into()))?;
    Ok(Json(serde_json::to_value(p).map_err(AeroError::from)?))
}

// ----- Rooms -----

#[derive(Deserialize)]
struct CreateRoomReq {
    kind: String,
    name: Option<String>,
}

async fn create_room(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateRoomReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let kind = parse_room_kind(&req.kind)?;
    let room = s.im.create_room(auth.participant_id, kind, req.name).await?;
    Ok(Json(serde_json::to_value(room).map_err(AeroError::from)?))
}

async fn list_rooms(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let rooms = s.im.list_my_rooms(auth.participant_id).await?;
    Ok(Json(serde_json::to_value(rooms).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct AddMemberReq {
    participant_id: String,
}

async fn add_member(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<AddMemberReq>,
) -> ApiResult<axum::http::StatusCode> {
    let room = parse_room_id(&room_str)?;
    let member = ParticipantId::from_str(&req.participant_id)
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))?;
    s.im.add_member(auth.participant_id, room, member).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct HistoryQuery {
    before: Option<String>,
    limit: Option<i64>,
}

async fn room_history(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&room_str)?;
    let before = q
        .before
        .as_deref()
        .map(MessageId::from_str)
        .transpose()
        .map_err(|e| AeroError::Invalid(format!("before id: {e}")))?;
    let limit = q.limit.unwrap_or(100);
    let msgs = s.im.history(auth.participant_id, room, before, limit).await?;
    Ok(Json(serde_json::to_value(msgs).map_err(AeroError::from)?))
}

// ----- Read receipts -----

#[derive(Deserialize)]
struct MarkReadReq {
    last_message_id: String,
}

async fn mark_read(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<MarkReadReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&room_str)?;
    let mid = MessageId::from_str(&req.last_message_id)
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let r = s.im.mark_read(auth.participant_id, room, mid).await?;
    Ok(Json(serde_json::to_value(r).map_err(AeroError::from)?))
}

async fn list_receipts(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&room_str)?;
    if !s.rooms.is_member(room, auth.participant_id).await.map_err(AeroError::from)? {
        return Err(AeroError::Forbidden("not a member".into()).into());
    }
    let rs = s.im.receipts_for(room).await?;
    Ok(Json(serde_json::to_value(rs).map_err(AeroError::from)?))
}

// ----- Messages -----

#[derive(Deserialize)]
struct EditMessageReq {
    blocks: Vec<aero_common::Block>,
}

async fn edit_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<EditMessageReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = MessageId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let m = s.im.edit_message(auth.participant_id, id, req.blocks).await?;
    Ok(Json(serde_json::to_value(m).map_err(AeroError::from)?))
}

async fn delete_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<StatusCode> {
    let id = MessageId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    s.im.delete_message(auth.participant_id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ToggleReactionReq {
    emoji: String,
}

async fn toggle_reaction(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<ToggleReactionReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = MessageId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let op = s.im.toggle_reaction(auth.participant_id, id, &req.emoji).await?;
    Ok(Json(serde_json::json!({
        "message_id": id,
        "emoji": req.emoji,
        "op": op,
    })))
}

#[derive(Deserialize)]
struct ReactionsBatchReq {
    message_ids: Vec<String>,
}

async fn reactions_batch(
    State(s): State<AppState>,
    _auth: AuthUser,
    Json(req): Json<ReactionsBatchReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ids: Vec<MessageId> = req
        .message_ids
        .iter()
        .map(|s| MessageId::from_str(s))
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let summaries = s.im.reactions_for(&ids).await?;
    let summaries_json: serde_json::Map<String, serde_json::Value> = summaries
        .into_iter()
        .map(|(mid, list)| {
            (
                mid.to_string(),
                serde_json::to_value(list).unwrap_or(serde_json::Value::Null),
            )
        })
        .collect();
    Ok(Json(serde_json::Value::Object(summaries_json)))
}

// ----- Search -----

#[derive(Deserialize)]
struct SearchReq {
    query: String,
    #[serde(default)]
    limit: Option<i64>,
    /// "fts" (default) | "vector" | "auto"
    #[serde(default)]
    mode: Option<String>,
}

async fn room_search(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<SearchReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&room_str)?;
    if !s.rooms.is_member(room, auth.participant_id).await.map_err(AeroError::from)? {
        return Err(AeroError::Forbidden("not a member".into()).into());
    }
    if req.query.trim().is_empty() {
        return Err(AeroError::Invalid("empty query".into()).into());
    }
    let limit = req.limit.unwrap_or(20);
    let mode = req.mode.as_deref().unwrap_or("auto");

    let hits = match mode {
        "vector" if s.ai.is_some() => {
            // Embed via AI service. Fallback to FTS if no AI.
            // Note: we can't call embed_text on the AiBackend trait directly here.
            // For now use FTS — vector search is wired via the AI worker when it
            // returns embedded query through `ai_jobs`. Keep client-facing fallback simple.
            s.messages.search_fts(room, &req.query, limit).await.map_err(AeroError::from)?
        }
        _ => s.messages.search_fts(room, &req.query, limit).await.map_err(AeroError::from)?,
    };

    Ok(Json(serde_json::json!({
        "query": req.query,
        "mode": mode,
        "results": hits.into_iter().map(|h| {
            serde_json::json!({
                "score": h.score,
                "message": h.message,
            })
        }).collect::<Vec<_>>(),
    })))
}

// ----- Blobs (multipart upload, byte download) -----

const MAX_BLOB_BYTES: usize = 32 * 1024 * 1024; // 32 MiB

async fn blob_upload(
    State(s): State<AppState>,
    auth: AuthUser,
    mut mp: Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    while let Some(field) = mp.next_field().await.map_err(|e| AeroError::Invalid(e.to_string()))? {
        if field.name() != Some("file") {
            continue;
        }
        let name = field.file_name().unwrap_or("untitled").to_owned();
        let mime = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_owned();
        let bytes = field.bytes().await.map_err(|e| AeroError::Invalid(e.to_string()))?;
        if bytes.len() > MAX_BLOB_BYTES {
            return Err(AeroError::Invalid(format!("blob too large: {} bytes", bytes.len())).into());
        }
        let kind = guess_file_kind(&mime);
        let size = bytes.len() as u64;
        let blob = s
            .blobs
            .create(NewBlob {
                owner_id: auth.participant_id,
                kind,
                name: name.clone(),
                mime: mime.clone(),
                size,
                sha256: None,
                storage_key: format!("pending:{}", uuid::Uuid::new_v4()),
            })
            .await
            .map_err(AeroError::from)?;
        let key = s
            .blob_store
            .put(blob.id, bytes)
            .await
            .map_err(|e| AeroError::Internal(anyhow::anyhow!("blob put: {e}")))?;
        let _ = key; // The store knows the key from the blob id.
        return Ok(Json(serde_json::json!({
            "id": blob.id,
            "name": blob.name,
            "mime": blob.mime,
            "size": blob.size,
            "kind": blob.kind,
        })));
    }
    Err(AeroError::Invalid("multipart missing 'file' field".into()).into())
}

async fn blob_download(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<axum::response::Response> {
    let id = BlobId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("blob id: {e}")))?;
    let meta = s
        .blobs
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("blob".into()))?;
    let bytes: Bytes = s
        .blob_store
        .get(id)
        .await
        .map_err(|e| AeroError::Internal(anyhow::anyhow!("blob get: {e}")))?;
    let mut resp = bytes.into_response();
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_str(&meta.mime).unwrap_or_else(|_| {
            header::HeaderValue::from_static("application/octet-stream")
        }),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_str(&format!("inline; filename=\"{}\"", meta.name))
            .unwrap_or_else(|_| header::HeaderValue::from_static("inline")),
    );
    Ok(resp)
}

fn guess_file_kind(mime: &str) -> FileKind {
    if mime.starts_with("image/") {
        FileKind::Image
    } else if mime.starts_with("video/") {
        FileKind::Video
    } else if mime.starts_with("audio/") {
        FileKind::Audio
    } else if mime == "application/pdf" || mime.starts_with("text/") || mime.contains("document") {
        FileKind::Document
    } else {
        FileKind::Other
    }
}

// ----- AI -----

#[derive(Deserialize)]
struct AiSummarizeReq {
    room_id: String,
    #[serde(default)]
    last_n: Option<usize>,
}

async fn ai_summarize(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<AiSummarizeReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&req.room_id)?;
    if !s.rooms.is_member(room, auth.participant_id).await.map_err(AeroError::from)? {
        return Err(AeroError::Forbidden("not a member".into()).into());
    }
    let last_n = req.last_n.unwrap_or(50);
    let ai = s.ai.as_ref().ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let summary = ai
        .summarize_room(room, last_n)
        .await
        .map_err(|e| AeroError::Upstream(format!("ai: {e}")))?;
    Ok(Json(serde_json::json!({"summary": summary})))
}

#[derive(Deserialize)]
struct AiAskReq {
    room_id: String,
    question: String,
    #[serde(default)]
    k: Option<usize>,
}

async fn ai_ask(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<AiAskReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&req.room_id)?;
    if !s.rooms.is_member(room, auth.participant_id).await.map_err(AeroError::from)? {
        return Err(AeroError::Forbidden("not a member".into()).into());
    }
    let k = req.k.unwrap_or(8);
    let ai = s.ai.as_ref().ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let answer = ai
        .answer_question(room, &req.question, k)
        .await
        .map_err(|e| AeroError::Upstream(format!("ai: {e}")))?;
    Ok(Json(serde_json::json!({
        "answer": answer.answer,
        "citations": answer.citations,
    })))
}

// ----- Streams (P4) -----

#[derive(Deserialize)]
struct CreateStreamReq {
    title: String,
    #[serde(default)]
    room_id: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
}

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
    let room_id = req
        .room_id
        .as_deref()
        .map(parse_room_id)
        .transpose()?;
    let stream = s
        .streams
        .create(NewStream {
            owner_id: auth.participant_id,
            room_id,
            title: req.title,
            protocol: proto,
            stream_key: None,
        })
        .await
        .map_err(AeroError::from)?;

    let ingest_url = match stream.protocol {
        StreamProtocol::Rtmp => format!("rtmp://{}/live/{}", strip_scheme(&s.public_base_url), stream.stream_key),
        StreamProtocol::Whip => format!("{}/whip/{}", s.public_base_url, stream.stream_key),
        StreamProtocol::Srt => format!("srt://{}?streamid={}", strip_scheme(&s.public_base_url), stream.stream_key),
    };
    let hls_url = format!("/hls/{}/index.m3u8", stream.id);

    Ok(Json(serde_json::json!({
        "stream": stream,
        "ingest_url": ingest_url,
        "hls_url": hls_url,
    })))
}

async fn stream_list(
    State(s): State<AppState>,
    _auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let live = s.streams.list_live().await.map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(live).map_err(AeroError::from)?))
}

async fn stream_get(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = ulid::Ulid::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("stream id: {e}")))?;
    let stream = s
        .streams
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("stream".into()))?;
    Ok(Json(serde_json::to_value(stream).map_err(AeroError::from)?))
}

fn strip_scheme(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_owned()
}

// ----- Agents -----

#[derive(Deserialize)]
struct CreateAgentReq {
    display_name: String,
    #[serde(default)]
    kind: Option<String>, // "bot" | "agent"
    #[serde(default)]
    avatar_url: Option<String>,
}

async fn create_agent(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateAgentReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let kind = match req.kind.as_deref().unwrap_or("bot") {
        "agent" => aero_common::ParticipantKind::Agent,
        _ => aero_common::ParticipantKind::Bot,
    };
    let bot = s
        .participants
        .create_bot(&req.display_name, kind, Some(auth.participant_id), req.avatar_url.as_deref())
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(bot).map_err(AeroError::from)?))
}

async fn get_participant(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = ParticipantId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))?;
    let p = s
        .participants
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("participant".into()))?;
    Ok(Json(serde_json::to_value(p).map_err(AeroError::from)?))
}

// ----- MLS (P8) — opaque-bytes relay -----

#[derive(Deserialize)]
struct PublishKpReq {
    ciphersuite: String,
    /// Base64-encoded KeyPackage bytes.
    payload_b64: String,
}

fn b64_decode(s: &str) -> Result<Vec<u8>, AeroError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| AeroError::Invalid(format!("base64: {e}")))
}
fn b64_encode(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}

async fn mls_publish_kp(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<PublishKpReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let payload = b64_decode(&req.payload_b64)?;
    if payload.len() > 16 * 1024 {
        return Err(AeroError::Invalid("KeyPackage too large".into()).into());
    }
    let kp = s
        .key_packages
        .publish(auth.participant_id, &req.ciphersuite, payload)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({
        "id": kp.id,
        "ciphersuite": kp.ciphersuite,
        "created_at": kp.created_at,
    })))
}

async fn mls_consume_kp(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(pid_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = ParticipantId::from_str(&pid_str)
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))?;
    let kp = s
        .key_packages
        .consume_one(target)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("no fresh KeyPackages".into()))?;
    Ok(Json(serde_json::json!({
        "id": kp.id,
        "participant_id": kp.participant_id,
        "ciphersuite": kp.ciphersuite,
        "payload_b64": b64_encode(&kp.payload),
        "created_at": kp.created_at,
    })))
}

#[derive(Deserialize)]
struct UpsertGroupReq {
    group_id_b64: String,
    ciphersuite: String,
    epoch: u64,
    state_b64: String,
    #[serde(default)]
    room_id: Option<String>,
}

async fn mls_upsert_group(
    State(s): State<AppState>,
    _auth: AuthUser,
    Json(req): Json<UpsertGroupReq>,
) -> ApiResult<axum::http::StatusCode> {
    let group_id = aero_common::mls::MlsGroupId::new(b64_decode(&req.group_id_b64)?);
    let state_bytes = b64_decode(&req.state_b64)?;
    let room_id = req.room_id.as_deref().map(parse_room_id).transpose()?;
    let g = aero_common::mls::MlsGroupState {
        group_id,
        room_id,
        ciphersuite: req.ciphersuite,
        epoch: req.epoch,
        state: state_bytes,
        updated_at: time::OffsetDateTime::now_utc(),
    };
    s.mls_groups.upsert(&g).await.map_err(AeroError::from)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn mls_get_group(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(gid_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let gid_bytes = b64_decode(&gid_str)?;
    let gid = aero_common::mls::MlsGroupId::new(gid_bytes);
    let g = s
        .mls_groups
        .get(&gid)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("group".into()))?;
    Ok(Json(serde_json::json!({
        "group_id_b64": b64_encode(g.group_id.as_bytes()),
        "room_id": g.room_id,
        "ciphersuite": g.ciphersuite,
        "epoch": g.epoch,
        "state_b64": b64_encode(&g.state),
        "updated_at": g.updated_at,
    })))
}

// ----- WHIP / WHEP -----

async fn whip_post(
    State(s): State<AppState>,
    Path(stream_key): Path<String>,
    sdp_offer: String,
) -> ApiResult<axum::response::Response> {
    use axum::http::{header, HeaderValue, StatusCode};
    let stream = s
        .streams
        .get_by_key(&stream_key)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("stream".into()))?;
    let resource = accept_whip_offer(&stream, &sdp_offer, &s.ingest_host, s.ingest_port)
        .map_err(|e| match e {
            WhipError::InvalidSdp(m) => AeroError::Invalid(m),
            WhipError::Conflict => AeroError::Conflict("publisher present".into()),
            WhipError::NotFound => AeroError::NotFound("stream".into()),
            WhipError::Internal(m) => AeroError::Internal(anyhow::anyhow!(m)),
        })?;
    s.whip
        .insert(resource.clone())
        .map_err(|_| AeroError::Conflict("publisher present".into()))?;
    let hls_path = format!("/hls/{}/index.m3u8", stream.id);
    if let Err(e) = s.streams.mark_live(stream.id, &hls_path).await {
        tracing::warn!(error=?e, "mark live failed");
    }
    let mut resp = (
        StatusCode::CREATED,
        [(header::CONTENT_TYPE, "application/sdp")],
        resource.answer_sdp.clone(),
    )
        .into_response();
    resp.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(&format!("/whip/resource/{}", stream.id))
            .unwrap_or_else(|_| HeaderValue::from_static("/whip/resource")),
    );
    Ok(resp)
}

async fn whip_delete(
    State(s): State<AppState>,
    Path(stream_id_str): Path<String>,
) -> ApiResult<StatusCode> {
    let stream_id = ulid::Ulid::from_str(&stream_id_str)
        .map_err(|e| AeroError::Invalid(format!("stream id: {e}")))?;
    s.whip.remove(stream_id);
    if let Err(e) = s.streams.mark_ended(stream_id).await {
        tracing::warn!(error=?e, "mark ended failed");
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn whep_post(
    State(s): State<AppState>,
    Path(stream_id_str): Path<String>,
    _sdp_offer: String,
) -> ApiResult<axum::response::Response> {
    use axum::http::{header, StatusCode};
    let stream_id = ulid::Ulid::from_str(&stream_id_str)
        .map_err(|e| AeroError::Invalid(format!("stream id: {e}")))?;
    let resource = s
        .whip
        .get(stream_id)
        .ok_or_else(|| AeroError::NotFound("no live publisher".into()))?;
    // P5 placeholder: return the *publisher's answer* SDP for clients to inspect.
    // Real WHEP requires re-publishing the SFU side as sendonly — wired in P6.
    let resp = (
        StatusCode::CREATED,
        [(header::CONTENT_TYPE, "application/sdp")],
        resource.answer_sdp,
    )
        .into_response();
    Ok(resp)
}

// ----- RTC config -----

async fn rtc_config(_auth: AuthUser) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(rtc_config_payload()))
}

fn rtc_config_payload() -> serde_json::Value {
    let stun = std::env::var("AERO_STUN_URLS")
        .unwrap_or_else(|_| "stun:stun.l.google.com:19302".into());
    let urls: Vec<String> = stun.split(',').map(|s| s.trim().to_owned()).collect();
    let mut ice_servers = vec![serde_json::json!({"urls": urls})];
    if let (Ok(url), Ok(user), Ok(pass)) = (
        std::env::var("AERO_TURN_URL"),
        std::env::var("AERO_TURN_USERNAME"),
        std::env::var("AERO_TURN_PASSWORD"),
    ) {
        ice_servers.push(serde_json::json!({
            "urls": [url],
            "username": user,
            "credential": pass,
        }));
    }
    serde_json::json!({
        "ice_servers": ice_servers,
        "ice_transport_policy": "all",
    })
}

// ----- helpers -----

fn parse_room_kind(s: &str) -> AeroResult<RoomKind> {
    match s {
        "direct" => Ok(RoomKind::Direct),
        "group" => Ok(RoomKind::Group),
        "channel" => Ok(RoomKind::Channel),
        _ => Err(AeroError::Invalid(format!("unknown room kind: {s}"))),
    }
}

fn parse_room_id(s: &str) -> AeroResult<RoomId> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}
