//! Relay-terminal drills (R5–R8) over write-path rows — see the parent
//! module doc for the slot map and the load-bearing invariants (start-of-test
//! singleton re-assert, `source_system` pin, set-parity, self-isolation).

use std::sync::Arc;

use aero_audit_connector::{
    client::AuditClient,
    pg::PgOutboxRepo,
    relay::AuditRelay,
    stub::{SinkBehavior, StubSink},
};
use uuid::Uuid;

use super::*;

// ---------------------------------------------------------------------------
// R5/R6 — permanent terminals on write-path rows (AC3a/AC3b).
// ---------------------------------------------------------------------------

/// 422 is a permanent class: attempt 1 requeues with backoff(1) = 1s, the
/// 1.2s re-poll reclaims (attempts = 2) and `is_dead_at(2)` reaches the dead
/// terminal — status 3, `delivered_at` NULL, `last_error` naming the class.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_422_permanent_deads_at_attempt_two() {
    let pool = pool();
    let svc = service(pool.clone());
    let rows = write_path_rows(&pool, &svc, "drill-r5", 1).await;
    let event_id = rows.event_ids[0];

    let stub = StubSink::start().await.expect("start stub");
    stub.set_behavior(SinkBehavior {
        events_status: 422,
        ..SinkBehavior::default()
    })
    .await;
    let (relay, _repo, _config) = relay_for(&pool, &stub, &rows.source).await;

    assert_eq!(
        relay.dispatch_batch().await.expect("round 1"),
        1,
        "round 1 claims the write-path row"
    );
    let row = outbox_row(&pool, event_id).await;
    assert_eq!(row.status, 0, "attempt 1 requeues, never dead");
    assert_eq!(row.attempts, 1);
    assert!(
        row.last_error
            .as_deref()
            .is_some_and(|e| e.contains("Unprocessable")),
        "last_error names the permanent class (got {:?})",
        row.last_error
    );
    assert_eq!(stub.posts(), 1);

    // 1.2s margin > backoff(1) = 1s (pg.rs reclaim-test precedent).
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert_eq!(
        relay.dispatch_batch().await.expect("round 2"),
        1,
        "round 2 reclaims after the backoff"
    );
    let row = outbox_row(&pool, event_id).await;
    assert_eq!(row.status, 3, "dead terminal at attempt 2 (≤1 retry)");
    assert_eq!(row.attempts, 2);
    assert!(row.delivered_at.is_none(), "never delivered");
    assert!(
        row.last_error
            .as_deref()
            .is_some_and(|e| e.contains("classified permanent: Unprocessable")),
        "last_error pins the dead cause (got {:?})",
        row.last_error
    );
    assert_eq!(stub.posts(), 2, "exactly two real POSTs");

    cleanup_drill_rows(&pool, &rows).await;
}

/// 403 is fail-closed IMMEDIATE death (T-11): one round, `attempts == 1`,
/// exact Forbidden-arm `last_error` — no retry budget is consumed.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_403_forbidden_deads_immediately() {
    let pool = pool();
    let svc = service(pool.clone());
    let rows = write_path_rows(&pool, &svc, "drill-r6", 1).await;
    let event_id = rows.event_ids[0];

    let stub = StubSink::start().await.expect("start stub");
    stub.set_behavior(SinkBehavior {
        events_status: 403,
        ..SinkBehavior::default()
    })
    .await;
    let (relay, _repo, _config) = relay_for(&pool, &stub, &rows.source).await;

    assert_eq!(
        relay.dispatch_batch().await.expect("single round"),
        1,
        "the write-path row is claimed"
    );
    let row = outbox_row(&pool, event_id).await;
    assert_eq!(row.status, 3, "403 → immediate dead (T-11 fail-closed)");
    assert_eq!(row.attempts, 1, "zero retries for the identity fault");
    assert_eq!(stub.posts(), 1);
    assert_eq!(
        row.last_error.as_deref(),
        Some("audit sink rejected the service identity (HTTP 403)"),
        "exact relay Forbidden-arm string"
    );
    assert!(row.delivered_at.is_none());

    cleanup_drill_rows(&pool, &rows).await;
}

// ---------------------------------------------------------------------------
// R7/R8 — T-11 relay-absent / closed-endpoint arms (AC4).
// ---------------------------------------------------------------------------

/// Closed token endpoint (bind + drop, t11-drill shape): the relay is
/// present but the sink is unreachable, so every claim fails transiently and
/// requeues — rows stay status 0 FOREVER, attempts grow N→2N across two
/// rounds (non-vacuous), `last_error` carries the stable transport fragment.
/// Reconciler-overrides-switch doc-pin: the 0241 reconciler is
/// runtime-gate-free and runs ahead of every claim batch, so the switch is
/// an enqueue-time gate only — not a delivery gate.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_closed_endpoint_t11_pending_with_attempts_growth() {
    let pool = pool();
    let svc = service(pool.clone());
    let rows = write_path_rows(&pool, &svc, "drill-r7", 3).await;

    let stub = StubSink::start().await.expect("start stub");
    let config = drill_config(&stub, &rows.source);
    stub.shutdown(); // closed endpoint: the listener is dropped (bind + drop)
    let repo = Arc::new(PgOutboxRepo::new(pool.clone()));
    let client = AuditClient::new(config.clone()).expect("build audit client");
    let relay = AuditRelay::new(repo.clone(), client, config);

    assert_eq!(
        relay.dispatch_batch().await.expect("round 1"),
        3,
        "round 1 claims all 3 write-path rows"
    );
    assert_eq!(
        count_status(&pool, &rows.event_ids, 0).await,
        3,
        "all requeued"
    );
    assert_eq!(
        count_status(&pool, &rows.event_ids, 1).await
            + count_status(&pool, &rows.event_ids, 2).await
            + count_status(&pool, &rows.event_ids, 3).await,
        0,
        "zero rows in any terminal/claimed state"
    );
    assert_eq!(sum_attempts(&pool, &rows.event_ids).await, 3);
    assert_eq!(
        count_error_fragment(
            &pool,
            &rows.event_ids,
            "audit connector HTTP transport failed"
        )
        .await,
        3,
        "every row records the stable transport failure"
    );

    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert_eq!(
        relay.dispatch_batch().await.expect("round 2"),
        3,
        "round 2 reclaims after the backoff"
    );
    assert_eq!(
        sum_attempts(&pool, &rows.event_ids).await,
        6,
        "attempts grow N→2N (non-vacuous — the relay is really trying)"
    );
    assert_eq!(
        count_status(&pool, &rows.event_ids, 0).await,
        3,
        "still pending"
    );
    assert_eq!(
        count_status(&pool, &rows.event_ids, 3).await,
        0,
        "never dead"
    );

    cleanup_drill_rows(&pool, &rows).await;
}

/// Relay never constructed: write-path rows stay status 0 with zero attempts
/// and no error — never silently delivered, never stuck. The claim-predicate
/// mirror (pg.rs `claim_due` WHERE verbatim) returns all N: due/claimable.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_relay_absent_rows_stay_status_zero() {
    let pool = pool();
    let svc = service(pool.clone());
    let rows = write_path_rows(&pool, &svc, "drill-r8", 3).await;

    assert_eq!(
        count_status(&pool, &rows.event_ids, 0).await,
        3,
        "all rows enqueued, never delivered without a relay"
    );
    assert_eq!(sum_attempts(&pool, &rows.event_ids).await, 0, "zero claims");
    assert_eq!(
        count_null_errors(&pool, &rows.event_ids).await,
        3,
        "no errors"
    );

    // Claim-predicate mirror (pg.rs:78-88 WHERE verbatim, cross-pin comment):
    // every row satisfies the due predicate — the T-11 arm is "relay absent",
    // never "row stuck".
    let ids: Vec<Uuid> = rows.event_ids.iter().map(AuditId::to_uuid).collect();
    let due: i64 = sqlx::query_scalar(
        r"SELECT COUNT(*)::bigint FROM audit_governance_outbox
           WHERE event_id = ANY($1)
             AND status IN (0, 1)
             AND available_at <= clock_timestamp()
             AND (lease_expires_at IS NULL OR lease_expires_at <= clock_timestamp())",
    )
    .bind(&ids)
    .fetch_one(&pool)
    .await
    .expect("claim-predicate mirror");
    assert_eq!(due, 3, "all rows are due/claimable");

    cleanup_drill_rows(&pool, &rows).await;
}
