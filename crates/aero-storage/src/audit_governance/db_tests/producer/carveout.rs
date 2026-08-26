//! Audit-boundary carve-out pins (G-SEC1 / D-5 / D-7): audited-but-unmapped
//! producers and the DM / group-DM domain carve-outs — each pins the
//! documented boundary, so a future seam append reds immediately.

use super::*;

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

/// `message.deleted` is audited in-tx and R-D2 writer-owned: the audit row is
/// present AND exactly ONE 1:1 class-'message' outbox row is produced by the
/// Rust writer (AC-1) — the 0245/0246 declared carve-out ("NOT trigger-owned;
/// the sibling Rust outbox write … remains the planned path") now fulfilled
/// by `AuditGovernanceOutboxRepo::append_message_delete_in_tx` from the
/// soft-delete choke point.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn moderation_finalize_outbox_parity_message_deleted_lane() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor, room) = room_fixture(&p, "producer-deleted-lane").await;
    let repo = crate::message::MessageRepo::new(p.clone());
    let seeded = repo
        .insert_outboxed(new_message(room, actor), None, Vec::new(), None)
        .await
        .expect("seed message")
        .message()
        .clone();
    // Seed send already produced 1 window row (0242, message.create).
    let before = governance_rows_for(&p, ws).await;
    assert_eq!(before, 1, "seed send produced one window row");

    // --- Commit half: delete through the EXACT `ImService::delete_message`
    // seam (`soft_delete_outboxed_authorized` — sender-only, room access
    // re-fenced at commit time). The actor IS the sender and the room
    // creator (owner member edge from room_fixture). ---
    repo.soft_delete_outboxed_authorized(seeded.id, actor, None)
        .await
        .expect("authorized delete commits")
        .expect("message deleted");

    let deleted_rows = audit_rows_for(&p, ws, LOCAL_ACTION_MESSAGE_DELETED).await;
    assert_eq!(deleted_rows.len(), 1, "message.deleted is audited in-tx");
    let after = governance_rows_for(&p, ws).await;
    assert_eq!(
        after,
        before + 1,
        "exactly one 1:1 outbox row added by the Rust writer (R-D2)"
    );

    // The delete row: 1:1 with the audit id, class 'message', priority 10,
    // 16-key envelope field-by-field (payload = the {room_id, digest} detail).
    let (audit_id, target): (uuid::Uuid, String) = deleted_rows[0].clone();
    assert_eq!(target, seeded.id.to_string(), "target = message id");
    let row: (i32, String, i16, serde_json::Value) = sqlx::query_as(
        "SELECT status, class, priority, payload
               FROM audit_governance_outbox WHERE event_id = $1",
    )
    .bind(audit_id)
    .fetch_one(&p)
    .await
    .expect("delete outbox row");
    assert_eq!(row.0, 0, "status 0 = enqueued (0239 normative)");
    assert_eq!(
        row.1, GOVERNANCE_CLASS_MESSAGE,
        "class 'message' (GOVERNANCE_CLASS_MESSAGE leaf const)"
    );
    assert_eq!(
        row.2, 10,
        "priority 10 = GOVERNANCE_PRIORITY_BACKLOG (comment-pinned; aero-storage cannot import aero-ai)"
    );
    let envelope = row.3;
    assert_eq!(
        envelope["event_id"],
        audit_id.to_string(),
        "payload event_id mirrors"
    );
    assert_eq!(
        envelope["source_system"], AUDIT_SOURCE_SYSTEM,
        "source_system = AUDIT_SOURCE_SYSTEM (connector payload-guard value)"
    );
    assert_eq!(
        envelope["action"], LOCAL_ACTION_MESSAGE_DELETED,
        "action = local token VERBATIM (no fabricated contract token)"
    );
    assert_eq!(
        envelope["targets"],
        serde_json::json!([{ "id": seeded.id.to_string(), "type": "resource" }]),
        "targets = [{{id: message id, type: 'resource'}}] — non-empty"
    );
    assert_eq!(
        envelope["payload"],
        serde_json::json!({
            "room_id": room, // RoomId own serialization (same as the producer)
            "digest": seeded.searchable_text().chars().take(120).collect::<String>(),
        }),
        "payload = the {{room_id, digest}} detail object (authorization.rs shape)"
    );
    assert_eq!(
        envelope["aggregate_id"],
        ws.to_uuid().to_string(),
        "aggregate_id == workspace id"
    );
    assert_eq!(
        envelope["idempotency_key"],
        audit_id.to_string(),
        "idempotency_key = event_id (sink dedup)"
    );
    for marker in ["count", "aggregated", "spill", "window_start", "window_end"] {
        assert!(
            envelope.get(marker).is_none(),
            "no L1 marker key {marker} on 1:1 delete rows"
        );
    }
    // Fail-closed parse into the typed twin + Value equality (drift alarm).
    let parsed: AuditClaimPayload = serde_json::from_value(envelope.clone())
        .expect("typed twin parses the R-D2 payload (deny_unknown_fields)");
    assert_eq!(
        serde_json::to_value(&parsed).unwrap(),
        envelope,
        "re-serialized twin must equal the stored JSONB (semantic parity)"
    );
    assert_eq!(
        parsed.action, LOCAL_ACTION_MESSAGE_DELETED,
        "typed action == the leaf const verbatim"
    );

    // --- Replay half: the same delete is a no-op (deleted_at guard) → the
    // writer never re-fires; still exactly 1 row. ---
    let replay = repo
        .soft_delete_outboxed_authorized(seeded.id, actor, None)
        .await
        .expect("replay delete returns Ok");
    assert!(
        replay.is_none(),
        "replay of an already-deleted message is Ok(None)"
    );
    assert_eq!(
        governance_rows_for(&p, ws).await,
        after,
        "replay appends no second row (at-most-one)"
    );

    // --- Rollback half: aborted tx → zero new rows (no partial state). ---
    // A FRESH message (the commit-half one is already deleted — the
    // `deleted_at` guard would make the rollback half vacuous).
    let rollback_msg = repo
        .insert_outboxed(new_message(room, actor), None, Vec::new(), None)
        .await
        .expect("seed rollback message")
        .message()
        .clone();
    let mut tx = p.begin().await.expect("begin rollback tx");
    let existing = crate::message::MessageRepo::lock_message_in_tx(&mut tx, rollback_msg.id)
        .await
        .expect("lock the message")
        .expect("message row exists");
    crate::message::MessageRepo::soft_delete_locked_outboxed_in_tx(
        &mut tx,
        existing,
        Some(ws),
        Some(actor),
        Some(LOCAL_ACTION_MESSAGE_DELETED),
        serde_json::json!({ "room_id": room }),
        actor,
        None,
    )
    .await
    .expect("in-tx delete succeeds")
    .expect("deleted");
    tx.rollback().await.expect("rollback delete tx");
    assert_eq!(
        governance_rows_for(&p, ws).await,
        after,
        "rollback aborts the outbox row with the delete (writer is in-tx)"
    );
    assert_eq!(
        audit_rows_for(&p, ws, LOCAL_ACTION_MESSAGE_DELETED)
            .await
            .len(),
        1,
        "rollback aborted the audit row too"
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

/// D-7: the R3 live producer pin. `DmRepo::find_or_create_in_workspace`
/// (the REAL DM INSERT branch — `dm.rs:240`) is a deliberate domain
/// carve-out: direct-message rooms are internal conversation aggregates
/// (immutable 2-member fixed set, nil-workspace tenant attribution for
/// user-initiated DMs), so they commit with ZERO audit rows and ZERO
/// class-'room' outbox rows. Any future routing of DM creation through an
/// audit append reds `audit == 0` immediately.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn moderation_finalize_outbox_parity_dm_find_or_create_carved_out() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor) = fixture(&p).await;
    let peer = enroll_member(&p, ws, "member").await;
    let repo = crate::dm::DmRepo::new(p.clone());
    let room = repo
        .find_or_create_in_workspace(ws, actor, peer)
        .await
        .expect("dm find-or-create commits");
    assert_eq!(room.kind, RoomKind::Direct, "DM room kind");
    assert_eq!(room.created_by, actor, "creator = first participant");

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
        (1, 0, 0),
        "DM creation is carved out of audit + the 0245 room lane (R3)"
    );

    // Idempotent re-find: same room id, counts unchanged (no double-insert).
    let again = repo
        .find_or_create_in_workspace(ws, actor, peer)
        .await
        .expect("dm re-find");
    assert_eq!(again.id, room.id, "re-find returns the same DM room");
    let rooms_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rooms WHERE workspace_id = $1")
        .bind(ws.to_uuid())
        .fetch_one(&p)
        .await
        .expect("rooms count after re-find");
    let audit_after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE workspace_id = $1")
            .bind(ws.to_uuid())
            .fetch_one(&p)
            .await
            .expect("audit count after re-find");
    assert_eq!(
        (rooms_after, audit_after),
        (1, 0),
        "re-find is a no-op — zero new rows"
    );
}

/// D-7: the R4 live producer pin — group-DM twin of the DM carve-out.
/// `GroupDmRepo::find_or_create_in_workspace` (`group_dm.rs:268` INSERT)
/// commits with ZERO audit rows + ZERO class-'room' outbox rows; the fixed
/// 3..=8 member set is an internal conversation aggregate, outside the 0245
/// room lane (which is workspace-channel lifecycle only).
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn moderation_finalize_outbox_parity_group_dm_find_or_create_carved_out() {
    let p = pool();
    reset_governance_table(&p).await;
    let (ws, actor) = fixture(&p).await;
    // MIN_GROUP_DM_MEMBERS = 3; creator must be in the set.
    let second = enroll_member(&p, ws, "member").await;
    let third = enroll_member(&p, ws, "member").await;
    let repo = crate::group_dm::GroupDmRepo::new(p.clone());
    let members = [actor, second, third];
    let room = repo
        .find_or_create_in_workspace(ws, &members, actor)
        .await
        .expect("group dm find-or-create commits");
    assert_eq!(room.kind, RoomKind::Group, "group DM room kind");
    assert_eq!(room.created_by, actor, "creator = actor");

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
        (1, 0, 0),
        "group-DM creation is carved out of audit + the 0245 room lane (R4)"
    );

    // Idempotent re-find (via the `claim_exact_room_in_tx` revalidation
    // path): same room id, counts unchanged.
    let again = repo
        .find_or_create_in_workspace(ws, &members, actor)
        .await
        .expect("group dm re-find");
    assert_eq!(again.id, room.id, "re-find returns the same group-DM room");
    let rooms_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rooms WHERE workspace_id = $1")
        .bind(ws.to_uuid())
        .fetch_one(&p)
        .await
        .expect("rooms count after re-find");
    let audit_after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE workspace_id = $1")
            .bind(ws.to_uuid())
            .fetch_one(&p)
            .await
            .expect("audit count after re-find");
    assert_eq!(
        (rooms_after, audit_after),
        (1, 0),
        "re-find is a no-op — zero new rows"
    );
}

/// D-4b: enforcement-ON rollback variant of the integration carve-out. The
/// real `IntegrationRepo::publish` with enforcement ON and NO binding fails
/// fail-closed at the 0235 `messages_snaplink_metering` trigger (fires on
/// the messages INSERT before the audit append; 0236 would raise at the
/// append — same outcome) → the whole tx rolls back: zero rows in messages /
/// `event_outbox` / `integration_notification_receipts` / audit / governance
/// outbox. The commit half (exactly 1 `integration.notification.published`
/// row, actor NULL, 0 outbox rows) is pinned by
/// `…_integration_notification_carved_out` above.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn moderation_finalize_outbox_parity_integration_notification_carved_out_rollback() {
    let p = pool();
    reset_governance_table(&p).await;
    // Defensive re-assert OFF (fresh-DB default) before flipping ON.
    restore_enforcement_disabled(&p).await;
    let (ws, actor) = fixture(&p).await;
    // Seed bot + room + installation while enforcement is OFF — the fixture
    // paths must not trip the 0236 audit-channel RAISE themselves.
    let (bot, _token) = crate::bot::BotRepo::new(p.clone())
        .create_authorized_with_token(actor, "carveout-rb-bot", None, Some(ws))
        .await
        .expect("create bot");
    let room = crate::room::RoomRepo::new(p.clone())
        .create_in_workspace(
            ws,
            RoomKind::Channel,
            Some(format!("carveout-rb-{}", uuid::Uuid::new_v4())),
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
            issuer: format!("https://carveout-rb.test/{}", uuid::Uuid::new_v4()),
            user_identity_issuer: format!(
                "https://human-carveout-rb.test/{}",
                uuid::Uuid::new_v4()
            ),
            client_id: format!("erp-rb-{}", uuid::Uuid::new_v4()),
            name: "carveout-rb".into(),
            allow_user_dm: false,
            room_ids: vec![room],
            created_by: actor,
        })
        .await
        .expect("create installation");

    sqlx::query(
        "UPDATE snaplink_commercial_runtime
            SET enabled = TRUE, updated_at = clock_timestamp()
          WHERE singleton",
    )
    .execute(&p)
    .await
    .expect("enable commercial enforcement");

    let err = integration
        .publish(crate::integration::NewIntegrationNotification {
            installation_id: installation.id,
            issuer: installation.issuer.clone(),
            client_id: installation.client_id.clone(),
            idempotency_key: uuid::Uuid::new_v4(),
            request_hash: [0x5c; 32],
            target: crate::integration::IntegrationTarget::Room(room),
            room_id: room,
            recipient: None,
            blocks: vec![Block::text("carveout rollback notification")],
            traceparent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into()),
            lease_token: None,
        })
        .await
        .expect_err("publish must fail fail-closed without a binding");
    assert!(
        matches!(err, aero_common::Error::Upstream(_)),
        "0235 metering raises snaplink_binding_required → Error::Upstream"
    );

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
    let receipts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM integration_notification_receipts WHERE installation_id = $1",
    )
    .bind(installation.id)
    .fetch_one(&p)
    .await
    .expect("receipts count");
    // Audit assert is scoped to the publish's own token: the fixture's
    // `IntegrationRepo::create` legitimately wrote one
    // `integration.installation.created` row (pre-flip, enforcement OFF);
    // the failed publish must have written ZERO
    // `integration.notification.published` rows.
    let audit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events
          WHERE workspace_id = $1 AND action = 'integration.notification.published'",
    )
    .bind(ws.to_uuid())
    .fetch_one(&p)
    .await
    .expect("audit count");
    let governance = governance_rows_for(&p, ws).await;
    assert_eq!(
        (message_count, outbox_count, receipts, audit, governance),
        (0, 0, 0, 0, 0),
        "zero rows escape a fail-closed integration publish (rollback atomicity)"
    );
    restore_enforcement_disabled(&p).await;
}
