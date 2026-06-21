//! Channel canvas — per-channel collaborative documents (Slack Canvas / Lark
//! Docs-in-channel).
//!
//! A channel ("room") can own several canvases; each is a titled rich document
//! whose body is an arbitrary JSON array of blocks (stored verbatim as JSONB, NOT
//! coupled to [`aero_common::Block`]). A canvas is *collaborative*: every member
//! with room access may create, read, edit, and delete one.
//!
//! Thin handlers over [`CanvasRepo`](aero_storage::CanvasRepo). Every route is
//! gated on the SAME tenant + membership guard the rest of the app uses
//! ([`assert_room_access`](aero_im_core::ImService::assert_room_access)): for the
//! room-scoped routes the room id is in the path; for the canvas-scoped routes we
//! first `get` the canvas to learn its `room_id` (`404` if missing), then assert
//! access against that room. Mounted via [`routes`] and `.merge`d into the main
//! router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{CanvasId, Error as AeroError, RoomId};
use aero_storage::{Canvas, CanvasOpRepo, CanvasRepo};
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
            "/api/canvases/:cid",
            get(get_canvas).put(update_canvas).delete(delete_canvas),
        )
        // ROADMAP 方向五·③: append-only collaborative op log. POST appends one
        // incremental edit op; GET fetches the ordered delta after `?since=`.
        .route(
            "/api/canvases/:cid/ops",
            get(list_ops).post(append_op),
        )
}

/// Max length of a canvas title, validated at the edge (`400` when exceeded).
const MAX_TITLE_LEN: usize = 512;

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
    if title.len() > MAX_TITLE_LEN {
        return Err(AeroError::Invalid("title too long".into()));
    }
    Ok(title)
}

/// Validate an optional `blocks` payload, defaulting absent to an empty array.
/// Must be a JSON *array* (the canvas body is a list of blocks); anything else is
/// rejected `400`.
fn clean_blocks(blocks: Option<serde_json::Value>) -> Result<serde_json::Value, AeroError> {
    match blocks {
        None => Ok(serde_json::Value::Array(Vec::new())),
        Some(v) if v.is_array() => Ok(v),
        Some(_) => Err(AeroError::Invalid("blocks must be a JSON array".into())),
    }
}

/// Load a canvas by id (`404` if missing), then assert the caller may access the
/// room it lives in (the shared tenant + membership guard). Returns the canvas so
/// the handler can reuse its current fields.
async fn load_with_access(
    s: &AppState,
    id: CanvasId,
    caller: aero_common::ParticipantId,
) -> Result<Canvas, AeroError> {
    let canvas = repo(s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("canvas {id}")))?;
    s.im.assert_room_access(caller, canvas.room_id).await?;
    Ok(canvas)
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
    s.im.assert_room_access(auth.participant_id, room).await?;

    let title = clean_title(&req.title)?;
    let blocks = clean_blocks(req.blocks)?;

    let id = repo(&s)
        .create(room, auth.participant_id, title, &blocks)
        .await
        .map_err(AeroError::from)?;
    // Re-read so the response carries the full, canonical row (timestamps).
    let row = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("canvas".into()))?;
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
    s.im.assert_room_access(auth.participant_id, room).await?;
    let canvases = repo(&s)
        .list_for_room(room)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(canvases).map_err(AeroError::from)?))
}

/// `GET /api/canvases/:cid` — one canvas by id. Resolves the canvas's room and
/// asserts the caller may access it (`404` if unknown, `403` if not a member).
async fn get_canvas(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_canvas(&id_str)?;
    let canvas = load_with_access(&s, id, auth.participant_id).await?;
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
    /// When present, the edit is rejected with `409` if another editor has since
    /// bumped the version (no silent lost update). Absent ⇒ last-write-wins (legacy).
    #[serde(default)]
    expected_version: Option<i64>,
}

/// `PUT /api/canvases/:cid` — edit a canvas (collaborative: any room member may
/// edit). Resolves the canvas's room and asserts access first. `title` and
/// `blocks` are each optional; an omitted field keeps its current value. A blank/
/// over-long title or a non-array `blocks` is `400`. Returns the updated canvas.
async fn update_canvas(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<UpdateCanvasReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_canvas(&id_str)?;
    let current = load_with_access(&s, id, auth.participant_id).await?;

    // Merge the optional fields over the canvas's current values.
    let title = match req.title.as_deref() {
        Some(raw) => clean_title(raw)?.to_owned(),
        None => current.title.clone(),
    };
    let blocks = match req.blocks {
        Some(v) if v.is_array() => v,
        Some(_) => return Err(AeroError::Invalid("blocks must be a JSON array".into()).into()),
        None => current.blocks.clone(),
    };

    let updated = repo(&s)
        .update(id, &title, &blocks, req.expected_version)
        .await
        .map_err(AeroError::from)?;
    if updated.is_none() {
        // No row matched. With an expected_version, that means a concurrent editor
        // bumped the version → reject (409) so the caller reloads + retries rather
        // than silently clobbering the other edit. Without one, the row was deleted.
        if req.expected_version.is_some() {
            return Err(AeroError::Conflict(
                "canvas was modified concurrently; reload and retry".into(),
            )
            .into());
        }
        return Err(AeroError::NotFound(format!("canvas {id}")).into());
    }
    let row = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("canvas {id}")))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `DELETE /api/canvases/:cid` — delete a canvas. Room-access gated (any member
/// may delete): resolves the canvas's room and asserts access, then removes it.
async fn delete_canvas(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_canvas(&id_str)?;
    // Establishes existence (`404`) and access (`403`) before deleting.
    load_with_access(&s, id, auth.participant_id).await?;
    let removed = repo(&s).delete(id).await.map_err(AeroError::from)?;
    if !removed {
        return Err(AeroError::NotFound(format!("canvas {id}")).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[derive(Deserialize)]
struct AppendOpReq {
    /// The incremental edit op — a JSON object (insert/delete/format/…). Stored
    /// verbatim; the server orders but never interprets it.
    op: serde_json::Value,
}

/// `POST /api/canvases/:cid/ops` — append one collaborative edit op to the
/// canvas's log (方向五·③). Room-access gated (any member may edit). The op must
/// be a JSON object within [`MAX_OP_BYTES`]. Returns the persisted op with its
/// assigned per-canvas `seq`, so the client can advance its local cursor.
async fn append_op(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<AppendOpReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_canvas(&id_str)?;
    // Existence (404) + membership (403) before recording anything.
    load_with_access(&s, id, auth.participant_id).await?;
    if !req.op.is_object() {
        return Err(AeroError::Invalid("op must be a JSON object".into()).into());
    }
    let encoded = serde_json::to_string(&req.op).map_err(AeroError::from)?;
    if encoded.len() > MAX_OP_BYTES {
        return Err(AeroError::Invalid("op too large".into()).into());
    }
    let op = op_repo(&s)
        .append(id, auth.participant_id, &req.op)
        .await
        .map_err(AeroError::from)?
        // Unreachable in practice (load_with_access just proved existence), but a
        // delete racing between the check and the append yields None → 404.
        .ok_or_else(|| AeroError::NotFound(format!("canvas {id}")))?;
    Ok(Json(serde_json::to_value(op).map_err(AeroError::from)?))
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

/// `GET /api/canvases/:cid/ops?since=&limit=` — the ordered op delta after
/// `since` (方向五·③), the catch-up a reconnecting/late editor reduces to rebuild
/// state. Room-access gated. Bounded page; the client continues from the last
/// returned `seq`.
async fn list_ops(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<OpsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_canvas(&id_str)?;
    load_with_access(&s, id, auth.participant_id).await?;
    let since = q.since.unwrap_or(0).max(0);
    let limit = q.limit.unwrap_or(DEFAULT_OPS_LIMIT).clamp(1, MAX_OPS_LIMIT);
    let ops = op_repo(&s)
        .ops_since(id, since, limit)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({
        "canvas_id": id,
        "since": since,
        "ops": ops,
    })))
}
