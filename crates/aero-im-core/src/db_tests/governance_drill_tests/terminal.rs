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

use super::shared::*;
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

// ---------------------------------------------------------------------------
// R-D2 AC-2b — the message-delete lane through the REAL relay (drill).
// ---------------------------------------------------------------------------

/// AC-2b happy path: N `message.deleted` rows produced by the REAL
/// `svc.delete_message` seam (R-D2 writer — gate-free, class 'message',
/// priority 10, `source_system = AUDIT_SOURCE_SYSTEM`) claim/deliver
/// (202 + receipt)/settle through the real `AuditRelay` + `StubSink` to
/// status 2. Status-2 set-parity `{event_id}` == the seeded delete set.
/// The relay config pins `source_system = AUDIT_SOURCE_SYSTEM` (the R-D2
/// writer stamps the leaf const — unlike moderation rows which stamp the
/// binding value; both must equal the relay's config or PayloadGuard deads).
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_message_deleted_lane_delivers_through_real_relay() {
    let pool = pool();
    let svc = service(pool.clone());
    self_isolate(&pool).await;
    restore_enforcement_disabled(&pool).await; // 0242/0245/R-D2 writer are all gate-free
    let (ws, owner) = workspace_fixture(&pool, "drill-del").await;
    let room = svc
        .create_room_in_workspace(owner, ws, RoomKind::Channel, Some("drill-del-room".into()))
        .await
        .expect("create room")
        .id;
    let mut message_ids = Vec::new();
    let mut event_ids = Vec::new();
    for i in 0..3 {
        let msg = svc
            .send_message(
                owner,
                room,
                vec![Block::text(format!("drill-del message {i}"))],
                None,
                None,
            )
            .await
            .expect("send message");
        svc.delete_message(owner, msg.id).await.expect("delete message");
        let audit_id: Uuid = sqlx::query_scalar(
            "SELECT id FROM audit_events
              WHERE workspace_id = $1 AND target = $2::text AND action = 'message.deleted'",
        )
        .bind(ws.to_uuid())
        .bind(msg.id.to_string())
        .fetch_one(&pool)
        .await
        .expect("exactly one delete audit row");
        event_ids.push(AuditId::from_uuid(audit_id));
        message_ids.push(msg.id);
    }
    // Scrub the seam's own rows (room.create / message.create window rows) —
    // the drill's claim set must hold EXACTLY the delete rows (priority-10
    // order, D3 set-parity).
    sqlx::query(
        "DELETE FROM audit_governance_outbox WHERE COALESCE(payload->>'action','') <> 'message.deleted'",
    )
    .execute(&pool)
    .await
    .expect("scrub seam rows");
    assert_eq!(
        count_status(&pool, &event_ids, 0).await,
        3,
        "exactly three delete rows enqueued"
    );

    let stub = StubSink::start().await.expect("start stub");
    let (relay, _repo, _config) =
        relay_for(&pool, &stub, aero_common::AUDIT_SOURCE_SYSTEM).await;
    let claimed = relay.dispatch_batch().await.expect("dispatch");
    assert_eq!(claimed, 3, "all three delete rows claimed in one round");
    assert_eq!(stub.posts(), 3, "three real POSTs");
    assert_eq!(
        count_status(&pool, &event_ids, 2).await,
        3,
        "all three settle to status 2 (202 + receipt echo)"
    );
    let ids: Vec<Uuid> = event_ids.iter().map(AuditId::to_uuid).collect();
    let settled: HashSet<Uuid> = sqlx::query_scalar(
        "SELECT event_id FROM audit_governance_outbox
          WHERE status = 2 AND event_id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(&pool)
    .await
    .expect("settled ids")
    .into_iter()
    .collect();
    assert_eq!(
        settled,
        ids.into_iter().collect::<HashSet<_>>(),
        "status-2 set-parity == the seeded delete set"
    );
    for id in &event_ids {
        let row = outbox_row(&pool, *id).await;
        assert!(row.delivered_at.is_some(), "delivered_at stamped");
        assert_eq!(row.attempts, 1, "exactly one claim (relay sole claimer)");
        assert!(row.claim_token.is_none(), "fencing token cleared on settle");
    }

    let rows = DrillRows {
        ws,
        source: aero_common::AUDIT_SOURCE_SYSTEM.into(),
        event_ids,
        message_ids,
    };
    cleanup_drill_rows(&pool, &rows).await;
}

/// AC-2b negative controls: direct-seeded corrupted rows dead at ≤1 retry
/// with the permanent class recorded in `last_error`. (1) `payload.event_id`
/// ≠ PK → the stub echoes the payload's event_id → `ReceiptMismatch` → dead
/// at attempt 2; (2) `payload.source_system` mismatch → `PayloadGuard`
/// (pre-POST) → dead at attempt 2. No POST is ever attempted for the
/// PayloadGuard row.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_delete_lane_permanent_negatives_dead() {
    let pool = pool();
    let svc = service(pool.clone());
    self_isolate(&pool).await;
    restore_enforcement_disabled(&pool).await;
    let (ws, owner) = workspace_fixture(&pool, "drill-del-neg").await;
    let room = svc
        .create_room_in_workspace(owner, ws, RoomKind::Channel, Some("drill-del-neg-room".into()))
        .await
        .expect("create room")
        .id;
    // One real delete row (the fixture's own — the negatives must coexist
    // with a conforming row to prove isolation, not just self-death).
    let msg = svc
        .send_message(owner, room, vec![Block::text("drill-del-neg")], None, None)
        .await
        .expect("send message");
    svc.delete_message(owner, msg.id).await.expect("delete message");
    sqlx::query(
        "DELETE FROM audit_governance_outbox WHERE COALESCE(payload->>'action','') <> 'message.deleted'",
    )
    .execute(&pool)
    .await
    .expect("scrub seam rows");

    let receipt_mismatch_id = Uuid::new_v4();
    let payload_event_id = Uuid::new_v4();
    sqlx::query(
        r"INSERT INTO audit_governance_outbox
                (event_id, payload, status, attempts, priority, available_at, created_at)
          VALUES ($1, $2, 0, 0, 10, clock_timestamp(), clock_timestamp())",
    )
    .bind(receipt_mismatch_id)
    .bind(serde_json::json!({
        "event_id": payload_event_id.to_string(), // ≠ PK → receipt echo mismatches
        "source_system": aero_common::AUDIT_SOURCE_SYSTEM,
        "action": "message.deleted",
    }))
    .execute(&pool)
    .await
    .expect("seed receipt-mismatch row");
    let payload_guard_id = Uuid::new_v4();
    sqlx::query(
        r"INSERT INTO audit_governance_outbox
                (event_id, payload, status, attempts, priority, available_at, created_at)
          VALUES ($1, $2, 0, 0, 10, clock_timestamp(), clock_timestamp())",
    )
    .bind(payload_guard_id)
    .bind(serde_json::json!({
        "event_id": payload_guard_id.to_string(),
        "source_system": "source-wrong", // ≠ relay config → PayloadGuard pre-POST
        "action": "message.deleted",
    }))
    .execute(&pool)
    .await
    .expect("seed payload-guard row");

    let stub = StubSink::start().await.expect("start stub");
    let (relay, _repo, _config) =
        relay_for(&pool, &stub, aero_common::AUDIT_SOURCE_SYSTEM).await;

    // Round 1: the conforming delete row settles; both negatives requeue
    // (permanent attempt-1 — the ≤1-retry budget is not yet spent).
    assert_eq!(
        relay.dispatch_batch().await.expect("round 1"),
        3,
        "round 1 claims the delete row + both negatives"
    );
    assert_eq!(stub.posts(), 2, "one POST for the conforming row + one for the receipt-mismatch row; the payload-guard row never POSTs");
    let conforming = outbox_row(&pool, AuditId::from_uuid(
        sqlx::query_scalar(
            "SELECT id FROM audit_events
              WHERE action = 'message.deleted' AND target = $1::text",
        )
            .bind(msg.id.to_string())
            .fetch_one(&pool)
            .await
            .expect("delete audit id"),
    ))
    .await;
    assert_eq!(conforming.status, 2, "conforming row settles");

    let rm = outbox_row(&pool, AuditId::from_uuid(receipt_mismatch_id)).await;
    assert_eq!(rm.status, 0, "receipt-mismatch requeues on attempt 1");
    assert_eq!(rm.attempts, 1);
    assert!(
        rm.last_error
            .as_deref()
            .is_some_and(|e| e.contains("ReceiptMismatch")),
        "last_error records the permanent class (got {:?})",
        rm.last_error
    );
    let pg = outbox_row(&pool, AuditId::from_uuid(payload_guard_id)).await;
    assert_eq!(pg.status, 0, "payload-guard requeues on attempt 1");
    assert_eq!(pg.attempts, 1);
    assert!(
        pg.last_error
            .as_deref()
            .is_some_and(|e| e.contains("PayloadGuard")),
        "last_error records the permanent class (got {:?})",
        pg.last_error
    );

    // Round 2: reclaim after the backoff → both negatives reach the dead
    // terminal (status 3) at `PERMANENT_DEAD_AT` (≤1 retry).
    force_re_due(&pool, AuditId::from_uuid(receipt_mismatch_id)).await;
    force_re_due(&pool, AuditId::from_uuid(payload_guard_id)).await;
    assert_eq!(
        relay.dispatch_batch().await.expect("round 2"),
        2,
        "round 2 reclaims both negatives"
    );
    let rm = outbox_row(&pool, AuditId::from_uuid(receipt_mismatch_id)).await;
    assert_eq!(rm.status, 3, "receipt-mismatch dead at attempt 2");
    assert_eq!(rm.attempts, 2);
    assert!(rm.delivered_at.is_none(), "never delivered");
    let pg = outbox_row(&pool, AuditId::from_uuid(payload_guard_id)).await;
    assert_eq!(pg.status, 3, "payload-guard dead at attempt 2");
    assert_eq!(pg.attempts, 2);
    assert!(pg.delivered_at.is_none(), "never delivered");
    assert_eq!(stub.posts(), 2, "the payload-guard row never produced a POST");

    // Cleanup: synthetic rows have no audit twins — delete by outbox id.
    sqlx::query(
        "DELETE FROM audit_governance_outbox WHERE event_id = ANY($1)",
    )
    .bind(vec![receipt_mismatch_id, payload_guard_id])
    .execute(&pool)
    .await
    .expect("clean synthetic rows");
    let rows = DrillRows {
        ws,
        source: aero_common::AUDIT_SOURCE_SYSTEM.into(),
        event_ids: vec![AuditId::from_uuid(
            sqlx::query_scalar(
                "SELECT id FROM audit_events
                  WHERE action = 'message.deleted' AND target = $1::text",
            )
                .bind(msg.id.to_string())
                .fetch_one(&pool)
                .await
                .expect("delete audit id"),
        )],
        message_ids: vec![msg.id],
    };
    cleanup_drill_rows(&pool, &rows).await;
}
