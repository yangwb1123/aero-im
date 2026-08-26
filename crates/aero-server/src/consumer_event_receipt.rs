//! Shared durable-consumer receipt protocol.
//!
//! A stable producer `event_id` is lifted from the raw event before the typed
//! enum decoder drops unknown fields.  Completed receipts are ACK-only; an
//! active lease or any database/handler/completion failure is left unACKed for
//! `JetStream` redelivery.

use std::{future::Future, time::Duration as StdDuration};

use aero_bus::Subscription;
use aero_storage::{ConsumerEventClaim, ConsumerEventReceiptRepo};
use time::{Duration, OffsetDateTime};
use tracing::{debug, warn};
use uuid::Uuid;

/// Must exceed `aero-bus`'s 120-second durable-consumer `ack_wait`.
const PROCESSING_LEASE: Duration = Duration::minutes(5);
/// Refresh well before expiry, leaving four minutes of safety after each success.
const LEASE_RENEW_INTERVAL: StdDuration = StdDuration::from_secs(60);
/// Do not let a wedged pool/renewal query monopolize handler polling.
const LEASE_RENEW_CALL_TIMEOUT: StdDuration = StdDuration::from_secs(10);

#[derive(Debug, serde::Deserialize)]
struct EventMetadata {
    event_id: Option<Uuid>,
}

#[async_trait::async_trait]
trait ReceiptStore: Sync {
    async fn claim(
        &self,
        consumer: &str,
        event_id: Uuid,
        now: OffsetDateTime,
        lease: Duration,
    ) -> Result<ConsumerEventClaim, sqlx::Error>;

    async fn complete(
        &self,
        consumer: &str,
        event_id: Uuid,
        attempts: i32,
        now: OffsetDateTime,
    ) -> Result<bool, sqlx::Error>;

    async fn renew(
        &self,
        consumer: &str,
        event_id: Uuid,
        attempts: i32,
        now: OffsetDateTime,
        lease: Duration,
    ) -> Result<bool, sqlx::Error>;

    async fn release(
        &self,
        consumer: &str,
        event_id: Uuid,
        attempts: i32,
        now: OffsetDateTime,
        error: &str,
    ) -> Result<bool, sqlx::Error>;
}

#[async_trait::async_trait]
impl ReceiptStore for ConsumerEventReceiptRepo {
    async fn claim(
        &self,
        consumer: &str,
        event_id: Uuid,
        now: OffsetDateTime,
        lease: Duration,
    ) -> Result<ConsumerEventClaim, sqlx::Error> {
        ConsumerEventReceiptRepo::claim(self, consumer, event_id, now, lease).await
    }

    async fn complete(
        &self,
        consumer: &str,
        event_id: Uuid,
        attempts: i32,
        now: OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        ConsumerEventReceiptRepo::complete(self, consumer, event_id, attempts, now).await
    }

    async fn renew(
        &self,
        consumer: &str,
        event_id: Uuid,
        attempts: i32,
        now: OffsetDateTime,
        lease: Duration,
    ) -> Result<bool, sqlx::Error> {
        ConsumerEventReceiptRepo::renew(self, consumer, event_id, attempts, now, lease).await
    }

    async fn release(
        &self,
        consumer: &str,
        event_id: Uuid,
        attempts: i32,
        now: OffsetDateTime,
        error: &str,
    ) -> Result<bool, sqlx::Error> {
        ConsumerEventReceiptRepo::release(self, consumer, event_id, attempts, now, error).await
    }
}

/// Poll `handler` while periodically extending its fenced processing lease.
///
/// `None` means the `(consumer,event,attempts)` fence no longer owns an
/// unexpired lease. The handler future is dropped immediately and the caller
/// must neither release nor complete that stale claim. A transient renewal
/// error is retried only while the last confirmed lease remains unexpired.
async fn run_handler_with_renewal<R, Fut>(
    repo: &R,
    consumer: &'static str,
    event_id: Uuid,
    attempts: i32,
    handler: Fut,
    lease: Duration,
    renew_interval: StdDuration,
) -> Option<anyhow::Result<()>>
where
    R: ReceiptStore + ?Sized,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let lease_seconds = lease.whole_seconds().max(1);
    let lease_span = StdDuration::from_secs(u64::try_from(lease_seconds).unwrap_or(u64::MAX));
    let now = tokio::time::Instant::now();
    let first_tick = now + renew_interval;
    let mut ticker = tokio::time::interval_at(first_tick, renew_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let lease_deadline = tokio::time::sleep_until(now + lease_span);
    tokio::pin!(handler);
    tokio::pin!(lease_deadline);

    loop {
        tokio::select! {
            result = &mut handler => return Some(result),
            () = &mut lease_deadline => {
                warn!(
                    %consumer,
                    %event_id,
                    attempts,
                    "consumer receipt lease expired without a confirmed renewal; abandoning handler"
                );
                return None;
            }
            _ = ticker.tick() => {
                let renew_call_timeout = LEASE_RENEW_CALL_TIMEOUT.min(lease_span / 4);
                let renewal = tokio::time::timeout(
                    renew_call_timeout,
                    repo.renew(
                        consumer,
                        event_id,
                        attempts,
                        OffsetDateTime::now_utc(),
                        lease,
                    ),
                )
                .await;
                match renewal {
                    Ok(Ok(true)) => {
                        lease_deadline
                            .as_mut()
                            .reset(tokio::time::Instant::now() + lease_span);
                        debug!(%consumer, %event_id, attempts, "consumer receipt lease renewed");
                    }
                    Ok(Ok(false)) => {
                        warn!(
                            %consumer,
                            %event_id,
                            attempts,
                            "consumer receipt renewal lost its fencing token; abandoning handler"
                        );
                        return None;
                    }
                    Ok(Err(error)) => {
                        warn!(
                            %consumer,
                            %event_id,
                            attempts,
                            ?error,
                            "consumer receipt renewal failed; retaining last confirmed deadline"
                        );
                    }
                    Err(_) => {
                        warn!(
                            %consumer,
                            %event_id,
                            attempts,
                            "consumer receipt renewal timed out; retaining last confirmed deadline"
                        );
                    }
                }
            }
        }
    }
}

/// Process one durable delivery under a persistent idempotency receipt.
///
/// Legacy events without `event_id` cannot use the persistent receipt, but they
/// still retain `JetStream`'s at-least-once contract: handler failure is left
/// unACKed for broker redelivery. New outbox events are `ACK`ed only after the
/// fenced receipt is completed.
///
/// No receipt protocol can atomically couple `PostgreSQL` to an arbitrary remote
/// side effect: a process can still crash after an HTTP/AI/push call succeeds
/// but before `complete`.  The receipt closes the distinct late-republish gap:
/// once completion is recorded, producer retries outside NATS's finite
/// duplicate window never invoke the handler again.
async fn process_with_store<R, F, Fut>(
    repo: &R,
    consumer: &'static str,
    delivery: Box<dyn Subscription + Send>,
    handler: F,
) -> bool
where
    R: ReceiptStore + ?Sized,
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    process_with_store_timing(
        repo,
        consumer,
        delivery,
        handler,
        PROCESSING_LEASE,
        LEASE_RENEW_INTERVAL,
    )
    .await
}

async fn process_with_store_timing<R, F, Fut>(
    repo: &R,
    consumer: &'static str,
    delivery: Box<dyn Subscription + Send>,
    handler: F,
    processing_lease: Duration,
    renew_interval: StdDuration,
) -> bool
where
    R: ReceiptStore + ?Sized,
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let Some(event_id) = extract_event_id(delivery.payload()) else {
        if let Err(error) = handler().await {
            warn!(%consumer, ?error, "legacy consumer event handler failed; leaving event unacked");
            return false;
        }
        return ack(delivery, consumer, None).await;
    };

    let claim = match repo
        .claim(
            consumer,
            event_id,
            OffsetDateTime::now_utc(),
            processing_lease,
        )
        .await
    {
        Ok(claim) => claim,
        Err(error) => {
            warn!(%consumer, %event_id, ?error, "consumer receipt claim failed; leaving event unacked");
            return false;
        }
    };

    let attempts = match claim {
        ConsumerEventClaim::Completed => {
            debug!(%consumer, %event_id, "completed consumer event redelivered; ACK without side effect");
            return ack(delivery, consumer, Some(event_id)).await;
        }
        ConsumerEventClaim::Busy => {
            debug!(%consumer, %event_id, "consumer event lease busy; leaving event unacked");
            return false;
        }
        ConsumerEventClaim::Claimed { attempts } => attempts,
    };

    let Some(handler_result) = run_handler_with_renewal(
        repo,
        consumer,
        event_id,
        attempts,
        handler(),
        processing_lease,
        renew_interval,
    )
    .await
    else {
        return false;
    };

    if let Err(error) = handler_result {
        match repo
            .release(
                consumer,
                event_id,
                attempts,
                OffsetDateTime::now_utc(),
                &error.to_string(),
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => warn!(
                %consumer,
                %event_id,
                attempts,
                "consumer receipt failure release lost its fencing token"
            ),
            Err(release_error) => warn!(
                %consumer,
                %event_id,
                attempts,
                ?release_error,
                "consumer event failed and receipt release failed"
            ),
        }
        warn!(%consumer, %event_id, attempts, ?error, "consumer event handler failed; leaving event unacked");
        return false;
    }

    match repo
        .complete(consumer, event_id, attempts, OffsetDateTime::now_utc())
        .await
    {
        Ok(true) => return ack(delivery, consumer, Some(event_id)).await,
        Ok(false) => {
            warn!(
                %consumer,
                %event_id,
                attempts,
                "consumer receipt completion lost its fencing token; leaving event unacked"
            );
        }
        Err(error) => {
            warn!(
                %consumer,
                %event_id,
                attempts,
                ?error,
                "consumer receipt completion failed; leaving event unacked"
            );
        }
    }
    false
}

pub(crate) async fn process<F, Fut>(
    repo: &ConsumerEventReceiptRepo,
    consumer: &'static str,
    delivery: Box<dyn Subscription + Send>,
    handler: F,
) -> bool
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    process_with_store(repo, consumer, delivery, handler).await
}

/// Process a durable event whose handler atomically completes the receipt with
/// its own `PostgreSQL` outbox writes.
///
/// The handler receives the producer event id and the receipt fencing attempt.
/// Returning `Ok(())` promises that its transaction committed both its complete
/// fan-out and [`ConsumerEventReceiptRepo::complete_in_tx`]. A failure releases
/// the claim and leaves the broker delivery unACKed.
pub(crate) async fn process_atomic<F, Fut>(
    repo: &ConsumerEventReceiptRepo,
    consumer: &'static str,
    delivery: Box<dyn Subscription + Send>,
    handler: F,
) -> bool
where
    F: FnOnce(Uuid, i32) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    process_atomic_with_store(repo, consumer, delivery, handler).await
}

async fn process_atomic_with_store<R, F, Fut>(
    repo: &R,
    consumer: &'static str,
    delivery: Box<dyn Subscription + Send>,
    handler: F,
) -> bool
where
    R: ReceiptStore + ?Sized,
    F: FnOnce(Uuid, i32) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let Some(event_id) = extract_event_id(delivery.payload()) else {
        warn!(%consumer, "atomic consumer event has no producer event_id; leaving event unacked");
        return false;
    };

    let claim = match repo
        .claim(
            consumer,
            event_id,
            OffsetDateTime::now_utc(),
            PROCESSING_LEASE,
        )
        .await
    {
        Ok(claim) => claim,
        Err(error) => {
            warn!(%consumer, %event_id, ?error, "atomic consumer receipt claim failed; leaving event unacked");
            return false;
        }
    };

    let attempts = match claim {
        ConsumerEventClaim::Completed => {
            debug!(%consumer, %event_id, "completed atomic consumer event redelivered; ACK without materialization");
            return ack(delivery, consumer, Some(event_id)).await;
        }
        ConsumerEventClaim::Busy => {
            debug!(%consumer, %event_id, "atomic consumer event lease busy; leaving event unacked");
            return false;
        }
        ConsumerEventClaim::Claimed { attempts } => attempts,
    };

    if let Err(error) = handler(event_id, attempts).await {
        match repo
            .release(
                consumer,
                event_id,
                attempts,
                OffsetDateTime::now_utc(),
                &error.to_string(),
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => warn!(
                %consumer,
                %event_id,
                attempts,
                "atomic consumer failure release lost its fencing token"
            ),
            Err(release_error) => warn!(
                %consumer,
                %event_id,
                attempts,
                ?release_error,
                "atomic consumer handler and receipt release both failed"
            ),
        }
        warn!(%consumer, %event_id, attempts, ?error, "atomic consumer handler failed; leaving event unacked");
        return false;
    }

    ack(delivery, consumer, Some(event_id)).await
}

pub(crate) fn extract_event_id(payload: &[u8]) -> Option<Uuid> {
    serde_json::from_slice::<EventMetadata>(payload)
        .ok()
        .and_then(|metadata| metadata.event_id)
}

async fn ack(
    delivery: Box<dyn Subscription + Send>,
    consumer: &'static str,
    event_id: Option<Uuid>,
) -> bool {
    if let Err(error) = delivery.ack().await {
        warn!(%consumer, ?event_id, ?error, "consumer event ACK failed");
        false
    } else {
        true
    }
}

#[cfg(test)]
mod tests;
