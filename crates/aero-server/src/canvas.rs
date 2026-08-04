//! Channel canvas — per-channel collaborative documents (Slack Canvas / Lark
//! Docs-in-channel).
//!
//! A channel ("room") can own several canvases; each is a titled rich document
//! whose body is an arbitrary JSON array of blocks (stored verbatim as JSONB, NOT
//! coupled to [`aero_common::Block`]). A canvas is *collaborative*: every member
//! with room access may create, read, edit, and delete one.
//!
//! Thin handlers over transaction-authorized storage APIs. Every resource route
//! carries both room and canvas ids, so storage can lock effective access and
//! bind the opaque resource id to the path room in one transaction.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{CanvasId, Error as AeroError, RoomId};
use aero_storage::{canvas::CanvasPatch, CanvasOpRepo, CanvasRepo};
use axum::{
    extract::{Path, Query, State},
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All channel-canvas routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/rooms/:id/canvases",
            get(list_canvases).post(create_canvas),
        )
        .route(
            "/api/rooms/:id/canvases/:cid",
            get(get_canvas).put(update_canvas).delete(delete_canvas),
        )
        // ROADMAP 方向五·③: append-only collaborative op log. POST appends one
        // incremental edit op; GET fetches the ordered delta after `?since=`.
        .route(
            "/api/rooms/:id/canvases/:cid/ops",
            get(list_ops).post(append_op),
        )
}

/// Max length of a canvas title, validated at the edge (`400` when exceeded).
const MAX_TITLE_LEN: usize = 512;
/// Maximum compact-JSON size of a full canvas snapshot accepted on create or
/// update. Incremental collaboration should use the much smaller op endpoint.
const MAX_BLOCKS_BYTES: usize = 1024 * 1024;

/// Build a [`CanvasRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> CanvasRepo {
    CanvasRepo::new(s.pg.clone())
}

/// Build a [`CanvasOpRepo`] (the collaborative op log) over the shared pool.
fn op_repo(s: &AppState) -> CanvasOpRepo {
    CanvasOpRepo::new(s.pg.clone())
}

/// Max serialized size of a single canvas op. An op is one incremental edit
/// (insert/delete/format); an unbounded blob would be a memory / amplification
/// vector against the shared op log, so anything larger is `400`.
const MAX_OP_BYTES: usize = 64 * 1024;
/// Default / hard ceiling on ops returned per delta fetch — the same
/// bounded-page discipline as message backfill, so a long log can't dump
/// unboundedly over one request (the client pages via `?since=`).
const DEFAULT_OPS_LIMIT: i64 = 500;
const MAX_OPS_LIMIT: i64 = 1000;

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

fn parse_canvas(s: &str) -> Result<CanvasId, AeroError> {
    CanvasId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("canvas id: {e}")))
}

/// Validate a (trimmed) title: non-empty and within [`MAX_TITLE_LEN`].
fn clean_title(raw: &str) -> Result<&str, AeroError> {
    let title = raw.trim();
    if title.is_empty() {
        return Err(AeroError::Invalid("title must not be empty".into()));
    }
    if title.chars().count() > MAX_TITLE_LEN {
        return Err(AeroError::Invalid("title too long".into()));
    }
    Ok(title)
}

/// Validate an optional `blocks` payload, defaulting absent to an empty array.
/// Must be a JSON *array* (the canvas body is a list of blocks); anything else is
/// rejected `400`.
fn clean_blocks(blocks: Option<serde_json::Value>) -> Result<serde_json::Value, AeroError> {
    let value = blocks.unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
    if !value.is_array() {
        return Err(AeroError::Invalid("blocks must be a JSON array".into()));
    }
    if serde_json::to_vec(&value)?.len() > MAX_BLOCKS_BYTES {
        return Err(AeroError::Invalid("blocks too large".into()));
    }
    Ok(value)
}

fn validate_op(op: &serde_json::Value) -> Result<(), AeroError> {
    if !op.is_object() {
        return Err(AeroError::Invalid("op must be a JSON object".into()));
    }
    if serde_json::to_vec(op)?.len() > MAX_OP_BYTES {
        return Err(AeroError::Invalid("op too large".into()));
    }
    Ok(())
}

#[derive(Deserialize)]
struct CreateCanvasReq {
    /// Human-readable title for the new canvas.
    title: String,
    /// Optional document body (a JSON array of blocks); absent ⇒ empty.
    #[serde(default)]
    blocks: Option<serde_json::Value>,
}

/// `POST /api/rooms/:id/canvases` — create a canvas in the room. Room-access
/// gated; the author is the caller. A blank/over-long title is `400`, and a
/// non-array `blocks` is `400`. Returns the created canvas.
async fn create_canvas(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<CreateCanvasReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    let title = clean_title(&req.title)?;
    let blocks = clean_blocks(req.blocks)?;

    let row = repo(&s)
        .create_canvas_authorized(room, auth.participant_id, title, &blocks)
        .await?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `GET /api/rooms/:id/canvases` — the room's canvases, newest edit first.
/// Room-access gated.
async fn list_canvases(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    let canvases = repo(&s)
        .list_canvases_authorized(room, auth.participant_id)
        .await?;
    Ok(Json(
        serde_json::to_value(canvases).map_err(AeroError::from)?,
    ))
}

/// `GET /api/rooms/:id/canvases/:cid` — one path-bound canvas.
async fn get_canvas(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((room_str, id_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    let id = parse_canvas(&id_str)?;
    let canvas = repo(&s)
        .get_canvas_authorized(room, id, auth.participant_id)
        .await?;
    Ok(Json(serde_json::to_value(canvas).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct UpdateCanvasReq {
    /// New title; absent ⇒ keep the current one.
    #[serde(default)]
    title: Option<String>,
    /// New document body (a JSON array of blocks); absent ⇒ keep the current one.
    #[serde(default)]
    blocks: Option<serde_json::Value>,
    /// Optimistic-concurrency guard (方向五): the `version` the client last read.
    /// The edit is rejected with `409` if another editor has since bumped the
    /// version (no silent lost update). Required for snapshot (`blocks`) writes;
    /// optional for legacy title-only edits.
    #[serde(default)]
    expected_version: Option<i64>,
    /// Highest op included in `blocks`. Required whenever `blocks` is supplied
    /// and atomically compared with the current durable op tail.
    #[serde(default)]
    snapshot_op_seq: Option<i64>,
}

/// `PUT /api/rooms/:id/canvases/:cid` — edit a path-bound canvas. `title` and
/// `blocks` are each optional; an omitted field keeps its current database value.
/// Snapshot writes must echo both `expected_version` and the highest materialized
/// `snapshot_op_seq`; a concurrent snapshot or op append is `409`. A blank or
/// over-long title / non-array `blocks` is `400`. Returns the committed canvas.
async fn update_canvas(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((room_str, id_str)): Path<(String, String)>,
    Json(req): Json<UpdateCanvasReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    let id = parse_canvas(&id_str)?;

    if req.title.is_none() && req.blocks.is_none() {
        return Err(AeroError::Invalid("title or blocks is required".into()).into());
    }
    let title = req
        .title
        .as_deref()
        .map(clean_title)
        .transpose()?
        .map(str::to_owned);
    if req.expected_version.is_some_and(|version| version < 0) {
        return Err(AeroError::Invalid("expected_version must be non-negative".into()).into());
    }
    if req.snapshot_op_seq.is_some_and(|seq| seq < 0) {
        return Err(AeroError::Invalid("snapshot_op_seq must be non-negative".into()).into());
    }
    if req.blocks.is_some() && (req.expected_version.is_none() || req.snapshot_op_seq.is_none()) {
        return Err(AeroError::Invalid(
            "blocks requires expected_version and snapshot_op_seq".into(),
        )
        .into());
    }
    if req.blocks.is_none() && req.snapshot_op_seq.is_some() {
        return Err(AeroError::Invalid("snapshot_op_seq is only valid with blocks".into()).into());
    }
    let blocks = match req.blocks {
        Some(v) => Some(clean_blocks(Some(v))?),
        None => None,
    };

    let row = repo(&s)
        .update_canvas_authorized(
            room,
            id,
            auth.participant_id,
            CanvasPatch {
                title: title.as_deref(),
                blocks: blocks.as_ref(),
                expected_version: req.expected_version,
                snapshot_op_seq: req.snapshot_op_seq,
            },
        )
        .await?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `DELETE /api/rooms/:id/canvases/:cid` — delete a path-bound canvas.
async fn delete_canvas(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((room_str, id_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    let id = parse_canvas(&id_str)?;
    repo(&s)
        .delete_canvas_authorized(room, id, auth.participant_id)
        .await?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[derive(Deserialize)]
struct AppendOpReq {
    /// Caller-generated retry key. It is unique only within this canvas and
    /// participant, so unrelated authors/canvases may reuse the same UUID.
    client_op_id: uuid::Uuid,
    /// The incremental edit op — a JSON object (insert/delete/format/…). Stored
    /// verbatim; the server orders but never interprets it.
    op: serde_json::Value,
}

/// `POST /api/rooms/:id/canvases/:cid/ops` — append one collaborative edit op to the
/// canvas's log (方向五·③). Room-access gated (any member may edit). The op must
/// be a JSON object within [`MAX_OP_BYTES`]. Returns the persisted op with its
/// assigned per-canvas `seq`, so the client can advance its local cursor.
/// `client_op_id` makes participant/canvas-scoped retries idempotent.
async fn append_op(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((room_str, id_str)): Path<(String, String)>,
    Json(req): Json<AppendOpReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    let id = parse_canvas(&id_str)?;
    validate_op(&req.op)?;
    let traceparent = aero_common::telemetry::current_traceparent();
    let appended = op_repo(&s)
        .append_canvas_op_authorized(
            room,
            id,
            auth.participant_id,
            req.client_op_id,
            &req.op,
            traceparent.as_deref(),
        )
        .await?;

    // Commit is already durable. This is only the low-latency attempt; the
    // background relay owns retries and NATS idempotency.
    if let Err(error) = s.im.dispatch_event_outbox_id(appended.outbox_id).await {
        tracing::warn!(
            ?error,
            outbox_id = %appended.outbox_id,
            canvas_id = %id,
            "fast canvas-op outbox dispatch failed"
        );
    }

    Ok(Json(
        serde_json::to_value(appended.op).map_err(AeroError::from)?,
    ))
}

#[derive(Deserialize)]
struct OpsQuery {
    /// Return ops strictly after this seq (the client's last-seen position).
    /// Absent ⇒ 0 (the whole log).
    #[serde(default)]
    since: Option<i64>,
    /// Page size, clamped to [1, [`MAX_OPS_LIMIT`]]. Absent ⇒ [`DEFAULT_OPS_LIMIT`].
    #[serde(default)]
    limit: Option<i64>,
}

/// `GET /api/rooms/:id/canvases/:cid/ops?since=&limit=` — the ordered op delta after
/// `since` (方向五·③), the catch-up a reconnecting/late editor reduces to rebuild
/// state. Room-access gated. Bounded page; the client continues from the last
/// returned `seq`.
async fn list_ops(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((room_str, id_str)): Path<(String, String)>,
    Query(q): Query<OpsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    let id = parse_canvas(&id_str)?;
    let since = q.since.unwrap_or(0).max(0);
    let limit = q.limit.unwrap_or(DEFAULT_OPS_LIMIT).clamp(1, MAX_OPS_LIMIT);
    let ops = op_repo(&s)
        .list_canvas_ops_authorized(room, id, auth.participant_id, since, limit)
        .await?;
    Ok(Json(serde_json::json!({
        "canvas_id": id,
        "since": since,
        "ops": ops,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handler_validation_bounds_canvas_snapshots_and_ops() {
        assert!(clean_blocks(Some(serde_json::json!([]))).is_ok());
        assert!(clean_blocks(Some(serde_json::json!({}))).is_err());
        assert!(clean_blocks(Some(serde_json::json!([
            {"text": "x".repeat(MAX_BLOCKS_BYTES)}
        ])))
        .is_err());

        assert!(validate_op(&serde_json::json!({"type": "insert"})).is_ok());
        assert!(validate_op(&serde_json::json!([])).is_err());
        assert!(validate_op(&serde_json::json!({
            "text": "x".repeat(MAX_OP_BYTES)
        }))
        .is_err());
    }

    #[test]
    fn append_request_requires_a_uuid_retry_key() {
        let client_op_id = uuid::Uuid::new_v4();
        let parsed: AppendOpReq = serde_json::from_value(serde_json::json!({
            "client_op_id": client_op_id,
            "op": {"type": "set_text", "text": "one"}
        }))
        .unwrap();
        assert_eq!(parsed.client_op_id, client_op_id);
        assert!(serde_json::from_value::<AppendOpReq>(serde_json::json!({
            "op": {"type": "set_text", "text": "one"}
        }))
        .is_err());
        assert!(serde_json::from_value::<AppendOpReq>(serde_json::json!({
            "client_op_id": "not-a-uuid",
            "op": {"type": "set_text", "text": "one"}
        }))
        .is_err());
    }
}
