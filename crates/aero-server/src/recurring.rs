//! Recurring scheduled messages (repeating "Send later").
//!
//! Complements the one-shot [`crate::scheduled`] handlers: a user stages a
//! message that is re-posted to a room on a repeating cadence (`hourly` /
//! `daily` / `weekly`). Thin handlers — persistence lives in
//! [`RecurringMessageRepo`](aero_storage::RecurringMessageRepo); actual delivery
//! replays each firing through
//! [`ImService::send_message`](aero_im_core::ImService::send_message) so every
//! normal invariant (membership, persistence, broadcast, mention notifications)
//! holds at delivery time.
//!
//! Mounted via [`routes`] and `.merge`d into the main router, mirroring
//! [`crate::scheduled`]. The background [`run_recurring_dispatcher`] polls due
//! series, fires each, then advances `next_run` to the next occurrence.

use std::str::FromStr;
use std::sync::Arc;

use aero_auth::AuthUser;
use aero_common::{Block, Error as AeroError, RecurringMessageId, RoomId};
use aero_im_core::ImService;
use aero_storage::{next_occurrence, RecurringMessageRepo};
use axum::{
    extract::{Path, State},
    routing::{delete, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// The cadences a recurring message may use; anything else is a `400`.
const CADENCES: [&str; 3] = ["hourly", "daily", "weekly"];

/// All recurring-message routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/rooms/:id/recurring",
            post(create_recurring).get(list_recurring),
        )
        .route("/api/recurring/:rid", delete(cancel_recurring))
}

/// Build a [`RecurringMessageRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> RecurringMessageRepo {
    RecurringMessageRepo::new(s.pg.clone())
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

fn parse_recurring(s: &str) -> Result<RecurringMessageId, AeroError> {
    RecurringMessageId::from_str(s)
        .map_err(|e| AeroError::Invalid(format!("recurring message id: {e}")))
}

#[derive(Deserialize)]
struct CreateRecurringReq {
    /// The message payload, replayed on each firing.
    blocks: Vec<Block>,
    /// The cadence (`hourly` / `daily` / `weekly`).
    cadence: String,
}

/// `POST /api/rooms/:id/recurring` — stage a message re-posted on a repeating
/// cadence. Requires room access; rejects an empty body or an unknown cadence
/// (`400`). The first firing is one cadence step from now. Returns the created
/// row.
async fn create_recurring(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<CreateRecurringReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    // Tenant + room-membership guard before we let the caller stage a series.
    s.im.assert_room_access(auth.participant_id, room).await?;

    if req.blocks.is_empty() {
        return Err(AeroError::Invalid("blocks must not be empty".into()).into());
    }
    let cadence = req.cadence.trim();
    if !CADENCES.contains(&cadence) {
        return Err(AeroError::Invalid("cadence must be hourly, daily, or weekly".into()).into());
    }
    let now = time::OffsetDateTime::now_utc();
    let first_run = next_occurrence(cadence, now)
        .ok_or_else(|| AeroError::Invalid("cadence must be hourly, daily, or weekly".into()))?;

    let blocks_json = serde_json::to_value(&req.blocks).map_err(AeroError::from)?;
    let id = repo(&s)
        .create(room, auth.participant_id, &blocks_json, cadence, first_run)
        .await
        .map_err(AeroError::from)?;
    // Re-read so the response carries the full, canonical row (created_at).
    let row = repo(&s)
        .list_for_room(room)
        .await
        .map_err(AeroError::from)?
        .into_iter()
        .find(|m| m.id == id)
        .ok_or_else(|| AeroError::NotFound("recurring message".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `GET /api/rooms/:id/recurring` — the room's still-active recurring messages,
/// newest first. Requires room access.
async fn list_recurring(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let listed = repo(&s).list_for_room(room).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(listed).map_err(AeroError::from)?))
}

/// `DELETE /api/recurring/:rid` — cancel one of the caller's own recurring
/// series. Owner-scoped: a `404` if it isn't the caller's active series
/// (someone else's, already cancelled, or unknown).
async fn cancel_recurring(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_recurring(&id_str)?;
    let cancelled = repo(&s)
        .cancel(id, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    if !cancelled {
        return Err(AeroError::NotFound(format!("recurring message {id}")).into());
    }
    Ok(Json(serde_json::json!({ "cancelled": true })))
}

/// Background delivery worker: every `interval_secs` it lists the active series
/// now due ([`RecurringMessageRepo::list_due`]) and, for each, replays the stored
/// blocks through [`ImService::send_message`], then advances `next_run` to the
/// next occurrence of its cadence ([`RecurringMessageRepo::reschedule`]).
///
/// Best-effort per series — a single send failure (e.g. the sender was removed
/// from the room since scheduling) or a malformed stored payload is logged and
/// skipped, never aborting the batch. The series is always rescheduled after an
/// attempt, so a transient send error advances `next_run` rather than wedging
/// the worker in a tight redelivery loop. Runs until the future is dropped.
///
/// Not spawned here — the orchestrator `tokio::spawn`s it in the binary alongside
/// `run_scheduled_dispatcher`.
pub async fn run_recurring_dispatcher(
    repo: RecurringMessageRepo,
    im: Arc<ImService>,
    interval_secs: u64,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tracing::info!(interval_secs, "recurring-message dispatcher started");
    loop {
        tick.tick().await;
        let now = time::OffsetDateTime::now_utc();
        let due = match repo.list_due(now).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = ?e, "recurring dispatcher: list_due failed");
                continue;
            }
        };
        if due.is_empty() {
            continue;
        }
        tracing::debug!(count = due.len(), "recurring dispatcher: firing due series");
        for msg in due {
            let blocks: Vec<Block> = match serde_json::from_value(msg.blocks.clone()) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(
                        error = ?e,
                        recurring_id = %msg.id,
                        "recurring dispatcher: malformed stored blocks, skipping firing"
                    );
                    Vec::new()
                }
            };
            if !blocks.is_empty() {
                if let Err(e) = im
                    .send_message(msg.sender_id, msg.room_id, blocks, None, None)
                    .await
                {
                    tracing::warn!(
                        error = ?e,
                        recurring_id = %msg.id,
                        room = %msg.room_id,
                        sender = %msg.sender_id,
                        "recurring dispatcher: send failed (series still rescheduled)"
                    );
                }
            }
            // Always advance the series so a failed firing does not wedge the
            // worker in a tight redelivery loop on the same overdue row.
            if let Some(next) = next_occurrence(&msg.cadence, now) {
                if let Err(e) = repo.reschedule(msg.id, next).await {
                    tracing::warn!(
                        error = ?e,
                        recurring_id = %msg.id,
                        "recurring dispatcher: reschedule failed"
                    );
                }
            } else {
                tracing::warn!(
                    recurring_id = %msg.id,
                    cadence = %msg.cadence,
                    "recurring dispatcher: unknown cadence, cannot reschedule"
                );
            }
        }
    }
}
