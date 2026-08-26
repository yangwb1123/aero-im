//! Settle-face heartbeat freshness gate (B5-4 R4) — the fail-closed
//! acknowledgement guard.
//!
//! The relay only acknowledges deliveries (fenced settle → status 2) while
//! the durable provisioning heartbeat exists and is fresh. This decorator
//! wraps any [`OutboxRepo`] so its `settle` consults the [`HeartbeatRecorder`]
//! first:
//!
//! * `Ok(true)` → delegate to the inner fenced settle; on `Ok(true)` record a
//!   heartbeat (Err or timeout → warn only; the acknowledgement stands).
//!   Bounded false-green ≤ one freshness window (the check-to-settle gap is
//!   milliseconds against a ≥60s window).
//! * `Ok(false)` (stale or absent) → **`Ok(false)` without delegating**: the
//!   row stays status 1 claimed with `attempts` untouched; lease expiry
//!   reclaims it through the existing `claim_due` machinery. The status-2
//!   transition never fires while unverified and, being fenced, fires at most
//!   once per row (never double-settled).
//! * `Err` (freshness query failed) → fail-closed `Ok(false)` (never
//!   acknowledge on uncertainty), one `warn!`.
//!
//! Claims are deliberately NOT freshness-gated: the rejection is on the
//! acknowledgement face only. Liveness is tick-driven in production (the
//! server's fixed 60s heartbeat tick + one-shot bootstrap arm), so a quiet
//! period cannot stale the gate — the decorator's record-on-fenced-settle is
//! the second refresh path, not the only one.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use tracing::warn;
use uuid::Uuid;

use aero_common::AuditId;

use crate::outbox::{Claim, Error, OutboxRepo, StatusBuckets, VerdictProbe};

/// Durable liveness recorder behind the settle-face freshness gate.
#[async_trait]
pub trait HeartbeatRecorder: Send + Sync {
    /// Record "relay works": called on a fenced settle `Ok(true)` and by the
    /// server's periodic tick / bootstrap arm. Errors are logged by the
    /// caller and never feed back into delivery results.
    async fn record_heartbeat(&self) -> Result<(), sqlx::Error>;

    /// True only when a durable heartbeat row exists and is fresh
    /// (`now - verified_at <= provision_freshness`, DB clock). Any anomaly
    /// (row missing / stale / DB error) → `false` (fail-closed).
    async fn heartbeat_fresh(&self) -> Result<bool, sqlx::Error>;
}

/// Maximum time a fenced settle may spend refreshing the durable heartbeat.
///
/// The refresh is best-effort after the acknowledgement has already committed:
/// a slow or unavailable database must not hold the relay's whole concurrent
/// delivery batch open. A timeout leaves the next freshness check to fail
/// closed once the existing heartbeat ages out.
const HEARTBEAT_RECORD_TIMEOUT: Duration = Duration::from_secs(5);

/// [`OutboxRepo`] decorator gating the acknowledgement face on heartbeat
/// freshness. The 3rd `OutboxRepo` implementer.
pub struct HeartbeatOutboxRepo<R> {
    inner: Arc<dyn OutboxRepo>,
    recorder: Arc<R>,
    record_timeout: Duration,
}

impl<R: HeartbeatRecorder + 'static> HeartbeatOutboxRepo<R> {
    #[must_use]
    pub fn new(inner: Arc<dyn OutboxRepo>, recorder: Arc<R>) -> Self {
        Self::with_record_timeout(inner, recorder, HEARTBEAT_RECORD_TIMEOUT)
    }

    fn with_record_timeout(
        inner: Arc<dyn OutboxRepo>,
        recorder: Arc<R>,
        record_timeout: Duration,
    ) -> Self {
        Self {
            inner,
            recorder,
            record_timeout,
        }
    }
}

#[async_trait]
impl<R: HeartbeatRecorder + 'static> OutboxRepo for HeartbeatOutboxRepo<R> {
    async fn reconcile(&self, limit: i64) -> Result<i64, Error> {
        self.inner.reconcile(limit).await
    }

    async fn claim_due(&self, lease: time::Duration, limit: i64) -> Result<Vec<Claim>, Error> {
        self.inner.claim_due(lease, limit).await
    }

    async fn settle(&self, event_id: AuditId, claim_token: Uuid) -> Result<bool, Error> {
        match self.recorder.heartbeat_fresh().await {
            Ok(true) => {
                let settled = self.inner.settle(event_id, claim_token).await?;
                if settled {
                    // Record-on-fenced-settle: only an acknowledgement that
                    // actually fired refreshes liveness. Err → warn only; the
                    // delivery stands (the tick path keeps the row fresh).
                    match tokio::time::timeout(
                        self.record_timeout,
                        self.recorder.record_heartbeat(),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            warn!(
                                %event_id,
                                ?error,
                                "audit relay heartbeat record failed after a fenced settle; the delivery is acknowledged"
                            );
                        }
                        Err(_) => {
                            warn!(
                                %event_id,
                                timeout_ms = self.record_timeout.as_millis(),
                                "audit relay heartbeat record timed out after a fenced settle; the delivery is acknowledged"
                            );
                        }
                    }
                }
                Ok(settled)
            }
            Ok(false) => {
                // Stale or absent row: fail-closed rejection. The row stays
                // status 1 claimed; lease expiry reclaims it (never
                // double-settled — the inner fence still fires at most once).
                warn!(
                    %event_id,
                    "audit relay heartbeat is stale or absent; settle rejected fail-closed (row stays claimed until lease expiry)"
                );
                Ok(false)
            }
            Err(error) => {
                warn!(
                    %event_id,
                    ?error,
                    "audit relay heartbeat freshness check failed; settle rejected fail-closed (never acknowledge on uncertainty)"
                );
                Ok(false)
            }
        }
    }

    async fn requeue(
        &self,
        event_id: AuditId,
        claim_token: Uuid,
        attempts: i64,
        error: &str,
    ) -> Result<bool, Error> {
        self.inner
            .requeue(event_id, claim_token, attempts, error)
            .await
    }

    async fn mark_dead(
        &self,
        event_id: AuditId,
        claim_token: Uuid,
        attempts: i64,
        error: &str,
    ) -> Result<bool, Error> {
        self.inner
            .mark_dead(event_id, claim_token, attempts, error)
            .await
    }

    async fn verdict_probe(&self) -> Result<VerdictProbe, Error> {
        self.inner.verdict_probe().await
    }

    async fn status_buckets(&self) -> Result<StatusBuckets, Error> {
        self.inner.status_buckets().await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration as StdDuration;

    use serde_json::json;
    use time::Duration;
    use uuid::Uuid;

    use aero_common::AuditId;
    use async_trait::async_trait;

    use super::{HeartbeatOutboxRepo, HeartbeatRecorder};
    use crate::fake::{FakeOutbox, FakeStatus};
    use crate::outbox::OutboxRepo;

    /// Counted recorder: records call counts and an injectable freshness
    /// verdict / record error (the "inner never called" counter pins the
    /// decorator's gate before-delegation order). Flags are interior-mutable
    /// (the tests flip them after Arc construction).
    struct CountingRecorder {
        records: AtomicUsize,
        fresh: bool,
        record_err: AtomicBool,
        fresh_err: bool,
    }

    impl CountingRecorder {
        fn fresh(fresh: bool) -> Self {
            Self {
                records: AtomicUsize::new(0),
                fresh,
                record_err: AtomicBool::new(false),
                fresh_err: false,
            }
        }
    }

    /// Recorder whose write never resolves. The settle must still return once
    /// the decorator's bounded refresh timeout expires.
    struct HangingRecorder {
        records: AtomicUsize,
    }

    #[async_trait]
    impl HeartbeatRecorder for HangingRecorder {
        async fn record_heartbeat(&self) -> Result<(), sqlx::Error> {
            self.records.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<Result<(), sqlx::Error>>().await
        }

        async fn heartbeat_fresh(&self) -> Result<bool, sqlx::Error> {
            Ok(true)
        }
    }

    #[async_trait]
    impl HeartbeatRecorder for CountingRecorder {
        async fn record_heartbeat(&self) -> Result<(), sqlx::Error> {
            self.records.fetch_add(1, Ordering::SeqCst);
            if self.record_err.load(Ordering::SeqCst) {
                return Err(sqlx::Error::RowNotFound);
            }
            Ok(())
        }

        async fn heartbeat_fresh(&self) -> Result<bool, sqlx::Error> {
            if self.fresh_err {
                return Err(sqlx::Error::RowNotFound);
            }
            Ok(self.fresh)
        }
    }

    fn t0() -> time::OffsetDateTime {
        time::OffsetDateTime::now_utc()
    }

    /// Seed one due row and claim it (the settle face needs a live claim).
    async fn seed_claimed(fake: &FakeOutbox) -> (AuditId, Uuid) {
        let t0 = t0();
        fake.set_now(Some(t0)).await;
        let id = AuditId::from_uuid(Uuid::new_v4());
        fake.insert(
            id,
            json!({"event_id": id.to_uuid().to_string(), "source_system": "aero-im.source"}),
            t0,
        )
        .await;
        let claims = fake
            .claim_due(Duration::seconds(30), 1)
            .await
            .expect("claim");
        assert_eq!(claims.len(), 1);
        (id, claims[0].claim_token)
    }

    /// R4.5 — freshness gate: `heartbeat_fresh() == false` → settle returns
    /// `Ok(false)` and the inner settle is **never called** (counted fake);
    /// `true` → the inner settle runs and a fenced `Ok(true)` records +1.
    #[tokio::test]
    async fn stale_heartbeat_never_reaches_the_inner_settle() {
        let fake = Arc::new(FakeOutbox::new());
        let (id, token) = seed_claimed(&fake).await;

        let stale = Arc::new(CountingRecorder::fresh(false));
        let decorated = Arc::new(HeartbeatOutboxRepo::new(fake.clone(), stale.clone()));
        let settled = decorated
            .settle(id, token)
            .await
            .expect("decorator settle never errors on a stale gate");
        assert!(!settled, "stale heartbeat must reject the settle");
        assert_eq!(
            stale.records.load(Ordering::SeqCst),
            0,
            "no record without a fenced settle"
        );
        let row = fake.row(id).await.expect("row");
        assert_eq!(
            row.status,
            FakeStatus::Claimed,
            "row stays claimed (lease reclaims it)"
        );
        assert_eq!(row.attempts, 1, "attempts untouched by the rejection");
    }

    /// R4.5 — fresh heartbeat + fenced `Ok(true)` → settle value unchanged and
    /// the recorder gains exactly one record; the row flips to Delivered.
    #[tokio::test]
    async fn fresh_heartbeat_settles_and_records_exactly_once() {
        let fake = Arc::new(FakeOutbox::new());
        let (id, token) = seed_claimed(&fake).await;

        let fresh = Arc::new(CountingRecorder::fresh(true));
        let decorated = Arc::new(HeartbeatOutboxRepo::new(fake.clone(), fresh.clone()));
        let settled = decorated.settle(id, token).await.expect("decorator settle");
        assert!(settled, "fresh heartbeat must allow the fenced settle");
        assert_eq!(
            fresh.records.load(Ordering::SeqCst),
            1,
            "record-on-fenced-settle"
        );
        assert_eq!(
            fake.row(id).await.expect("row").status,
            FakeStatus::Delivered
        );

        // A direct second settle on the settled row → false (fence: status
        // IN (0,1) fails) and the recorder does NOT gain another record.
        let before = fresh.records.load(Ordering::SeqCst);
        let second = decorated.settle(id, token).await.expect("decorator settle");
        assert!(!second, "never double-settled");
        assert_eq!(
            fresh.records.load(Ordering::SeqCst),
            before,
            "no record for a fenced-false settle"
        );
    }

    /// A fenced settle must not hang the relay batch when the best-effort
    /// heartbeat write never resolves. The acknowledgement remains committed
    /// and the row is still delivered.
    #[tokio::test]
    async fn hanging_heartbeat_record_is_bounded_after_settle() {
        let fake = Arc::new(FakeOutbox::new());
        let (id, token) = seed_claimed(&fake).await;

        let recorder = Arc::new(HangingRecorder {
            records: AtomicUsize::new(0),
        });
        let decorated = Arc::new(HeartbeatOutboxRepo::with_record_timeout(
            fake.clone(),
            recorder.clone(),
            StdDuration::from_millis(10),
        ));

        let settled = tokio::time::timeout(StdDuration::from_secs(1), decorated.settle(id, token))
            .await
            .expect("heartbeat timeout must release settle")
            .expect("decorator settle");
        assert!(settled, "the fenced acknowledgement must stand");
        assert_eq!(recorder.records.load(Ordering::SeqCst), 1);
        assert_eq!(
            fake.row(id).await.expect("row").status,
            FakeStatus::Delivered
        );
    }

    /// R4.5 — a fenced `Ok(false)` (wrong token) does not refresh (the
    /// settle-only pin) and the recorder Err never fails the settle value.
    #[tokio::test]
    async fn fenced_false_settle_does_not_record() {
        let fake = Arc::new(FakeOutbox::new());
        let (id, token) = seed_claimed(&fake).await;

        let fresh = Arc::new(CountingRecorder::fresh(true));
        let decorated = Arc::new(HeartbeatOutboxRepo::new(fake.clone(), fresh.clone()));
        let wrong = Uuid::new_v4();
        let settled = decorated.settle(id, wrong).await.expect("decorator settle");
        assert!(!settled, "wrong token must fence the settle");
        assert_eq!(
            fresh.records.load(Ordering::SeqCst),
            0,
            "fenced-false never records"
        );
        assert_eq!(fake.row(id).await.expect("row").status, FakeStatus::Claimed);

        // Recorder failure after a fenced Ok(true) → warn only; the settle
        // value stays Ok(true) (the acknowledgement is not rolled back).
        let failing = Arc::new(CountingRecorder::fresh(true));
        failing.record_err.store(true, Ordering::SeqCst);
        let decorated = Arc::new(HeartbeatOutboxRepo::new(fake.clone(), failing.clone()));
        let settled = decorated.settle(id, token).await.expect("decorator settle");
        assert!(settled, "recorder Err must not fail the settle value");
    }

    /// R4.5 — non-settle faces (requeue / `mark_dead` / `claim_due` / reconcile /
    /// both probes) are pure delegation: the recorder is never touched.
    #[tokio::test]
    async fn non_settle_faces_are_pure_delegation() {
        let fake = Arc::new(FakeOutbox::new());
        let t0 = t0();
        fake.set_now(Some(t0)).await;
        let id = AuditId::from_uuid(Uuid::new_v4());
        fake.insert(id, json!({"n": 1}), t0).await;

        let recorder = Arc::new(CountingRecorder::fresh(true));
        let decorated = Arc::new(HeartbeatOutboxRepo::new(fake.clone(), recorder.clone()));

        assert_eq!(decorated.reconcile(10).await.expect("reconcile"), 0);
        let claims = decorated
            .claim_due(Duration::seconds(30), 1)
            .await
            .expect("claim");
        assert_eq!(claims.len(), 1);
        assert_eq!(
            decorated.verdict_probe().await.expect("probe").claimed,
            1,
            "Tier-1 probe delegates"
        );
        assert_eq!(
            decorated.status_buckets().await.expect("buckets").claimed,
            1,
            "Tier-2 buckets delegate"
        );
        assert!(decorated
            .requeue(id, claims[0].claim_token, 1, "drill")
            .await
            .expect("requeue"));
        // Requeue re-parks with backoff(1) = 1s on the fake's clock — advance
        // the clock so the row is due again (single clock domain).
        fake.set_now(Some(t0 + Duration::seconds(1))).await;
        let (id2, token2) = {
            let claims = decorated
                .claim_due(Duration::seconds(30), 1)
                .await
                .expect("claim again");
            assert_eq!(claims.len(), 1);
            (claims[0].event_id, claims[0].claim_token)
        };
        assert!(decorated
            .mark_dead(id2, token2, 2, "drill dead")
            .await
            .expect("mark dead"));
        assert_eq!(
            recorder.records.load(Ordering::SeqCst),
            0,
            "claims/requeues/deads/probes never touch the recorder"
        );
    }
}
