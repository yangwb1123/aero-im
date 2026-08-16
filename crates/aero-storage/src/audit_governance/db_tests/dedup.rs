use super::*;

/// F6 / §6.4 dedup: the trigger's `ON CONFLICT (event_id) DO NOTHING`
/// swallows a second fire for the same audit id (`audit_events`' composite
/// PK `(id, created_at)` admits a same-id/different-created_at duplicate
/// at the DB level), and a plain duplicate INSERT raises `unique_violation`
/// — the pinned `UNIQUE(event_id)` dedup contract the sibling's redirect
/// must also honor with its own ON CONFLICT (handoff H4).
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn duplicate_event_id_is_deduped_by_on_conflict() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, _actor) = fixture(&p).await;
    enable_enforcement_with_binding(&p, ws).await;

    let event_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO audit_events (id, workspace_id, action, detail)
             VALUES ($1, $2, 'message.moderated', '{\"reason\":\"spam\"}'::jsonb)",
    )
    .bind(event_id)
    .bind(ws.to_uuid())
    .execute(&p)
    .await
    .expect("first audit row");
    assert_eq!(
        count_governance_rows(&p).await,
        1,
        "first fire enqueues one governance row"
    );

    // Clear the v1 row first: v1's UNIQUE(destination, idempotency_key)
    // would otherwise abort the second fire — that coexistence is the
    // sibling redirect's concern, not this test's.
    sqlx::query(
        "DELETE FROM snaplink_delivery_outbox
              WHERE destination = 'audit' AND idempotency_key = $1",
    )
    .bind(event_id.to_string())
    .execute(&p)
    .await
    .expect("clear v1 row");

    sqlx::query(
        "INSERT INTO audit_events (id, workspace_id, action, detail, created_at)
             VALUES ($1, $2, 'message.moderated', '{\"reason\":\"spam\"}'::jsonb,
                     clock_timestamp() + interval '1 second')",
    )
    .bind(event_id)
    .bind(ws.to_uuid())
    .execute(&p)
    .await
    .expect("second fire (same id, different created_at)");
    assert_eq!(
        count_governance_rows(&p).await,
        1,
        "ON CONFLICT (event_id) DO NOTHING dedups the second fire"
    );

    // And the plain duplicate INSERT raises (pins the dedup contract).
    let err = sqlx::query(
        "INSERT INTO audit_governance_outbox (event_id, payload)
             VALUES ($1, '{}'::jsonb)",
    )
    .bind(event_id)
    .execute(&p)
    .await;
    assert!(
        err.is_err(),
        "duplicate event_id must raise unique_violation"
    );
    assert_eq!(
        count_governance_rows(&p).await,
        1,
        "failed duplicate left the first row intact"
    );
    // Restore the fresh-DB default (see `restore_enforcement_disabled`
    // doc): the singleton is global and the entry DB may be shared.
    restore_enforcement_disabled(&p).await;
}

/// Security Q1 closure (0241): the disabled-window reconciler. A
/// `message.moderated` audit row accepted while the runtime switch is off
/// commits with zero outbox rows (A2 half 4); after re-enablement,
/// `aero_reconcile_governance_audit` backfills it — enabled-binding join,
/// NOT EXISTS scan, trigger-identical envelope, idempotent, token-keyed
/// (an unmapped `message.deleted` row is NEVER fabricated into the admin
/// lane), dead rows never resurrected. Parity converges to
/// COUNT(outbox) == COUNT(audit) over the workspace's mapped subset
/// (assertions are ws-scoped: the entry DB is shared with the parity
/// test, whose enabled-binding workspace is legitimately backfilled too).
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn governance_reconcile_backfills_disabled_window() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor) = fixture(&p).await;
    // Isolate from earlier tests in the same entry (they share the
    // throwaway DB; their audit rows persist while the outbox reset above
    // cleared only outbox rows): the reconciler is a GLOBAL scan (v1
    // mirror), so other workspaces' `message.moderated` rows with enabled
    // bindings would be backfilled too and make the returned count
    // non-deterministic. Deleting other workspaces' moderation rows makes
    // the count exact; later tests query only their own workspace.
    sqlx::query(
        "DELETE FROM audit_events
              WHERE action = 'message.moderated' AND workspace_id <> $1",
    )
    .bind(ws.to_uuid())
    .execute(&p)
    .await
    .expect("isolate reconcile scan to this workspace");
    // Window open: enforcement disabled (fresh-DB default; defensively
    // re-assert — the singleton is global and an earlier test in the same
    // entry may have flipped it). No binding seeded — Gate 1 skips before
    // the binding lookup.
    sqlx::query(
        "UPDATE snaplink_commercial_runtime
                SET enabled = FALSE, updated_at = clock_timestamp()
              WHERE singleton",
    )
    .execute(&p)
    .await
    .expect("re-assert enforcement disabled");

    // Moderated message commits during the window: audit row yes, outbox
    // row no (the fail-open gate meaning, A2 half 4).
    let id = message_in_workspace(&p, ws, actor).await;
    let deleted = moderate_finalize(&p, id, ws, serde_json::json!({ "reason": "spam" }))
        .await
        .expect("moderation finalize commits while disabled");
    assert!(deleted.is_some(), "soft delete commits");
    assert_eq!(
        count_governance_rows(&p).await,
        0,
        "|G|=0 while disabled (A2 half 4 pin)"
    );

    // A NON-moderation audit row commits in the same window — the
    // reconciler must not fabricate a MODERATION_OUTBOUND_ACTION claim
    // for it (token-keyed R-D2). The R-D2 writer is GATE-FREE (0242 D2
    // precedent): this `message.deleted` delete writes its own MESSAGE-class
    // outbox row even while enforcement is disabled — the admin lane stays
    // empty until the reconcile backfill below.
    let id2 = message_in_workspace(&p, ws, actor).await;
    crate::message::MessageRepo::new(p.clone())
        .soft_delete_audited(id2, ws, Some(actor), serde_json::json!({ "digest": "x" }))
        .await
        .expect("plain delete commits while disabled");

    // Window closed: re-enable + binding = the recovery precondition
    // (same setup the parity test uses).
    enable_enforcement_with_binding(&p, ws).await;

    // Backfill: the workspace's moderation row is backfilled (the global
    // return value may include earlier tests' enabled-binding workspaces
    // on the shared entry DB — the reconciler must backfill those too,
    // so the pin is ws-scoped). Second run is a no-op (idempotent).
    let backfilled: i32 = sqlx::query_scalar("SELECT aero_reconcile_governance_audit(10)")
        .fetch_one(&p)
        .await
        .expect("reconcile");
    assert!(
        backfilled >= 1,
        "the disabled-window moderation row must be backfilled (got {backfilled})"
    );
    assert_eq!(
        governance_rows_for(&p, ws).await,
        2,
        "exactly two governance rows for THIS workspace: the R-D2 writer's \
             message-class delete row (gate-free) + the backfilled admin row; \
             the delete row is never fabricated into the admin lane (token-keyed)"
    );
    // The writer's delete row is class 'message', action verbatim — the
    // admin backfill is the ONLY admin-class row.
    let delete_class: i64 = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM audit_governance_outbox
          WHERE payload->>'aggregate_id' = $1 AND payload->>'action' = 'message.deleted'",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .unwrap()
    .0;
    assert_eq!(delete_class, 1, "the delete row is message-class");
    let admin_class: i64 = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM audit_governance_outbox
          WHERE payload->>'aggregate_id' = $1 AND class = 'admin'",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .unwrap()
    .0;
    assert_eq!(admin_class, 1, "exactly one admin row (the backfill)");
    let again: i32 = sqlx::query_scalar("SELECT aero_reconcile_governance_audit(10)")
        .fetch_one(&p)
        .await
        .expect("reconcile idempotent");
    assert_eq!(again, 0, "second reconcile inserts nothing");

    // Envelope is byte-identical to the trigger path (A2 field asserts
    // hold for both): event_id 1:1, status 0, class 'admin', priority
    // 100, action = leaf MODERATION_OUTBOUND_ACTION, binding
    // source_system, system actor, idempotency_key = event_id.
    let audit: (String, String, Option<String>, String, serde_json::Value) = sqlx::query_as(
        "SELECT id::text, action, actor_id::text, target, detail
               FROM audit_events
              WHERE workspace_id = $1 AND action = 'message.moderated'",
    )
    .bind(ws.to_uuid())
    .fetch_one(&p)
    .await
    .expect("audit row");
    assert_eq!(audit.1, "message.moderated", "local token verbatim");
    let gov: (String, i32, String, i16, serde_json::Value) = sqlx::query_as(
        "SELECT event_id::text, status, class, priority, payload
               FROM audit_governance_outbox
              WHERE payload->>'aggregate_id' = $1 AND class = 'admin'",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .expect("backfilled governance row");
    assert_eq!(gov.0, audit.0, "event_id 1:1 with audit_events.id");
    assert_eq!(gov.1, 0, "status 0 = enqueued, same as trigger path");
    assert_eq!(gov.2, "admin", "class 'admin' (GOVERNANCE_CLASS_ADMIN)");
    assert_eq!(gov.3, 100, "priority 100 (GOVERNANCE_PRIORITY_MODERATION)");
    assert_eq!(
        gov.4["action"], MODERATION_OUTBOUND_ACTION,
        "payload action"
    );
    assert_eq!(gov.4["event_id"], audit.0, "payload event_id mirrors");
    assert_eq!(gov.4["idempotency_key"], audit.0, "sink Idempotency-Key");
    assert_eq!(gov.4["source_system"], format!("source-{ws}"));
    assert_eq!(
        gov.4["actor"]["type"], "system",
        "system-initiated moderation"
    );

    // Parity over the mapped subset (enabled window): exactly one outbox
    // row per message.moderated audit row — scoped to class 'admin' (the
    // R-D2 delete row is message-class and never counts toward the
    // moderation parity). Dead rows still count (parity is row existence,
    // never delivery status).
    let audit_count: i64 = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM audit_events
              WHERE workspace_id = $1 AND action = 'message.moderated'",
    )
    .bind(ws.to_uuid())
    .fetch_one(&p)
    .await
    .unwrap()
    .0;
    let admin_parity: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_governance_outbox
          WHERE payload->>'aggregate_id' = $1 AND class = 'admin'",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .expect("admin-class parity count");
    assert_eq!(
        admin_parity, audit_count,
        "P2 parity after reconcile (over the workspace's mapped admin subset)"
    );

    // Dead is terminal: a dead row exists in the outbox, so NOT EXISTS
    // skips it — the reconciler never resurrects a relay-terminal row
    // (it cannot fight a 403-outage dead-letter).
    sqlx::query(
        "UPDATE audit_governance_outbox
                SET status = 3, last_error = 'reconcile-test dead'
              WHERE event_id = $1::uuid",
    )
    .bind(&audit.0)
    .execute(&p)
    .await
    .expect("mark dead");
    let resurrected: i32 = sqlx::query_scalar("SELECT aero_reconcile_governance_audit(10)")
        .fetch_one(&p)
        .await
        .expect("reconcile after dead");
    assert_eq!(resurrected, 0, "dead row never resurrected");
    let status: i32 =
        sqlx::query_scalar("SELECT status FROM audit_governance_outbox WHERE event_id = $1::uuid")
            .bind(&audit.0)
            .fetch_one(&p)
            .await
            .expect("dead row status");
    assert_eq!(status, 3, "dead stays dead");

    // Restore the fresh-DB default LAST: the singleton is GLOBAL and this
    // entry's DB may be shared (the main integration leg runs the whole
    // ignored suite on one DB). Leaving the switch on makes every later
    // unbound message INSERT raise P0001 (0235 metering) — empirically
    // `message_reports::db_tests` 3/3 (the F1 hazard, see
    // governance_drill_tests/crash.rs drill_panic_leaves_enforcement_on_*).
    restore_enforcement_disabled(&p).await;
}

// ---- L1 window aggregation (migration 0242) — AC2 / AC3 ----------------
//
// Harness contract (`scripts/test-integration.sh`): the 0242 file gate
// runs the `l1_window_aggregates_` filter on its own throwaway DB (the
// empty-filter guard requires ≥1 matched test). Every literal below is
// derived from the leaf consts — never inline (G2 closure: truth-check
// scans `crates --glob '*.rs'` only, so the 0242 SQL side is unpoliced;
// these tests ARE the SQL pin: a drift in the trigger's allowlist token,
