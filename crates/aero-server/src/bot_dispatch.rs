//! Reliable bot event-subscription delivery.
//!
//! The durable NATS consumer performs no network I/O. It lifts the producer's
//! stable `event_id` from the raw JSON, applies subscription filters and tenant
//! authorization, then atomically commits every `(subscription,event_id)` queue
//! row together with its consumer receipt. A separate leased worker signs and
//! sends the exact stored bytes, retrying failures with exponential backoff and
//! parking exhausted work in a durable DLQ.

use std::{collections::HashMap, sync::Arc, time::Duration as StdDuration};

use aero_common::{metrics, Error as AeroError, ParticipantId, RoomEvent, RoomId, WorkspaceId};
use aero_im_core::ImService;
use aero_storage::webhook::build_delivery_from_bytes;
use aero_storage::{
    BotDeliveryOutbox, BotDeliveryOutboxRepo, BotRepo, ConsumerEventReceiptRepo, DeliveryStatus,
    PgPool, ReqwestSender, RoomRepo, WebhookSender, MAX_BOT_EVENT_CANDIDATES,
};
use anyhow::{anyhow, Context};
use futures::{stream, StreamExt};
use time::{Duration, OffsetDateTime};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    state::AppState,
    task_shutdown::{self, NextOrCancelled},
};

/// Durable consumer name — distinct from every other bus listener so the
/// dispatcher resumes from its own committed cursor and doesn't compete with the
/// WS / webhook / built-in-bot consumers for one shared cursor.
const CONSUMER: &str = "aero-bot-dispatch";
const DELIVERY_LEASE: Duration = Duration::minutes(2);
const CLAIM_BATCH: i64 = 32;
const DELIVERY_CONCURRENCY: usize = 8;
const WORKER_IDLE: StdDuration = StdDuration::from_millis(250);
const DEFAULT_DELIVERED_RETENTION_DAYS: i64 = 30;
const DEFAULT_DLQ_RETENTION_DAYS: i64 = 90;
const DEFAULT_SWEEP_SECS: u64 = 3_600;
/// Events whose scoped external-bot candidate set exceeded the hard cap.
pub const BOT_CANDIDATE_TRUNCATIONS_TOTAL: &str = "aero_bot_candidate_truncations_total";

/// The canonical event-type string a bot subscribes to, matching the
/// `#[serde(tag = "kind", rename_all = "snake_case")]` discriminant on
/// [`RoomEvent`] (so it equals the value in the delivered JSON's `kind` field, and
/// the value `webhooks::event_kind` uses). Pure — unit-tested against the wire tag.
#[must_use]
fn event_type(event: &RoomEvent) -> &'static str {
    match event {
        RoomEvent::Message(_) => "message",
        RoomEvent::Edited(_) => "edited",
        RoomEvent::Recalled(_) => "recalled",
        RoomEvent::Deleted { .. } => "deleted",
        RoomEvent::Reaction { .. } => "reaction",
        RoomEvent::Read { .. } => "read",
        RoomEvent::Typing { .. } => "typing",
        RoomEvent::Notify { .. } | RoomEvent::NotifyBatch { .. } => "notify",
        RoomEvent::Pin { .. } => "pin",
        RoomEvent::Membership { .. } => "membership",
        RoomEvent::Poll { .. } => "poll",
        RoomEvent::CanvasOp { .. } => "canvas_op",
        RoomEvent::MessageSeen { .. } => "message_seen",
        RoomEvent::Interaction { .. } => "interaction",
        RoomEvent::Call(_) => "call",
    }
}

/// The `action_id` an event carries, when it has one (only `Interaction` does
/// today). Used to honor a subscription's `{"action_id": "..."}` filter. Pure.
#[must_use]
fn event_action_id(event: &RoomEvent) -> Option<&str> {
    match event {
        RoomEvent::Interaction { action_id, .. } => Some(action_id.as_str()),
        _ => None,
    }
}

/// Whether one subscription's `filters` JSON matches an event, given the event's
/// resolved `room_id`, its `workspace_id` (looked up lazily — `None` when unknown /
/// not yet resolved), and its `action_id`.
///
/// Each present filter key is an AND-constraint: a `room_id`/`workspace_id`/
/// `action_id` in the filter must equal the event's. An absent key (or the empty
/// `{}` default) constrains nothing. A filter that names a dimension the event
/// lacks (e.g. `workspace_id` when the event's workspace is unknown, or `action_id`
/// on a non-interaction event) does NOT match — the subscriber asked to be narrowed
/// to something this event can't satisfy. A non-object `filters` value is
/// malformed and fails closed; interpreting it as an empty filter would turn a
/// bad legacy row into a cluster-wide subscription. Pure + total, so it's
/// exhaustively unit-tested without a DB.
#[must_use]
fn filter_matches(
    filters: &serde_json::Value,
    room_id: RoomId,
    workspace_id: Option<WorkspaceId>,
    action_id: Option<&str>,
) -> bool {
    let Some(obj) = filters.as_object() else {
        return false;
    };

    if let Some(value) = obj.get("room_id") {
        let Some(want) = value.as_str() else {
            return false;
        };
        if want != room_id.to_string() {
            return false;
        }
    }
    if let Some(value) = obj.get("workspace_id") {
        let Some(want) = value.as_str() else {
            return false;
        };
        // The subscriber narrowed to a workspace; if we couldn't resolve the
        // event's workspace, we can't honor that, so it does not match.
        match workspace_id {
            Some(ws) if want == ws.to_string() => {}
            _ => return false,
        }
    }
    if let Some(value) = obj.get("action_id") {
        let Some(want) = value.as_str() else {
            return false;
        };
        match action_id {
            Some(got) if want == got => {}
            _ => return false,
        }
    }
    true
}

/// Run the bot-subscription dispatcher with the real reqwest transport, until the
/// process exits. The public entry point spawned at boot.
///
/// # Errors
/// Propagates only a fatal setup error; the steady-state loop never returns `Err`
/// (it resubscribes on bus failures, like the other built-in bots).
pub async fn run(state: AppState) -> anyhow::Result<()> {
    run_until_cancelled(state, CancellationToken::new()).await
}

/// Run with the production transport until `cancel` is triggered.
pub async fn run_until_cancelled(state: AppState, cancel: CancellationToken) -> anyhow::Result<()> {
    run_with_until_cancelled(state, Arc::new(ReqwestSender::new()), cancel).await
}

/// Run the dispatcher with an injected [`WebhookSender`].
pub async fn run_with(state: AppState, sender: Arc<dyn WebhookSender>) -> anyhow::Result<()> {
    run_with_until_cancelled(state, sender, CancellationToken::new()).await
}

/// Run the atomic bus materializer and independent HTTP worker together.
pub async fn run_with_until_cancelled(
    state: AppState,
    sender: Arc<dyn WebhookSender>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let ingest = run_ingest(state.clone(), cancel.clone());
    let worker = run_delivery_worker(state, sender, cancel);
    tokio::try_join!(ingest, worker)?;
    Ok(())
}

async fn run_ingest(state: AppState, cancel: CancellationToken) -> anyhow::Result<()> {
    let bus = state.bus.clone();
    let bots = BotRepo::new(state.pg.clone());
    let rooms = RoomRepo::new(state.pg.clone());
    let receipts = ConsumerEventReceiptRepo::new(state.pg.clone());
    loop {
        let subscribed =
            task_shutdown::subscribe_or_cancelled(&bus, "im.room.*", Some(CONSUMER), &cancel).await;
        let mut stream = match subscribed {
            None => return Ok(()),
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                warn!(error = %e, "bot_dispatch subscribe failed; retrying");
                if task_shutdown::delay_or_cancelled(std::time::Duration::from_secs(1), &cancel)
                    .await
                {
                    return Ok(());
                }
                continue;
            }
        };
        info!("bot_dispatch listener started");
        loop {
            let sub = match task_shutdown::next_or_cancelled(&mut stream, &cancel).await {
                NextOrCancelled::Item(sub) => sub,
                NextOrCancelled::Ended => break,
                NextOrCancelled::Cancelled => return Ok(()),
            };
            // Lift producer metadata before typed serde drops unknown envelope
            // fields. The same immutable bytes are persisted for every retry.
            let payload = sub.payload().to_vec();
            let producer_event_id = crate::consumer_event_receipt::extract_event_id(&payload);
            let event = serde_json::from_slice::<RoomEvent>(&payload);
            let im = state.im.as_ref();
            let handler_bots = &bots;
            let handler_rooms = &rooms;
            let handler_pool = &state.pg;
            if producer_event_id.is_none() {
                debug!("bot_dispatch: legacy event has no producer event_id");
            }
            let _ = crate::consumer_event_receipt::process_atomic(
                &receipts,
                CONSUMER,
                sub,
                |event_id, receipt_attempts| async move {
                    match event {
                        Ok(event) => {
                            materialize_deliveries(
                                im,
                                handler_bots,
                                handler_rooms,
                                handler_pool,
                                event_id,
                                receipt_attempts,
                                &payload,
                                &event,
                            )
                            .await?;
                        }
                        Err(error) => {
                            debug!(%error, %event_id, "bot_dispatch: undecodable event payload skipped");
                            complete_receipt_only(
                                handler_pool,
                                event_id,
                                receipt_attempts,
                            )
                            .await?;
                        }
                    }
                    Ok(())
                },
            )
            .await;
        }
        if cancel.is_cancelled() {
            return Ok(());
        }
        warn!("bot_dispatch subscription stream ended; resubscribing");
        if task_shutdown::delay_or_cancelled(std::time::Duration::from_secs(1), &cancel).await {
            return Ok(());
        }
    }
}

/// Resolve the complete fan-out, then commit it together with the receipt.
#[allow(clippy::too_many_arguments)]
async fn materialize_deliveries(
    im: &ImService,
    bots: &BotRepo,
    rooms: &RoomRepo,
    pool: &PgPool,
    event_id: Uuid,
    receipt_attempts: i32,
    raw_payload: &[u8],
    event: &RoomEvent,
) -> anyhow::Result<usize> {
    let Some(room) = event.room_id() else {
        complete_receipt_only(pool, event_id, receipt_attempts).await?;
        return Ok(0);
    };
    let kind = event_type(event);

    // Resolve scope before querying so PostgreSQL can use the indexed generated
    // room/workspace projections instead of fetching every tenant's event-type
    // subscription and filtering the global candidate set in process memory.
    let workspace = rooms
        .room_workspace(room)
        .await
        .context("resolve event room workspace")?;
    let (subs, candidates_truncated) = bots
        .subscriptions_for_event(kind, room, workspace)
        .await
        .context("lookup scoped bot event subscriptions")?;
    if candidates_truncated {
        metrics::inc_counter(BOT_CANDIDATE_TRUNCATIONS_TOTAL, 1);
        warn!(
            event_type = kind,
            %room,
            ?workspace,
            max_candidates = MAX_BOT_EVENT_CANDIDATES,
            "bot subscription candidate set exceeded hard cap; fan-out truncated"
        );
    }

    let action_id = event_action_id(event);
    let mut owner_access = HashMap::<ParticipantId, bool>::new();
    let mut matches = Vec::new();

    for sub in subs {
        if !filter_matches(&sub.filters, room, workspace, action_id) {
            continue;
        }
        let authorized = if let Some(authorized) = owner_access.get(&sub.owner_id) {
            *authorized
        } else {
            let authorized = match im.assert_room_access(sub.owner_id, room).await {
                Ok(()) => true,
                Err(error) if is_access_revoked(&error) => {
                    debug!(
                        owner = %sub.owner_id,
                        bot = %sub.bot_id,
                        %room,
                        %error,
                        "bot_dispatch owner no longer has room access; skipping external delivery"
                    );
                    false
                }
                Err(error) => {
                    return Err(
                        anyhow::Error::new(error).context("re-authorize bot subscription owner")
                    );
                }
            };
            owner_access.insert(sub.owner_id, authorized);
            authorized
        };
        if !authorized {
            continue;
        }
        matches.push((sub.id, sub.bot_id));
    }

    let count = matches.len();
    let mut tx = pool
        .begin()
        .await
        .context("begin bot delivery materialization")?;
    for (subscription_id, bot_id) in matches {
        BotDeliveryOutboxRepo::enqueue_in_tx(
            &mut tx,
            subscription_id,
            bot_id,
            event_id,
            kind,
            room,
            raw_payload,
        )
        .await
        .context("enqueue bot subscription delivery")?;
    }
    complete_receipt_in_tx(&mut tx, event_id, receipt_attempts).await?;
    tx.commit()
        .await
        .context("commit bot delivery materialization")?;
    debug!(%event_id, %room, kind, count, "bot subscription deliveries materialized");
    Ok(count)
}

fn is_access_revoked(error: &AeroError) -> bool {
    matches!(
        error,
        AeroError::Forbidden(_) | AeroError::NotFound(_) | AeroError::Unauthorized(_)
    )
}

async fn complete_receipt_only(
    pool: &PgPool,
    event_id: Uuid,
    receipt_attempts: i32,
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await.context("begin bot receipt completion")?;
    complete_receipt_in_tx(&mut tx, event_id, receipt_attempts).await?;
    tx.commit().await.context("commit bot receipt completion")
}

async fn complete_receipt_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event_id: Uuid,
    receipt_attempts: i32,
) -> anyhow::Result<()> {
    let completed = ConsumerEventReceiptRepo::complete_in_tx(
        tx,
        CONSUMER,
        event_id,
        receipt_attempts,
        OffsetDateTime::now_utc(),
    )
    .await
    .context("complete bot consumer receipt")?;
    if !completed {
        return Err(anyhow!("bot consumer receipt fencing token was superseded"));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RetentionConfig {
    delivered_age: Duration,
    dlq_age: Duration,
    interval: StdDuration,
}

impl RetentionConfig {
    fn from_env() -> Self {
        let delivered_days = std::env::var("AERO_BOT_DELIVERY_RETENTION_DAYS").ok();
        let dlq_days = std::env::var("AERO_BOT_DELIVERY_DLQ_RETENTION_DAYS").ok();
        let sweep_secs = std::env::var("AERO_BOT_DELIVERY_SWEEP_SECS").ok();
        Self::from_values(
            delivered_days.as_deref(),
            dlq_days.as_deref(),
            sweep_secs.as_deref(),
        )
    }

    fn from_values(
        delivered_days: Option<&str>,
        dlq_days: Option<&str>,
        sweep_secs: Option<&str>,
    ) -> Self {
        let delivered_days = delivered_days
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(DEFAULT_DELIVERED_RETENTION_DAYS)
            .clamp(1, 3_650);
        let dlq_days = dlq_days
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(DEFAULT_DLQ_RETENTION_DAYS)
            .clamp(1, 3_650);
        let sweep_secs = sweep_secs
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_SWEEP_SECS)
            .clamp(60, 86_400);
        Self {
            delivered_age: Duration::days(delivered_days),
            dlq_age: Duration::days(dlq_days),
            interval: StdDuration::from_secs(sweep_secs),
        }
    }

    fn cutoffs(self, now: OffsetDateTime) -> (OffsetDateTime, OffsetDateTime) {
        (now - self.delivered_age, now - self.dlq_age)
    }
}

async fn run_delivery_worker(
    state: AppState,
    sender: Arc<dyn WebhookSender>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let bots = BotRepo::new(state.pg.clone());
    let outbox = BotDeliveryOutboxRepo::new(state.pg.clone());
    let retention = RetentionConfig::from_env();
    let mut next_sweep = tokio::time::Instant::now();
    info!(
        delivered_retention_days = retention.delivered_age.whole_days(),
        dlq_retention_days = retention.dlq_age.whole_days(),
        sweep_secs = retention.interval.as_secs(),
        "bot delivery worker started"
    );

    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }

        if tokio::time::Instant::now() >= next_sweep {
            let (delivered_cutoff, dlq_cutoff) = retention.cutoffs(OffsetDateTime::now_utc());
            match outbox
                .sweep_retention_before(delivered_cutoff, dlq_cutoff)
                .await
            {
                Ok(removed)
                    if removed.delivered_outbox > 0
                        || removed.dead_outbox > 0
                        || removed.attempt_history > 0 =>
                {
                    info!(
                        delivered_outbox = removed.delivered_outbox,
                        dead_outbox = removed.dead_outbox,
                        attempt_history = removed.attempt_history,
                        "swept retained bot delivery rows"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    warn!(?error, "bot delivery retention sweep failed");
                }
            }
            next_sweep = tokio::time::Instant::now() + retention.interval;
        }

        let rows = match outbox
            .claim_due(OffsetDateTime::now_utc(), DELIVERY_LEASE, CLAIM_BATCH)
            .await
        {
            Ok(rows) => rows,
            Err(error) => {
                warn!(?error, "bot delivery claim failed; retrying");
                if task_shutdown::delay_or_cancelled(StdDuration::from_secs(1), &cancel).await {
                    return Ok(());
                }
                continue;
            }
        };
        if rows.is_empty() {
            if task_shutdown::delay_or_cancelled(WORKER_IDLE, &cancel).await {
                return Ok(());
            }
            continue;
        }

        stream::iter(rows)
            .for_each_concurrent(Some(DELIVERY_CONCURRENCY), |row| {
                deliver_claimed(
                    state.im.as_ref(),
                    &bots,
                    &outbox,
                    sender.as_ref(),
                    &cancel,
                    row,
                )
            })
            .await;
    }
}

async fn deliver_claimed(
    im: &ImService,
    bots: &BotRepo,
    outbox: &BotDeliveryOutboxRepo,
    sender: &dyn WebhookSender,
    cancel: &CancellationToken,
    row: BotDeliveryOutbox,
) {
    let Some(claim_token) = row.claim_token else {
        warn!(delivery = %row.id, "claimed bot delivery has no fencing token");
        return;
    };

    if cancel.is_cancelled() {
        release_interrupted(outbox, &row, claim_token).await;
        return;
    }

    let target = match bots.subscription_delivery_target(row.subscription_id).await {
        Ok(Some(target)) => target,
        Ok(None) => {
            park_dead(
                outbox,
                &row,
                claim_token,
                "subscription target is no longer active",
            )
            .await;
            return;
        }
        Err(error) => {
            repark_failed(
                outbox,
                &row,
                claim_token,
                None,
                &format!("subscription target lookup failed: {error}"),
            )
            .await;
            return;
        }
    };

    if target.bot_id != row.bot_id {
        park_dead(
            outbox,
            &row,
            claim_token,
            "subscription bot identity changed",
        )
        .await;
        return;
    }

    match im.assert_room_access(target.owner_id, row.room_id).await {
        Ok(()) => {}
        Err(error) if is_access_revoked(&error) => {
            park_dead(
                outbox,
                &row,
                claim_token,
                &format!("subscription owner access revoked: {error}"),
            )
            .await;
            return;
        }
        Err(error) => {
            repark_failed(
                outbox,
                &row,
                claim_token,
                None,
                &format!("subscription owner authorization failed: {error}"),
            )
            .await;
            return;
        }
    }

    if cancel.is_cancelled() {
        release_interrupted(outbox, &row, claim_token).await;
        return;
    }

    let headers = [("Content-Type".to_owned(), "application/json".to_owned())];
    let delivery = build_delivery_from_bytes(
        &target.webhook_url,
        &target.webhook_secret,
        &row.request_body,
        &headers,
        OffsetDateTime::now_utc().unix_timestamp(),
    );

    match sender.deliver(&delivery).await {
        Ok(response) if (200..300).contains(&response.status) => {
            record_attempt(
                bots,
                &row,
                DeliveryStatus::Delivered,
                Some(response.status),
                None,
            )
            .await;
            match outbox
                .mark_delivered(
                    row.id,
                    claim_token,
                    OffsetDateTime::now_utc(),
                    response.status,
                )
                .await
            {
                Ok(true) => debug!(
                    delivery = %row.id,
                    event_id = %row.event_id,
                    subscription = %row.subscription_id,
                    status = response.status,
                    attempts = row.attempts,
                    "bot subscription delivery completed"
                ),
                Ok(false) => warn!(
                    delivery = %row.id,
                    %claim_token,
                    "bot delivery success lost its lease fence"
                ),
                Err(error) => warn!(
                    delivery = %row.id,
                    %claim_token,
                    ?error,
                    "bot delivery succeeded but durable completion failed"
                ),
            }
        }
        Ok(response) => {
            let error = format!("non-2xx: {}", response.status);
            record_attempt(
                bots,
                &row,
                DeliveryStatus::Failed,
                Some(response.status),
                Some(&error),
            )
            .await;
            repark_failed(outbox, &row, claim_token, Some(response.status), &error).await;
        }
        Err(error) => {
            record_attempt(bots, &row, DeliveryStatus::Failed, None, Some(&error)).await;
            repark_failed(outbox, &row, claim_token, None, &error).await;
        }
    }
}

async fn record_attempt(
    bots: &BotRepo,
    row: &BotDeliveryOutbox,
    status: DeliveryStatus,
    http_status: Option<u16>,
    error: Option<&str>,
) {
    if let Err(log_error) = bots
        .record_delivery_attempt(
            row.subscription_id,
            row.bot_id,
            &row.event_type,
            status,
            http_status,
            error,
            row.attempts,
        )
        .await
    {
        // Observability is deliberately independent from durable settlement.
        warn!(
            delivery = %row.id,
            event_id = %row.event_id,
            ?log_error,
            "bot delivery attempt-log write failed"
        );
    }
}

async fn repark_failed(
    outbox: &BotDeliveryOutboxRepo,
    row: &BotDeliveryOutbox,
    claim_token: Uuid,
    http_status: Option<u16>,
    error: &str,
) {
    match outbox
        .mark_failed(
            row.id,
            claim_token,
            row.attempts,
            OffsetDateTime::now_utc(),
            http_status,
            error,
        )
        .await
    {
        Ok(Some("dead")) => warn!(
            delivery = %row.id,
            event_id = %row.event_id,
            attempts = row.attempts,
            error,
            "bot subscription delivery moved to DLQ"
        ),
        Ok(Some(_)) => warn!(
            delivery = %row.id,
            event_id = %row.event_id,
            attempts = row.attempts,
            error,
            "bot subscription delivery failed; retry scheduled"
        ),
        Ok(None) => warn!(
            delivery = %row.id,
            %claim_token,
            "bot delivery failure lost its lease fence"
        ),
        Err(mark_error) => warn!(
            delivery = %row.id,
            %claim_token,
            ?mark_error,
            error,
            "bot delivery failure could not be durably re-parked"
        ),
    }
}

async fn park_dead(
    outbox: &BotDeliveryOutboxRepo,
    row: &BotDeliveryOutbox,
    claim_token: Uuid,
    error: &str,
) {
    match outbox
        .mark_dead(row.id, claim_token, OffsetDateTime::now_utc(), error)
        .await
    {
        Ok(true) => warn!(
            delivery = %row.id,
            event_id = %row.event_id,
            error,
            "bot subscription delivery parked in DLQ without HTTP"
        ),
        Ok(false) => warn!(
            delivery = %row.id,
            %claim_token,
            "bot delivery terminal park lost its lease fence"
        ),
        Err(mark_error) => warn!(
            delivery = %row.id,
            %claim_token,
            ?mark_error,
            "bot delivery could not be parked in DLQ"
        ),
    }
}

async fn release_interrupted(
    outbox: &BotDeliveryOutboxRepo,
    row: &BotDeliveryOutbox,
    claim_token: Uuid,
) {
    match outbox
        .release_claim(row.id, claim_token, OffsetDateTime::now_utc())
        .await
    {
        Ok(true) => debug!(delivery = %row.id, "released unstarted bot delivery during shutdown"),
        Ok(false) => debug!(
            delivery = %row.id,
            %claim_token,
            "shutdown release lost its lease fence"
        ),
        Err(error) => warn!(
            delivery = %row.id,
            %claim_token,
            ?error,
            "failed to release bot delivery during shutdown; lease recovery will retry it"
        ),
    }
}

#[cfg(test)]
#[path = "bot_dispatch/tests.rs"]
mod tests;
