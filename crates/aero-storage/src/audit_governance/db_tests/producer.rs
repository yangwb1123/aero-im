use super::*;

use aero_common::{
    Block, RoomKind, GOVERNANCE_CLASS_MESSAGE, GOVERNANCE_CLASS_ROOM, LOCAL_ACTION_MESSAGE_EDIT,
    LOCAL_ACTION_ROOM_ARCHIVED, LOCAL_ACTION_ROOM_CREATE,
};

// B5-1 producer seam tests (S1–S4): the in-tx `AuditRepo::append_in_tx`
// appends on the message send/edit and room create/archive write paths, and
// the AFTER INSERT triggers (0242 / 0245) that materialize the governance
// outbox rows in the SAME transaction. Three-part shape per test (Commit /
// Rollback / Replay) + the security-boundary carve-out pins (G-SEC1).
//
// Commit half runs with enforcement OFF (fresh-DB default) and NO binding —
// proving 0242/0245 enqueue unconditionally (no runtime gate) and pinning
// the row shape (priority 10, source_system, status 0).
//
// Naming prefix `moderation_finalize_outbox_parity_` keeps the harness slots
// (`audit_governance::` and `moderation_finalize_outbox_parity`) non-empty.
// Every test resets the governance table at start (order-independence within
// a harness entry run) and re-asserts the enforcement singleton OFF where it
// flips it on (restore at end — the singleton is GLOBAL).

/// Fixture: workspace + owner actor + a channel room (bare
/// `RoomRepo::create_in_workspace` — NEVER the authorized path, which would
/// self-produce a `room.create` audit row and pollute the seam assertions).
async fn room_fixture(p: &PgPool, label: &str) -> (WorkspaceId, ParticipantId, RoomId) {
    let (ws, actor) = fixture(p).await;
    let room = crate::room::RoomRepo::new(p.clone())
        .create_in_workspace(
            ws,
            RoomKind::Channel,
            Some(format!("{label}-{}", uuid::Uuid::new_v4())),
            actor,
        )
        .await
        .expect("create channel room")
        .id;
    (ws, actor, room)
}

fn new_message(room: RoomId, sender: ParticipantId) -> crate::message::NewMessage {
    crate::message::NewMessage {
        room_id: room,
        sender_id: sender,
        blocks: vec![Block::text("b5-1-producer-seam")],
        reply_to: None,
        metadata: serde_json::Value::Null,
        expires_at: None,
    }
}

async fn audit_rows_for(p: &PgPool, ws: WorkspaceId, action: &str) -> Vec<(uuid::Uuid, String)> {
    sqlx::query_as::<_, (uuid::Uuid, String)>(
        "SELECT id, target FROM audit_events
          WHERE workspace_id = $1 AND action = $2
          ORDER BY created_at, id",
    )
    .bind(ws.to_uuid())
    .bind(action)
    .fetch_all(p)
    .await
    .expect("query audit rows")
}

/// 0242 window key recomputed from the leaf consts — the assertion target for
/// the merge test (same window key as the trigger's md5 preimage).
async fn window_key_for(
    p: &PgPool,
    ws: WorkspaceId,
    created_at: time::OffsetDateTime,
) -> uuid::Uuid {
    let window_epoch: i64 =
        sqlx::query_scalar("SELECT floor(extract(epoch FROM $1::timestamptz) / $2)::bigint")
            .bind(created_at)
            .bind(L1_WINDOW_SECONDS)
            .fetch_one(p)
            .await
            .expect("window epoch");
    sqlx::query_scalar("SELECT md5($1)::uuid")
        .bind(format!(
            "{}|{}|{}",
            ws.to_uuid(),
            GOVERNANCE_CLASS_MESSAGE,
            window_epoch
        ))
        .fetch_one(p)
        .await
        .expect("recompute window key")
}

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

/// room.create yields 1 audit row + 1:1 class 'room' outbox row (0245 pin):
/// `event_id` == `audit_events.id`, `idempotency_key` == `event_id`, no L1
/// aggregated/spill/count/window keys. Detail carries `room_id` + kind (the
/// unvalidated `name` is deliberately absent — G-SEC2).
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn moderation_finalize_outbox_parity_room_create() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor) = fixture(&p).await;
    let repo = crate::room::RoomRepo::new(p.clone());
    let room = repo
        .create_in_workspace_authorized(
            ws,
            RoomKind::Channel,
            Some("audited channel".into()),
            actor,
        )
        .await
        .expect("room.create commits");

    let rows = audit_rows_for(&p, ws, LOCAL_ACTION_ROOM_CREATE).await;
    assert_eq!(rows.len(), 1, "exactly one room.create audit row");
    assert_eq!(rows[0].1, room.id.to_string(), "target = room id");
    let (audit_id, audit_actor, detail): (uuid::Uuid, Option<uuid::Uuid>, serde_json::Value) =
        sqlx::query_as(
            "SELECT id, actor_id, detail FROM audit_events
              WHERE workspace_id = $1 AND action = $2",
        )
        .bind(ws.to_uuid())
        .bind(LOCAL_ACTION_ROOM_CREATE)
        .fetch_one(&p)
        .await
        .expect("room.create audit row");
    assert_eq!(audit_actor, Some(actor.to_uuid()), "actor = creator");
    assert_eq!(
        detail.get("kind").and_then(serde_json::Value::as_str),
        Some("channel"),
        "detail.kind"
    );
    assert!(
        detail.get("name").is_none(),
        "unvalidated name must not enter the audit payload (G-SEC2)"
    );

    // 1:1 class 'room' outbox row (0245).
    let (event_id, class, priority, status, payload): (
        uuid::Uuid,
        String,
        i16,
        i32,
        serde_json::Value,
    ) = sqlx::query_as(
        "SELECT event_id, class, priority, status, payload
           FROM audit_governance_outbox
          WHERE payload->>'aggregate_id' = $1 AND class = 'room'",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .expect("one 1:1 room outbox row");
    assert_eq!(class, GOVERNANCE_CLASS_ROOM, "class room");
    assert_eq!(priority, 10, "priority 10 = GOVERNANCE_PRIORITY_BACKLOG");
    assert_eq!(status, 0, "status 0 pending");
    assert_eq!(event_id, audit_id, "event_id 1:1 with audit_events.id");
    assert_eq!(
        payload.get("event_id").and_then(serde_json::Value::as_str),
        Some(audit_id.to_string().as_str()),
        "envelope event_id"
    );
    assert_eq!(
        payload
            .get("idempotency_key")
            .and_then(serde_json::Value::as_str),
        Some(audit_id.to_string().as_str()),
        "idempotency_key == event_id"
    );
    for forbidden in ["aggregated", "spill", "count", "window_start", "window_end"] {
        assert!(
            payload.get(forbidden).is_none(),
            "1:1 rows never carry the L1 marker key {forbidden}"
        );
    }
    assert_eq!(
        payload.get("action").and_then(serde_json::Value::as_str),
        Some(LOCAL_ACTION_ROOM_CREATE),
        "envelope action = local token verbatim"
    );
}

/// room.archived (archive + unarchive) each yield one audit row (same token,
/// detail.archived carries the new flag) and one 1:1 class 'room' outbox row
/// — both directions, 0245.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn moderation_finalize_outbox_parity_room_archive() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor, room) = room_fixture(&p, "producer-room-archive").await;
    let repo = crate::room::RoomRepo::new(p.clone());
    repo.set_channel_archived_authorized(room, actor, true)
        .await
        .expect("archive commits");
    let rows_true = audit_rows_for(&p, ws, LOCAL_ACTION_ROOM_ARCHIVED).await;
    assert_eq!(rows_true.len(), 1, "one room.archived row for archive");
    let archived_true: bool = sqlx::query_scalar(
        "SELECT (detail->>'archived')::boolean FROM audit_events
          WHERE workspace_id = $1 AND action = $2",
    )
    .bind(ws.to_uuid())
    .bind(LOCAL_ACTION_ROOM_ARCHIVED)
    .fetch_one(&p)
    .await
    .expect("detail.archived");
    assert!(archived_true, "detail.archived = true");

    repo.set_channel_archived_authorized(room, actor, false)
        .await
        .expect("unarchive commits");
    let rows_false = audit_rows_for(&p, ws, LOCAL_ACTION_ROOM_ARCHIVED).await;
    assert_eq!(rows_false.len(), 2, "second row for unarchive, same token");
    let archived: Vec<bool> = sqlx::query_scalar(
        "SELECT (detail->>'archived')::boolean FROM audit_events
          WHERE workspace_id = $1 AND action = $2 ORDER BY created_at, id",
    )
    .bind(ws.to_uuid())
    .bind(LOCAL_ACTION_ROOM_ARCHIVED)
    .fetch_all(&p)
    .await
    .expect("archive flags");
    assert_eq!(
        archived,
        vec![true, false],
        "detail.archived carries the new flag"
    );

    let governance: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_governance_outbox
          WHERE payload->>'aggregate_id' = $1 AND class = 'room'",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .expect("room outbox count");
    assert_eq!(governance, 2, "both directions yield 1:1 room rows");
}

/// room.create under enforcement ON + no binding fails fail-closed through
/// the PURE 0236 audit channel (no messages INSERT on this path — the audit
/// append's own binding lookup RAISEs). rooms 0, `room_members` 0, audit 0,
/// outbox 0.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn moderation_finalize_outbox_parity_room_create_rollback() {
    let p = pool();
    reset_governance_table(&p).await;
    restore_enforcement_disabled(&p).await;
    let (ws, actor) = fixture(&p).await;
    sqlx::query(
        "UPDATE snaplink_commercial_runtime
            SET enabled = TRUE, updated_at = clock_timestamp()
          WHERE singleton",
    )
    .execute(&p)
    .await
    .expect("enable commercial enforcement");
    let repo = crate::room::RoomRepo::new(p.clone());
    let err = repo
        .create_in_workspace_authorized(ws, RoomKind::Channel, Some("blocked".into()), actor)
        .await
        .expect_err("room.create must fail fail-closed without a binding");
    // Pure 0236 channel: the audit append's binding lookup raises (constraint
    // snaplink_binding_required) → mapped to Error::Upstream.
    let storage = match err {
        crate::room::RoomMembershipWriteError::Storage(sqlx::Error::Database(db)) => {
            db.constraint().map(str::to_owned)
        }
        other => panic!("unexpected room.create error: {other:?}"),
    };
    assert_eq!(
        storage.as_deref(),
        Some("snaplink_binding_required"),
        "the 0236 audit-channel RAISE constraint surfaces"
    );

    let rooms: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rooms WHERE workspace_id = $1")
        .bind(ws.to_uuid())
        .fetch_one(&p)
        .await
        .expect("rooms count");
    let members: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM room_members rm
          JOIN rooms r ON r.id = rm.room_id
         WHERE r.workspace_id = $1",
    )
    .bind(ws.to_uuid())
    .fetch_one(&p)
    .await
    .expect("room_members count");
    let audit: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE workspace_id = $1")
            .bind(ws.to_uuid())
            .fetch_one(&p)
            .await
            .expect("audit count");
    let governance = governance_rows_for(&p, ws).await;
    assert_eq!(
        (rooms, members, audit, governance),
        (0, 0, 0, 0),
        "zero rows escape a fail-closed room.create (pure 0236 channel)"
    );
    restore_enforcement_disabled(&p).await;
}

// --- G-SEC1 boundary pins: audited-but-unmapped is INTENTIONAL ---

/// `integration.notification.published` is already audited in-tx (M2) and its
/// token is absent from every trigger allowlist → the governance outbox half
/// is an explicit carve-out: 1 audit row, ZERO outbox rows. Routing it
/// through the seam would double-audit and pollute the 0242 message window.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn moderation_finalize_outbox_parity_integration_notification_carved_out() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor) = fixture(&p).await;
    // Workspace owner + bot + channel room (bare paths — no audit pollution).
    let (bot, _token) = crate::bot::BotRepo::new(p.clone())
        .create_authorized_with_token(actor, "carveout-bot", None, Some(ws))
        .await
        .expect("create bot");
    let room = crate::room::RoomRepo::new(p.clone())
        .create_in_workspace(
            ws,
            RoomKind::Channel,
            Some(format!("carveout-{}", uuid::Uuid::new_v4())),
            actor,
        )
        .await
        .expect("create room")
        .id;
    crate::room::RoomRepo::new(p.clone())
        .add_member(room, bot)
        .await
        .expect("enroll bot in room");
    let integration = crate::integration::IntegrationRepo::new(p.clone());
    let installation = integration
        .create(crate::integration::NewIntegrationInstallation {
            workspace_id: ws,
            bot_id: bot,
            issuer: format!("https://carveout.test/{}", uuid::Uuid::new_v4()),
            user_identity_issuer: format!("https://human-carveout.test/{}", uuid::Uuid::new_v4()),
            client_id: format!("erp-{}", uuid::Uuid::new_v4()),
            name: "carveout".into(),
            allow_user_dm: false,
            room_ids: vec![room],
            created_by: actor,
        })
        .await
        .expect("create installation");
    let published = integration
        .publish(crate::integration::NewIntegrationNotification {
            installation_id: installation.id,
            issuer: installation.issuer.clone(),
            client_id: installation.client_id.clone(),
            idempotency_key: uuid::Uuid::new_v4(),
            request_hash: [0x5c; 32],
            target: crate::integration::IntegrationTarget::Room(room),
            room_id: room,
            recipient: None,
            blocks: vec![Block::text("carveout notification")],
            traceparent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into()),
            lease_token: None,
        })
        .await
        .expect("publish commits");
    assert_eq!(
        published.message.room_id, room,
        "publish created the message in the target room"
    );

    // Exactly 1 audit row, actor NULL (machine actor), zero outbox rows.
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events
          WHERE workspace_id = $1 AND action = 'integration.notification.published'",
    )
    .bind(ws.to_uuid())
    .fetch_one(&p)
    .await
    .expect("audit count");
    assert_eq!(audit_count, 1, "integration publish is audited in-tx");
    let actor_null: bool = sqlx::query_scalar(
        "SELECT EXISTS(
            SELECT 1 FROM audit_events
             WHERE workspace_id = $1 AND action = 'integration.notification.published'
               AND actor_id IS NULL)",
    )
    .bind(ws.to_uuid())
    .fetch_one(&p)
    .await
    .expect("actor null probe");
    assert!(actor_null, "integration audit actor is NULL (machine path)");
    let governance = governance_rows_for(&p, ws).await;
    assert_eq!(
        governance, 0,
        "integration.notification.published is carved out of the governance outbox"
    );
}

/// `message.deleted` is audited in-tx but R-D2-unmapped: the audit row is
/// present, and ZERO outbox rows are produced for it.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn moderation_finalize_outbox_parity_message_deleted_unmapped() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor, room) = room_fixture(&p, "producer-deleted-unmapped").await;
    let repo = crate::message::MessageRepo::new(p.clone());
    let seeded = repo
        .insert_outboxed(new_message(room, actor), None, Vec::new(), None)
        .await
        .expect("seed message")
        .message()
        .clone();
    // Seed send already produced 1 window row.
    let before = governance_rows_for(&p, ws).await;
    assert_eq!(before, 1, "seed send produced one window row");

    repo.soft_delete_outboxed_system(
        seeded.id,
        Some(ws),
        Some(actor),
        Some("message.deleted"),
        serde_json::json!({ "room_id": room }),
        actor,
        None,
    )
    .await
    .expect("delete commits")
    .expect("message deleted");

    let deleted_rows = audit_rows_for(&p, ws, "message.deleted").await;
    assert_eq!(deleted_rows.len(), 1, "message.deleted is audited in-tx");
    let after = governance_rows_for(&p, ws).await;
    assert_eq!(
        after, 1,
        "no outbox row for message.deleted (R-D2 unmapped)"
    );
    let deleted_outbox: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_governance_outbox
          WHERE payload->>'aggregate_id' = $1 AND payload->>'action' = 'message.deleted'",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(&p)
    .await
    .expect("deleted outbox count");
    assert_eq!(
        deleted_outbox, 0,
        "no 1:1 row carries the message.deleted action"
    );
}

/// DM creation is structurally carved out: `create_in_workspace_authorized`
/// rejects `RoomKind::Direct` BEFORE any INSERT (F-2) — zero audit rows, zero
/// rooms.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn moderation_finalize_outbox_parity_dm_create_carved_out() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor) = fixture(&p).await;
    let repo = crate::room::RoomRepo::new(p.clone());
    let err = repo
        .create_in_workspace_authorized(ws, RoomKind::Direct, None, actor)
        .await
        .expect_err("direct room creation is rejected pre-INSERT");
    assert!(matches!(
        err,
        crate::room::RoomMembershipWriteError::FixedMembership
    ));
    let rooms: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rooms WHERE workspace_id = $1")
        .bind(ws.to_uuid())
        .fetch_one(&p)
        .await
        .expect("rooms count");
    let audit: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE workspace_id = $1")
            .bind(ws.to_uuid())
            .fetch_one(&p)
            .await
            .expect("audit count");
    let governance = governance_rows_for(&p, ws).await;
    assert_eq!(
        (rooms, audit, governance),
        (0, 0, 0),
        "DM creation never reaches the room.create seam (F-2 carve-out)"
    );
}
