//! Message permalink / jump-to-message context (Slack "jump to message").
//!
//! When a client follows a permalink to a single message, it wants not just that
//! message but the surrounding conversation so the message can be rendered in
//! context. This module exposes the READ side: resolve the target message
//! (`404` if unknown / deleted), assert the caller may see its room (`403` for
//! non-members), then return the target plus a centered window of the messages
//! immediately before and after it.
//!
//! Thin handler over [`MessageRepo`](aero_storage::MessageRepo): it resolves the
//! message via `get` (so the room can be derived and access membership-gated),
//! asserts room access via the shared
//! [`ImService::assert_room_access`](aero_im_core::ImService::assert_room_access)
//! (the same workspace + room membership guard `crate::routes` uses), then reads
//! the window via
//! [`MessageRepo::messages_around`](aero_storage::MessageRepo::messages_around).
//! Mounted via [`routes`] and `.merge`d into the main router. Mirrors the
//! structure of [`crate::message_history`].

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, MessageId};
use aero_storage::MessageRepo;
use axum::{
    extract::{Path, Query, State},
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// Default half-window when the caller omits `?window=`: 25 messages on each
/// side of the target, matching a comfortable "jump to message" viewport.
const DEFAULT_WINDOW: i64 = 25;

/// All message-context routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/messages/:id/context", get(message_context))
}

/// Build a [`MessageRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> MessageRepo {
    MessageRepo::new(s.pg.clone())
}

fn parse_message(s: &str) -> Result<MessageId, AeroError> {
    MessageId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("message id: {e}")))
}

#[derive(Debug, Deserialize)]
struct ContextQuery {
    /// Half-window: how many messages to fetch on each side of the target. The
    /// repo clamps this to `[1, 100]`; omitted defaults to [`DEFAULT_WINDOW`].
    window: Option<i64>,
}

/// `GET /api/messages/:id/context?window=N` — the target message plus a centered
/// window of the messages before and after it (Slack "jump to message"). `404`
/// if the message is unknown or deleted; `403` if the caller is not a member of
/// the message's room (workspace + room membership, via `assert_room_access`).
/// Returns `{ "target": <msg>, "messages": [<window before..target..after>] }`.
async fn message_context(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<ContextQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_message(&id_str)?;
    let r = repo(&s);
    // Resolve the message first so access can be membership-gated; an unknown or
    // soft-deleted message is a `404`.
    let target = r
        .get(id)
        .await
        .map_err(AeroError::from)?
        .filter(|m| m.deleted_at.is_none())
        .ok_or_else(|| AeroError::NotFound(format!("message {id}")))?;
    // Workspace + room membership guard (read-only; `ImService` is not mutated).
    s.im.assert_room_access(auth.participant_id, target.room_id).await?;
    let half_window = q.window.unwrap_or(DEFAULT_WINDOW);
    let messages = r
        .messages_around(target.room_id, id, half_window)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "target": target, "messages": messages })))
}
