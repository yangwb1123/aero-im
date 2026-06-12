//! Scheduled messages + reminders ("Send later").
//!
//! A user composes a message now and it is delivered to a room later (Slack's
//! "Send later"); a reminder is just a self-targeted scheduled note. Thin
//! handlers — persistence + the atomic due-claim live in
//! [`ScheduledRepo`](aero_storage::ScheduledRepo); actual delivery replays the
//! message through [`ImService::send_message`](aero_im_core::ImService::send_message)
//! so every normal invariant (membership, persistence, broadcast, mention
//! notifications) holds at delivery time.
//!
//! Mounted via [`routes`] and `.merge`d into the main router, mirroring
//! [`crate::workspaces`]. The background [`run_scheduled_dispatcher`] polls due
//! rows.

use std::str::FromStr;
use std::time::Duration;

use aero_auth::AuthUser;
use aero_common::{Block, Error as AeroError, MessageId, RoomId, ScheduledMessageId};
use aero_storage::ScheduledRepo;
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::error::ApiResult;
use crate::state::AppState;

/// How often the dispatcher polls for due messages.
const POLL_INTERVAL: Duration = Duration::from_secs(10);
/// Max messages claimed per dispatcher tick (bounds per-tick work; the repo
/// claims atomically so a backlog drains over successive ticks).
const CLAIM_BATCH: i64 = 100;

/// All scheduled-message routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/rooms/:id/scheduled",
            post(create_scheduled).get(list_scheduled),
        )
        .route(
            "/api/scheduled/:id",
            axum::routing::patch(update_scheduled).delete(cancel_scheduled),
        )
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

/// Build a [`ScheduledRepo`] from shared state. The `PgPool` is reached through
/// an existing repo's accessor (there is no bare pool field on [`AppState`]);
/// `ParticipantRepo::pool` is the canonical handle, mirroring how `routes::build`
/// probes the DB via `participants.pool()`.
fn repo(s: &AppState) -> ScheduledRepo {
    ScheduledRepo::new(s.participants.pool().clone())
}

#[derive(Deserialize)]
struct CreateScheduledReq {
    blocks: Vec<Block>,
    #[serde(default)]
    reply_to: Option<String>,
    /// Delivery time, RFC 3339 (e.g. `2026-06-03T18:30:00Z`). Parsed via serde's
    /// rfc3339, so a malformed value is a 400 at body-deserialization time.
    #[serde(with = "time::serde::rfc3339")]
    scheduled_at: time::OffsetDateTime,
}

/// `POST /api/rooms/:id/scheduled` — schedule a message for later delivery to the
/// room. Requires room access; rejects an empty body or a past `scheduled_at`.
async fn create_scheduled(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<CreateScheduledReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    // Tenant + room-membership guard before we let the caller stage a message.
    s.im.assert_room_access(auth.participant_id, room).await?;

    if req.blocks.is_empty() {
        return Err(AeroError::Invalid("blocks must not be empty".into()).into());
    }
    if req.scheduled_at <= time::OffsetDateTime::now_utc() {
        return Err(AeroError::Invalid("scheduled_at must be in the future".into()).into());
    }
    let reply_to = match req.reply_to.as_deref() {
        Some(r) => Some(
            MessageId::from_str(r).map_err(|e| AeroError::Invalid(format!("reply_to id: {e}")))?,
        ),
        None => None,
    };

    let id = repo(&s)
        .create(room, auth.participant_id, &req.blocks, reply_to, req.scheduled_at)
        .await?;
    Ok(Json(serde_json::json!({
        "id": id,
        "room_id": room,
        "scheduled_at": req.scheduled_at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
    })))
}

/// `GET /api/rooms/:id/scheduled` — the caller's own still-pending scheduled
/// messages for the room (soonest first). Requires room access.
async fn list_scheduled(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let pending = repo(&s)
        .list_pending_for_sender(auth.participant_id, Some(room))
        .await?;
    Ok(Json(serde_json::to_value(pending).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct UpdateScheduledReq {
    blocks: Vec<Block>,
    #[serde(default)]
    reply_to: Option<String>,
    /// New delivery time, RFC 3339. Rejected if in the past (mirroring create).
    #[serde(with = "time::serde::rfc3339")]
    scheduled_at: time::OffsetDateTime,
}

/// `PATCH /api/scheduled/:id` — edit one of the caller's own still-pending
/// scheduled messages (replace blocks + reply_to + delivery time). Sender-scoped:
/// a 404 if it isn't the caller's pending message (already delivered/canceled,
/// someone else's, or unknown). Rejects an empty body or a past `scheduled_at`.
async fn update_scheduled(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<UpdateScheduledReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = ScheduledMessageId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("scheduled message id: {e}")))?;
    if req.blocks.is_empty() {
        return Err(AeroError::Invalid("blocks must not be empty".into()).into());
    }
    if req.scheduled_at <= time::OffsetDateTime::now_utc() {
        return Err(AeroError::Invalid("scheduled_at must be in the future".into()).into());
    }
    let reply_to = match req.reply_to.as_deref() {
        Some(r) => Some(
            MessageId::from_str(r).map_err(|e| AeroError::Invalid(format!("reply_to id: {e}")))?,
        ),
        None => None,
    };
    let updated = repo(&s)
        .update(id, auth.participant_id, req.scheduled_at, &req.blocks, reply_to)
        .await?;
    if !updated {
        return Err(AeroError::NotFound(format!("scheduled message {id}")).into());
    }
    Ok(Json(serde_json::json!({
        "id": id,
        "updated": true,
        "scheduled_at": req.scheduled_at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
    })))
}

/// `DELETE /api/scheduled/:id` — cancel one of the caller's own pending
/// scheduled messages. Sender-scoped: a 404 if it isn't the caller's pending
/// message (already delivered/canceled, someone else's, or unknown).
async fn cancel_scheduled(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = ScheduledMessageId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("scheduled message id: {e}")))?;
    let canceled = repo(&s).cancel(id, auth.participant_id).await?;
    if !canceled {
        return Err(AeroError::NotFound(format!("scheduled message {id}")).into());
    }
    Ok(Json(serde_json::json!({ "canceled": true })))
}

/// Background delivery worker: every [`POLL_INTERVAL`] it atomically claims the
/// batch of messages now due ([`ScheduledRepo::claim_due`]) and replays each
/// through [`ImService::send_message`]. Best-effort per message — a single send
/// failure (e.g. the sender was removed from the room since scheduling) is
/// logged and skipped, never aborting the batch. The claim marks rows delivered
/// up front, so a transient send error will not cause an infinite redelivery
/// loop. Exits when `cancel` fires.
pub async fn run_scheduled_dispatcher(state: AppState, cancel: CancellationToken) {
    let sched = repo(&state);
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tracing::info!(interval_secs = POLL_INTERVAL.as_secs(), "scheduled-message dispatcher started");
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!("scheduled-message dispatcher: shutdown signal received, exiting");
                break;
            }
            _ = tick.tick() => {
                let now = time::OffsetDateTime::now_utc();
                let due = match sched.claim_due(now, CLAIM_BATCH).await {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(error = ?e, "scheduled dispatcher: claim_due failed");
                        continue;
                    }
                };
                if due.is_empty() {
                    continue;
                }
                tracing::debug!(count = due.len(), "scheduled dispatcher: delivering due messages");
                for msg in due {
                    if let Err(e) = state
                        .im
                        .send_message(msg.sender_id, msg.room_id, msg.blocks, msg.reply_to, None)
                        .await
                    {
                        tracing::warn!(
                            error = ?e,
                            scheduled_id = %msg.id,
                            room = %msg.room_id,
                            sender = %msg.sender_id,
                            "scheduled dispatcher: send failed (marked delivered, not retried)"
                        );
                    }
                }
            }
        }
    }
}
