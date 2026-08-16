//! Relay loop: claim due outbox rows, deliver through the audit client, and
//! apply exactly one terminal/retry transition per row.
//!
//! The relay never binds a wall clock into the repo: `claim_due`/`requeue`
//! take no `now`, and every lease/`available_at` value is minted on the
//! repository's own clock (PG `clock_timestamp()`; fake's pinned clock) —
//! single clock domain, so |app↔DB skew| >= lease cannot livelock a row in
//! claim→POST→fence-fail.
//!
//! Policy (state machine table F1–F13 in the design):
//! - success → `settle`;
//! - transient (transport / timeout / 5xx / unspecified 4xx / claim-validation
//!   drift / 401-after-refresh) → `requeue` with exponential backoff, never
//!   dead — the v1 retry-forever posture is preserved for transients;
//! - permanent (422 / 409 / receipt mismatch / payload guard) → `requeue` on
//!   attempt 1, `mark_dead` from attempt 2 (dead after ≤1 retry);
//! - HTTP 403 → `mark_dead` immediately (T-11 fail-closed, no requeue).

use std::sync::Arc;

use futures::{stream, StreamExt};
use time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::client::{AuditClient, DeliveryError};
use crate::config::RelayConfig;
use crate::outbox::{Claim, Error, OutboxRepo};

/// Lease upper bound (clone of `AiUsageRepo::MAX_LEASE_SECONDS`).
pub const MAX_LEASE_SECONDS: i64 = 86_400;
/// Backoff upper bound (clone of `AiUsageRepo::MAX_BACKOFF_SECONDS`).
pub const MAX_BACKOFF_SECONDS: i64 = 300;
/// Batch claim upper bound (clone of `AiUsageRepo::MAX_CLAIM`).
pub const MAX_CLAIM: i64 = 500;
/// Per-tick minimum service floor divisor (D-CAP, design §7.1): the
/// lowest-priority lane is guaranteed `max(1, batch / 20)` claim slots per
/// round. Config-visible constant pattern (clone of [`MAX_CLAIM`]); no env
/// var, no `RelayConfig` field.
pub const MIN_SERVICE_BATCH_DIVISOR: i64 = 20;
/// `last_error` truncation (clone of `AiUsageRepo::MAX_ERROR_CHARS`).
pub const MAX_ERROR_CHARS: usize = 2_048;

/// Permanent-class dead threshold: dead after ≤1 retry (attempt 1 → requeue,
/// attempt ≥ 2 → `mark_dead`). Pure + total, unit-tested without clock/DB
/// (`webhook_delivery.rs::is_dead_at` shape).
pub const PERMANENT_DEAD_AT: i64 = 2;

/// Whether a permanent-class delivery that has made `attempts` attempts is
/// now permanently dead (the ≤1-retry budget is exhausted). Pure, so the cap
/// is unit-tested without a clock.
#[must_use]
pub fn is_dead_at(attempts: i64) -> bool {
    attempts >= PERMANENT_DEAD_AT
}

/// Exponential backoff parity with the proven sequence
/// (`AiUsageRepo::ai_usage_backoff`): `2^(attempts-1)` seconds capped at 300s.
/// `1→1s, 2→2s, 3→4s, …, 9→256s, 10..=i32::MAX→300s`.
#[must_use]
pub fn audit_backoff(attempts: i64) -> Duration {
    let shift = u32::try_from(attempts.saturating_sub(1).clamp(0, 30)).unwrap_or(0);
    let seconds = 1_i64
        .checked_shl(shift)
        .unwrap_or(MAX_BACKOFF_SECONDS)
        .min(MAX_BACKOFF_SECONDS);
    Duration::seconds(seconds)
}

/// K = per-tick minimum service floor for the lowest-priority lane:
/// `min(max(1, b / MIN_SERVICE_BATCH_DIVISOR), b − 1)` where `b =
/// batch.clamp(1, MAX_CLAIM)`. The `b − 1` upper clamp keeps the floor
/// strictly below the batch so arm A (top `limit − K`) is never empty: at
/// the degenerate `batch = 1` the floor is 0 and the claim degenerates to
/// today's uncapped top-1 (precedence preserved, never inverted). Pure +
/// total — no clock, no DB, no panic on any i64 input.
#[must_use]
pub fn min_service_floor(batch: i64) -> i64 {
    let b = batch.clamp(1, MAX_CLAIM);
    (b / MIN_SERVICE_BATCH_DIVISOR).max(1).min(b - 1)
}

/// Clamp a lease into `[1s, MAX_LEASE_SECONDS]` (clone of `clamped_lease`).
#[must_use]
pub fn clamped_lease(lease: Duration) -> Duration {
    Duration::seconds(lease.whole_seconds().clamp(1, MAX_LEASE_SECONDS))
}

/// Bound `last_error` to [`MAX_ERROR_CHARS`] (clone of `truncate_error`).
#[must_use]
pub fn truncate_error(error: &str) -> String {
    error.chars().take(MAX_ERROR_CHARS).collect()
}

/// The relay: a repo seam + HTTP client + poll config.
///
/// Note: there is deliberately *no* app-side clock seam on the relay. The
/// repo owns time exclusively (PG `clock_timestamp()`; the fake's pinned
/// clock), so a relay-supplied `now` — the `ai_usage` pattern — cannot
/// reintroduce the |app↔DB skew| >= lease livelock. Tests pin the fake's
/// clock instead of the relay's.
pub struct AuditRelay {
    repo: Arc<dyn OutboxRepo>,
    client: AuditClient,
    config: RelayConfig,
}

impl std::fmt::Debug for AuditRelay {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuditRelay")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl AuditRelay {
    #[must_use]
    pub fn new(repo: Arc<dyn OutboxRepo>, client: AuditClient, config: RelayConfig) -> Self {
        Self {
            repo,
            client,
            config,
        }
    }

    /// Spawn the poll loop on the shared cancellation token. The loop logs and
    /// retries on SQL errors (e.g., booting before B5-1's 0239 table exists)
    /// instead of panicking; cancellation triggers a bounded drain.
    pub fn spawn(self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move { self.run(cancel).await })
    }

    async fn run(self, cancel: CancellationToken) {
        let mut tick = tokio::time::interval(self.config.poll_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    self.shutdown_drain().await;
                    return;
                }
                _ = tick.tick() => {
                    if let Err(error) = self.dispatch_batch().await {
                        warn!(?error, "audit relay claim failed; durable rows remain queued");
                    }
                }
            }
        }
    }

    async fn shutdown_drain(&self) {
        let drained = tokio::time::timeout(self.config.shutdown_drain, async {
            let mut total = 0;
            loop {
                let count = self.dispatch_batch().await?;
                total += count;
                if count == 0 {
                    return Ok::<_, Error>(total);
                }
            }
        })
        .await;
        match drained {
            Ok(Ok(count)) => info!(count, "audit relay shutdown drain complete"),
            Ok(Err(error)) => warn!(?error, "audit relay shutdown drain failed"),
            Err(_) => warn!("audit relay shutdown drain timed out; rows remain durable"),
        }
    }

    /// Recover the disabled window, then claim up to the configured batch and
    /// deliver each row concurrently. Returns the number of claimed rows.
    ///
    /// `reconcile` runs FIRST, mirroring v1's dispatch loop
    /// (`aero-server/src/snaplink_commercial/runtime.rs`, which runs
    /// `reconcile_usage` + `reconcile_audit` ahead of `claim_due`): rows
    /// moderated while the commercial runtime switch was off are backfilled
    /// (idempotent, bounded — 0241) and become claimable on the next tick.
    /// A reconcile error aborts the batch (warn + next tick retries), the
    /// same degrade class as a claim error on a pre-0239 DB (F11).
    pub async fn dispatch_batch(&self) -> Result<usize, Error> {
        let lease = Duration::seconds(
            i64::try_from(self.config.delivery_lease.as_secs()).unwrap_or(MAX_LEASE_SECONDS),
        );
        self.repo.reconcile(self.config.batch_size).await?;
        let claims = self.repo.claim_due(lease, self.config.batch_size).await?;
        let count = claims.len();
        stream::iter(claims)
            .for_each_concurrent(self.config.concurrency, |claim| async move {
                self.deliver_claim(claim).await;
            })
            .await;
        Ok(count)
    }

    async fn deliver_claim(&self, claim: Claim) {
        let event_id = claim.event_id;
        let token = claim.claim_token;
        let attempts = claim.attempts;
        match self.client.deliver(&claim).await {
            Ok(()) => {
                crate::metrics::inc_delivery_outcome("delivered");
                match self.repo.settle(event_id, token).await {
                    Ok(true) => {}
                    Ok(false) => warn!(
                        %event_id,
                        "audit delivery lost its lease before acknowledgement; idempotent retry will recover"
                    ),
                    Err(error) => warn!(
                        %event_id,
                        ?error,
                        "audit delivery succeeded but acknowledgement failed; idempotent retry will recover"
                    ),
                }
            }
            // Deliberately NOT `is_dead_at`: HTTP 403 is fail-closed immediate
            // death (T-11) regardless of the retry budget — it is an identity/
            // provisioning fault, not a payload-class fault that earns one retry.
            Err(DeliveryError::Forbidden) => {
                crate::metrics::inc_delivery_outcome("forbidden");
                crate::metrics::inc_dead();
                let dead = self
                    .repo
                    .mark_dead(
                        event_id,
                        token,
                        attempts,
                        "audit sink rejected the service identity (HTTP 403)",
                    )
                    .await;
                if !matches!(dead, Ok(true)) {
                    warn!(%event_id, ?dead, "audit 403 terminal transition lost its fence; lease expiry will reclaim");
                }
            }
            Err(DeliveryError::Permanent(kind)) => {
                crate::metrics::inc_delivery_outcome("permanent");
                let error = format!("audit delivery classified permanent: {kind:?}");
                // Single decision point on the pure predicate (webhook_delivery.rs
                // `mark_failed_with_backoff` shape): attempt 1 requeues with
                // backoff, attempt ≥ 2 reaches the dead terminal.
                let parked = if is_dead_at(attempts) {
                    crate::metrics::inc_dead();
                    self.repo.mark_dead(event_id, token, attempts, &error).await
                } else {
                    crate::metrics::inc_transient_requeue();
                    self.repo.requeue(event_id, token, attempts, &error).await
                };
                if !matches!(parked, Ok(true)) {
                    warn!(%event_id, ?parked, "audit permanent-class transition lost its fence; lease expiry will reclaim");
                }
            }
            Err(DeliveryError::Transient(error)) => {
                crate::metrics::inc_delivery_outcome("transient");
                crate::metrics::inc_transient_requeue();
                let parked = self
                    .repo
                    .requeue(event_id, token, attempts, &error.to_string())
                    .await;
                if !matches!(parked, Ok(true)) {
                    warn!(%event_id, ?error, ?parked, "audit delivery failed and explicit re-park lost its fence; lease expiry will reclaim");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aero_auth::{JwksKeyProvider, KeyProvider};
    use jsonwebtoken::Algorithm;
    use rand::rngs::OsRng;
    use rsa::RsaPrivateKey;
    use serde_json::json;
    use time::{Duration, OffsetDateTime};
    use uuid::Uuid;

    use aero_common::AuditId;

    use super::{audit_backoff, AuditRelay};
    use crate::client::AuditClient;
    use crate::config::RelayConfig;
    use crate::fake::{FakeOutbox, FakeStatus};
    use crate::outbox::OutboxRepo;
    use crate::stub::{SinkBehavior, StubSink};
    use reqwest::Url;

    /// Key source whose *mechanism* is always unavailable (fetch-plane
    /// failure): the fallible face returns `Err` — the connector's Transient
    /// plane (AC3(b) relay half / F9 double-pin).
    struct FailingKeyProvider;

    #[async_trait::async_trait]
    impl KeyProvider for FailingKeyProvider {
        async fn decoding_key(
            &self,
            _kid: Option<&str>,
            _algorithm: Algorithm,
        ) -> Option<jsonwebtoken::DecodingKey> {
            None
        }

        async fn decoding_key_fallible(
            &self,
            _kid: Option<&str>,
            _algorithm: Algorithm,
        ) -> Result<Option<jsonwebtoken::DecodingKey>, String> {
            Err("test jwks endpoint is down".to_owned())
        }
    }

    /// A fresh RS256 keypair for rotation fixtures.
    fn test_keypair(kid: &str) -> (RsaPrivateKey, String) {
        let mut rng = OsRng;
        (
            RsaPrivateKey::new(&mut rng, 2048).expect("rsa keygen"),
            kid.to_owned(),
        )
    }

    const LEASE: Duration = Duration::seconds(30);

    fn test_config(stub: &StubSink) -> RelayConfig {
        RelayConfig {
            token_endpoint: Url::parse(&stub.token_url()).expect("stub token URL"),
            events_url: Url::parse(&stub.events_url()).expect("stub events URL"),
            resource: "audit-governance".into(),
            client_id: "drill-client".into(),
            client_secret: "0123456789abcdef0123456789abcdef".into(),
            expected_iss: "https://idp.example.test".into(),
            expected_aud: "audit-governance".into(),
            expected_scope: "audit:event:write".into(),
            expected_sub: "aero-im.source".into(),
            source_system: "aero-im.source".into(),
            request_timeout: std::time::Duration::from_secs(5),
            delivery_lease: std::time::Duration::from_secs(30),
            poll_interval: std::time::Duration::from_secs(1),
            shutdown_drain: std::time::Duration::from_secs(2),
            batch_size: 100,
            concurrency: 4,
            jwks_uri: None,
        }
    }

    /// Production-mirror payload builder (D1): `Uuid` parameter keeps the
    /// payload's `event_id` in 0239 trigger format while the typed claim id
    /// is `AuditId` — never lockstep-construct both in base32.
    fn claim_payload(event_id: Uuid) -> serde_json::Value {
        json!({
            "event_id": event_id.to_string(),
            "source_system": "aero-im.source",
        })
    }

    /// Drive one transient delivery outcome through `deliver_claim`'s
    /// Transient arm and assert requeue + fresh-token rotation. The fake's
    /// clock is pinned (the PG `clock_timestamp()` analog — single clock
    /// domain), so backoff arithmetic is exact, not a wall-clock window.
    async fn assert_transient_requeue(
        behavior: SinkBehavior,
        request_timeout: std::time::Duration,
        expected_posts: usize,
        expected_error_fragment: &str,
    ) {
        assert_transient_requeue_with_keys(
            behavior,
            request_timeout,
            expected_posts,
            expected_error_fragment,
            None,
        )
        .await;
    }

    /// Same shape with an injected key source (JWKS-on transient plane).
    async fn assert_transient_requeue_with_keys(
        behavior: SinkBehavior,
        request_timeout: std::time::Duration,
        expected_posts: usize,
        expected_error_fragment: &str,
        keys: Option<Arc<dyn KeyProvider>>,
    ) {
        let stub = StubSink::start().await.expect("start stub");
        stub.set_behavior(behavior).await;
        let fake = Arc::new(FakeOutbox::new());
        let t0 = OffsetDateTime::now_utc();
        fake.set_now(Some(t0)).await;
        let id = Uuid::new_v4();
        fake.insert(AuditId::from_uuid(id), claim_payload(id), t0)
            .await;

        let mut config = test_config(&stub);
        config.request_timeout = request_timeout;
        let client =
            AuditClient::with_key_provider(config.clone(), keys).expect("build audit client");
        let relay = AuditRelay::new(fake.clone(), client, config);

        let claims = fake.claim_due(LEASE, 10).await.expect("claim");
        assert_eq!(claims.len(), 1);
        let token_a = claims[0].claim_token;
        relay
            .deliver_claim(claims.into_iter().next().expect("one claim"))
            .await;

        let row = fake.row(AuditId::from_uuid(id)).await.expect("row");
        assert_eq!(
            row.status,
            FakeStatus::Ready,
            "transient must requeue, never dead"
        );
        assert_eq!(row.attempts, 1);
        assert_eq!(
            row.available_at,
            t0 + Duration::seconds(1),
            "requeue must re-park with backoff(1) = 1s on the repo clock"
        );
        assert_eq!(
            row.claim_token, None,
            "requeue must drop the fencing token and lease"
        );
        assert_eq!(row.lease_expires_at, None);
        assert!(
            row.last_error
                .as_deref()
                .is_some_and(|error| error.contains(expected_error_fragment)),
            "last_error must record the transient cause (got {:?})",
            row.last_error
        );
        assert_eq!(stub.posts(), expected_posts, "delivery POST count");

        // Fresh-token rotation (A5 transient half): after the backoff the row
        // is claimable again and the new claim carries a rotated token.
        fake.set_now(Some(t0 + Duration::seconds(1))).await;
        let reclaimed = fake
            .claim_due(LEASE, 10)
            .await
            .expect("reclaim after backoff");
        assert_eq!(reclaimed.len(), 1, "requeued row must be claimable again");
        assert_eq!(reclaimed[0].attempts, 2);
        assert_ne!(
            reclaimed[0].claim_token, token_a,
            "requeue must rotate a fresh token"
        );
        stub.shutdown();
    }

    /// A5 relay half — transient 5xx drives `deliver_claim`'s Transient arm:
    /// requeue (never dead) + fresh-token rotation on the next claim.
    #[tokio::test]
    async fn transient_5xx_requeues_and_rotates_a_fresh_token() {
        assert_transient_requeue(
            SinkBehavior {
                events_status: 500,
                ..SinkBehavior::default()
            },
            std::time::Duration::from_secs(5),
            1,
            "500",
        )
        .await;
    }

    /// Timeout: the sink parks the response longer than the client's request
    /// timeout, so the POST times out and the relay must re-park, never dead.
    #[tokio::test]
    async fn transient_timeout_requeues_and_rotates_a_fresh_token() {
        assert_transient_requeue(
            SinkBehavior {
                delay_ms: 400,
                ..SinkBehavior::default()
            },
            std::time::Duration::from_millis(100),
            1,
            "transport failed",
        )
        .await;
    }

    /// Claim drift (`IdP` misconfiguration): the token's claims fail
    /// validation, so no POST is ever attempted — the row requeues to rotate
    /// a fresh token (the A5 "行按 transient requeue 轮换新 token" half).
    #[tokio::test]
    async fn transient_claim_drift_requeues_without_any_post() {
        assert_transient_requeue(
            SinkBehavior {
                token_claims: json!({
                    "iss": "https://evil.example.test",
                    "aud": ["audit-governance"],
                    "scope": "audit:event:write",
                    "sub": "aero-im.source",
                    "client_id": "aero-im.source",
                }),
                ..SinkBehavior::default()
            },
            std::time::Duration::from_secs(5),
            0,
            "claim validation failed",
        )
        .await;
    }

    /// M2 posture pin (protocol review finding M2): token-endpoint failures
    /// are ALL transient by the current production classification
    /// (`request_token` bails on any non-200 → `access_token()?` →
    /// `DeliveryError::Transient`). RFC 6749 §5.2 error responses (401
    /// `invalid_client` / 400 `invalid_grant` / 5xx) are NOT distinguished at
    /// the connector — there is no terminal, no distinct alert, and the stub
    /// `/token` historically always answered 200 (zero coverage). This test
    /// pins the CURRENT contract as the regression net: any token-endpoint
    /// failure requeues with the capped backoff (never dead), `posts() == 0`
    /// (no POST without a validated token), `last_error` carries a
    /// per-status fragment, and the row recovers when the identity provider
    /// returns.
    ///
    /// A positive terminal class (401/400 → dead) is a DEFERRED production
    /// classification change (Change-Manifest-gated; would red this test by
    /// design). Three phases, one stub, fake clock as the single domain:
    /// 5xx → 401 → recovery, mirroring the facade drill
    /// `drill_posture_token_endpoint_failures_requeue_never_dead`.
    #[tokio::test]
    async fn token_endpoint_failure_requeues_without_any_post() {
        let stub = StubSink::start().await.expect("start stub");
        let fake = Arc::new(FakeOutbox::new());
        let t0 = OffsetDateTime::now_utc();
        fake.set_now(Some(t0)).await;
        let id = Uuid::new_v4();
        fake.insert(AuditId::from_uuid(id), claim_payload(id), t0)
            .await;

        // Phase 1: 5xx token endpoint (IdP outage) → transient requeue.
        stub.set_behavior(SinkBehavior {
            token_status: Some(500),
            ..SinkBehavior::default()
        })
        .await;
        let config = test_config(&stub);
        let client = AuditClient::new(config.clone()).expect("build audit client");
        let relay = AuditRelay::new(fake.clone(), client, config);
        let claims = fake.claim_due(LEASE, 10).await.expect("claim");
        assert_eq!(claims.len(), 1);
        let token_a = claims[0].claim_token;
        relay
            .deliver_claim(claims.into_iter().next().expect("one claim"))
            .await;
        let row = fake.row(AuditId::from_uuid(id)).await.expect("row");
        assert_eq!(
            row.status,
            FakeStatus::Ready,
            "token failure must requeue, never dead (M2 all-transient)"
        );
        assert_eq!(row.attempts, 1);
        assert_eq!(
            row.available_at,
            t0 + Duration::seconds(1),
            "requeue re-parks with backoff(1) = 1s on the repo clock"
        );
        assert_eq!(row.claim_token, None, "requeue drops the fencing token");
        assert_eq!(row.lease_expires_at, None);
        assert!(
            row.last_error
                .as_deref()
                .is_some_and(|error| error.contains("audit token endpoint returned HTTP 500")),
            "last_error must carry the per-status fragment (got {:?})",
            row.last_error
        );
        assert_eq!(stub.posts(), 0, "no POST without a validated token");
        assert_eq!(stub.token_requests(), 1, "the token path was actually hit");

        // Phase 2: 401 invalid_client (revoked/rotated secret) — the
        // operator-critical case — same transient shape, distinct fragment.
        stub.set_behavior(SinkBehavior {
            token_status: Some(401),
            ..SinkBehavior::default()
        })
        .await;
        fake.set_now(Some(t0 + Duration::seconds(1))).await;
        let reclaimed = fake.claim_due(LEASE, 10).await.expect("reclaim");
        assert_eq!(reclaimed.len(), 1, "requeued row must be claimable again");
        assert_eq!(reclaimed[0].attempts, 2);
        assert_ne!(
            reclaimed[0].claim_token, token_a,
            "requeue must rotate a fresh token"
        );
        relay
            .deliver_claim(reclaimed.into_iter().next().expect("one claim"))
            .await;
        let row = fake.row(AuditId::from_uuid(id)).await.expect("row");
        assert_eq!(
            row.status,
            FakeStatus::Ready,
            "401 must also requeue, never dead — there is no token terminal today"
        );
        assert_eq!(row.attempts, 2);
        assert_eq!(
            row.available_at,
            t0 + Duration::seconds(3),
            "backoff(2) = 2s on the repo clock"
        );
        assert!(
            row.last_error
                .as_deref()
                .is_some_and(|error| error.contains("audit token endpoint returned HTTP 401")),
            "401 must be distinguishable from 5xx via the last_error fragment (got {:?})",
            row.last_error
        );
        assert_eq!(stub.posts(), 0, "still no POST across the failure phases");

        // Phase 3: the IdP returns → the row recovers and settles (the
        // all-transient requeue is a retry ladder, not a dead end).
        stub.set_behavior(SinkBehavior::default()).await;
        fake.set_now(Some(t0 + Duration::seconds(3))).await;
        let recovered = fake.claim_due(LEASE, 10).await.expect("recovery claim");
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].attempts, 3);
        relay
            .deliver_claim(recovered.into_iter().next().expect("one claim"))
            .await;
        let row = fake.row(AuditId::from_uuid(id)).await.expect("row");
        assert_eq!(
            row.status,
            FakeStatus::Delivered,
            "the row must settle once the token endpoint returns"
        );
        assert_eq!(row.attempts, 3);
        assert_eq!(stub.posts(), 1, "exactly one delivery POST, after recovery");
        stub.shutdown();
    }

    #[test]
    fn backoff_is_bounded_and_exponential() {
        let expected = [1, 2, 4, 8, 16, 32, 64, 128, 256];
        for (attempts, seconds) in expected.iter().enumerate() {
            assert_eq!(
                audit_backoff(i64::try_from(attempts + 1).expect("small attempts")),
                Duration::seconds(*seconds),
                "backoff({})",
                attempts + 1
            );
        }
        assert_eq!(audit_backoff(10), Duration::seconds(300));
        assert_eq!(audit_backoff(i64::MAX), Duration::seconds(300));
    }

    #[test]
    fn permanent_dead_threshold_is_pure_and_total() {
        use super::{is_dead_at, PERMANENT_DEAD_AT};
        assert!(!is_dead_at(1), "attempt 1 must requeue (≤1 retry)");
        assert!(is_dead_at(PERMANENT_DEAD_AT), "attempt 2 must be dead");
        assert!(is_dead_at(i64::MAX), "any count past the threshold is dead");
    }

    /// B5-3 D-CAP AC2 — the minimum-service floor is pure, total, and pinned:
    /// `(0)→0, (1)→0, (2)→1, (20)→1, (21)→1, (25)→1, (40)→2, (100)→5,
    /// (500)→25, (i64::MAX)→25` plus the QA F5 negative pin `(i64::MIN)→0`
    /// (clamp → batch 1 → K = 0). Over the whole relay domain `[1,
    /// MAX_CLAIM]` the floor stays below the batch (arm A never empty) and
    /// the batch splits pin the arm sizes (95/5 at 100, 475/25 at 500,
    /// 24/1 at 25).
    #[test]
    fn min_service_floor_is_pinned_and_total() {
        use super::{min_service_floor, MAX_CLAIM};
        let pins = [
            (i64::MIN, 0),
            (0, 0),
            (1, 0),
            (2, 1),
            (20, 1),
            (21, 1),
            (25, 1),
            (40, 2),
            (100, 5),
            (500, 25),
            (i64::MAX, 25),
        ];
        for (batch, expected) in pins {
            assert_eq!(
                min_service_floor(batch),
                expected,
                "min_service_floor({batch})"
            );
        }
        for batch in 1..=MAX_CLAIM {
            let floor = min_service_floor(batch);
            assert!(
                floor < batch,
                "K({batch}) = {floor} must stay below the batch (arm A never empty)"
            );
        }
        assert_eq!(min_service_floor(100), 5, "batch 100 → K = 5");
        assert_eq!(100 - min_service_floor(100), 95, "batch 100 → arm A 95");
        assert_eq!(500 - min_service_floor(500), 475, "batch 500 → arm A 475");
        assert_eq!(25 - min_service_floor(25), 24, "batch 25 → arm A 24");
    }

    #[test]
    fn lease_is_clamped_into_the_proven_bounds() {
        use super::clamped_lease;
        assert_eq!(clamped_lease(Duration::ZERO), Duration::seconds(1));
        assert_eq!(
            clamped_lease(Duration::seconds(1_000_000)),
            Duration::seconds(86_400)
        );
        assert_eq!(clamped_lease(Duration::seconds(30)), Duration::seconds(30));
    }

    /// AC3(b) relay half — a JWKS fetch failure drives the Transient arm with
    /// the exact `transient_claim_drift_requeues_without_any_post` shape:
    /// `FakeStatus::Ready` (requeue, never dead), `attempts == 1`,
    /// `available_at == t0 + backoff(1)` on the pinned fake clock,
    /// `claim_token`/lease cleared, `last_error` carrying the stable `audit
    /// jwks unavailable` fragment, `posts() == 0`, and a rotated fresh token
    /// on the reclaim. (F9 double-pin: the failing key source must never
    /// classify permanent — that would dead every row during a JWKS outage.)
    #[tokio::test]
    async fn jwks_fetch_failure_requeues_without_any_post() {
        // The token must be RS256-signed: an alg:none token would die at the
        // alg gate (D6 order) before the key source is ever consulted.
        assert_transient_requeue_with_keys(
            SinkBehavior {
                signing_key: Some(crate::stub::trusted_key()),
                ..SinkBehavior::default()
            },
            std::time::Duration::from_secs(5),
            0,
            "audit jwks unavailable",
            Some(Arc::new(FailingKeyProvider)),
        )
        .await;
    }

    /// D10/F2/F7 discriminator — a JWKS endpoint that LAGS the `IdP`'s signing
    /// key rotation must not dead the row at attempt 2: the same-kid retry
    /// inside the unknown-kid throttle window earns exactly one bypass
    /// refresh, which recovers once the endpoint catches up. Without D10,
    /// attempt 2's refresh is throttled → stale set → `Ok(None)` →
    /// `mark_dead` at attempt 2.
    #[tokio::test]
    async fn rotation_lag_recovers_via_bypass_before_dead() {
        let (key_a, kid_a) = test_keypair("key-a");
        let (key_b, kid_b) = test_keypair("key-b");
        let stub = StubSink::start().await.expect("start stub");
        stub.set_behavior(SinkBehavior {
            signing_key: Some((key_b.clone(), kid_b.clone())),
            jwks_keys: vec![(key_a.clone(), kid_a.clone())],
            ..SinkBehavior::default()
        })
        .await;
        let fake = Arc::new(FakeOutbox::new());
        let t0 = OffsetDateTime::now_utc();
        fake.set_now(Some(t0)).await;
        let id = Uuid::new_v4();
        fake.insert(AuditId::from_uuid(id), claim_payload(id), t0)
            .await;
        let config = test_config(&stub);
        let client = AuditClient::with_key_provider(
            config.clone(),
            Some(Arc::new(JwksKeyProvider::new(stub.jwks_url()))),
        )
        .expect("build audit client");
        let relay = AuditRelay::new(fake.clone(), client, config);

        // Attempt 1: the IdP signs with key-b while the endpoint still serves
        // {A} — unknown kid after a refresh → permanent signature rejection
        // → requeue with ≤1-retry budget (never dead at attempt 1).
        assert_eq!(relay.dispatch_batch().await.expect("attempt 1"), 1);
        let after_first = fake
            .row(AuditId::from_uuid(id))
            .await
            .expect("row after attempt 1");
        assert_eq!(
            after_first.status,
            FakeStatus::Ready,
            "attempt 1 must requeue, not dead"
        );
        assert_eq!(after_first.attempts, 1);
        assert_eq!(
            after_first.available_at,
            t0 + Duration::seconds(1),
            "requeue must re-park with backoff(1) = 1s on the repo clock"
        );
        assert!(
            after_first
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("SignatureRejected")),
            "last_error must carry the SignatureRejected sentinel"
        );
        assert_eq!(stub.posts(), 0, "signature-plane failure must never POST");

        // The endpoint catches up (serves {A, B}) before attempt 2.
        stub.set_behavior(SinkBehavior {
            signing_key: Some((key_b.clone(), kid_b.clone())),
            jwks_keys: vec![
                (key_a.clone(), kid_a.clone()),
                (key_b.clone(), kid_b.clone()),
            ],
            ..SinkBehavior::default()
        })
        .await;
        fake.set_now(Some(t0 + Duration::seconds(1))).await;
        assert_eq!(relay.dispatch_batch().await.expect("attempt 2"), 1);
        let after_second = fake
            .row(AuditId::from_uuid(id))
            .await
            .expect("row after attempt 2");
        assert_eq!(
            after_second.status,
            FakeStatus::Delivered,
            "the same-kid bypass refresh must recover the row before it deads"
        );
        assert_eq!(stub.posts(), 1, "exactly one delivery POST");
        stub.shutdown();
    }
}
