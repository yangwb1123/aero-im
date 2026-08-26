//! Message send/edit producer-seam tests (S1/S2 + the 0242 L1 window).

use super::*;

/// One send tx commits exactly 1 `message.create` audit row AND 1 L1 window
/// outbox row (count 1) per (workspace, 60s window), plus the `event_outbox`
/// row — the S1 + 0242 allowlist pin.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn moderation_finalize_outbox_parity_send() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor, room) = room_fixture(&p, "producer-send").await;
    let repo = crate::message::MessageRepo::new(p.clone());
    let inserted = repo
        .insert_outboxed(new_message(room, actor), None, Vec::new(), None)
        .await
        .expect("send commits under enforcement OFF");
    let message = inserted.message();

    // 1 message.create audit row, exact shape.
    let rows = audit_rows_for(&p, ws, LOCAL_ACTION_MESSAGE_CREATE).await;
    assert_eq!(rows.len(), 1, "exactly one message.create audit row");
    assert_eq!(rows[0].1, message.id.to_string(), "target = message id");
    let audit: (uuid::Uuid, Option<uuid::Uuid>, serde_json::Value) = sqlx::query_as(
        "SELECT id, actor_id, detail FROM audit_events
          WHERE workspace_id = $1 AND action = $2",
    )
    .bind(ws.to_uuid())
    .bind(LOCAL_ACTION_MESSAGE_CREATE)
    .fetch_one(&p)
    .await
    .expect("message.create audit row");
    assert_eq!(audit.1, Some(actor.to_uuid()), "actor = sender");
    assert_eq!(
        audit.2.get("room_id").and_then(serde_json::Value::as_str),
        Some(room.to_string().as_str()),
        "detail.room_id"
    );

    // Exactly 1 message-class window row (0242): aggregated, count 1, class
    // message, priority 10, status 0, attempts 0, last_error NULL, T-11
    // claim predicate selects it.
    let window: (
        uuid::Uuid,
        String,
        i16,
        i32,
        i64,
        Option<String>,
        serde_json::Value,
    ) = sqlx::query_as(
        "SELECT event_id, class, priority, status, attempts, last_error, payload
               FROM audit_governance_outbox
              WHERE payload->>'aggregate_id' = $1
                AND class = 'message'
                AND (payload->>'aggregated') = 'true'",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .expect("one window row for the send");
    assert_eq!(window.1, GOVERNANCE_CLASS_MESSAGE, "class message");
    assert_eq!(window.2, 10, "priority 10 = GOVERNANCE_PRIORITY_BACKLOG");
    assert_eq!(window.3, 0, "status 0 (pending, T-11 shape)");
    assert_eq!(window.4, 0, "attempts 0");
    assert!(window.5.is_none(), "last_error NULL");
    assert_eq!(
        window.6.get("count").and_then(serde_json::Value::as_i64),
        Some(1),
        "window count 1"
    );
    assert_eq!(
        window
            .6
            .get("aggregated")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        window.6.get("action").and_then(serde_json::Value::as_str),
        Some(AGGREGATED_MESSAGE_ACTION),
        "envelope action = message.batch"
    );
    assert_eq!(
        window
            .6
            .get("source_system")
            .and_then(serde_json::Value::as_str),
        Some(AUDIT_SOURCE_SYSTEM),
        "source_system written by the trigger"
    );
    // T-11 claim predicate (pg.rs claim CTE) selects the row.
    let claimable: bool = sqlx::query_scalar(
        "SELECT EXISTS(
            SELECT 1 FROM audit_governance_outbox
             WHERE payload->>'aggregate_id' = $1
               AND status IN (0, 1)
               AND available_at <= clock_timestamp()
               AND (lease_expires_at IS NULL OR lease_expires_at <= clock_timestamp())
         )",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .expect("claim predicate probe");
    assert!(claimable, "the window row is claimable (T-11 shape)");

    // 1 event_outbox row for the message.
    let outbox: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM event_outbox WHERE message_id = $1")
        .bind(message.id.to_uuid())
        .fetch_one(&p)
        .await
        .expect("event_outbox count");
    assert_eq!(outbox, 1, "one event_outbox row per send");
}

/// Two sends millisecond-spaced in the same 60s window merge into exactly 1
/// window row with count = 2 (the 0242 `ON CONFLICT DO UPDATE count+1`
/// path); the audit side keeps 2 rows.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn moderation_finalize_outbox_parity_send_merge() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor, room) = room_fixture(&p, "producer-send-merge").await;
    let repo = crate::message::MessageRepo::new(p.clone());
    let first = repo
        .insert_outboxed(new_message(room, actor), None, Vec::new(), None)
        .await
        .expect("first send");
    let second = repo
        .insert_outboxed(new_message(room, actor), None, Vec::new(), None)
        .await
        .expect("second send");
    assert_ne!(first.message().id, second.message().id);

    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events
          WHERE workspace_id = $1 AND action = $2",
    )
    .bind(ws.to_uuid())
    .bind(LOCAL_ACTION_MESSAGE_CREATE)
    .fetch_one(&p)
    .await
    .expect("audit count");
    assert_eq!(audit_count, 2, "two audit rows for two sends");

    // Recompute the window key from the first audit row's server-stamped
    // created_at — both sends land in the same 60s window (app-side
    // now_utc() stamping, millisecond spacing).
    let first_audit_id: uuid::Uuid = sqlx::query_scalar(
        "SELECT id FROM audit_events
          WHERE workspace_id = $1 AND action = $2 AND target = $3",
    )
    .bind(ws.to_uuid())
    .bind(LOCAL_ACTION_MESSAGE_CREATE)
    .bind(first.message().id.to_string())
    .fetch_one(&p)
    .await
    .expect("first audit row id");
    let first_created_at: time::OffsetDateTime =
        sqlx::query_scalar("SELECT created_at FROM audit_events WHERE id = $1")
            .bind(first_audit_id)
            .fetch_one(&p)
            .await
            .expect("first audit created_at");
    let key = window_key_for(&p, ws, first_created_at).await;
    let (count, status): (i64, i32) = sqlx::query_as(
        "SELECT (payload->>'count')::bigint, status
           FROM audit_governance_outbox WHERE event_id = $1",
    )
    .bind(key)
    .fetch_one(&p)
    .await
    .expect("window row for the recomputed key");
    assert_eq!(
        (count, status),
        (2, 0),
        "exactly 1 window row, count merged to 2"
    );

    // SUM conservation over the ws message-class rows always holds.
    let sum: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM((payload->>'count')::bigint), 0)::bigint
           FROM audit_governance_outbox
          WHERE payload->>'aggregate_id' = $1
            AND class = 'message'
            AND ((payload->>'aggregated') = 'true' OR (payload->>'spill') = 'true')",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .expect("window sum");
    assert_eq!(sum, 2, "SUM conservation: two events, two counts");
}

/// Idempotent replay (same `client_message_id` twice) commits 1 message, 1
/// audit row, and a window count of 1 — the pre-tx early return writes zero
/// audit rows (F5).
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn moderation_finalize_outbox_parity_send_replay() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor, room) = room_fixture(&p, "producer-send-replay").await;
    let repo = crate::message::MessageRepo::new(p.clone());
    let key = uuid::Uuid::new_v4();
    let idempotency = crate::message::MessageIdempotency::new(key, [0x42; 32]);
    let first = repo
        .insert_outboxed(
            new_message(room, actor),
            Some(idempotency),
            Vec::new(),
            None,
        )
        .await
        .expect("first send claims the key");
    let replay = repo
        .insert_outboxed(
            new_message(room, actor),
            Some(idempotency),
            Vec::new(),
            None,
        )
        .await
        .expect("replay resolves the canonical message");
    assert_eq!(first.message().id, replay.message().id);

    let message_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE room_id = $1")
        .bind(room.to_uuid())
        .fetch_one(&p)
        .await
        .expect("message count");
    let audit_count = audit_rows_for(&p, ws, LOCAL_ACTION_MESSAGE_CREATE)
        .await
        .len();
    assert_eq!(
        (message_count, audit_count),
        (1, 1),
        "replay is a zero-write no-op"
    );
    let sum: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM((payload->>'count')::bigint), 0)::bigint
           FROM audit_governance_outbox
          WHERE payload->>'aggregate_id' = $1 AND class = 'message'",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .expect("window sum");
    assert_eq!(sum, 1, "one window count for one committed send");
}

/// CONCURRENT duplicate submit (same `client_message_id`, both racing past
/// the pre-tx lookup before either commits): the ledger
/// `ON CONFLICT DO NOTHING` serializes the outcome — the winner commits, the
/// claim-loser's full `tx.rollback()` (which runs AFTER the S1 audit append
/// inside `insert_outboxed`) must roll back its speculative message,
/// `event_outbox` row AND audit row together: exactly 1 message, 1
/// `message.create` audit row, 1 window row with count 1, zero orphan audit
/// rows (F5 concurrency half — the sequential-replay half is
/// `…_send_replay`).
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn moderation_finalize_outbox_parity_send_concurrent_claim_loser() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor, room) = room_fixture(&p, "producer-send-race").await;
    let repo = crate::message::MessageRepo::new(p.clone());
    let key = uuid::Uuid::new_v4();
    let idempotency = crate::message::MessageIdempotency::new(key, [0x51; 32]);

    let (first, second) = tokio::join!(
        repo.insert_outboxed(
            new_message(room, actor),
            Some(idempotency),
            Vec::new(),
            None,
        ),
        repo.insert_outboxed(
            new_message(room, actor),
            Some(idempotency),
            Vec::new(),
            None,
        ),
    );
    let first = first.expect("first send");
    let second = second.expect("second send");
    assert_eq!(
        first.message().id,
        second.message().id,
        "both resolves to the SAME canonical message"
    );
    let created = usize::from(matches!(
        first.outcome,
        crate::message::MessageInsertOutcome::Created(_)
    )) + usize::from(matches!(
        second.outcome,
        crate::message::MessageInsertOutcome::Created(_)
    ));
    assert_eq!(created, 1, "exactly one Created outcome (the winner)");

    let message_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE room_id = $1")
        .bind(room.to_uuid())
        .fetch_one(&p)
        .await
        .expect("message count");
    assert_eq!(message_count, 1, "claim-loser rolled back its message");
    let audit = audit_rows_for(&p, ws, LOCAL_ACTION_MESSAGE_CREATE).await;
    assert_eq!(audit.len(), 1, "claim-loser rolled back its audit append");
    assert_eq!(
        audit[0].1,
        first.message().id.to_string(),
        "audit target = the canonical message"
    );
    let sum: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM((payload->>'count')::bigint), 0)::bigint
           FROM audit_governance_outbox
          WHERE payload->>'aggregate_id' = $1 AND class = 'message'
            AND ((payload->>'aggregated') = 'true' OR (payload->>'spill') = 'true')",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .expect("window sum");
    assert_eq!(sum, 1, "exactly one window count — no orphan rows");
}

/// Commercial enforcement ON + no binding → send fails fail-closed (0235
/// metering fires on the messages INSERT before the audit append — or 0236
/// raises at the audit INSERT; the outcome is asserted source-agnostically).
/// Zero rows escape: messages 0, `event_outbox` 0, audit 0, outbox 0.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn moderation_finalize_outbox_parity_send_rollback() {
    let p = pool();
    reset_governance_table(&p).await;
    // Defensive re-assert OFF (fresh-DB default) before flipping ON.
    restore_enforcement_disabled(&p).await;
    let (ws, actor, room) = room_fixture(&p, "producer-send-rollback").await;
    sqlx::query(
        "UPDATE snaplink_commercial_runtime
            SET enabled = TRUE, updated_at = clock_timestamp()
          WHERE singleton",
    )
    .execute(&p)
    .await
    .expect("enable commercial enforcement");
    let repo = crate::message::MessageRepo::new(p.clone());
    let err = repo
        .insert_outboxed(new_message(room, actor), None, Vec::new(), None)
        .await
        .expect_err("send must fail fail-closed without a binding");
    // 0235 `messages_snaplink_metering` raises (constraint
    // `snaplink_binding_required`) → mapped to Error::Upstream by
    // aero_common::Error::from. Outcome-asserted (source-agnostic: 0235 fires
    // before the audit append; 0236 would raise at the append — same outcome).
    assert!(matches!(err, aero_common::Error::Upstream(_)));

    let message_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE room_id = $1")
        .bind(room.to_uuid())
        .fetch_one(&p)
        .await
        .expect("message count");
    let outbox_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM event_outbox WHERE subject = $1")
            .bind(format!("im.room.{room}"))
            .fetch_one(&p)
            .await
            .expect("event_outbox count");
    let audit_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE workspace_id = $1")
            .bind(ws.to_uuid())
            .fetch_one(&p)
            .await
            .expect("audit count");
    let governance_count = governance_rows_for(&p, ws).await;
    assert_eq!(
        (message_count, outbox_count, audit_count, governance_count),
        (0, 0, 0, 0),
        "zero rows escape a fail-closed send (rollback atomicity)"
    );
    restore_enforcement_disabled(&p).await;
}

/// Edit: a successful edit appends 1 `message.edit` audit row (actor = editor)
/// and merges into the L1 window; an `Ok(None)` (wrong `expected_version`) is a
/// zero-write no-op (F5).
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn moderation_finalize_outbox_parity_edit() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor, room) = room_fixture(&p, "producer-edit").await;
    let repo = crate::message::MessageRepo::new(p.clone());
    let seeded = repo
        .insert_outboxed(new_message(room, actor), None, Vec::new(), None)
        .await
        .expect("seed message")
        .message()
        .clone();

    let edited = repo
        .edit_outboxed_authorized(
            seeded.id,
            actor,
            vec![Block::text("edited content")],
            seeded.version,
            true,
            None,
        )
        .await
        .expect("edit commits")
        .expect("edit produced a new version");
    assert_eq!(edited.message.id, seeded.id);

    let edit_rows = audit_rows_for(&p, ws, LOCAL_ACTION_MESSAGE_EDIT).await;
    assert_eq!(edit_rows.len(), 1, "exactly one message.edit audit row");
    assert_eq!(edit_rows[0].1, seeded.id.to_string(), "target = message id");
    let (audit_actor, detail): (Option<uuid::Uuid>, serde_json::Value) = sqlx::query_as(
        "SELECT actor_id, detail FROM audit_events
          WHERE workspace_id = $1 AND action = $2",
    )
    .bind(ws.to_uuid())
    .bind(LOCAL_ACTION_MESSAGE_EDIT)
    .fetch_one(&p)
    .await
    .expect("message.edit audit row");
    assert_eq!(audit_actor, Some(actor.to_uuid()), "actor = editor");
    assert_eq!(
        detail.get("room_id").and_then(serde_json::Value::as_str),
        Some(room.to_string().as_str()),
        "detail.room_id"
    );

    // Merge into the L1 window: send (1) + edit (1) = count 2 in the window.
    let sum: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM((payload->>'count')::bigint), 0)::bigint
           FROM audit_governance_outbox
          WHERE payload->>'aggregate_id' = $1 AND class = 'message'",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .expect("window sum");
    assert_eq!(sum, 2, "send + edit merge into the same L1 window");

    // Wrong expected_version → Err(Conflict) (version-fence), zero new audit
    // rows — the rejected edit writes no audit row (F5).
    let conflict = repo
        .edit_outboxed_authorized(
            seeded.id,
            actor,
            vec![Block::text("stale edit")],
            edited.message.version + 100,
            true,
            None,
        )
        .await
        .expect_err("stale edit is rejected by the version fence");
    assert!(matches!(conflict, aero_common::Error::Conflict(_)));
    let edit_rows_after = audit_rows_for(&p, ws, LOCAL_ACTION_MESSAGE_EDIT)
        .await
        .len();
    assert_eq!(edit_rows_after, 1, "no audit row for a rejected edit");
}
