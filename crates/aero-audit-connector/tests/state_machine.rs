//! A1 — connector unit tests against the in-memory fake outbox + relay
//! policy, mirroring the proven `ai_usage` `db_tests` assertions
//! (`crates/aero-storage/src/ai_usage/tests.rs`). No database required.
//!
//! Clock discipline: the fake is the *only* clock (single clock domain,
//! PG analog `clock_timestamp()`). Tests pin it via `set_now` and never
//! pass a `now` into the repo — so every lease mint, backoff arithmetic,
//! and fence is deterministic, and the |skew| >= lease livelock regression
//! (`skew_gt_lease_cannot_livelock_claim_fence_settle`) is pinned with the
//! fake clock held ahead of a simulated relay wall clock.

use std::sync::Arc;

use aero_audit_connector::client::AuditClient;
use aero_audit_connector::config::RelayConfig;
use aero_audit_connector::fake::{FakeOutbox, FakeStatus, StaticScopeProvisioner};
use aero_audit_connector::outbox::OutboxRepo;
use aero_audit_connector::relay::{audit_backoff, is_dead_at, AuditRelay, PERMANENT_DEAD_AT};
use aero_audit_connector::stub::{SinkBehavior, StubSink};
use aero_auth::{KeyProvider, StaticKeyProvider};
use aero_common::AuditId;
use rand::rngs::OsRng;
use reqwest::Url;
use rsa::RsaPrivateKey;
use serde_json::json;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

const LEASE: Duration = Duration::seconds(30);

/// Production-mirror payload builder: keeps its `Uuid` parameter so the
/// payload's embedded `event_id` stays the 0239 trigger format (hyphenated
/// `uuid::text`) while the typed claim id is `AuditId` — the D1 divergent-
/// format scenario. Do NOT switch this to `AuditId` + `to_string()`: that
/// would mask the receipt-comparison regression by lockstep construction.
fn claim_payload(event_id: Uuid) -> serde_json::Value {
    json!({
        "event_id": event_id.to_string(),
        "source_system": "aero-im.source",
    })
}

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
        provision_freshness: std::time::Duration::from_secs(300),
    }
}

fn relay(stub: &StubSink, fake: Arc<FakeOutbox>) -> AuditRelay {
    relay_with_keys(stub, fake, None)
}

fn relay_with_keys(
    stub: &StubSink,
    fake: Arc<FakeOutbox>,
    keys: Option<Arc<dyn KeyProvider>>,
) -> AuditRelay {
    let config = test_config(stub);
    let client = AuditClient::with_key_provider(config.clone(), keys).expect("build audit client");
    AuditRelay::new(fake, client, config)
        .with_scope_provisioner(Arc::new(StaticScopeProvisioner::new(true)))
}

/// B5-4 `AC1a` — an unconfigured relay is fail-closed at the claim boundary:
/// it performs no claim and leaves the durable row in status 0 with no token.
#[tokio::test]
async fn relay_disabled_rows_never_claimed() {
    let fake = Arc::new(FakeOutbox::new());
    let now = OffsetDateTime::now_utc();
    fake.set_now(Some(now)).await;
    let id = AuditId::from_uuid(Uuid::new_v4());
    fake.insert(id, claim_payload(id.to_uuid()), now).await;

    let stub = StubSink::start().await.expect("start stub");
    let config = test_config(&stub);
    let client = AuditClient::new(config.clone()).expect("build audit client");
    // No scope provisioner is intentionally installed: this is the T-11
    // disabled/unconfigured path, not a transport failure.
    let relay = AuditRelay::new(fake.clone(), client, config);

    assert_eq!(relay.dispatch_batch().await.expect("disabled dispatch"), 0);
    let row = fake.row(id).await.expect("row snapshot");
    assert_eq!(row.status, FakeStatus::Ready);
    assert_eq!(row.status.code(), 0);
    assert!(row.claim_token.is_none());
    assert_eq!(stub.posts(), 0, "a disabled relay must never POST");
    stub.shutdown();
}

/// A1-1 — lease expiry makes a row reclaimable with a rotated token; the
/// stale token can never ack (mirrors `stale_claim_cannot_ack_after_reclaim`).
#[tokio::test]
async fn stale_token_cannot_ack_after_reclaim() {
    let fake = Arc::new(FakeOutbox::new());
    let t0 = OffsetDateTime::now_utc();
    let id = Uuid::new_v4();
    fake.set_now(Some(t0)).await;
    fake.insert(AuditId::from_uuid(id), claim_payload(id), t0)
        .await;

    let first = fake
        .claim_due(Duration::seconds(1), 10)
        .await
        .expect("first claim");
    assert_eq!(first.len(), 1);
    let token_a = first[0].claim_token;
    assert_eq!(
        first[0].lease_expires_at,
        t0 + Duration::seconds(1),
        "lease must be minted on the repo clock (fake clock == PG clock_timestamp())"
    );

    fake.set_now(Some(t0 + Duration::seconds(2))).await;
    let second = fake
        .claim_due(Duration::seconds(1), 10)
        .await
        .expect("reclaim after lease expiry");
    assert_eq!(
        second.len(),
        1,
        "expired lease must make the row reclaimable"
    );
    let token_b = second[0].claim_token;
    assert_ne!(token_a, token_b, "claim must rotate a fresh fencing token");

    assert!(
        !fake
            .settle(AuditId::from_uuid(id), token_a)
            .await
            .expect("stale settle"),
        "stale token must never acknowledge"
    );
    assert!(
        fake.settle(AuditId::from_uuid(id), token_b)
            .await
            .expect("current settle"),
        "current token must acknowledge"
    );
    assert!(
        fake.claim_due(LEASE, 10)
            .await
            .expect("claim after settle")
            .is_empty(),
        "settled row must leave the claimable set"
    );
}

/// A1-2 — backoff sequence capped at 300s and requeue re-parks with it
/// (mirrors `backoff_is_bounded_and_exponential`).
#[tokio::test]
async fn backoff_is_bounded_and_exponential() {
    assert_eq!(audit_backoff(1), Duration::seconds(1));
    assert_eq!(audit_backoff(2), Duration::seconds(2));
    assert_eq!(audit_backoff(3), Duration::seconds(4));
    assert_eq!(audit_backoff(9), Duration::seconds(256));
    assert_eq!(audit_backoff(10), Duration::seconds(300));
    assert_eq!(audit_backoff(i64::MAX), Duration::seconds(300));

    let fake = Arc::new(FakeOutbox::new());
    let t0 = OffsetDateTime::now_utc();
    let id = Uuid::new_v4();
    fake.set_now(Some(t0)).await;
    fake.insert(AuditId::from_uuid(id), claim_payload(id), t0)
        .await;
    let claims = fake.claim_due(LEASE, 10).await.expect("claim");
    assert_eq!(claims[0].attempts, 1);
    assert!(fake
        .requeue(AuditId::from_uuid(id), claims[0].claim_token, 1, "boom")
        .await
        .expect("requeue"));
    let row = fake.row(AuditId::from_uuid(id)).await.expect("row");
    assert_eq!(
        row.available_at,
        t0 + Duration::seconds(1),
        "requeue must re-park with backoff(attempts) on the repo clock"
    );
    assert_eq!(row.attempts, 1);
    assert_eq!(row.status, FakeStatus::Ready);
}

/// A1-3 — 422/409/receipt-mismatch are permanent: requeued on attempt 1,
/// dead on attempt 2 (≤1 retry), excluded from future claims.
#[tokio::test]
async fn permanent_error_dead_after_exactly_two_attempts() {
    for behavior in [
        SinkBehavior {
            events_status: 422,
            ..SinkBehavior::default()
        },
        SinkBehavior {
            events_status: 409,
            ..SinkBehavior::default()
        },
        SinkBehavior {
            events_status: 202,
            receipt_valid: false,
            ..SinkBehavior::default()
        },
    ] {
        let stub = StubSink::start().await.expect("start stub");
        stub.set_behavior(behavior).await;
        let fake = Arc::new(FakeOutbox::new());
        let t0 = OffsetDateTime::now_utc();
        let id = Uuid::new_v4();
        // Pin the fake clock: claim, requeue arithmetic, and fences all run
        // on t0, so `available_at` is exactly `t0 + backoff(1)` — no wall
        // clock window (single clock domain).
        fake.set_now(Some(t0)).await;
        fake.insert(AuditId::from_uuid(id), claim_payload(id), t0)
            .await;
        let relay = relay(&stub, fake.clone());

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
        assert_eq!(
            after_first.attempts, 1,
            "attempt 1 must not be dead yet (is_dead_at(1) == false)"
        );
        assert!(
            !is_dead_at(after_first.attempts),
            "A2/A3 must route attempt 1 through requeue, never dead"
        );
        assert_eq!(
            after_first.available_at,
            t0 + Duration::seconds(1),
            "requeue must re-park with backoff(1) = 1s on the repo clock"
        );
        assert!(
            after_first
                .last_error
                .as_deref()
                .is_some_and(|error| { error.contains("permanent") || error.contains("receipt") }),
            "last_error must record the permanent class"
        );

        fake.make_due_now(AuditId::from_uuid(id)).await;
        assert_eq!(relay.dispatch_batch().await.expect("attempt 2"), 1);
        let after_second = fake
            .row(AuditId::from_uuid(id))
            .await
            .expect("row after attempt 2");
        assert_eq!(
            after_second.status,
            FakeStatus::Dead,
            "attempt 2 must reach the dead terminal"
        );
        assert_eq!(
            after_second.attempts, PERMANENT_DEAD_AT,
            "attempt {PERMANENT_DEAD_AT} must be dead (is_dead_at threshold)"
        );
        assert!(
            is_dead_at(after_second.attempts),
            "A2/A3 must route attempt {PERMANENT_DEAD_AT} through the dead terminal"
        );
        assert!(
            fake.claim_due(LEASE, 10)
                .await
                .expect("claim after dead")
                .is_empty(),
            "dead rows must never be claimed again"
        );
        stub.shutdown();
    }
}

/// A1-4 — HTTP 403 dies immediately on attempt 1 (T-11 fail-closed): exactly
/// one attempt, no requeue, excluded from future claims.
#[tokio::test]
async fn forbidden_dead_on_first_attempt() {
    let stub = StubSink::start().await.expect("start stub");
    stub.set_behavior(SinkBehavior {
        events_status: 403,
        ..SinkBehavior::default()
    })
    .await;
    let fake = Arc::new(FakeOutbox::new());
    let t0 = OffsetDateTime::now_utc();
    let id = Uuid::new_v4();
    fake.set_now(Some(t0)).await;
    fake.insert(AuditId::from_uuid(id), claim_payload(id), t0)
        .await;
    let relay = relay(&stub, fake.clone());

    assert_eq!(relay.dispatch_batch().await.expect("attempt 1"), 1);
    let row = fake.row(AuditId::from_uuid(id)).await.expect("row");
    assert_eq!(row.status, FakeStatus::Dead, "403 must dead immediately");
    assert_eq!(row.attempts, 1, "exactly one attempt recorded");
    assert_eq!(stub.posts(), 1, "exactly one delivery POST");
    assert!(fake
        .claim_due(LEASE, 10)
        .await
        .expect("claim after dead")
        .is_empty());
    stub.shutdown();
}

/// A1-5 — happy path: 202 + valid receipt settles and removes the row from
/// the claimable set (mirrors `stable_reservation_and_settlement_are_exactly_once`).
#[tokio::test]
async fn happy_path_settles_and_removes_from_claimable() {
    let stub = StubSink::start().await.expect("start stub");
    let fake = Arc::new(FakeOutbox::new());
    let t0 = OffsetDateTime::now_utc();
    let id = Uuid::new_v4();
    fake.set_now(Some(t0)).await;
    fake.insert(AuditId::from_uuid(id), claim_payload(id), t0)
        .await;
    let relay = relay(&stub, fake.clone());

    assert_eq!(relay.dispatch_batch().await.expect("dispatch"), 1);
    let row = fake.row(AuditId::from_uuid(id)).await.expect("row");
    assert_eq!(row.status, FakeStatus::Delivered);
    assert_eq!(row.claim_token, None);
    assert_eq!(row.lease_expires_at, None);
    assert!(fake
        .claim_due(LEASE, 10)
        .await
        .expect("claim after settle")
        .is_empty());
    stub.shutdown();
}

/// A1-6 — skew > lease must not livelock a row in claim→POST→fence-fail.
///
/// The old (ai_usage-cloned) design minted `lease_expires_at` on the relay's
/// app clock while fences read the repo clock; a repo clock ahead of the
/// claim `now` by >= lease made the *first* fence fail, and the claim filter
/// (comparing against the same behind-clock `now`) never re-exposed the row
/// — claim→POST→fence-fail forever, no settle, no dead. The fix: the lease
/// is minted on the repo clock alone (`clock_timestamp() + lease` in PG;
/// the fake's pinned clock here). Regression pin: fake clock held 3× lease
/// ahead of a simulated relay wall clock; claim + first fence must succeed
/// in the same repo-clock instant.
#[tokio::test]
async fn skew_gt_lease_cannot_livelock_claim_fence_settle() {
    let fake = Arc::new(FakeOutbox::new());
    let relay_wall = OffsetDateTime::now_utc();
    let repo_clock = relay_wall + Duration::seconds(90); // |skew| = 90s > 30s lease
    fake.set_now(Some(repo_clock)).await;
    let id = Uuid::new_v4();
    fake.insert(AuditId::from_uuid(id), claim_payload(id), repo_clock)
        .await;

    let claims = fake.claim_due(LEASE, 10).await.expect("claim");
    assert_eq!(claims.len(), 1);
    assert_eq!(
        claims[0].lease_expires_at,
        repo_clock + LEASE,
        "lease must be minted on the repo clock (PG: clock_timestamp() + lease), \
         never a caller-supplied now — otherwise a repo clock ahead by >= lease \
         mints an already-expired lease and livelocks the row"
    );
    assert!(
        fake.settle(AuditId::from_uuid(id), claims[0].claim_token)
            .await
            .expect("settle"),
        "first fence must succeed at the same repo-clock instant: skew cannot enter the fence"
    );
    assert!(
        fake.claim_due(LEASE, 10)
            .await
            .expect("claim after settle")
            .is_empty(),
        "settled row must leave the claimable set"
    );
}

/// A1-7 (B5-3 R5) — the governance lane preempts FIFO: a moderation row
/// enqueued LAST with a LATER `available_at` wins `claims[0]` purely on
/// `priority DESC` (only priority can explain its precedence), and `limit=1`
/// hands the single slot to the top lane. Vec position IS the fake's
/// ordering contract (D1 — the fake builds its claims from its sorted due
/// list; PG tests stay set-based, D3).
#[tokio::test]
async fn priority_first_claim_preempts_fifo_and_limit1_keeps_top_lane() {
    const BACKLOG_PRIORITY: i16 = 10; // governance.rs:33
    const BACKLOG_CLASS: &str = "message"; // governance.rs:39
    const MODERATION_PRIORITY: i16 = 100; // governance.rs:31
    const MODERATION_CLASS: &str = "admin"; // governance.rs:37

    let t0 = OffsetDateTime::now_utc();

    // Backlog lane literals pinned to aero_ai::governance
    // (crates/aero-ai/src/governance.rs): priority 10 =
    // GOVERNANCE_PRIORITY_BACKLOG (:33); class "message" =
    // GOVERNANCE_CLASS_MESSAGE (:39). Moderation lane: priority 100 =
    // GOVERNANCE_PRIORITY_MODERATION (:31); class "admin" =
    // GOVERNANCE_CLASS_ADMIN (:37).
    // Section 1 — full drain: 20 backlog rows due at t0 − 200s + i (earlier
    // `available_at`, FIFO-ordered) plus ONE moderation row due at
    // t0 − 100s, strictly LATER than every backlog row and enqueued last —
    // only `priority DESC` can put it first.
    let fake = FakeOutbox::new();
    fake.set_now(Some(t0)).await;
    let n_backlog = 20;
    let mut backlog_ids = Vec::with_capacity(n_backlog);
    for i in 0..n_backlog {
        let id = Uuid::new_v4();
        fake.insert_lane(
            AuditId::from_uuid(id),
            claim_payload(id),
            t0 - Duration::seconds(200) + Duration::seconds(i64::try_from(i).expect("fits")),
            BACKLOG_PRIORITY,
            BACKLOG_CLASS.into(),
        )
        .await;
        backlog_ids.push(id);
    }
    let mod_id = Uuid::new_v4();
    fake.insert_lane(
        AuditId::from_uuid(mod_id),
        claim_payload(mod_id),
        t0 - Duration::seconds(100),
        MODERATION_PRIORITY,
        MODERATION_CLASS.into(),
    )
    .await;

    let claims = fake
        .claim_due(LEASE, i64::try_from(n_backlog + 1).expect("fits"))
        .await
        .expect("drain all due rows");
    assert_eq!(claims.len(), n_backlog + 1, "all due rows must be claimed");
    assert_eq!(
        claims[0].event_id,
        AuditId::from_uuid(mod_id),
        "moderation lane must claim first despite later available_at and enqueue order"
    );
    assert_eq!(claims[0].priority, MODERATION_PRIORITY);
    assert_eq!(claims[0].class, MODERATION_CLASS);
    let got: Vec<AuditId> = claims.iter().map(|claim| claim.event_id).collect();
    let expected_backlog: Vec<AuditId> = backlog_ids
        .iter()
        .map(|id| AuditId::from_uuid(*id))
        .collect();
    assert_eq!(
        &got[1..],
        expected_backlog.as_slice(),
        "backlog lane must stay FIFO (available_at, created_at, event_id) ascending"
    );
    let row = fake
        .row(AuditId::from_uuid(mod_id))
        .await
        .expect("moderation row snapshot");
    assert_eq!(
        row.priority, MODERATION_PRIORITY,
        "snapshot exposes priority"
    );
    assert_eq!(row.class, MODERATION_CLASS, "snapshot exposes class");

    // Section 2 — `limit = 1` on a fresh fake with the same seed shape: the
    // single slot goes to the top lane (drill batch-membership at unit scale).
    let fake = FakeOutbox::new();
    fake.set_now(Some(t0)).await;
    for i in 0..n_backlog {
        let id = Uuid::new_v4();
        fake.insert_lane(
            AuditId::from_uuid(id),
            claim_payload(id),
            t0 - Duration::seconds(200) + Duration::seconds(i64::try_from(i).expect("fits")),
            BACKLOG_PRIORITY,
            BACKLOG_CLASS.into(),
        )
        .await;
    }
    fake.insert_lane(
        AuditId::from_uuid(mod_id),
        claim_payload(mod_id),
        t0 - Duration::seconds(100),
        MODERATION_PRIORITY,
        MODERATION_CLASS.into(),
    )
    .await;
    let one = fake.claim_due(LEASE, 1).await.expect("limit=1 claim");
    assert_eq!(one.len(), 1, "limit=1 claims exactly one row");
    assert_eq!(
        one[0].event_id,
        AuditId::from_uuid(mod_id),
        "the single slot must go to the top lane"
    );
}

/// Signature-plane permanent (D4/AC4 shape, mirror of
/// `permanent_error_dead_after_exactly_two_attempts`): a token signed by a
/// key outside the trusted set requeues on attempt 1 (≤1 retry) and deads on
/// attempt 2 — never transient, never a requeue-forever loop. The relay's
/// permanent arm is kind-generic; `last_error` carries the
/// `SignatureRejected` sentinel.
#[tokio::test]
async fn signature_rejected_dead_after_exactly_two_attempts() {
    let mut rng = OsRng;
    let foreign = RsaPrivateKey::new(&mut rng, 2048).expect("foreign rsa keygen");
    let stub = StubSink::start().await.expect("start stub");
    stub.set_behavior(SinkBehavior {
        signing_key: Some((foreign, "foreign-1".into())),
        ..SinkBehavior::default()
    })
    .await;
    let fake = Arc::new(FakeOutbox::new());
    let t0 = OffsetDateTime::now_utc();
    let id = Uuid::new_v4();
    fake.set_now(Some(t0)).await;
    fake.insert(AuditId::from_uuid(id), claim_payload(id), t0)
        .await;
    let relay = relay_with_keys(
        &stub,
        fake.clone(),
        Some(Arc::new(StaticKeyProvider::single(stub.decoding_key()))),
    );

    assert_eq!(relay.dispatch_batch().await.expect("attempt 1"), 1);
    let after_first = fake
        .row(AuditId::from_uuid(id))
        .await
        .expect("row after attempt 1");
    assert_eq!(
        after_first.status,
        FakeStatus::Ready,
        "signature rejection must requeue on attempt 1 (≤1 retry), never dead"
    );
    assert_eq!(after_first.attempts, 1);
    assert!(!is_dead_at(after_first.attempts));
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
    assert!(
        !after_first
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("transient")),
        "a signature rejection must never be classified transient"
    );
    assert_eq!(stub.posts(), 0, "signature-plane failure must never POST");

    fake.make_due_now(AuditId::from_uuid(id)).await;
    assert_eq!(relay.dispatch_batch().await.expect("attempt 2"), 1);
    let after_second = fake
        .row(AuditId::from_uuid(id))
        .await
        .expect("row after attempt 2");
    assert_eq!(
        after_second.status,
        FakeStatus::Dead,
        "attempt 2 must reach the dead terminal"
    );
    assert_eq!(after_second.attempts, PERMANENT_DEAD_AT);
    assert!(is_dead_at(after_second.attempts));
    assert!(
        fake.claim_due(LEASE, 10)
            .await
            .expect("claim after dead")
            .is_empty(),
        "dead rows must never be claimed again"
    );
    stub.shutdown();
}
