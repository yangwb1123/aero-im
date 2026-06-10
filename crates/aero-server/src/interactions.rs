//! Interactive message-block interactions HTTP surface (Slack Block Kit-lite).
//!
//! Messages may carry interactive [`Block`](aero_common::Block)s — a `Button` or
//! a `Select` dropdown. When a participant clicks a button or picks an option, the
//! client POSTs here; the server verifies the message actually contains a
//! component with that `action_id` (404 otherwise), records the interaction, and
//! broadcasts a [`RoomEvent::Interaction`](aero_common::RoomEvent) so the message's
//! poster — typically a bot/webhook/app integration — sees the click live and can
//! react. The durable record lives in `block_interactions`.
//!
//! Thin handlers: every path first resolves the message to find its room and
//! asserts the caller's room access (so you can only interact with / read
//! interactions on a message you can see), then delegates to
//! [`BlockInteractionRepo`](aero_storage::BlockInteractionRepo). The repo is built
//! inline from the shared pool (no dedicated `AppState` field), mirroring
//! [`crate::message_receipts`]. Mounted via [`routes`] and `.merge`d into the main
//! router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{message_has_action, Error as AeroError, MessageId, RoomEvent};
use aero_storage::BlockInteractionRepo;
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All block-interaction routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/messages/:id/interact", post(interact))
        .route("/api/messages/:id/interactions", get(list_interactions))
}

fn parse_message(s: &str) -> Result<MessageId, AeroError> {
    MessageId::from_str(s).map_err(|e| AeroError::Invalid(format!("message id: {e}")))
}

/// The block-interaction repo over the shared pool. `AppState` carries no
/// dedicated field for it, so we build it inline (cheap — a `PgPool` clone),
/// keeping this feature self-contained and `AppState` untouched.
fn repo(s: &AppState) -> BlockInteractionRepo {
    BlockInteractionRepo::new(s.participants.pool().clone())
}

/// Body of `POST /api/messages/:id/interact`: which component was hit, plus an
/// optional value (a chosen `Select` option's value / a button payload).
#[derive(Deserialize)]
struct InteractReq {
    /// The interactive block's `action_id` the caller is interacting with.
    action_id: String,
    /// The chosen `Select` option's value / button payload; absent for a
    /// value-less button click.
    #[serde(default)]
    value: Option<String>,
}

/// `POST /api/messages/:id/interact` — record an interaction with an interactive
/// block on the message. Resolves the message to find its room, asserts the caller
/// may access that room, then verifies the message actually contains a `Button` or
/// `Select` with the given `action_id` (404 if not — you can't fabricate an
/// interaction against a component that isn't there). Records the interaction and
/// broadcasts [`RoomEvent::Interaction`] so the poster's client sees it live.
/// Returns the recorded row. 404 if the message does not exist or is soft-deleted.
async fn interact(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<InteractReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let message = parse_message(&id_str)?;
    let msg = s
        .messages
        .get(message)
        .await?
        .filter(|m| m.deleted_at.is_none())
        .ok_or_else(|| AeroError::NotFound(format!("message {message}")))?;
    s.im.assert_room_access(auth.participant_id, msg.room_id).await?;

    let action_id = req.action_id.trim();
    if action_id.is_empty() {
        return Err(AeroError::Invalid("action_id is empty".into()).into());
    }
    // The interaction must target a real interactive component on this message.
    // Treating an unknown action as a 404 keeps a stale/forged client honest and
    // mirrors "the thing you're acting on isn't there".
    if !message_has_action(&msg.blocks, action_id) {
        return Err(AeroError::NotFound(format!(
            "message {message} has no interactive block with action_id {action_id:?}"
        ))
        .into());
    }

    let value = req.value.as_deref().map(str::trim).filter(|v| !v.is_empty());
    let recorded = repo(&s)
        .record(message, msg.room_id, auth.participant_id, action_id, value)
        .await?;

    s.im
        .broadcast_room_event(
            msg.room_id,
            RoomEvent::Interaction {
                room_id: msg.room_id,
                message_id: message,
                participant: auth.participant_id,
                action_id: action_id.to_owned(),
            },
        )
        .await;

    Ok(Json(serde_json::to_value(recorded).map_err(AeroError::from)?))
}

/// `GET /api/messages/:id/interactions` — the chronological list of interactions
/// recorded on a message (oldest first). Room-access gated (you can only read the
/// interactions on a message you can see). 404 if the message does not exist or is
/// soft-deleted.
async fn list_interactions(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let message = parse_message(&id_str)?;
    let msg = s
        .messages
        .get(message)
        .await?
        .filter(|m| m.deleted_at.is_none())
        .ok_or_else(|| AeroError::NotFound(format!("message {message}")))?;
    s.im.assert_room_access(auth.participant_id, msg.room_id).await?;

    let interactions = repo(&s).list_for_message(message).await?;
    Ok(Json(serde_json::json!({
        "message_id": message,
        "interactions": interactions,
    })))
}

#[cfg(test)]
mod tests {
    use aero_common::{message_has_action, Block, SelectOption};

    /// The pure `message_has_action` gate the `interact` handler relies on:
    /// it matches a real `Button`/`Select` by `action_id`, and rejects anything
    /// else (so a forged/stale action is a 404).
    #[test]
    fn message_has_action_gates_interactions() {
        let blocks = vec![
            Block::text("decide:"),
            Block::Button {
                action_id: "approve".into(),
                label: "Approve".into(),
                style: Some("primary".into()),
                url: None,
            },
            Block::Select {
                action_id: "assignee".into(),
                placeholder: Some("Assign to".into()),
                options: vec![SelectOption { value: "u1".into(), label: "Alice".into() }],
            },
        ];
        // Real components match.
        assert!(message_has_action(&blocks, "approve"));
        assert!(message_has_action(&blocks, "assignee"));
        // Unknown / non-interactive content does not.
        assert!(!message_has_action(&blocks, "reject"));
        assert!(!message_has_action(&[Block::text("plain")], "approve"));
    }
}
