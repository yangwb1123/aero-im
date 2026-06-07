//! Message templates / canned responses — per-user reusable message bodies.
//!
//! A user saves a reusable message body (a JSON array of blocks) under a name,
//! then lists, deletes, or posts it into a room with one call. Saving/listing/
//! deleting is owner-scoped at the SQL layer (a non-owner's id resolves to
//! `None`/`false` ⇒ `404`). *Sending* a template replays its stored blocks
//! through the SAME send path the normal message route uses
//! ([`ImService::send_message`](aero_im_core::ImService::send_message)), so the
//! usual room-membership / post-policy / moderation gates still apply — this
//! module never bypasses them.
//!
//! Thin handlers over [`MessageTemplateRepo`](aero_storage::MessageTemplateRepo).
//! Mounted via [`routes`] and `.merge`d into the main router, mirroring
//! [`crate::saved_searches`].

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Block, Error as AeroError, MessageTemplateId, RoomId};
use aero_storage::MessageTemplateRepo;
use axum::{
    extract::{Path, State},
    routing::{delete, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All message-template routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/templates", post(create_template).get(list_templates))
        .route("/api/templates/:tid", delete(delete_template))
        .route("/api/templates/:tid/send", post(send_template))
}

/// Max length (in chars) of a template name.
const MAX_NAME_LEN: usize = 128;

/// Build a [`MessageTemplateRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> MessageTemplateRepo {
    MessageTemplateRepo::new(s.pg.clone())
}

fn parse_template(s: &str) -> Result<MessageTemplateId, AeroError> {
    MessageTemplateId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("template id: {e}")))
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

/// Validate the `blocks` payload: it must be a non-empty JSON array. Pure, so the
/// rule is unit-tested without a database or bus.
fn validate_blocks(blocks: &serde_json::Value) -> Result<(), AeroError> {
    match blocks.as_array() {
        Some(arr) if !arr.is_empty() => Ok(()),
        Some(_) => Err(AeroError::Invalid("blocks must not be empty".into())),
        None => Err(AeroError::Invalid("blocks must be an array".into())),
    }
}

#[derive(Deserialize)]
struct CreateTemplateReq {
    /// Human-readable name for the template.
    name: String,
    /// The message body to persist — a JSON array of blocks, replayed on send.
    blocks: serde_json::Value,
}

/// `POST /api/templates` — save a reusable message body for the caller. A blank
/// or over-long name (`400`), or `blocks` that isn't a non-empty array (`400`),
/// is rejected. Returns the created row.
async fn create_template(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateTemplateReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(AeroError::Invalid("name must not be empty".into()).into());
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(AeroError::Invalid("name too long".into()).into());
    }
    validate_blocks(&req.blocks)?;

    let id = repo(&s)
        .create(auth.participant_id, name, &req.blocks)
        .await
        .map_err(AeroError::from)?;
    // Re-read so the response carries the full, canonical row (created_at).
    let row = repo(&s)
        .get(id, auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("template".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `GET /api/templates` — the caller's templates, newest first. Owner-scoped at
/// the SQL layer.
async fn list_templates(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let templates = repo(&s)
        .list_for(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "templates": templates })))
}

/// `DELETE /api/templates/:tid` — delete one of the caller's own templates.
/// Owner-scoped: a `404` if it isn't the caller's row (someone else's or
/// unknown).
async fn delete_template(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_template(&id_str)?;
    let removed = repo(&s)
        .delete(id, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    if !removed {
        return Err(AeroError::NotFound(format!("template {id}")).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[derive(Deserialize)]
struct SendTemplateReq {
    /// Destination room the template is posted into.
    room_id: String,
}

/// `POST /api/templates/:tid/send` — load the template (owner-scoped) and post
/// its stored body into a room. `404` if the template isn't the caller's. The
/// stored blocks are replayed through [`ImService::send_message`], which
/// re-checks room membership and applies the post-policy / moderation gates, so
/// this never bypasses them. Returns the newly-created message.
async fn send_template(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<SendTemplateReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_template(&id_str)?;
    let room = parse_room(&req.room_id)?;
    let template = repo(&s)
        .get(id, auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("template {id}")))?;

    let blocks: Vec<Block> = serde_json::from_value(template.blocks)
        .map_err(|e| AeroError::Invalid(format!("template blocks: {e}")))?;

    let message = s
        .im
        .send_message(auth.participant_id, room, blocks, None)
        .await?;
    Ok(Json(serde_json::to_value(message).map_err(AeroError::from)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A non-empty JSON array is accepted; anything else is rejected `400`.
    #[test]
    fn validate_blocks_accepts_non_empty_array_only() {
        assert!(validate_blocks(&serde_json::json!([{ "type": "text", "content": "hi" }])).is_ok());

        // Empty array, object, string, and null are all rejected.
        assert!(validate_blocks(&serde_json::json!([])).is_err());
        assert!(validate_blocks(&serde_json::json!({})).is_err());
        assert!(validate_blocks(&serde_json::json!("hi")).is_err());
        assert!(validate_blocks(&serde_json::Value::Null).is_err());
    }
}
