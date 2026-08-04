//! Scheduled messages + reminders ("Send later").
//!
//! A user composes a message now and it is delivered to a room later (Slack's
//! "Send later"); a reminder is just a self-targeted scheduled note. Thin
//! handlers — persistence + the atomic due-claim live in
//! [`ScheduledRepo`](aero_storage::ScheduledRepo); actual delivery replays the
//! message through sender-idempotent
//! [`ImService::send_message_idempotent`](aero_im_core::ImService::send_message_idempotent)
//! so every normal invariant holds at delivery time and a process crash between
//! message commit and scheduled-row confirmation cannot duplicate the message.
//!
//! Mounted via [`routes`] and `.merge`d into the main router, mirroring
//! [`crate::workspaces`]. The background [`run_scheduled_dispatcher`] polls due
//! rows.

use std::str::FromStr;
use std::time::Duration;

use aero_auth::AuthUser;
use aero_common::{metrics, Block, Error as AeroError, MessageId, RoomId, ScheduledMessageId};
use aero_storage::{
    scheduled::{ScheduledDeliveryClaim, ScheduledFailureDisposition},
    ScheduledRepo,
};
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};
use futures::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::error::ApiResult;
use crate::state::AppState;

const SCHEDULED_DELIVERY_OUTCOMES_TOTAL: &str = "aero_scheduled_delivery_outcomes_total";
const SCHEDULED_DELIVERY_DURATION_SECONDS: &str = "aero_scheduled_delivery_duration_seconds";

const SCHEDULED_MESSAGE_NAMESPACE: uuid::Uuid = uuid::Uuid::from_bytes([
    0x45, 0x9e, 0x0f, 0x8b, 0x91, 0x6f, 0x4f, 0xa8, 0xa2, 0x19, 0x72, 0x90, 0x79, 0x93, 0x3c, 0x4d,
]);

#[derive(Debug, Clone, Copy)]
struct DispatcherConfig {
    poll: Duration,
    batch: i64,
    concurrency: usize,
    lease_secs: i64,
    max_attempts: i32,
    send_timeout: Duration,
    base_backoff_secs: i64,
    max_backoff_secs: i64,
}

impl DispatcherConfig {
    fn from_env() -> Self {
        let poll_ms = env_u64("AERO__SERVER__SCHEDULED_POLL_MS", 10_000).max(10);
        let concurrency = env_u64("AERO__SERVER__SCHEDULED_CONCURRENCY", 8).clamp(1, 64) as usize;
        let send_timeout_secs =
            env_u64("AERO__SERVER__SCHEDULED_SEND_TIMEOUT_SECS", 60).clamp(1, 600);
        let lease_secs = env_u64("AERO__SERVER__SCHEDULED_LEASE_SECS", 300)
            .max(send_timeout_secs + 30)
            .clamp(31, 3600);
        let requested_batch = env_u64("AERO__SERVER__SCHEDULED_BATCH", 32).clamp(1, 500);
        // All claimed work must start before its lease expires. Bound the batch
        // to the number of timeout-sized waves that fit inside the lease.
        let waves = ((lease_secs.saturating_sub(30)) / send_timeout_secs).max(1);
        let safe_batch = (concurrency as u64).saturating_mul(waves).max(1);
        Self {
            poll: Duration::from_millis(poll_ms),
            batch: i64::try_from(requested_batch.min(safe_batch)).unwrap_or(500),
            concurrency,
            lease_secs: i64::try_from(lease_secs).unwrap_or(3600),
            max_attempts: env_u64("AERO__SERVER__SCHEDULED_MAX_ATTEMPTS", 8).clamp(1, 100) as i32,
            send_timeout: Duration::from_secs(send_timeout_secs),
            base_backoff_secs: i64::try_from(
                env_u64("AERO__SERVER__SCHEDULED_BACKOFF_BASE_SECS", 5).clamp(1, 3600),
            )
            .unwrap_or(5),
            max_backoff_secs: i64::try_from(
                env_u64("AERO__SERVER__SCHEDULED_BACKOFF_MAX_SECS", 3600).clamp(1, 86_400),
            )
            .unwrap_or(3600),
        }
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[derive(Serialize)]
struct ScheduledRequestFingerprint<'a> {
    version: u8,
    room_id: RoomId,
    blocks: &'a [Block],
    reply_to: Option<MessageId>,
    expires_after_secs: Option<u64>,
}

fn scheduled_client_message_id(id: ScheduledMessageId) -> uuid::Uuid {
    uuid::Uuid::new_v5(&SCHEDULED_MESSAGE_NAMESPACE, id.to_uuid().as_bytes())
}

fn scheduled_request_hash(claim: &ScheduledDeliveryClaim) -> Result<[u8; 32], AeroError> {
    let canonical = serde_json::to_vec(&ScheduledRequestFingerprint {
        version: 1,
        room_id: claim.message.room_id,
        blocks: &claim.message.blocks,
        reply_to: claim.message.reply_to,
        expires_after_secs: None,
    })?;
    Ok(Sha256::digest(canonical).into())
}

fn retry_backoff(config: DispatcherConfig, attempt: i32) -> time::Duration {
    let exponent = u32::try_from(attempt.saturating_sub(1))
        .unwrap_or(0)
        .min(20);
    let multiplier = 1_i64.checked_shl(exponent).unwrap_or(i64::MAX);
    time::Duration::seconds(
        config
            .base_backoff_secs
            .saturating_mul(multiplier)
            .min(config.max_backoff_secs.max(config.base_backoff_secs)),
    )
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

fn record_outcome(outcome: &'static str) {
    metrics::inc_counter_labeled(
        SCHEDULED_DELIVERY_OUTCOMES_TOTAL,
        1,
        &[("outcome", outcome)],
    );
}

/// All scheduled-message routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/rooms/:id/scheduled",
            post(create_scheduled).get(list_scheduled),
        )
        .route(
            "/api/rooms/:room/scheduled/:id",
            axum::routing::patch(update_scheduled_in_room)
                .post(retry_scheduled_in_room)
                .delete(cancel_scheduled_in_room),
        )
        .route("/api/scheduled", axum::routing::get(list_all_scheduled))
        .route(
            "/api/scheduled/:id",
            axum::routing::patch(update_scheduled)
                .post(retry_scheduled)
                .delete(cancel_scheduled),
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
    blocks: serde_json::Value,
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

    let blocks = crate::deferred_blocks::decode(req.blocks)?;
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
        .create_authorized(
            room,
            auth.participant_id,
            &blocks,
            reply_to,
            req.scheduled_at,
        )
        .await?;
    Ok(Json(serde_json::json!({
        "id": id,
        "room_id": room,
        "scheduled_at": req.scheduled_at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
    })))
}

/// `GET /api/rooms/:id/scheduled` — the caller's actionable scheduled
/// deliveries for the room. Pending/retrying, claimed, and dead rows remain
/// visible with attempts and `last_error`; delivered/canceled rows are omitted.
/// Requires room access.
async fn list_scheduled(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let pending = repo(&s)
        .list_actionable_authorized(auth.participant_id, Some(room))
        .await?;
    Ok(Json(
        serde_json::to_value(pending).map_err(AeroError::from)?,
    ))
}

/// `GET /api/scheduled` — all actionable deliveries owned by the caller,
/// filtered to rooms the caller can still enter. Revoked-room payloads are not
/// returned even though their immutable owner id still matches.
async fn list_all_scheduled(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let actionable = repo(&s)
        .list_actionable_authorized(auth.participant_id, None)
        .await?;
    Ok(Json(
        serde_json::to_value(actionable).map_err(AeroError::from)?,
    ))
}

#[derive(Deserialize)]
struct UpdateScheduledReq {
    blocks: serde_json::Value,
    #[serde(default)]
    reply_to: Option<String>,
    /// New delivery time, RFC 3339. Rejected if in the past (mirroring create).
    #[serde(with = "time::serde::rfc3339")]
    scheduled_at: time::OffsetDateTime,
}

/// `PATCH /api/scheduled/:id` — edit one of the caller's own still-pending
/// scheduled messages (replace blocks + `reply_to` + delivery time). Sender-scoped:
/// a 404 if it isn't the caller's unattempted pending message (delivery already
/// started, delivered/canceled, someone else's, or unknown). Rejects an empty
/// body or a past `scheduled_at`.
async fn update_scheduled(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<UpdateScheduledReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = ScheduledMessageId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("scheduled message id: {e}")))?;
    let blocks = crate::deferred_blocks::decode(req.blocks)?;
    if req.scheduled_at <= time::OffsetDateTime::now_utc() {
        return Err(AeroError::Invalid("scheduled_at must be in the future".into()).into());
    }
    let reply_to = match req.reply_to.as_deref() {
        Some(r) => Some(
            MessageId::from_str(r).map_err(|e| AeroError::Invalid(format!("reply_to id: {e}")))?,
        ),
        None => None,
    };
    repo(&s)
        .update_authorized(
            None,
            id,
            auth.participant_id,
            req.scheduled_at,
            &blocks,
            reply_to,
        )
        .await?;
    Ok(Json(serde_json::json!({
        "id": id,
        "updated": true,
        "scheduled_at": req.scheduled_at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
    })))
}

/// Canonical room-scoped edit route. The preliminary service guard preserves
/// the gateway's standard response shape; storage repeats it under the write
/// transaction and binds the opaque scheduled id to this exact path room.
async fn update_scheduled_in_room(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((room_str, id_str)): Path<(String, String)>,
    Json(req): Json<UpdateScheduledReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let id = ScheduledMessageId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("scheduled message id: {e}")))?;
    let blocks = crate::deferred_blocks::decode(req.blocks)?;
    if req.scheduled_at <= time::OffsetDateTime::now_utc() {
        return Err(AeroError::Invalid("scheduled_at must be in the future".into()).into());
    }
    let reply_to = match req.reply_to.as_deref() {
        Some(raw) => Some(
            MessageId::from_str(raw)
                .map_err(|e| AeroError::Invalid(format!("reply_to id: {e}")))?,
        ),
        None => None,
    };
    repo(&s)
        .update_authorized(
            Some(room),
            id,
            auth.participant_id,
            req.scheduled_at,
            &blocks,
            reply_to,
        )
        .await?;
    Ok(Json(serde_json::json!({
        "id": id,
        "room_id": room,
        "updated": true,
        "scheduled_at": req.scheduled_at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
    })))
}

/// `DELETE /api/scheduled/:id` — cancel one of the caller's own unclaimed
/// scheduled deliveries, including a retry/backoff or dead row. Sender-scoped:
/// a 404 while a worker owns the lease, or when delivered/canceled, someone
/// else's, or unknown.
async fn cancel_scheduled(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = ScheduledMessageId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("scheduled message id: {e}")))?;
    repo(&s)
        .cancel_authorized(None, id, auth.participant_id)
        .await?;
    Ok(Json(serde_json::json!({ "canceled": true })))
}

async fn cancel_scheduled_in_room(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((room_str, id_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let id = ScheduledMessageId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("scheduled message id: {e}")))?;
    repo(&s)
        .cancel_authorized(Some(room), id, auth.participant_id)
        .await?;
    Ok(Json(
        serde_json::json!({ "room_id": room, "canceled": true }),
    ))
}

/// `POST /api/scheduled/:id` — explicitly requeue an owner-visible dead
/// delivery. The payload remains immutable and the sender idempotency key is
/// retained; only the bounded-attempt generation is reset.
async fn retry_scheduled(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = ScheduledMessageId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("scheduled message id: {e}")))?;
    repo(&s)
        .retry_dead_authorized(
            None,
            id,
            auth.participant_id,
            time::OffsetDateTime::now_utc(),
        )
        .await?;
    Ok(Json(serde_json::json!({ "retried": true })))
}

async fn retry_scheduled_in_room(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((room_str, id_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let id = ScheduledMessageId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("scheduled message id: {e}")))?;
    repo(&s)
        .retry_dead_authorized(
            Some(room),
            id,
            auth.participant_id,
            time::OffsetDateTime::now_utc(),
        )
        .await?;
    Ok(Json(
        serde_json::json!({ "room_id": room, "retried": true }),
    ))
}

async fn deliver_claim(
    state: &AppState,
    sched: &ScheduledRepo,
    config: DispatcherConfig,
    claim: ScheduledDeliveryClaim,
    cancel: &CancellationToken,
) {
    let started = std::time::Instant::now();
    let id = claim.message.id;
    let request_hash = match scheduled_request_hash(&claim) {
        Ok(hash) => hash,
        Err(error) => {
            settle_failure(sched, config, &claim, &error.to_string(), false).await;
            return;
        }
    };
    let client_message_id = scheduled_client_message_id(id);
    let send = state.im.send_message_idempotent(
        claim.message.sender_id,
        claim.message.room_id,
        claim.message.blocks.clone(),
        claim.message.reply_to,
        None,
        client_message_id,
        request_hash,
    );
    let result = tokio::select! {
        biased;
        () = cancel.cancelled() => Err(("dispatcher shutdown".to_owned(), true)),
        result = tokio::time::timeout(config.send_timeout, send) => match result {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => {
                let retryable = send_error_retryable(&error);
                Err((error.to_string(), retryable))
            }
            Err(_) => Err((
                format!("send timed out after {}s", config.send_timeout.as_secs()),
                true,
            )),
        }
    };

    match result {
        Ok(()) => {
            match sched
                .confirm_delivered(
                    id,
                    claim.claim_token,
                    claim.attempt,
                    claim.generation,
                    time::OffsetDateTime::now_utc(),
                )
                .await
            {
                Ok(true) => record_outcome("delivered"),
                Ok(false) => {
                    record_outcome("fence_lost");
                    tracing::warn!(
                        scheduled_id = %id,
                        attempt = claim.attempt,
                        "scheduled dispatcher: success confirmation lost claim fence"
                    );
                }
                Err(error) => {
                    // Leave the claim leased. After expiry, a retry uses the
                    // stable sender key and resolves the already-committed
                    // canonical message before confirming this row.
                    record_outcome("confirm_error");
                    tracing::warn!(
                        ?error,
                        scheduled_id = %id,
                        attempt = claim.attempt,
                        "scheduled dispatcher: success confirmation failed; lease will be reclaimed"
                    );
                }
            }
        }
        Err((error, retryable)) => {
            settle_failure(sched, config, &claim, &error, retryable).await;
        }
    }
    metrics::observe_histogram_labeled(
        SCHEDULED_DELIVERY_DURATION_SECONDS,
        started.elapsed().as_secs_f64(),
        &[("stage", "attempt")],
    );
}

async fn settle_failure(
    sched: &ScheduledRepo,
    config: DispatcherConfig,
    claim: &ScheduledDeliveryClaim,
    error: &str,
    retryable: bool,
) {
    let retry_at = time::OffsetDateTime::now_utc() + retry_backoff(config, claim.attempt);
    match sched
        .record_failure(
            claim.message.id,
            claim.claim_token,
            claim.attempt,
            claim.generation,
            error,
            retry_at,
            retryable,
            config.max_attempts,
        )
        .await
    {
        Ok(ScheduledFailureDisposition::RetryScheduled) => {
            record_outcome("retry");
            tracing::warn!(
                scheduled_id = %claim.message.id,
                attempt = claim.attempt,
                retry_at = %retry_at,
                error,
                "scheduled dispatcher: transient send failed; retry parked"
            );
        }
        Ok(ScheduledFailureDisposition::Dead) => {
            record_outcome("dead");
            tracing::error!(
                scheduled_id = %claim.message.id,
                attempt = claim.attempt,
                retryable,
                error,
                "scheduled dispatcher: delivery moved to dead-letter state"
            );
        }
        Ok(ScheduledFailureDisposition::FenceLost) => {
            record_outcome("fence_lost");
            tracing::warn!(
                scheduled_id = %claim.message.id,
                attempt = claim.attempt,
                "scheduled dispatcher: failure settlement lost claim fence"
            );
        }
        Err(settle_error) => {
            // Do not pretend the attempt was settled. The lease is retained and
            // can be reclaimed after expiry.
            record_outcome("settle_error");
            tracing::warn!(
                error = ?settle_error,
                scheduled_id = %claim.message.id,
                attempt = claim.attempt,
                "scheduled dispatcher: failure settlement failed; lease will be reclaimed"
            );
        }
    }
}

/// Leased, multi-replica scheduled-message delivery loop. The enclosing boot
/// [`TaskTracker`](tokio_util::task::TaskTracker) tracks this future; cancellation
/// stops new claims and any interrupted idempotent attempt is immediately
/// re-parked (or reclaimed after its lease if settlement itself fails).
pub async fn run_scheduled_dispatcher(state: AppState, cancel: CancellationToken) {
    let config = DispatcherConfig::from_env();
    let sched = repo(&state);
    let mut tick = tokio::time::interval(config.poll);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tracing::info!(
        interval_ms = config.poll.as_millis(),
        batch = config.batch,
        concurrency = config.concurrency,
        lease_secs = config.lease_secs,
        max_attempts = config.max_attempts,
        "scheduled-message dispatcher started"
    );
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                tracing::info!("scheduled-message dispatcher: shutdown signal received, exiting");
                break;
            }
            _ = tick.tick() => {
                let now = time::OffsetDateTime::now_utc();
                let due = match sched
                    .claim_due_with_policy(
                        now,
                        config.batch,
                        config.lease_secs,
                        config.max_attempts,
                    )
                    .await
                {
                    Ok(d) => d,
                    Err(e) => {
                        record_outcome("claim_error");
                        tracing::warn!(error = ?e, "scheduled dispatcher: claim_due failed");
                        continue;
                    }
                };
                if due.is_empty() {
                    continue;
                }
                tracing::debug!(count = due.len(), "scheduled dispatcher: delivering due messages");
                stream::iter(due)
                    .for_each_concurrent(config.concurrency, |claim| {
                        deliver_claim(&state, &sched, config, claim, &cancel)
                    })
                    .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::ParticipantId;

    fn claim(id: ScheduledMessageId, blocks: Vec<Block>) -> ScheduledDeliveryClaim {
        ScheduledDeliveryClaim {
            message: aero_storage::ScheduledMessage {
                id,
                room_id: RoomId::from_uuid(
                    uuid::Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap(),
                ),
                sender_id: ParticipantId::new(),
                blocks,
                reply_to: None,
                scheduled_at: time::OffsetDateTime::UNIX_EPOCH,
                created_at: time::OffsetDateTime::UNIX_EPOCH,
                delivered_at: None,
                canceled_at: None,
                delivery_status: "claimed".into(),
                attempts: 1,
                delivery_generation: 0,
                available_at: time::OffsetDateTime::UNIX_EPOCH,
                last_error: None,
                dead_at: None,
            },
            claim_token: uuid::Uuid::new_v4(),
            attempt: 1,
            generation: 0,
            lease_expires_at: time::OffsetDateTime::UNIX_EPOCH + time::Duration::minutes(5),
        }
    }

    #[test]
    fn stable_client_key_and_payload_hash_close_crash_retry_window() {
        let id = ScheduledMessageId::from_uuid(
            uuid::Uuid::parse_str("018f0f8b-916f-7fa8-a219-729079933c4d").unwrap(),
        );
        let first = claim(id, vec![Block::text("hello")]);
        let second = first.clone();
        assert_eq!(
            scheduled_client_message_id(first.message.id),
            scheduled_client_message_id(second.message.id)
        );
        assert_eq!(
            scheduled_request_hash(&first).unwrap(),
            scheduled_request_hash(&second).unwrap()
        );
        let changed = claim(id, vec![Block::text("changed")]);
        assert_ne!(
            scheduled_request_hash(&first).unwrap(),
            scheduled_request_hash(&changed).unwrap()
        );
    }

    #[test]
    fn retry_backoff_is_exponential_and_capped() {
        let config = DispatcherConfig {
            poll: Duration::from_secs(1),
            batch: 1,
            concurrency: 1,
            lease_secs: 300,
            max_attempts: 8,
            send_timeout: Duration::from_secs(10),
            base_backoff_secs: 5,
            max_backoff_secs: 60,
        };
        assert_eq!(retry_backoff(config, 1), time::Duration::seconds(5));
        assert_eq!(retry_backoff(config, 2), time::Duration::seconds(10));
        assert_eq!(retry_backoff(config, 3), time::Duration::seconds(20));
        assert_eq!(retry_backoff(config, 99), time::Duration::seconds(60));
    }

    #[test]
    fn only_transient_send_errors_are_retried() {
        assert!(send_error_retryable(&AeroError::RateLimited));
        assert!(send_error_retryable(&AeroError::Upstream("down".into())));
        assert!(!send_error_retryable(&AeroError::Forbidden(
            "removed".into()
        )));
        assert!(!send_error_retryable(&AeroError::Invalid("bad".into())));
    }
}
