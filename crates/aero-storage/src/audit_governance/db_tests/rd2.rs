//! R-D2 design-gate resolutions (§10.2 / §10.3 / §10.5) — the governance
//! outbox retention sweep, the writer-failure fail-closed injection, and the
//! version-skew Rust reconciler. All three live here (R1 seam-family split —
//! lanes.rs already carries the parity tests).

use super::*;

/// §10.2 — `sweep_terminal_before`: only TERMINAL rows (status 2/3) older
/// than the TTL are deleted; LIVE rows (status 0/1) of the same age survive
/// (0246 header: "never delete while the relay runs" — a live row is still
/// due for claim). 0 = off is the caller's responsibility (the boot leg
/// returns early; this pins the SQL predicate itself).
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn delete_lane_governance_outbox_retention_sweep() {
    let p = pool();
    reset_governance_table(&p).await;
    let repo = crate::audit_governance::AuditGovernanceOutboxRepo::new(p.clone());
    let old = time::OffsetDateTime::now_utc() - time::Duration::days(400);
    let fresh = time::OffsetDateTime::now_utc();

    // Seed two terminal rows (delivered / dead) and two live rows, one of
    // each age (all with distinct event_ids — PK).
    let mut ids = Vec::new();
    for (i, status) in [2_i32, 3, 0, 1].into_iter().enumerate() {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO audit_governance_outbox (event_id, status, class, priority, payload, created_at)
                 VALUES ($1, $2, 'message', 10, '{\"k\":\"v\"}'::jsonb, $3)",
        )
        .bind(id)
        .bind(status)
        .bind(if i < 2 { old } else { fresh })
        .execute(&p)
        .await
        .expect("seed sweep row");
        ids.push(id);
    }

    let cutoff = time::OffsetDateTime::now_utc() - time::Duration::days(365);
    let swept = repo
        .sweep_terminal_before(cutoff)
        .await
        .expect("sweep commits");
    assert_eq!(swept, 2, "exactly the terminal+old rows are swept");

    let remaining: Vec<(i32,)> =
        sqlx::query_as("SELECT status FROM audit_governance_outbox ORDER BY event_id")
            .fetch_all(&p)
            .await
            .expect("remaining rows");
    let mut statuses: Vec<i32> = remaining.iter().map(|r| r.0).collect();
    statuses.sort_unstable();
    assert_eq!(
        statuses, vec![0, 1],
        "live rows survive; terminal rows deleted"
    );
}

/// §10.3 (round-2 revised) — F2 writer-failure is fail-closed: DROP the
/// outbox table mid-test → the writer's INSERT raises 42P01 → the whole
/// delete transaction rolls back → the message is NOT deleted, zero audit /
/// zero outbox / zero event-outbox rows. (The earlier broken-FK shape was
/// vacuous: 0239 has NO FK on the outbox — a bad workspace fails at the
/// AUDIT append, which is F1's already-pinned coverage.) The table is
/// recreated from the 0239 DDL afterwards so the shared harness DB stays
/// healthy for subsequent tests.
#[tokio::test]
#[ignore = "requires live Postgres (throwaway DB only — drops the outbox table)"]
async fn delete_lane_writer_failure_aborts_delete_fail_closed() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor) = fixture(&p).await;
    let room = crate::room::RoomRepo::new(p.clone())
        .create_in_workspace(
            ws,
            aero_common::RoomKind::Channel,
            Some(format!("writer-failure-{}", uuid::Uuid::new_v4())),
            actor,
        )
        .await
        .expect("create channel room")
        .id;
    let msg = crate::message::MessageRepo::new(p.clone())
        .insert(crate::message::NewMessage {
            room_id: room,
            sender_id: actor,
            blocks: vec![aero_common::Block::text("writer-failure")],
            reply_to: None,
            metadata: serde_json::json!({}),
            expires_at: None,
        })
        .await
        .expect("seed message");

    // Inject at the WRITER (42P01): the audit append precedes the writer, so
    // the delete must fail AT the writer's outbox INSERT — never before.
    // RENAME (not DROP) so the restore is a single reversible statement and
    // the 0239 trigger/indexes come back with the table — the shared
    // harness DB must be whole for every later test in the slot.
    sqlx::query("ALTER TABLE audit_governance_outbox RENAME TO audit_governance_outbox_injected")
        .execute(&p)
        .await
        .expect("rename the outbox table away (fail-closed injection)");

    let err = crate::message::MessageRepo::new(p.clone())
        .soft_delete_outboxed_authorized(msg.id, actor, None)
        .await
        .expect_err("the writer's INSERT must fail the delete (fail-closed)");
    assert!(
        matches!(err, aero_common::Error::Database(_)),
        "writer failure propagates as a DB error, got {err:?}"
    );

    // No partial state: message visible, zero audit rows, zero event-outbox.
    let current = crate::message::MessageRepo::new(p.clone())
        .get(msg.id)
        .await
        .expect("get message")
        .expect("message retained");
    assert!(current.deleted_at.is_none(), "delete rolled back (fail-closed)");
    let audit_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE workspace_id = $1")
            .bind(ws.to_uuid())
            .fetch_one(&p)
            .await
            .expect("audit count");
    assert_eq!(audit_count, 0, "no audit row escaped the rollback");
    // Scope to this message: event_outbox accumulates across the suite run.
    let outbox_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM event_outbox WHERE message_id = $1")
            .bind(msg.id.to_uuid())
            .fetch_one(&p)
            .await
            .expect("event outbox count");
    assert_eq!(outbox_count, 0, "no RoomEvent::Deleted escaped the rollback");

    // Restore: rename the table back (the 0239 trigger + indexes ride the
    // table — the shared harness DB is whole for every later test).
    sqlx::query(
        "ALTER TABLE audit_governance_outbox_injected RENAME TO audit_governance_outbox",
    )
    .execute(&p)
    .await
    .expect("restore the outbox table (rename back)");
    // Sanity: the restored table accepts a normal insert (CHECKs intact).
    sqlx::query(
        "INSERT INTO audit_governance_outbox (event_id, payload)
             VALUES ($1, '{\"k\":\"v\"}'::jsonb)",
    )
    .bind(Uuid::new_v4())
    .execute(&p)
    .await
    .expect("restored table accepts the drill-shape insert");
}

/// §10.5 — `reconcile_message_deleted`: an orphaned `message.deleted` audit
/// row (version-skew: old binary wrote the audit row, no outbox twin) is
/// backfilled into exactly ONE outbox row via the same 16-key envelope;
/// a re-run is a no-op (idempotent, ON CONFLICT). The scan never fabricates
/// a `message.moderated` claim (exact-token) and never touches rows outside
/// the retention window.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn delete_lane_version_skew_backfills_on_reconcile() {
    let p = pool();
    reset_governance_table(&p).await;
    // Module discipline: audit_events accumulates across a run, and the
    // reconcile scan is global — clean this test's OWN orphans (message.deleted
    // audit rows without an outbox twin) so `inserted` counts only the seed.
    sqlx::query(
        r"DELETE FROM audit_events a
           WHERE a.action = 'message.deleted'
             AND NOT EXISTS (
                   SELECT 1 FROM audit_governance_outbox o
                    WHERE o.event_id = a.id)",
    )
    .execute(&p)
    .await
    .expect("clean orphaned message.deleted audit rows");
    let (ws, actor) = fixture(&p).await;
    let room = crate::room::RoomRepo::new(p.clone())
        .create_in_workspace(
            ws,
            aero_common::RoomKind::Channel,
            Some(format!("reconcile-deleted-{}", uuid::Uuid::new_v4())),
            actor,
        )
        .await
        .expect("create channel room")
        .id;
    let msg = crate::message::MessageRepo::new(p.clone())
        .insert(crate::message::NewMessage {
            room_id: room,
            sender_id: actor,
            blocks: vec![aero_common::Block::text("reconcile-deleted")],
            reply_to: None,
            metadata: serde_json::json!({}),
            expires_at: None,
        })
        .await
        .expect("seed message");

    // Orphan: audit row WITHOUT an outbox twin (simulates the old-binary
    // skew window — the writer never ran). Inserted via the bare audit
    // append so no writer fires.
    let mut orphan_tx = p.begin().await.expect("begin orphan tx");
    let audit_id = crate::AuditRepo::append_in_tx(
        &mut orphan_tx,
        ws,
        Some(actor),
        LOCAL_ACTION_MESSAGE_DELETED,
        Some(&msg.id.to_string()),
        serde_json::json!({ "room_id": room }),
    )
    .await
    .expect("orphan audit row");
    orphan_tx.commit().await.expect("commit orphan tx");
    assert_eq!(
        governance_rows_for(&p, ws).await,
        0,
        "orphan state: audit row present, zero outbox rows"
    );

    let repo = crate::audit_governance::AuditGovernanceOutboxRepo::new(p.clone());
    let cutoff = time::OffsetDateTime::now_utc() - time::Duration::days(365);
    let inserted = repo
        .reconcile_message_deleted(cutoff, 50)
        .await
        .expect("reconcile commits");
    assert_eq!(inserted, 1, "the orphan is backfilled exactly once");

    // The backfilled row is byte-identical to the writer's envelope.
    let row: (i32, String, i16, serde_json::Value) = sqlx::query_as(
        "SELECT status, class, priority, payload
               FROM audit_governance_outbox WHERE event_id = $1",
    )
    .bind(audit_id.to_uuid())
    .fetch_one(&p)
    .await
    .expect("backfilled row");
    assert_eq!(row.0, 0, "status 0 = enqueued");
    assert_eq!(
        row.1, GOVERNANCE_CLASS_MESSAGE,
        "class 'message' (GOVERNANCE_CLASS_MESSAGE)"
    );
    assert_eq!(row.2, 10, "priority 10 (GOVERNANCE_PRIORITY_BACKLOG)");
    let created_at: time::OffsetDateTime =
        sqlx::query_scalar("SELECT created_at FROM audit_events WHERE id = $1")
            .bind(audit_id.to_uuid())
            .fetch_one(&p)
            .await
            .expect("audit created_at");
    let expected_occurred = aero_common::audit_wire_occurred_at(created_at);
    assert_eq!(
        row.3["occurred_at"].as_str().expect("occurred_at string"),
        expected_occurred,
        "reconciler envelope uses the canonical occurred_at spelling"
    );
    assert_eq!(
        row.3["source_system"], AUDIT_SOURCE_SYSTEM,
        "source_system = AUDIT_SOURCE_SYSTEM"
    );
    assert_eq!(
        row.3["action"], LOCAL_ACTION_MESSAGE_DELETED,
        "action verbatim"
    );
    assert_eq!(
        row.3["idempotency_key"],
        audit_id.to_uuid().to_string(),
        "idempotency_key = event_id"
    );
    let parsed: AuditClaimPayload = serde_json::from_value(row.3.clone())
        .expect("backfilled payload parses into the typed twin");
    assert_eq!(
        serde_json::to_value(&parsed).unwrap(),
        row.3,
        "re-serialized twin equals the stored JSONB"
    );

    // Re-run is a no-op (idempotent, ON CONFLICT).
    let again = repo
        .reconcile_message_deleted(cutoff, 50)
        .await
        .expect("reconcile re-run");
    assert_eq!(again, 0, "second reconcile inserts nothing");
    assert_eq!(
        governance_rows_for(&p, ws).await,
        1,
        "still exactly one row (at-most-one)"
    );

    // Exact-token scan: a `message.moderated` orphan is NEVER backfilled by
    // the delete reconciler (the 0241 admin reconciler owns that token).
    let mut mod_tx = p.begin().await.expect("begin moderation orphan tx");
    let mod_id = crate::AuditRepo::append_in_tx(
        &mut mod_tx,
        ws,
        None,
        aero_common::LOCAL_ACTION_MODERATED,
        Some(&msg.id.to_string()),
        serde_json::json!({ "reason": "spam" }),
    )
    .await
    .expect("moderation orphan");
    mod_tx.commit().await.expect("commit moderation orphan tx");
    let inserted2 = repo
        .reconcile_message_deleted(cutoff, 50)
        .await
        .expect("reconcile after moderation orphan");
    assert_eq!(inserted2, 0, "message.moderated stays out of the delete scan");
    let mod_row: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_governance_outbox WHERE event_id = $1",
    )
    .bind(mod_id.to_uuid())
    .fetch_one(&p)
    .await
    .expect("moderation row count");
    assert_eq!(mod_row, 0, "no row fabricated for the moderation orphan");
}
