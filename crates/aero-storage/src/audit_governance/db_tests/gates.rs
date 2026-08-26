use super::*;

/// A2 half 4: enforcement disabled → the audit row still commits, zero
/// governance rows (the 0239 Gate 1 skip; the commercial kill-switch is
/// a delivery gate, never an audit-loss gate).
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn moderation_finalize_runtime_disabled_commits_1_plus_0() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor) = fixture(&p).await;
    // Fresh DB default: snaplink_commercial_runtime.enabled = FALSE (0235).
    // Defensively re-assert the disabled state BEFORE the message insert:
    // the singleton is global and an earlier test in the same DB run may
    // have flipped it (the harness grants each entry its own throwaway DB,
    // but tests within one entry share it). No binding seeded — Gate 1
    // skips before the binding lookup.
    sqlx::query(
        "UPDATE snaplink_commercial_runtime
                SET enabled = FALSE, updated_at = clock_timestamp()
              WHERE singleton",
    )
    .execute(&p)
    .await
    .expect("re-assert enforcement disabled");
    let id = message_in_workspace(&p, ws, actor).await;
    let deleted = moderate_finalize(&p, id, ws, serde_json::json!({ "reason": "spam" }))
        .await
        .expect("moderation finalize commits while disabled");
    assert!(deleted.is_some(), "soft delete commits");
    let audit: i64 = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM audit_events
              WHERE workspace_id = $1 AND action = 'message.moderated'",
    )
    .bind(ws.to_uuid())
    .fetch_one(&p)
    .await
    .unwrap()
    .0;
    assert_eq!(audit, 1, "audit trail commits while enforcement disabled");
    assert_eq!(
        count_governance_rows(&p).await,
        0,
        "|G|=0 while disabled (A2 half 4 pin)"
    );
}

/// A2 half 9 (pass-through no-raise) + R-D2 writer: a non-moderation action
/// keeps flowing through the shared 0239 trigger untouched — zero ADMIN
/// governance rows (the trigger's token-keyed mapping never fires), and the
/// Rust R-D2 writer now yields exactly one MESSAGE-class row for the
/// `message.deleted` delete (the 0245/0246 declared carve-out). The v1 row is
/// still produced (0236 trigger, action verbatim).
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn non_moderation_action_passes_through_unmapped() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor) = fixture(&p).await;
    // Enforcement + binding before the message insert (meter trigger, see
    // the parity test's setup note).
    enable_enforcement_with_binding(&p, ws).await;
    let id = message_in_workspace(&p, ws, actor).await;

    let deleted = crate::message::MessageRepo::new(p.clone())
        .soft_delete_audited(id, ws, Some(actor), serde_json::json!({ "digest": "x" }))
        .await
        .expect("plain delete commits");
    assert!(deleted, "delete commits");

    let action: String = sqlx::query_scalar(
        "SELECT action FROM audit_events WHERE workspace_id = $1 AND target = $2",
    )
    .bind(ws.to_uuid())
    .bind(id.to_string())
    .fetch_one(&p)
    .await
    .expect("audit row exists");
    assert_eq!(
        action, LOCAL_ACTION_MESSAGE_DELETED,
        "local token flows verbatim"
    );
    // 0239 trigger pass-through: zero ADMIN rows (the trigger maps only
    // `message.moderated`); the R-D2 Rust writer produced exactly one
    // MESSAGE-class row in the same tx.
    let admin_rows: i64 = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM audit_governance_outbox
          WHERE payload->>'aggregate_id' = $1 AND class = 'admin'",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .unwrap()
    .0;
    assert_eq!(
        admin_rows, 0,
        "unmapped token produces zero ADMIN governance rows (trigger pass-through no-raise)"
    );
    let message_rows: i64 = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM audit_governance_outbox
          WHERE payload->>'aggregate_id' = $1 AND class = 'message'
            AND payload->>'action' = 'message.deleted'",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .unwrap()
    .0;
    assert_eq!(
        message_rows, 1,
        "the R-D2 writer yields exactly one message-class delete row"
    );
    // v1 row for THIS audit row (scoped by idempotency_key = audit id —
    // the table is shared across tests within one entry run).
    let audit_id: String = sqlx::query_scalar(
        "SELECT id::text FROM audit_events WHERE workspace_id = $1 AND target = $2",
    )
    .bind(ws.to_uuid())
    .bind(id.to_string())
    .fetch_one(&p)
    .await
    .expect("audit id");
    let v1: i64 = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM snaplink_delivery_outbox
              WHERE destination = 'audit' AND idempotency_key = $1",
    )
    .bind(audit_id)
    .fetch_one(&p)
    .await
    .unwrap()
    .0;
    assert_eq!(v1, 1, "v1 row still produced for unmapped actions");
    // Restore the fresh-DB default (see `restore_enforcement_disabled`
    // doc): this test is the module's last in alphabetical run order, so
    // without the restore the shared main-integration DB would stay
    // enforcement-enabled and every later message-inserting test raises
    // P0001 (the 0235 metering trigger, binding lookup).
    restore_enforcement_disabled(&p).await;
}

/// R5 / A2 pin: AUTH tokens pass through the 0239 trigger unmapped — with
/// enforcement enabled, an `auth.register` / `auth.login.failed` row
/// produces ZERO governance outbox rows and never raises (a raise inside
/// the trigger would abort the WHOLE register transaction — load-bearing
/// for the in-tx path). The pin MUST insert into the BOUND fixture
/// workspace, never nil: the 0236 v1 trigger does its binding lookup
/// unconditionally and `aero_snaplink_binding_for_workspace` raises P0001
/// on a missing binding — nil raises in the fixture's raw-SQL enforcement
/// state (finding A2; supported deployments always bind nil at enable,
/// §3.5 of the design). With the binding present the v1 trigger enqueues
/// exactly one `snaplink_delivery_outbox` row per auth row (action
/// verbatim) — asserted, not assumed.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn auth_tokens_pass_through_unmapped_and_enqueue_v1() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor) = fixture(&p).await;
    enable_enforcement_with_binding(&p, ws).await;

    // Both producer shapes: the in-tx register seam and the best-effort
    // login seam (out-of-tx is fine here — the triggers fire on the
    // INSERT regardless).
    let audit_register = crate::AuditRepo::new(p.clone())
        .append(
            ws,
            Some(actor),
            "auth.register",
            Some(actor.to_string().as_str()),
            serde_json::json!({ "email": "pin@example.test", "user_agent": "pin" }),
        )
        .await
        .expect("auth.register audit row commits (no raise)");
    let audit_failed = crate::AuditRepo::new(p.clone())
        .append(
            ws,
            None,
            "auth.login.failed",
            None,
            serde_json::json!({ "email": "pin@example.test", "reason": "invalid_credentials" }),
        )
        .await
        .expect("auth.login.failed audit row commits (no raise)");

    assert_eq!(
        count_governance_rows(&p).await,
        0,
        "auth tokens produce zero governance rows (0239 pass-through no-raise)"
    );
    // One v1 row per auth row, scoped by idempotency_key = audit id (the
    // table is shared across tests within one entry run).
    for id in [audit_register, audit_failed] {
        let v1: i64 = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM snaplink_delivery_outbox
                  WHERE destination = 'audit' AND idempotency_key = $1",
        )
        .bind(id.to_uuid().to_string())
        .fetch_one(&p)
        .await
        .unwrap()
        .0;
        assert_eq!(v1, 1, "v1 row still produced for auth tokens");
    }
    // Restore the fresh-DB default (see `restore_enforcement_disabled`).
    restore_enforcement_disabled(&p).await;
}
