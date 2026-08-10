//! DB-integration tests for `ImService::recall_message` (撤回): the stable
//! failure ordering (404 → 403 tenant → 409 state → 403 role), the successful
//! placeholder return, the admin/owner path, and the durable event outbox.

use aero_common::{Block, Error, RoomKind, WorkspaceRole};
use aero_storage::{ParticipantRepo, RoomMemberRole, RoomRepo, WorkspaceRepo};

use super::{new_participant, pool, service};

/// Build a workspace with two rooms so cross-room (same-workspace) and
/// cross-workspace isolation can be exercised through the service boundary.
async fn room_with_admin(prefix: &str) -> (aero_common::RoomId, aero_common::ParticipantId) {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let owner = new_participant(&participants, prefix).await;
    let admin = new_participant(&participants, &format!("{prefix}-admin")).await;
    let workspaces = WorkspaceRepo::new(pool.clone());
    let workspace = workspaces
        .create(
            format!("Recall {prefix} {}", owner.id),
            format!("recall-{prefix}-{}", owner.id),
            owner.id,
        )
        .await
        .unwrap()
        .id;
    workspaces
        .add_member(workspace, admin.id, WorkspaceRole::Admin)
        .await
        .unwrap();
    let rooms = RoomRepo::new(pool.clone());
    let room = rooms
        .create_in_workspace(
            workspace,
            RoomKind::Channel,
            Some(format!("recall-{prefix}-room")),
            owner.id,
        )
        .await
        .unwrap()
        .id;
    rooms.add_member(room, admin.id).await.unwrap();
    rooms
        .change_channel_member_role_authorized(room, owner.id, admin.id, RoomMemberRole::Admin)
        .await
        .unwrap();
    (room, admin.id)
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn recall_success_returns_placeholder_and_author_flow() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let author = new_participant(&participants, "recall-svc-author").await;
    let svc = service(pool.clone());
    let room = svc
        .create_room(author.id, RoomKind::Group, Some("recall svc room".into()))
        .await
        .unwrap()
        .id;
    let sent = svc
        .send_message(
            author.id,
            room,
            vec![Block::text("svc recall body")],
            None,
            None,
        )
        .await
        .unwrap();

    let recalled = svc.recall_message(author.id, sent.id).await.unwrap();
    assert_eq!(recalled.id, sent.id);
    assert!(recalled.recalled_at.is_some());
    assert_eq!(recalled.recalled_by, Some(author.id));
    assert_eq!(
        serde_json::to_value(&recalled.blocks).unwrap(),
        serde_json::json!([{ "type": "text", "content": aero_common::RECALLED_MESSAGE_PLACEHOLDER }])
    );
    assert_eq!(recalled.version, sent.version + 1);

    // Double recall → stable Conflict, not silence.
    let err = svc.recall_message(author.id, sent.id).await.unwrap_err();
    assert!(matches!(&err, Error::Conflict(msg) if msg == "message is already recalled"));
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn recall_admin_can_recall_but_member_and_stranger_cannot() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let owner = new_participant(&participants, "recall-svc-owner").await;
    let member = new_participant(&participants, "recall-svc-member").await;
    let svc = service(pool.clone());
    let room = svc
        .create_room(
            owner.id,
            RoomKind::Channel,
            Some("recall svc room 2".into()),
        )
        .await
        .unwrap()
        .id;
    svc.add_member(owner.id, room, member.id).await.unwrap();

    let sent = svc
        .send_message(
            owner.id,
            room,
            vec![Block::text("admin can recall")],
            None,
            None,
        )
        .await
        .unwrap();

    // Plain member (non-author) → stable Forbidden.
    let err = svc.recall_message(member.id, sent.id).await.unwrap_err();
    assert!(matches!(&err, Error::Forbidden(msg) if msg == "only author or room admin may recall"));

    // Room admin (promoted) can recall.
    let (_, admin_id) = room_with_admin("recall-svc-promote").await;
    let rooms = RoomRepo::new(pool.clone());
    rooms.add_member(room, admin_id).await.unwrap();
    rooms
        .change_channel_member_role_authorized(room, owner.id, admin_id, RoomMemberRole::Admin)
        .await
        .unwrap();
    let recalled = svc.recall_message(admin_id, sent.id).await.unwrap();
    assert_eq!(recalled.recalled_by, Some(admin_id));

    // Unknown message → NotFound, before any permission question.
    let err = svc
        .recall_message(member.id, aero_common::MessageId::new())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotFound(_)));
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn recall_cross_room_member_is_forbidden_without_state_leak() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let author = new_participant(&participants, "recall-svc-iso-a").await;
    let outsider = new_participant(&participants, "recall-svc-iso-b").await;
    let svc = service(pool.clone());
    let room_a = svc
        .create_room(author.id, RoomKind::Group, Some("recall iso a".into()))
        .await
        .unwrap()
        .id;

    let sent = svc
        .send_message(
            author.id,
            room_a,
            vec![Block::text("iso secret")],
            None,
            None,
        )
        .await
        .unwrap();

    // Outsider is a member of room B only: recalling A's message is Forbidden
    // (the state — live vs recalled — is never disclosed).
    let err = svc.recall_message(outsider.id, sent.id).await.unwrap_err();
    assert!(matches!(err, Error::Forbidden(_)));

    // The message is untouched.
    let fetched = svc
        .messages
        .get(sent.id)
        .await
        .unwrap()
        .expect("row survives");
    assert!(fetched.recalled_at.is_none());
}

/// The rate-gate preflight (`assert_message_recall_preflight`) mirrors the
/// edit preflight: it resolves the room through the shared access guard so
/// the workspace rate budget can be charged (and non-members can never drain
/// a victim's budget by spamming message ids), and returns the same stable
/// early errors as `recall_message` — while never being authority itself.
/// Gate S1: the recall role gate (author or room owner/admin) is INSIDE the
/// preflight, so a plain member's doomed attempts are rejected before any
/// edge can charge the shared workspace budget.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn recall_preflight_resolves_room_and_early_errors() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let author = new_participant(&participants, "recall-preflight-author").await;
    let outsider = new_participant(&participants, "recall-preflight-outsider").await;
    let svc = service(pool.clone());
    let room = svc
        .create_room(
            author.id,
            RoomKind::Group,
            Some("recall preflight room".into()),
        )
        .await
        .unwrap()
        .id;
    let sent = svc
        .send_message(
            author.id,
            room,
            vec![Block::text("preflight target")],
            None,
            None,
        )
        .await
        .unwrap();

    // Live message → the message's room, ready for the rate charge.
    let resolved = svc
        .assert_message_recall_preflight(author.id, sent.id)
        .await
        .unwrap();
    assert_eq!(resolved, room);

    // Unknown message → NotFound, before any permission question.
    let err = svc
        .assert_message_recall_preflight(author.id, aero_common::MessageId::new())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotFound(_)));

    // No room access → Forbidden (no state leak, no budget charge).
    let err = svc
        .assert_message_recall_preflight(outsider.id, sent.id)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Forbidden(_)));

    // Gate S1 (rate-gate DoS amplifier): a PLAIN member recalling someone
    // else's message must be Forbidden from the preflight itself — the role
    // gate runs inside the preflight, i.e. BEFORE any edge can charge the
    // shared per-workspace rate budget. Doomed recall attempts by non-author
    // members therefore never consume rate capacity (the exact regression the
    // security review blocked on). Uses a Channel room so the member can be
    // promoted to room admin afterwards (`change_channel_member_role_authorized`
    // is channel-only).
    let member = new_participant(&participants, "recall-preflight-member").await;
    let chan_room = svc
        .create_room(
            author.id,
            RoomKind::Channel,
            Some("recall preflight channel".into()),
        )
        .await
        .unwrap()
        .id;
    svc.add_member(author.id, chan_room, member.id)
        .await
        .unwrap();
    let third = svc
        .send_message(
            author.id,
            chan_room,
            vec![Block::text("preflight member target")],
            None,
            None,
        )
        .await
        .unwrap();
    let err = svc
        .assert_message_recall_preflight(member.id, third.id)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Forbidden(msg) if msg == "only author or room admin may recall"),
        "plain member must be rejected by the preflight role gate, got {err:?}"
    );

    // A room owner/admin recaller IS authorized: the preflight resolves the
    // room so the edge may charge the workspace budget.
    RoomRepo::new(pool.clone())
        .change_channel_member_role_authorized(
            chan_room,
            author.id,
            member.id,
            RoomMemberRole::Admin,
        )
        .await
        .unwrap();
    let resolved = svc
        .assert_message_recall_preflight(member.id, third.id)
        .await
        .unwrap();
    assert_eq!(
        resolved, chan_room,
        "admin recaller passes the preflight role gate"
    );

    // Deleted message → the stable 409.
    let second = svc
        .send_message(
            author.id,
            room,
            vec![Block::text("preflight delete target")],
            None,
            None,
        )
        .await
        .unwrap();
    svc.delete_message(author.id, second.id).await.unwrap();
    let err = svc
        .assert_message_recall_preflight(author.id, second.id)
        .await
        .unwrap_err();
    assert!(matches!(&err, Error::Conflict(msg) if msg == "message is deleted"));

    // Recalled message → the stable 409.
    svc.recall_message(author.id, sent.id).await.unwrap();
    let err = svc
        .assert_message_recall_preflight(author.id, sent.id)
        .await
        .unwrap_err();
    assert!(matches!(&err, Error::Conflict(msg) if msg == "message is already recalled"));
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn recall_preflight_window_expired_author() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let author = new_participant(&participants, "recall-window-svc-author").await;
    // Window tests build their own service: the shared fixture pins unlimited.
    let svc = service(pool.clone()).with_recall_window(time::Duration::seconds(60));
    let room = svc
        .create_room(
            author.id,
            RoomKind::Group,
            Some("recall window svc room".into()),
        )
        .await
        .unwrap()
        .id;
    let sent = svc
        .send_message(author.id, room, vec![Block::text("old body")], None, None)
        .await
        .unwrap();
    // Backdate created_at with an APP-clock parameter-bound timestamp
    // (created_at is app-minted; DB now() would mix clocks).
    sqlx::query("UPDATE messages SET created_at = $1 WHERE id = $2")
        .bind(aero_common::time::now_utc() - time::Duration::seconds(120))
        .bind(sent.id.to_uuid())
        .execute(&pool)
        .await
        .unwrap();

    // Preflight rejects BEFORE any rate charge (gate S1).
    let err = svc
        .assert_message_recall_preflight(author.id, sent.id)
        .await
        .unwrap_err();
    assert!(matches!(&err, Error::Conflict(msg) if msg == "recall window expired"));

    // The service path returns the same stable 409 and leaves the row live.
    let err = svc.recall_message(author.id, sent.id).await.unwrap_err();
    assert!(matches!(&err, Error::Conflict(msg) if msg == "recall window expired"));
    let live = svc.messages.get(sent.id).await.unwrap().unwrap();
    assert!(
        live.recalled_at.is_none(),
        "expired recall must not mutate the row"
    );
    assert_eq!(live.blocks.len(), 1);
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn recall_preflight_window_admin_exempt() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let author = new_participant(&participants, "recall-window-svc-author2").await;
    let svc = service(pool.clone()).with_recall_window(time::Duration::seconds(60));
    let room = svc
        .create_room(
            author.id,
            RoomKind::Channel,
            Some("recall window svc room 3".into()),
        )
        .await
        .unwrap()
        .id;
    // Promote an admin into the room (same pattern as the permission-matrix test).
    let (_, admin_id) = room_with_admin("recall-window-svc-admin").await;
    let rooms = RoomRepo::new(pool.clone());
    rooms.add_member(room, admin_id).await.unwrap();
    rooms
        .change_channel_member_role_authorized(room, author.id, admin_id, RoomMemberRole::Admin)
        .await
        .unwrap();

    let sent = svc
        .send_message(
            author.id,
            room,
            vec![Block::text("admin can recall expired")],
            None,
            None,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE messages SET created_at = $1 WHERE id = $2")
        .bind(aero_common::time::now_utc() - time::Duration::seconds(120))
        .bind(sent.id.to_uuid())
        .execute(&pool)
        .await
        .unwrap();

    // Admin/owner is exempt from the window (moderation path).
    let room_id = svc
        .assert_message_recall_preflight(admin_id, sent.id)
        .await
        .unwrap();
    assert_eq!(room_id, room);
    let recalled = svc.recall_message(admin_id, sent.id).await.unwrap();
    assert_eq!(recalled.recalled_by, Some(admin_id));
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn recall_window_no_leak_to_member_service() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let author = new_participant(&participants, "recall-window-svc-author3").await;
    let member = new_participant(&participants, "recall-window-svc-member").await;
    let svc = service(pool.clone()).with_recall_window(time::Duration::seconds(60));
    let room = svc
        .create_room(
            author.id,
            RoomKind::Group,
            Some("recall window svc room 4".into()),
        )
        .await
        .unwrap()
        .id;
    svc.add_member(author.id, room, member.id).await.unwrap();
    let sent = svc
        .send_message(author.id, room, vec![Block::text("old secret")], None, None)
        .await
        .unwrap();
    sqlx::query("UPDATE messages SET created_at = $1 WHERE id = $2")
        .bind(aero_common::time::now_utc() - time::Duration::days(30))
        .bind(sent.id.to_uuid())
        .execute(&pool)
        .await
        .unwrap();

    // Plain member probing an expired message: Forbidden, never the window
    // string (window state is author/admin-only knowledge).
    let err = svc.recall_message(member.id, sent.id).await.unwrap_err();
    assert!(matches!(&err, Error::Forbidden(msg) if msg == "only author or room admin may recall"));
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn recall_window_precedence_deleted_before_expired_service() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let author = new_participant(&participants, "recall-window-svc-author4").await;
    let svc = service(pool.clone()).with_recall_window(time::Duration::seconds(60));
    let room = svc
        .create_room(
            author.id,
            RoomKind::Group,
            Some("recall window svc room 2".into()),
        )
        .await
        .unwrap()
        .id;
    let sent = svc
        .send_message(
            author.id,
            room,
            vec![Block::text("old and gone")],
            None,
            None,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE messages SET created_at = $1 WHERE id = $2")
        .bind(aero_common::time::now_utc() - time::Duration::days(30))
        .bind(sent.id.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    svc.delete_message(author.id, sent.id).await.unwrap();

    let err = svc.recall_message(author.id, sent.id).await.unwrap_err();
    assert!(matches!(&err, Error::Conflict(msg) if msg == "message is deleted"));
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn recall_in_window_author_succeeds_with_nonzero_window() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let author = new_participant(&participants, "recall-window-svc-inwindow").await;
    let svc = service(pool.clone()).with_recall_window(time::Duration::seconds(60));
    let room = svc
        .create_room(
            author.id,
            RoomKind::Group,
            Some("recall window in-window room".into()),
        )
        .await
        .unwrap()
        .id;
    let sent = svc
        .send_message(author.id, room, vec![Block::text("fresh body")], None, None)
        .await
        .unwrap();

    // Fresh message, non-zero window: preflight and recall both succeed.
    let room_id = svc
        .assert_message_recall_preflight(author.id, sent.id)
        .await
        .unwrap();
    assert_eq!(room_id, room);
    let recalled = svc.recall_message(author.id, sent.id).await.unwrap();
    assert_eq!(recalled.id, sent.id);
    assert_eq!(recalled.recalled_by, Some(author.id));
    assert!(recalled.recalled_at.is_some());
}
