//! Room lifecycle producer-seam tests (S3/S4 + the 0245 1:1 lane).

use super::*;

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
