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
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Default)]
    struct DeliveryProbe {
        acks: AtomicUsize,
        nacks: AtomicUsize,
    }

    struct FakeDelivery {
        payload: Vec<u8>,
        probe: Arc<DeliveryProbe>,
    }

    #[async_trait::async_trait]
    impl Subscription for FakeDelivery {
        fn subject(&self) -> &'static str {
            "im.room.test"
        }

        fn payload(&self) -> &[u8] {
            &self.payload
        }

        async fn ack(&self) -> aero_bus::traits::BusResult<()> {
            self.probe.acks.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn nack(&self) -> aero_bus::traits::BusResult<()> {
            self.probe.nacks.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    struct FakeStore {
        claim: ConsumerEventClaim,
        complete: bool,
        renew: bool,
        completions: AtomicUsize,
        renewals: AtomicUsize,
        releases: AtomicUsize,
        released_error: Mutex<Option<String>>,
    }

    impl FakeStore {
        fn new(claim: ConsumerEventClaim) -> Self {
            Self {
                claim,
                complete: true,
                renew: true,
                completions: AtomicUsize::new(0),
                renewals: AtomicUsize::new(0),
                releases: AtomicUsize::new(0),
                released_error: Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl ReceiptStore for FakeStore {
        async fn claim(
            &self,
            _consumer: &str,
            _event_id: Uuid,
            _now: OffsetDateTime,
            _lease: Duration,
        ) -> Result<ConsumerEventClaim, sqlx::Error> {
            Ok(self.claim)
        }

        async fn complete(
            &self,
            _consumer: &str,
            _event_id: Uuid,
            _attempts: i32,
            _now: OffsetDateTime,
        ) -> Result<bool, sqlx::Error> {
            self.completions.fetch_add(1, Ordering::Relaxed);
            Ok(self.complete)
        }

        async fn renew(
            &self,
            _consumer: &str,
            _event_id: Uuid,
            _attempts: i32,
            _now: OffsetDateTime,
            _lease: Duration,
        ) -> Result<bool, sqlx::Error> {
            self.renewals.fetch_add(1, Ordering::Relaxed);
            Ok(self.renew)
        }

        async fn release(
            &self,
            _consumer: &str,
            _event_id: Uuid,
            _attempts: i32,
            _now: OffsetDateTime,
            error: &str,
        ) -> Result<bool, sqlx::Error> {
            self.releases.fetch_add(1, Ordering::Relaxed);
            *self.released_error.lock().unwrap() = Some(error.to_owned());
            Ok(true)
        }
    }

    fn delivery(with_event_id: bool) -> (Box<dyn Subscription + Send>, Arc<DeliveryProbe>) {
        let probe = Arc::new(DeliveryProbe::default());
        let payload = if with_event_id {
            serde_json::to_vec(&serde_json::json!({
                "kind": "message",
                "event_id": Uuid::new_v4()
            }))
            .unwrap()
        } else {
            br#"{"kind":"message"}"#.to_vec()
        };
        (
            Box::new(FakeDelivery {
                payload,
                probe: Arc::clone(&probe),
            }),
            probe,
        )
    }

    #[test]
    fn extracts_valid_top_level_event_id_without_typed_decode() {
        let id = Uuid::new_v4();
        let payload = serde_json::json!({
            "kind": "future_variant_unknown_to_this_binary",
            "event_id": id,
            "nested": {"event_id": Uuid::new_v4()}
        });
        assert_eq!(
            extract_event_id(&serde_json::to_vec(&payload).unwrap()),
            Some(id)
        );
    }

    #[test]
    fn legacy_or_malformed_metadata_has_no_receipt_key() {
        assert_eq!(extract_event_id(br#"{"kind":"message"}"#), None);
        assert_eq!(extract_event_id(br#"{"event_id":"not-a-uuid"}"#), None);
        assert_eq!(extract_event_id(b"not-json"), None);
    }

    #[test]
    fn receipt_lease_exceeds_broker_ack_wait() {
        assert!(PROCESSING_LEASE > Duration::seconds(120));
        assert!(
            PROCESSING_LEASE.whole_seconds()
                > i64::try_from(LEASE_RENEW_INTERVAL.as_secs()).unwrap()
        );
    }

    #[tokio::test]
    async fn long_handler_renews_before_completion() {
        let store = FakeStore::new(ConsumerEventClaim::Claimed { attempts: 4 });
        let (delivery, probe) = delivery(true);
        let settled = process_with_store_timing(
            &store,
            "test",
            delivery,
            || async {
                tokio::time::sleep(StdDuration::from_millis(45)).await;
                Ok(())
            },
            Duration::seconds(1),
            StdDuration::from_millis(10),
        )
        .await;

        assert!(settled);
        assert!(store.renewals.load(Ordering::Relaxed) >= 2);
        assert_eq!(store.completions.load(Ordering::Relaxed), 1);
        assert_eq!(probe.acks.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn lost_renewal_abandons_handler_without_settlement() {
        let mut store = FakeStore::new(ConsumerEventClaim::Claimed { attempts: 9 });
        store.renew = false;
        let (delivery, probe) = delivery(true);
        let handler_finished = AtomicBool::new(false);
        let settled = process_with_store_timing(
            &store,
            "test",
            delivery,
            || async {
                tokio::time::sleep(StdDuration::from_millis(100)).await;
                handler_finished.store(true, Ordering::Relaxed);
                Ok(())
            },
            Duration::seconds(1),
            StdDuration::from_millis(10),
        )
        .await;

        assert!(!settled);
        assert!(!handler_finished.load(Ordering::Relaxed));
        assert_eq!(store.renewals.load(Ordering::Relaxed), 1);
        assert_eq!(store.completions.load(Ordering::Relaxed), 0);
        assert_eq!(store.releases.load(Ordering::Relaxed), 0);
        assert_eq!(probe.acks.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn completed_receipt_acks_without_invoking_handler() {
        let store = FakeStore::new(ConsumerEventClaim::Completed);
        let (delivery, probe) = delivery(true);
        let calls = AtomicUsize::new(0);
        let settled = process_with_store(&store, "test", delivery, || async {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .await;

        assert!(settled);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(probe.acks.load(Ordering::Relaxed), 1);
        assert_eq!(store.completions.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn successful_claim_completes_before_ack() {
        let store = FakeStore::new(ConsumerEventClaim::Claimed { attempts: 4 });
        let (delivery, probe) = delivery(true);
        let settled = process_with_store(&store, "test", delivery, || async { Ok(()) }).await;

        assert!(settled);
        assert_eq!(store.completions.load(Ordering::Relaxed), 1);
        assert_eq!(probe.acks.load(Ordering::Relaxed), 1);
        assert_eq!(probe.nacks.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn busy_or_failed_events_are_not_acked() {
        let busy = FakeStore::new(ConsumerEventClaim::Busy);
        let (busy_delivery, busy_probe) = delivery(true);
        assert!(!process_with_store(&busy, "test", busy_delivery, || async { Ok(()) }).await);
        assert_eq!(busy_probe.acks.load(Ordering::Relaxed), 0);

        let failed = FakeStore::new(ConsumerEventClaim::Claimed { attempts: 2 });
        let (failed_delivery, failed_probe) = delivery(true);
        assert!(
            !process_with_store(&failed, "test", failed_delivery, || async {
                anyhow::bail!("handler exploded")
            })
            .await
        );
        assert_eq!(failed_probe.acks.load(Ordering::Relaxed), 0);
        assert_eq!(failed.releases.load(Ordering::Relaxed), 1);
        assert_eq!(
            failed.released_error.lock().unwrap().as_deref(),
            Some("handler exploded")
        );

        let mut completion_failed = FakeStore::new(ConsumerEventClaim::Claimed { attempts: 3 });
        completion_failed.complete = false;
        let (completion_delivery, completion_probe) = delivery(true);
        assert!(
            !process_with_store(&completion_failed, "test", completion_delivery, || async {
                Ok(())
            })
            .await
        );
        assert_eq!(completion_probe.acks.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn legacy_failure_is_left_unacked_for_redelivery() {
        let store = FakeStore::new(ConsumerEventClaim::Busy);
        let (delivery, probe) = delivery(false);
        assert!(
            !process_with_store(&store, "test", delivery, || async {
                anyhow::bail!("legacy failure")
            })
            .await
        );
        assert_eq!(probe.acks.load(Ordering::Relaxed), 0);
        assert_eq!(store.releases.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn atomic_handler_owns_completion_then_ack() {
        let store = FakeStore::new(ConsumerEventClaim::Claimed { attempts: 7 });
        let (delivery, probe) = delivery(true);
        let seen_attempt = AtomicUsize::new(0);
        let seen_attempt_ref = &seen_attempt;
        let settled = process_atomic_with_store(
            &store,
            "atomic-test",
            delivery,
            move |_event_id, attempts| async move {
                seen_attempt_ref.store(usize::try_from(attempts).unwrap(), Ordering::Relaxed);
                Ok(())
            },
        )
        .await;

        assert!(settled);
        assert_eq!(seen_attempt.load(Ordering::Relaxed), 7);
        assert_eq!(probe.acks.load(Ordering::Relaxed), 1);
        assert_eq!(
            store.completions.load(Ordering::Relaxed),
            0,
            "the handler transaction, not the wrapper, completes the receipt"
        );
    }

    #[tokio::test]
    async fn atomic_handler_failure_releases_and_missing_id_never_acks() {
        let failed = FakeStore::new(ConsumerEventClaim::Claimed { attempts: 2 });
        let (failed_delivery, failed_probe) = delivery(true);
        assert!(
            !process_atomic_with_store(
                &failed,
                "atomic-test",
                failed_delivery,
                |_event_id, _attempts| async { anyhow::bail!("fanout transaction failed") },
            )
            .await
        );
        assert_eq!(failed.releases.load(Ordering::Relaxed), 1);
        assert_eq!(failed_probe.acks.load(Ordering::Relaxed), 0);

        let legacy = FakeStore::new(ConsumerEventClaim::Claimed { attempts: 1 });
        let (legacy_delivery, legacy_probe) = delivery(false);
        let calls = AtomicUsize::new(0);
        let calls_ref = &calls;
        assert!(
            !process_atomic_with_store(
                &legacy,
                "atomic-test",
                legacy_delivery,
                move |_event_id, _attempts| async move {
                    calls_ref.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                },
            )
            .await
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(legacy_probe.acks.load(Ordering::Relaxed), 0);
    }
}
