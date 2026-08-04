//! Recurring scheduled messages (repeating "Send later").
//!
//! Complements the one-shot [`crate::scheduled`] handlers: a user stages a
//! message that is re-posted to a room on a repeating cadence (`hourly` /
//! `daily` / `weekly`). Thin handlers — persistence lives in
//! [`RecurringMessageRepo`](aero_storage::RecurringMessageRepo); actual delivery
//! replays each leased occurrence through sender-idempotent message delivery, so
//! a crash between message commit and cadence confirmation cannot duplicate it.
//!
//! Mounted via [`routes`] and `.merge`d into the main router, mirroring
//! [`crate::scheduled`]. The background [`run_recurring_dispatcher`] polls due
//! series, fires each, then advances `next_run` to the next occurrence.

use std::str::FromStr;
use std::sync::Arc;

use aero_auth::AuthUser;
use aero_common::{Block, Error as AeroError, RecurringMessageId, RoomId};
use aero_im_core::ImService;
use aero_storage::{
    next_occurrence,
    recurring_message::{RecurringDeliveryClaim, RecurringFailureDisposition},
    RecurringMessageRepo,
};
use axum::{
    extract::{Path, State},
    routing::{delete, post},
    Json, Router,
};
use futures::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::error::ApiResult;
use crate::state::AppState;
use crate::task_shutdown;

/// The cadences a recurring message may use; anything else is a `400`.
const CADENCES: [&str; 3] = ["hourly", "daily", "weekly"];
const DELIVERY_LEASE: time::Duration = time::Duration::minutes(5);
const DELIVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const CLAIM_BATCH: i64 = 32;
const DELIVERY_CONCURRENCY: usize = 8;
const MAX_ATTEMPTS: i32 = 8;

#[derive(Serialize)]
struct RecurringRequestFingerprint<'a> {
    version: u8,
    room_id: RoomId,
    blocks: &'a [Block],
    reply_to: Option<aero_common::MessageId>,
    expires_after_secs: Option<u64>,
}

fn request_hash(room: RoomId, blocks: &[Block]) -> Result<[u8; 32], AeroError> {
    let canonical = serde_json::to_vec(&RecurringRequestFingerprint {
        version: 1,
        room_id: room,
        blocks,
        reply_to: None,
        expires_after_secs: None,
    })?;
    Ok(Sha256::digest(canonical).into())
}

fn retry_backoff(attempt: i32) -> time::Duration {
    let exponent = u32::try_from(attempt.saturating_sub(1))
        .unwrap_or(0)
        .min(20);
    let multiplier = 1_i64.checked_shl(exponent).unwrap_or(i64::MAX);
    time::Duration::seconds(5_i64.saturating_mul(multiplier).min(3600))
}

fn send_error_retryable(error: &AeroError) -> bool {
    matches!(
        error,
        AeroError::RateLimited
            | AeroError::Upstream(_)
            | AeroError::Database(_)
            | AeroError::Internal(_)
    )
}

/// All recurring-message routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/rooms/:id/recurring",
            post(create_recurring).get(list_recurring),
        )
        .route(
            "/api/rooms/:room/recurring/:rid",
            delete(cancel_recurring_in_room),
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
    blocks: serde_json::Value,
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

    let blocks = crate::deferred_blocks::decode(req.blocks)?;
    let cadence = req.cadence.trim();
    if !CADENCES.contains(&cadence) {
        return Err(AeroError::Invalid("cadence must be hourly, daily, or weekly".into()).into());
    }
    let now = time::OffsetDateTime::now_utc();
    let first_run = next_occurrence(cadence, now)
        .ok_or_else(|| AeroError::Invalid("cadence must be hourly, daily, or weekly".into()))?;

    let blocks_json = serde_json::to_value(&blocks).map_err(AeroError::from)?;
    let id = repo(&s)
        .create_authorized(room, auth.participant_id, &blocks_json, cadence, first_run)
        .await?;
    let row = repo(&s)
        .get_authorized(Some(room), id, auth.participant_id)
        .await?;
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
    let listed = repo(&s)
        .list_for_room_authorized(room, auth.participant_id)
        .await?;
    Ok(Json(serde_json::to_value(listed).map_err(AeroError::from)?))
}

/// `DELETE /api/recurring/:rid` — cancel one of the caller's own recurring
/// series. Owner-scoped: a `404` if it isn't the caller's active series
/// (someone else's, already cancelled, currently holding a delivery lease, or
/// unknown). A failed occurrence waiting for retry can still be cancelled.
async fn cancel_recurring(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_recurring(&id_str)?;
    repo(&s)
        .cancel_authorized(None, id, auth.participant_id)
        .await?;
    Ok(Json(serde_json::json!({ "cancelled": true })))
}

async fn cancel_recurring_in_room(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((room_str, id_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let id = parse_recurring(&id_str)?;
    repo(&s)
        .cancel_authorized(Some(room), id, auth.participant_id)
        .await?;
    Ok(Json(
        serde_json::json!({ "room_id": room, "cancelled": true }),
    ))
}

async fn settle_failure(
    repo: &RecurringMessageRepo,
    claim: &RecurringDeliveryClaim,
    error: &str,
    retryable: bool,
) {
    let retry_at = time::OffsetDateTime::now_utc() + retry_backoff(claim.attempt);
    match repo
        .record_failure(
            claim.series.id,
            claim.claim_token,
            claim.delivery_key,
            claim.attempt,
            retry_at,
            error,
            retryable,
            MAX_ATTEMPTS,
        )
        .await
    {
        Ok(RecurringFailureDisposition::RetryScheduled) => tracing::warn!(
            recurring_id = %claim.series.id,
            attempt = claim.attempt,
            retry_at = %retry_at,
            error,
            "recurring dispatcher: occurrence re-parked"
        ),
        Ok(RecurringFailureDisposition::Dead) => tracing::error!(
            recurring_id = %claim.series.id,
            attempt = claim.attempt,
            retryable,
            error,
            "recurring dispatcher: series disabled after terminal delivery failure"
        ),
        Ok(RecurringFailureDisposition::FenceLost) => tracing::warn!(
            recurring_id = %claim.series.id,
            attempt = claim.attempt,
            "recurring dispatcher: failure settlement lost claim fence"
        ),
        Err(settle_error) => tracing::warn!(
            error = ?settle_error,
            recurring_id = %claim.series.id,
            "recurring dispatcher: failure settlement failed; lease will be reclaimed"
        ),
    }
}

async fn deliver_claim(
    repo: &RecurringMessageRepo,
    im: &Arc<ImService>,
    claim: RecurringDeliveryClaim,
    cancel: &CancellationToken,
) {
    let blocks: Vec<Block> = match serde_json::from_value::<Vec<Block>>(claim.payload.clone()) {
        Ok(blocks) if !blocks.is_empty() => blocks,
        Ok(_) => {
            settle_failure(repo, &claim, "stored recurring payload is empty", false).await;
            return;
        }
        Err(error) => {
            settle_failure(
                repo,
                &claim,
                &format!("malformed stored blocks: {error}"),
                false,
            )
            .await;
            return;
        }
    };
    let hash = match request_hash(claim.series.room_id, &blocks) {
        Ok(hash) => hash,
        Err(error) => {
            settle_failure(repo, &claim, &error.to_string(), false).await;
            return;
        }
    };
    let send = im.send_message_idempotent(
        claim.series.sender_id,
        claim.series.room_id,
        blocks,
        None,
        None,
        claim.delivery_key,
        hash,
    );
    let result = tokio::select! {
        biased;
        () = cancel.cancelled() => Err(("dispatcher shutdown".to_owned(), true)),
        result = tokio::time::timeout(DELIVERY_TIMEOUT, send) => match result {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => {
                let retryable = send_error_retryable(&error);
                Err((error.to_string(), retryable))
            }
            Err(_) => Err((
                format!("send timed out after {}s", DELIVERY_TIMEOUT.as_secs()),
                true,
            )),
        }
    };
    if let Err((error, retryable)) = result {
        settle_failure(repo, &claim, &error, retryable).await;
        return;
    }

    let sent_at = time::OffsetDateTime::now_utc();
    let Some(next) = next_occurrence(&claim.series.cadence, sent_at) else {
        settle_failure(
            repo,
            &claim,
            &format!("unknown cadence {}", claim.series.cadence),
            false,
        )
        .await;
        return;
    };
    match repo
        .confirm_sent(
            claim.series.id,
            claim.claim_token,
            claim.delivery_key,
            claim.attempt,
            next,
            sent_at,
        )
        .await
    {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            recurring_id = %claim.series.id,
            attempt = claim.attempt,
            "recurring dispatcher: success confirmation lost claim fence"
        ),
        Err(error) => tracing::warn!(
            ?error,
            recurring_id = %claim.series.id,
            "recurring dispatcher: success confirmation failed; lease will be reclaimed"
        ),
    }
}

/// Compatibility entry point using a never-cancelled token.
pub async fn run_recurring_dispatcher(
    repo: RecurringMessageRepo,
    im: Arc<ImService>,
    interval_secs: u64,
) {
    run_recurring_dispatcher_until_cancelled(repo, im, interval_secs, CancellationToken::new())
        .await;
}

/// Run the recurring-message dispatcher until `cancel` is triggered.
pub async fn run_recurring_dispatcher_until_cancelled(
    repo: RecurringMessageRepo,
    im: Arc<ImService>,
    interval_secs: u64,
    cancel: CancellationToken,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_secs.max(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tracing::info!(interval_secs, "recurring-message dispatcher started");
    loop {
        if task_shutdown::tick_or_cancelled(&mut tick, &cancel).await {
            return;
        }
        let now = time::OffsetDateTime::now_utc();
        let due = match repo
            .claim_due_with_policy(now, DELIVERY_LEASE, MAX_ATTEMPTS, CLAIM_BATCH)
            .await
        {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = ?e, "recurring dispatcher: claim_due failed");
                continue;
            }
        };
        if due.is_empty() {
            continue;
        }
        tracing::debug!(count = due.len(), "recurring dispatcher: firing due series");
        stream::iter(due)
            .for_each_concurrent(DELIVERY_CONCURRENCY, |claim| {
                deliver_claim(&repo, &im, claim, &cancel)
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occurrence_hash_is_stable_and_payload_bound() {
        let room = RoomId::new();
        let blocks = vec![Block::text("standup")];
        assert_eq!(
            request_hash(room, &blocks).unwrap(),
            request_hash(room, &blocks).unwrap()
        );
        assert_ne!(
            request_hash(room, &blocks).unwrap(),
            request_hash(room, &[Block::text("changed")]).unwrap()
        );
    }

    #[test]
    fn occurrence_retry_backoff_is_exponential_and_capped() {
        assert_eq!(retry_backoff(1), time::Duration::seconds(5));
        assert_eq!(retry_backoff(2), time::Duration::seconds(10));
        assert_eq!(retry_backoff(3), time::Duration::seconds(20));
        assert_eq!(retry_backoff(99), time::Duration::seconds(3600));
    }

    #[test]
    fn permanent_access_errors_do_not_retry_forever() {
        assert!(!send_error_retryable(&AeroError::Forbidden(
            "membership revoked".into()
        )));
        assert!(!send_error_retryable(&AeroError::Invalid(
            "malformed payload".into()
        )));
        assert!(send_error_retryable(&AeroError::RateLimited));
        assert!(send_error_retryable(&AeroError::Upstream("down".into())));
    }
}
