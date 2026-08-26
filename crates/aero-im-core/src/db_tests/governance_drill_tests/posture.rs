//! Connector-posture facade drills (M1/M2 — D11/D12): pin the CURRENT
//! relay/connector operational contract at facade level, through the REAL
//! DB→relay→client→sink chain (design §8 / resolution-m1-m2-posture.md §4).
//!
//! Posture notes (verbatim from the resolution, also in `room.rs`):
//!   * M1: Facade drills run JWKS-off (`jwks_uri: None` ⇒
//!     `verify_token_signature` unconditional Ok — the production default).
//!     The signature plane is pinned at facade level by
//!     `drill_posture_jwks_on_*` and at relay-unit level by
//!     `jwks_fetch_failure_requeues_without_any_post` /
//!     `rotation_lag_recovers_via_bypass_before_dead`. Production MUST set
//!     `AERO_AUDIT_JWKS_URL`; the drill suite does not change that default.
//!   * M2: Token-endpoint failures are ALL transient by current production
//!     classification (RFC 6749 §5.2 error responses are not distinguished;
//!     any non-200 → Transient → infinite requeue, capped 300s backoff, no
//!     terminal). `drill_posture_token_endpoint_failures_requeue_never_dead`
//!     pins that contract (never dead, `posts() == 0`, per-status
//!     `last_error` fragment, recovery). A terminal class for 401/400 is a
//!     DEFERRED production change (Change Manifest); it must red this drill.
//!
//! Fixture discipline mirrors the room-lane drills: self-isolate → start-of-
//! test singleton re-assert (enforcement OFF — 0245 gate-free) →
//! workspace/room → exactly 1 class-'room' row. Relay config = the pinned
//! `AUDIT_SOURCE_SYSTEM` (0245 stamps it). No manual `claim_due` — the relay
//! is the sole claimer (DR-1 pattern, no live-lease conflict).

use aero_audit_connector::{
    client::AuditClient,
    config::RelayConfig,
    fake::StaticScopeProvisioner,
    pg::PgOutboxRepo,
    relay::AuditRelay,
    stub::{trusted_key, SinkBehavior, StubSink},
};
use aero_common::{AuditId, RoomKind, AUDIT_SOURCE_SYSTEM, LOCAL_ACTION_ROOM_CREATE};
use std::sync::Arc;

use super::shared::*;
use super::*;

/// `drill_config` with `jwks_uri: Some(stub.jwks_url())` — builds a real
/// `JwksKeyProvider` inside `AuditClient::new` that fetches the stub's
/// `/jwks` over HTTP (no in-process key sharing; `trusted_key()` is
/// deterministic and both the mint and the served set agree by construction).
fn relay_for_jwks(
    pool: &PgPool,
    stub: &StubSink,
    source_system: &str,
) -> (AuditRelay, Arc<PgOutboxRepo>, RelayConfig) {
    let mut config = drill_config(stub, source_system);
    config.jwks_uri = Some(Url::parse(&stub.jwks_url()).expect("stub JWKS URL"));
    let repo = Arc::new(PgOutboxRepo::new(pool.clone()));
    let client = AuditClient::new(config.clone()).expect("build audit client with JWKS provider");
    (
        AuditRelay::new(repo.clone(), client, config.clone())
            .with_scope_provisioner(Arc::new(StaticScopeProvisioner::new(true))),
        repo,
        config,
    )
}

/// One room-create row (D11/D12 fixture): self-isolate → singleton re-assert
/// (enforcement OFF) → workspace/room → the 1:1 class-'room' outbox row's
/// audit id.
async fn posture_room_row(
    pool: &PgPool,
    svc: &crate::service::ImService,
    prefix: &str,
) -> (WorkspaceId, AuditId) {
    self_isolate(pool).await;
    restore_enforcement_disabled(pool).await;
    let (ws, owner) = workspace_fixture(pool, prefix).await;
    svc.create_room_in_workspace(owner, ws, RoomKind::Channel, Some(format!("{prefix}-room")))
        .await
        .expect("create room in workspace");
    let (audit_id,): (Uuid,) =
        sqlx::query_as("SELECT id FROM audit_events WHERE workspace_id = $1 AND action = $2")
            .bind(ws.to_uuid())
            .bind(LOCAL_ACTION_ROOM_CREATE)
            .fetch_one(pool)
            .await
            .expect("exactly one room.create audit row");
    (ws, AuditId::from_uuid(audit_id))
}

/// D11 — M2 current-posture pin: token-endpoint failures are ALL transient
/// (any non-200 → Transient → requeue with capped backoff, never dead), and
/// recovery settles. Three phases on one stub + one room row, deterministic
/// re-due between phases (no sleeps). The drill asserts the ABSENCE of a
/// token terminal today; adding one is a production change that must red
/// this drill and carry a Change Manifest.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_posture_token_endpoint_failures_requeue_never_dead() {
    let pool = pool();
    let svc = service(pool.clone());
    let (ws, event_id) = posture_room_row(&pool, &svc, "drill-posture-m2").await;

    let stub = StubSink::start().await.expect("start stub");
    stub.set_behavior(SinkBehavior {
        token_status: Some(500),
        ..SinkBehavior::default()
    })
    .await;
    let (relay, _repo, _config) = relay_for(&pool, &stub, AUDIT_SOURCE_SYSTEM).await;

    // Phase 1: 5xx token endpoint → transient requeue (attempts 1, never
    // dead, zero POSTs — the token path was really hit, non-vacuous).
    assert_eq!(
        relay.dispatch_batch().await.expect("phase 1"),
        1,
        "claims the room row"
    );
    let row = outbox_row(&pool, event_id).await;
    assert_eq!(row.status, 0, "5xx → requeue, never dead");
    assert_eq!(row.attempts, 1, "attempts 1");
    assert!(row.delivered_at.is_none(), "never delivered");
    assert!(
        row.last_error
            .as_deref()
            .is_some_and(|e| e.contains("audit token endpoint returned HTTP 500")),
        "last_error names the 5xx status (got {:?})",
        row.last_error
    );
    assert_eq!(stub.posts(), 0, "zero POSTs without a validated token");
    assert_eq!(stub.token_requests(), 1, "the token path was hit once");

    // Phase 2: 401 invalid_client on the refresh → still transient across
    // cycles (attempts 2, still status 0, per-status fragment, zero POSTs).
    stub.set_behavior(SinkBehavior {
        token_status: Some(401),
        ..SinkBehavior::default()
    })
    .await;
    force_re_due(&pool, event_id).await;
    assert_eq!(
        relay.dispatch_batch().await.expect("phase 2"),
        1,
        "reclaims after re-due"
    );
    let row = outbox_row(&pool, event_id).await;
    assert_eq!(
        row.status, 0,
        "401 → still transient requeue (never dead — the all-transient contract)"
    );
    assert_eq!(row.attempts, 2, "attempts 2 across failure cycles");
    assert!(
        row.last_error
            .as_deref()
            .is_some_and(|e| e.contains("HTTP 401")),
        "last_error names the 401 status (got {:?})",
        row.last_error
    );
    assert_eq!(stub.posts(), 0, "still zero POSTs");

    // Phase 3: recovery — the requeue ladder ends in a settle, not a dead
    // end: status 2, delivered_at set, the one real POST.
    stub.set_behavior(SinkBehavior::default()).await;
    force_re_due(&pool, event_id).await;
    assert_eq!(
        relay.dispatch_batch().await.expect("phase 3"),
        1,
        "reclaims after recovery"
    );
    let row = outbox_row(&pool, event_id).await;
    assert_eq!(row.status, 2, "recovery settles to status 2");
    assert!(row.delivered_at.is_some(), "delivered_at stamped");
    assert_eq!(row.attempts, 3, "three claims across the three phases");
    assert!(row.last_error.is_none(), "no error recorded on settle");
    assert_eq!(stub.posts(), 1, "exactly one real POST (after recovery)");

    cleanup_facade_rows(&pool, ws, &[], &[event_id], &[]).await;
}

/// D12 — M1 JWKS-on facade leg: with `jwks_uri` configured, the REAL
/// `verify_token_signature` chain gates the signature plane — `alg:none` is
/// dead ≤1 retry with zero POSTs, RS256 settles status 2. Two legs on their
/// own stubs + room rows (row A deads in leg 1; row B is the leg-2 subject).
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_posture_jwks_on_rejects_alg_none_and_settles_rs256() {
    let pool = pool();
    let svc = service(pool.clone());
    let (ws, row_a) = posture_room_row(&pool, &svc, "drill-posture-m1a").await;

    // Leg 1: JWKS serves a real key but the token is minted `alg:none`
    // (signing_key: None) → SignatureRejected, requeued once, then dead —
    // zero POSTs ever (the token never passes validation).
    let stub = StubSink::start().await.expect("start stub");
    stub.set_behavior(SinkBehavior {
        jwks_keys: vec![trusted_key()],
        ..SinkBehavior::default()
    })
    .await;
    let (relay, _repo, _config) = relay_for_jwks(&pool, &stub, AUDIT_SOURCE_SYSTEM);
    assert_eq!(
        relay.dispatch_batch().await.expect("leg 1 round 1"),
        1,
        "claims row A"
    );
    let row = outbox_row(&pool, row_a).await;
    assert_eq!(row.status, 0, "attempt 1 requeues (≤1 retry before dead)");
    assert_eq!(row.attempts, 1);
    assert!(
        row.last_error
            .as_deref()
            .is_some_and(|e| e.contains("SignatureRejected")),
        "last_error names the signature rejection (got {:?})",
        row.last_error
    );
    assert_eq!(stub.posts(), 0, "alg:none never reaches the sink");
    force_re_due(&pool, row_a).await;
    assert_eq!(
        relay.dispatch_batch().await.expect("leg 1 round 2"),
        1,
        "reclaims row A"
    );
    let row = outbox_row(&pool, row_a).await;
    assert_eq!(row.status, 3, "alg:none → dead at attempt 2 (≤1 retry)");
    assert_eq!(row.attempts, 2, "budget counts claims: exactly two");
    assert!(row.delivered_at.is_none(), "never delivered");
    assert_eq!(stub.posts(), 0, "still zero POSTs");
    assert!(
        !claim_predicate_holds(&pool, row_a.to_uuid()).await,
        "dead is a real terminal"
    );

    // Leg 2: RS256-signed token + matching JWKS → full chain settles.
    let (ws2, row_b) = posture_room_row(&pool, &svc, "drill-posture-m1b").await;
    let stub = StubSink::start().await.expect("start stub");
    stub.set_behavior(SinkBehavior {
        signing_key: Some(trusted_key()),
        jwks_keys: vec![trusted_key()],
        ..SinkBehavior::default()
    })
    .await;
    let (relay, _repo, _config) = relay_for_jwks(&pool, &stub, AUDIT_SOURCE_SYSTEM);
    assert_eq!(
        relay.dispatch_batch().await.expect("leg 2"),
        1,
        "claims row B"
    );
    let row = outbox_row(&pool, row_b).await;
    assert_eq!(row.status, 2, "RS256 settles to status 2 through JWKS");
    assert!(row.delivered_at.is_some(), "delivered_at stamped");
    assert_eq!(row.attempts, 1, "exactly one claim");
    assert!(row.last_error.is_none(), "no error recorded on settle");
    assert_eq!(stub.posts(), 1, "exactly one real POST");

    cleanup_facade_rows(&pool, ws, &[], &[row_a], &[]).await;
    cleanup_facade_rows(&pool, ws2, &[], &[row_b], &[]).await;
}
