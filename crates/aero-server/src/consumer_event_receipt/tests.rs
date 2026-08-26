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
        PROCESSING_LEASE.whole_seconds() > i64::try_from(LEASE_RENEW_INTERVAL.as_secs()).unwrap()
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
