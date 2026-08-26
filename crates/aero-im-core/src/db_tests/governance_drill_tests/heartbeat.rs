//! Heartbeat freshness-gate + scope-provisioning drills (B5-4 R7): the
//! settle-face freshness rejection (S1–S3) and the scope-missing immediate
//! dead (S4) over write-path rows — see the parent module doc for the slot
//! map and the load-bearing invariants (start-of-test singleton re-assert,
//! `source_system` pin, set-parity, self-isolation).
//!
//! These drills exercise the decorator with a LOCAL [`DrillRecorder`] (drills
//! cannot depend on aero-server — same pattern as the `AlwaysProvisioned`
//! local structs): the decorator's settle consults the recorder's freshness
//! gate, delegates on fresh, and records on fenced `Ok(true)`. The drills
//! seed absent/stale states directly (the server's 60s tick is not present
//! here — liveness refresh is manual, which is exactly what pins the
//! decorator semantics).

use std::sync::Arc;

use aero_audit_connector::{
    client::AuditClient,
    fake::StaticScopeProvisioner,
    heartbeat::{HeartbeatOutboxRepo, HeartbeatRecorder},
    outbox::OutboxRepo,
    pg::PgOutboxRepo,
    relay::AuditRelay,
    stub::{SinkBehavior, StubSink},
};
use aero_storage::audit_relay_provision::{AuditRelayProvisionRepo, ProvisionCheck};
use async_trait::async_trait;
use serde_json::json;
use time::Duration as TimeDuration;
use uuid::Uuid;

use super::shared::*;
use super::*;

/// The drill's heartbeat recorder: a pure delegate over the storage repo
/// with the drill's freshness window (300s — the `RelayConfig` default, so
/// the 600s stale seed is exactly 2× the window, an implicit default pin).
struct DrillRecorder {
    repo: AuditRelayProvisionRepo,
    freshness: TimeDuration,
}

#[async_trait]
impl HeartbeatRecorder for DrillRecorder {
    async fn record_heartbeat(&self) -> Result<(), sqlx::Error> {
        self.repo.record_heartbeat().await
    }

    async fn heartbeat_fresh(&self) -> Result<bool, sqlx::Error> {
        Ok(matches!(
            self.repo.provision_check(self.freshness).await,
            Ok(ProvisionCheck::Verified(_))
        ))
    }
}

/// Self-isolating heartbeat state: the singleton row is global — a re-run or
/// a crashed sibling test must never leak a stale/absent state into the
/// freshness assertions.
async fn reset_heartbeat(pool: &PgPool) {
    sqlx::query("DELETE FROM audit_relay_provisioning")
        .execute(pool)
        .await
        .expect("reset the provisioning singleton");
}

/// Seed (or refresh) the heartbeat row at `verified_at` (DB clock).
async fn seed_heartbeat(pool: &PgPool, seconds_ago: i64) {
    sqlx::query(
        r"INSERT INTO audit_relay_provisioning (singleton, verified_at)
          VALUES (TRUE, clock_timestamp() - make_interval(secs => $1))
          ON CONFLICT (singleton) DO UPDATE
             SET verified_at = clock_timestamp() - make_interval(secs => $1),
                 updated_at = clock_timestamp()",
    )
    .bind(seconds_ago)
    .execute(pool)
    .await
    .expect("seed heartbeat");
}

/// The current `verified_at` (None = no row).
async fn verified_at(pool: &PgPool) -> Option<time::OffsetDateTime> {
    sqlx::query_scalar("SELECT verified_at FROM audit_relay_provisioning WHERE singleton = TRUE")
        .fetch_optional(pool)
        .await
        .expect("read verified_at")
}

/// Build a decorated relay: `HeartbeatOutboxRepo` over `PgOutboxRepo` with a
/// `DrillRecorder` (no server dependency).
fn heartbeat_relay_for(
    pool: &PgPool,
    stub: &StubSink,
    source_system: &str,
) -> (AuditRelay, Arc<PgOutboxRepo>, Arc<DrillRecorder>) {
    let repo = Arc::new(PgOutboxRepo::new(pool.clone()));
    let recorder = Arc::new(DrillRecorder {
        repo: AuditRelayProvisionRepo::new(pool.clone()),
        freshness: TimeDuration::seconds(300),
    });
    let decorated: Arc<dyn OutboxRepo> =
        Arc::new(HeartbeatOutboxRepo::new(repo.clone(), recorder.clone()));
    let config = drill_config(stub, source_system);
    let client = AuditClient::new(config.clone()).expect("build audit client");
    let relay = AuditRelay::new(decorated, client, config)
        .with_scope_provisioner(Arc::new(StaticScopeProvisioner::new(true)));
    (relay, repo, recorder)
}

/// S1 — stale heartbeat: the settle is rejected fail-closed (row stays
/// claimed, status never 2), the lease-reclaim loop re-claims and re-rejects,
/// and only an operator refresh lets the fenced settle fire — exactly once.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_stale_heartbeat_rejects_settle_row_reclaimed() {
    let pool = pool();
    let svc = service(pool.clone());
    let rows = write_path_rows(&pool, &svc, "drill-s1", 1).await;
    let event_id = rows.event_ids[0];
    reset_heartbeat(&pool).await;
    // 2× the 300s default freshness — implicit default pin (leg B3b shape).
    seed_heartbeat(&pool, 600).await;

    let stub = StubSink::start().await.expect("start stub");
    let (relay, repo, _recorder) = heartbeat_relay_for(&pool, &stub, &rows.source);

    // Round 1: claim 1, deliver 202, settle REJECTED (stale) — status stays
    // claimed, never delivered.
    assert_eq!(
        relay.dispatch_batch().await.expect("round 1"),
        1,
        "round 1 claims the write-path row"
    );
    assert_eq!(stub.posts(), 1, "the delivery POST succeeds (sink 202)");
    let row = outbox_row(&pool, event_id).await;
    assert_eq!(
        row.status, 1,
        "stale heartbeat rejects the acknowledgement; row stays claimed"
    );
    assert!(row.delivered_at.is_none(), "never acknowledged");
    assert_eq!(row.attempts, 1, "the rejection does not consume retries");

    // Round 2: lease expiry → the reclaim loop re-claims the SAME row with a
    // fresh token and a fresh attempt — and the settle is rejected again.
    force_lease_expiry(&pool, event_id).await;
    assert_eq!(
        relay.dispatch_batch().await.expect("round 2"),
        1,
        "expired lease re-exposes the row"
    );
    let row = outbox_row(&pool, event_id).await;
    assert_eq!(row.attempts, 2, "reclaim increments the claim budget");
    assert_eq!(
        row.status, 1,
        "still rejected — status never 2 across both rounds"
    );
    assert!(row.delivered_at.is_none());
    assert_eq!(
        stub.posts(),
        2,
        "exactly two real POSTs (replays dedup at the sink)"
    );

    // Operator refresh (the runbook's `UPDATE verified_at = clock_timestamp()`)
    // → round 3 settles exactly once. The lease minted by round 2's reclaim
    // must expire first (the claim filter gates on it).
    force_lease_expiry(&pool, event_id).await;
    sqlx::query(
        "UPDATE audit_relay_provisioning
            SET verified_at = clock_timestamp(), updated_at = clock_timestamp()
          WHERE singleton",
    )
    .execute(&pool)
    .await
    .expect("operator refresh");
    assert_eq!(
        relay.dispatch_batch().await.expect("round 3"),
        1,
        "fresh heartbeat admits the fenced settle"
    );
    let row = outbox_row(&pool, event_id).await;
    assert_eq!(row.status, 2, "fresh heartbeat → delivered");
    assert!(row.delivered_at.is_some(), "delivered_at stamped");

    // Direct second settle on the settled row → false (the fence
    // `status IN (0,1)` fails — no second status-2 transition = never
    // double-settled), and the record-on-fenced-settle advanced `verified_at`.
    let second = repo
        .settle(event_id, Uuid::new_v4())
        .await
        .expect("direct settle");
    assert!(!second, "the fenced settle fires at most once per row");

    cleanup_drill_rows(&pool, &rows).await;
    reset_heartbeat(&pool).await;
}

/// S2 — absent heartbeat (fail-closed NotVerified): the settle is rejected,
/// the row stays claimed and reclaimable (lease expiry re-exposes it), and
/// NO heartbeat row ever appears (no record without a fenced settle).
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_absent_heartbeat_rejects_settle() {
    let pool = pool();
    let svc = service(pool.clone());
    let rows = write_path_rows(&pool, &svc, "drill-s2", 1).await;
    let event_id = rows.event_ids[0];
    reset_heartbeat(&pool).await;
    assert!(
        verified_at(&pool).await.is_none(),
        "absent row = fail-closed NotVerified"
    );

    let stub = StubSink::start().await.expect("start stub");
    let (relay, _repo, _recorder) = heartbeat_relay_for(&pool, &stub, &rows.source);

    // Round 1: claim 1, deliver 202, settle rejected (absent row).
    assert_eq!(relay.dispatch_batch().await.expect("round 1"), 1);
    assert_eq!(stub.posts(), 1);
    let row = outbox_row(&pool, event_id).await;
    assert_eq!(row.status, 1, "absent heartbeat rejects the settle");
    assert_eq!(row.attempts, 1);
    assert!(row.delivered_at.is_none(), "never 2");

    // The row is NOT lost: once its lease expires it re-enters the claim
    // predicate (the reclaim machinery — the T-11 "row reclaimed" arm).
    force_lease_expiry(&pool, event_id).await;
    assert!(
        claim_predicate_holds(&pool, event_id.to_uuid()).await,
        "the row remains reclaimable after the rejection"
    );
    assert_eq!(
        relay.dispatch_batch().await.expect("round 2"),
        1,
        "round 2 reclaims the row"
    );
    let row = outbox_row(&pool, event_id).await;
    assert_eq!(row.attempts, 2, "reclaimed with a fresh attempt");
    assert_eq!(row.status, 1, "still rejected — never 2");
    assert!(row.delivered_at.is_none());

    // No heartbeat row ever appears: the rejection path never records.
    assert!(
        verified_at(&pool).await.is_none(),
        "no record without a fenced settle (settle-only pin)"
    );

    cleanup_drill_rows(&pool, &rows).await;
    reset_heartbeat(&pool).await;
}

/// S3 — fresh heartbeat (control): the settle fires, `verified_at` advances
/// on the record-on-fenced-settle, and a fenced `Ok(false)` settle (wrong
/// token) does NOT refresh `verified_at`.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_fresh_heartbeat_settles_and_refreshes() {
    let pool = pool();
    let svc = service(pool.clone());
    let rows = write_path_rows(&pool, &svc, "drill-s3", 1).await;
    let event_id = rows.event_ids[0];
    reset_heartbeat(&pool).await;
    seed_heartbeat(&pool, 0).await;
    let before = verified_at(&pool).await.expect("seeded row");

    let stub = StubSink::start().await.expect("start stub");
    let (relay, _repo, _recorder) = heartbeat_relay_for(&pool, &stub, &rows.source);

    // Round 1: fresh heartbeat → the fenced settle fires; the row delivers
    // and the record-on-fenced-settle advances `verified_at` strictly.
    assert_eq!(relay.dispatch_batch().await.expect("round 1"), 1);
    let row = outbox_row(&pool, event_id).await;
    assert_eq!(row.status, 2, "fresh heartbeat admits the settle");
    assert!(row.delivered_at.is_some());
    let after_settle = verified_at(&pool).await.expect("row exists");
    assert!(
        after_settle > before,
        "record-on-fenced-settle must advance verified_at"
    );

    // A fenced Ok(false) settle (wrong token) does NOT refresh (settle-only
    // pin: only a fenced Ok(true) records).
    let rejected = wrong_token_decorator_settle(&pool, event_id, Uuid::new_v4()).await;
    assert!(!rejected, "a wrong-token settle must fence false");
    assert_eq!(
        verified_at(&pool).await.expect("row exists"),
        after_settle,
        "a fenced-false settle must not refresh verified_at"
    );

    cleanup_drill_rows(&pool, &rows).await;
    reset_heartbeat(&pool).await;
}

/// Direct decorated-settle helper for S3's wrong-token leg (a fresh decorator
/// over the same DB — the gate is fresh, the inner fence rejects).
async fn wrong_token_decorator_settle(pool: &PgPool, event_id: AuditId, token: Uuid) -> bool {
    let repo = Arc::new(PgOutboxRepo::new(pool.clone()));
    let recorder = Arc::new(DrillRecorder {
        repo: AuditRelayProvisionRepo::new(pool.clone()),
        freshness: TimeDuration::seconds(300),
    });
    let decorated: Arc<dyn OutboxRepo> =
        Arc::new(HeartbeatOutboxRepo::new(repo.clone(), recorder.clone()));
    decorated
        .settle(event_id, token)
        .await
        .expect("decorator settle")
}

/// S4 — scope-provisioning auto-feedback: a client-credentials token whose
/// grant lacks `audit:event:write` is rejected pre-POST and deads IMMEDIATELY
/// (Forbidden-class T-11) with the exact `ScopeRejected` `last_error`, zero
/// retries, `delivered_at` None, and the heartbeat untouched (no settle ever
/// fired). The fixture carries a valid typed-gate shape (`sub`/`client_id`)
/// + a non-granting scope — the typed gate runs BEFORE `check_scope`, so a
/// bare scope claim would fail as Other (Transient) and never reach the
/// `ScopeMissing` classification (QA F-1).
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_scope_missing_client_credentials_deads_immediately() {
    let pool = pool();
    let svc = service(pool.clone());
    let rows = write_path_rows(&pool, &svc, "drill-s4", 1).await;
    let event_id = rows.event_ids[0];
    reset_heartbeat(&pool).await;

    let stub = StubSink::start().await.expect("start stub");
    stub.set_behavior(SinkBehavior {
        token_claims: json!({
            "iss": "https://idp.example.test",
            "aud": ["audit-governance"],
            "scope": "read:audit",
            "sub": "aero-im.source",
            "client_id": "aero-im.source",
        }),
        events_status: 202,
        ..SinkBehavior::default()
    })
    .await;
    let (relay, _repo, _config) = relay_for(&pool, &stub, &rows.source).await;

    assert_eq!(
        relay.dispatch_batch().await.expect("single round"),
        1,
        "the write-path row is claimed"
    );
    assert_eq!(
        stub.posts(),
        0,
        "scope-missing is rejected pre-POST (posts == 0)"
    );
    let row = outbox_row(&pool, event_id).await;
    assert_eq!(
        row.status, 3,
        "scope-missing → immediate dead (Forbidden-class T-11)"
    );
    assert_eq!(row.attempts, 1, "zero retries for the provisioning fault");
    assert_eq!(
        row.last_error.as_deref(),
        Some("audit:event:write scope missing from the client credentials token (T-11)"),
        "exact ScopeRejected terminal string"
    );
    assert!(row.delivered_at.is_none(), "never delivered");

    // Heartbeat untouched: no settle ever fired, so no record-on-fenced-settle.
    assert!(
        verified_at(&pool).await.is_none(),
        "the dead row never refreshes the heartbeat (403-loop causal chain)"
    );

    cleanup_drill_rows(&pool, &rows).await;
    reset_heartbeat(&pool).await;
}
