//! Message edit history — read prior versions of an edited message.
//!
//! Each time a message is edited, the OLD block content is captured into
//! `message_edits` BEFORE the live message is overwritten (the capture-on-edit
//! hook in the edit path is wired separately). This module exposes the READ
//! side: a member of the message's room can list those prior versions, newest
//! first.
//!
//! Thin handler over [`MessageEditRepo`](aero_storage::MessageEditRepo): it
//! resolves the message's room (`404` if the message is unknown), then asserts
//! room access via the shared
//! [`ImService::assert_room_access`](aero_im_core::ImService::assert_room_access)
//! (the same workspace + room membership guard `crate::routes` uses) before
//! returning any history (`403` for non-members). Mounted via [`routes`] and
//! `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, MessageId};
use aero_storage::MessageEditRepo;
use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// All message-history routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/messages/:id/history", get(message_history))
}

/// Build a [`MessageEditRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> MessageEditRepo {
    MessageEditRepo::new(s.pg.clone())
}

fn parse_message(s: &str) -> Result<MessageId, AeroError> {
    MessageId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("message id: {e}")))
}

/// `GET /api/messages/:id/history` — the prior versions of an edited message,
/// newest first. `404` if the message is unknown; `403` if the caller is not a
/// member of the message's room (workspace + room membership, via
/// `assert_room_access`). Returns `{ "history": [MessageEdit...] }`.
async fn message_history(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_message(&id_str)?;
    let r = repo(&s);
    // Resolve the message's room first so access can be membership-gated; an
    // unknown message is a `404`.
    let room = r
        .message_room(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("message {id}")))?;
    // Workspace + room membership guard (read-only; `ImService` is not mutated).
    s.im.assert_room_access(auth.participant_id, room).await?;
    let history = r.list_for_message(id).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "history": history })))
}
