//! "Who reacted" — list the participants behind each emoji on a message.
//!
//! The reaction picker's detail view (Slack's "Alice, Bob and 3 others reacted
//! with 👍"). Given a message, return every distinct emoji together with the
//! participants who reacted with it. This adds NO table, NO id and NO migration —
//! it is a read-only projection over the existing `reactions` table, owned by the
//! NEW [`ReactionDetailRepo`](aero_storage::ReactionDetailRepo).
//!
//! The single handler resolves + authorizes the message exactly like
//! [`crate::message_reminders`]: find the message's room
//! ([`ReactionDetailRepo::room_for`](aero_storage::ReactionDetailRepo::room_for)
//! → `404` if unknown), then [`assert_room_access`] → `403` for a non-member /
//! cross-tenant caller — so the reactor list can never leak from a room the caller
//! isn't in. Mounted via [`routes`] and `.merge`d into the gateway router.
//!
//! [`assert_room_access`]: aero_im_core::ImService::assert_room_access

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, MessageId};
use aero_storage::ReactionDetailRepo;
use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// Mount the "who reacted" route, folded into the gateway router by
/// [`crate::routes::build`].
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/messages/:id/reactions/detail", get(reaction_detail))
}

/// `GET /api/messages/:id/reactions/detail` — who reacted with each emoji.
///
/// Resolves the message's room (`404` if the message is unknown), authorizes the
/// caller against that room (`assert_room_access` → `403` for a non-member /
/// cross-tenant caller), then returns the per-emoji reactor lists. The response
/// shape is `{ "reactions": [ { "emoji", "participants": [ids] }, ... ] }`, emojis
/// ascending and each emoji's reactors oldest-first.
async fn reaction_detail(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let mid = MessageId::from_str(id_str.trim())
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let repo = ReactionDetailRepo::new(s.pg.clone());

    let room = repo
        .room_for(mid)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("message {mid}")))?;
    // Tenant + room-membership guard before exposing who reacted.
    s.im.assert_room_access(auth.participant_id, room).await?;

    let reactions = repo.reactors_for(mid).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "reactions": reactions })))
}
