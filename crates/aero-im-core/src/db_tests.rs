//! DB-integration tests for `ImService`.
//!
//! These tests require a running Postgres with the workspace migrations applied.
//! They are marked `#[ignore]` so `cargo test` stays hermetic; run explicitly with:
//!
//! ```bash
//! DATABASE_URL=postgres://aero:aero@localhost/aero_test \
//!   cargo test -p aero-im-core --lib -- --ignored
//! ```

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use aero_common::{Block, RoomKind, WorkspaceRole};
use aero_storage::{
    db::PgPool, AiJobRepo, CallRepo, MessageRepo, ParticipantRepo, ReactionRepo, ReceiptRepo,
    RoomRepo, WorkspaceRepo,
};

use crate::service::ImService;
use crate::test_util::MockBus;

fn unique_email(prefix: &str) -> String {
    // Avoid pulling in `uuid` directly — IDs already give us ULIDs through `aero-common`.
    format!("{prefix}-{}@test.local", aero_common::ParticipantId::new())
}

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero@localhost/aero_test".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on well-formed URL")
}

fn service(pool: PgPool) -> ImService {
    let bus = Arc::new(MockBus::default());
    ImService::new(
        RoomRepo::new(pool.clone()),
        MessageRepo::new(pool.clone()),
        ParticipantRepo::new(pool.clone()),
        ReceiptRepo::new(pool.clone()),
        ReactionRepo::new(pool.clone()),
        CallRepo::new(pool.clone()),
        AiJobRepo::new(pool.clone()),
        bus,
    )
    // Workspace-scoped methods need the tenancy repo wired in (additive builder).
    .with_workspaces(WorkspaceRepo::new(pool))
}

async fn new_participant(participants: &ParticipantRepo, prefix: &str) -> aero_common::Participant {
    participants
        .create_human(aero_storage::participant::NewHuman {
            email: unique_email(prefix),
            display_name: prefix.into(),
            password_hash: "x".into(),
        })
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn create_room_and_send_message() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let alice = participants
        .create_human(aero_storage::participant::NewHuman {
            email: unique_email("alice"),
            display_name: "Alice".into(),
            password_hash: "x".into(),
        })
        .await
        .unwrap();
    let bob = participants
        .create_human(aero_storage::participant::NewHuman {
            email: unique_email("bob"),
            display_name: "Bob".into(),
            password_hash: "x".into(),
        })
        .await
        .unwrap();

    let svc = service(pool);
    let room = svc
        .create_room(alice.id, RoomKind::Group, Some("test".into()))
        .await
        .unwrap();
    svc.add_member(alice.id, room.id, bob.id).await.unwrap();

    let msg = svc
        .send_message(alice.id, room.id, vec![Block::text("hi")], None)
        .await
        .unwrap();
    assert_eq!(msg.room_id, room.id);

    let history = svc.history(bob.id, room.id, None, 50).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, msg.id);
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn non_member_cannot_send_message() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let alice = participants
        .create_human(aero_storage::participant::NewHuman {
            email: unique_email("alice"),
            display_name: "Alice".into(),
            password_hash: "x".into(),
        })
        .await
        .unwrap();
    let intruder = participants
        .create_human(aero_storage::participant::NewHuman {
            email: unique_email("eve"),
            display_name: "Eve".into(),
            password_hash: "x".into(),
        })
        .await
        .unwrap();

    let svc = service(pool);
    let room = svc
        .create_room(alice.id, RoomKind::Group, Some("test".into()))
        .await
        .unwrap();

    let err = svc
        .send_message(intruder.id, room.id, vec![Block::text("hi")], None)
        .await
        .unwrap_err();
    assert!(matches!(err, aero_common::Error::Forbidden(_)));
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn send_message_publishes_envelope() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let alice = participants
        .create_human(aero_storage::participant::NewHuman {
            email: unique_email("alice"),
            display_name: "Alice".into(),
            password_hash: "x".into(),
        })
        .await
        .unwrap();

    let bus = Arc::new(MockBus::default());
    let svc = ImService::new(
        RoomRepo::new(pool.clone()),
        MessageRepo::new(pool.clone()),
        ParticipantRepo::new(pool.clone()),
        ReceiptRepo::new(pool.clone()),
        ReactionRepo::new(pool.clone()),
        CallRepo::new(pool.clone()),
        AiJobRepo::new(pool),
        bus.clone(),
    );
    let room = svc
        .create_room(alice.id, RoomKind::Group, Some("publish-test".into()))
        .await
        .unwrap();
    let _ = svc
        .send_message(alice.id, room.id, vec![Block::text("hi")], None)
        .await
        .unwrap();

    let log = bus.published.lock().unwrap();
    // Expect at least one publish to the per-room subject.
    assert!(
        log.iter().any(|(s, _)| s == &ImService::room_subject(room.id)),
        "no publish on im.room.{room_id}: {log:?}",
        room_id = room.id
    );
}

// ---------------------------------------------------------------------------
// Workspace-scoped tenancy methods (additive). These document and compile-verify
// the new RoomRepo/WorkspaceRepo/ImService behavior; they run only against a live
// Postgres with the 0006 workspace migration applied.
// ---------------------------------------------------------------------------

fn unique_slug(prefix: &str) -> String {
    format!("{prefix}-{}", aero_common::WorkspaceId::new())
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn create_room_in_workspace_member_succeeds_room_carries_workspace() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let workspaces = WorkspaceRepo::new(pool.clone());
    let rooms = RoomRepo::new(pool.clone());

    let alice = new_participant(&participants, "alice").await;
    // create() enrolls alice as workspace owner.
    let ws = workspaces
        .create("Acme".into(), unique_slug("acme"), alice.id)
        .await
        .unwrap();

    let svc = service(pool);
    let room = svc
        .create_room_in_workspace(alice.id, ws.id, RoomKind::Channel, Some("general".into()))
        .await
        .unwrap();

    // The room must be stamped with its owning workspace.
    assert_eq!(rooms.room_workspace(room.id).await.unwrap(), Some(ws.id));
    // And it must appear in the workspace-scoped listing for alice.
    let scoped = rooms.rooms_for_in_workspace(alice.id, ws.id).await.unwrap();
    assert!(scoped.iter().any(|r| r.id == room.id));
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn create_room_in_workspace_non_member_forbidden() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let workspaces = WorkspaceRepo::new(pool.clone());

    let owner = new_participant(&participants, "owner").await;
    let outsider = new_participant(&participants, "outsider").await;
    let ws = workspaces
        .create("Acme".into(), unique_slug("acme"), owner.id)
        .await
        .unwrap();

    let svc = service(pool);
    let err = svc
        .create_room_in_workspace(outsider.id, ws.id, RoomKind::Channel, None)
        .await
        .unwrap_err();
    assert!(matches!(err, aero_common::Error::Forbidden(_)), "got {err:?}");
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn create_room_in_workspace_guest_forbidden() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let workspaces = WorkspaceRepo::new(pool.clone());

    let owner = new_participant(&participants, "owner").await;
    let guest = new_participant(&participants, "guest").await;
    let ws = workspaces
        .create("Acme".into(), unique_slug("acme"), owner.id)
        .await
        .unwrap();
    workspaces
        .add_member(ws.id, guest.id, WorkspaceRole::Guest)
        .await
        .unwrap();

    let svc = service(pool);
    let err = svc
        .create_room_in_workspace(guest.id, ws.id, RoomKind::Channel, None)
        .await
        .unwrap_err();
    assert!(matches!(err, aero_common::Error::Forbidden(_)), "got {err:?}");
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn assert_room_access_allows_workspace_and_room_member() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let workspaces = WorkspaceRepo::new(pool.clone());

    let alice = new_participant(&participants, "alice").await;
    let ws = workspaces
        .create("Acme".into(), unique_slug("acme"), alice.id)
        .await
        .unwrap();

    let svc = service(pool);
    let room = svc
        .create_room_in_workspace(alice.id, ws.id, RoomKind::Channel, None)
        .await
        .unwrap();

    // alice is both a workspace member and (as creator/owner) a room member.
    svc.assert_room_access(alice.id, room.id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn assert_room_access_denies_non_room_member_in_same_workspace() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let workspaces = WorkspaceRepo::new(pool.clone());

    let alice = new_participant(&participants, "alice").await;
    let bob = new_participant(&participants, "bob").await;
    let ws = workspaces
        .create("Acme".into(), unique_slug("acme"), alice.id)
        .await
        .unwrap();
    // bob is in the workspace but NOT in the room alice creates.
    workspaces
        .add_member(ws.id, bob.id, WorkspaceRole::Member)
        .await
        .unwrap();

    let svc = service(pool);
    let room = svc
        .create_room_in_workspace(alice.id, ws.id, RoomKind::Channel, None)
        .await
        .unwrap();

    let err = svc.assert_room_access(bob.id, room.id).await.unwrap_err();
    assert!(matches!(err, aero_common::Error::Forbidden(_)), "got {err:?}");
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn assert_room_access_unknown_room_is_not_found() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let alice = new_participant(&participants, "alice").await;

    let svc = service(pool);
    let err = svc
        .assert_room_access(alice.id, aero_common::RoomId::new())
        .await
        .unwrap_err();
    assert!(matches!(err, aero_common::Error::NotFound(_)), "got {err:?}");
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn update_member_role_upserts_idempotently() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let workspaces = WorkspaceRepo::new(pool.clone());

    let owner = new_participant(&participants, "owner").await;
    let member = new_participant(&participants, "member").await;
    let ws = workspaces
        .create("Acme".into(), unique_slug("acme"), owner.id)
        .await
        .unwrap();

    // Insert path: member did not exist yet.
    workspaces
        .update_member_role(ws.id, member.id, WorkspaceRole::Member)
        .await
        .unwrap();
    assert_eq!(
        workspaces.member_role(ws.id, member.id).await.unwrap(),
        Some(WorkspaceRole::Member)
    );

    // Update path: overwrite existing role (what add_member's DO NOTHING couldn't do).
    workspaces
        .update_member_role(ws.id, member.id, WorkspaceRole::Admin)
        .await
        .unwrap();
    assert_eq!(
        workspaces.member_role(ws.id, member.id).await.unwrap(),
        Some(WorkspaceRole::Admin)
    );

    // Idempotent re-apply.
    workspaces
        .update_member_role(ws.id, member.id, WorkspaceRole::Admin)
        .await
        .unwrap();
    assert_eq!(
        workspaces.member_role(ws.id, member.id).await.unwrap(),
        Some(WorkspaceRole::Admin)
    );
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn rooms_for_in_workspace_is_tenant_scoped() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let workspaces = WorkspaceRepo::new(pool.clone());
    let rooms = RoomRepo::new(pool.clone());

    let alice = new_participant(&participants, "alice").await;
    let ws_a = workspaces
        .create("A".into(), unique_slug("a"), alice.id)
        .await
        .unwrap();
    let ws_b = workspaces
        .create("B".into(), unique_slug("b"), alice.id)
        .await
        .unwrap();

    let svc = service(pool);
    let room_a = svc
        .create_room_in_workspace(alice.id, ws_a.id, RoomKind::Channel, None)
        .await
        .unwrap();
    let room_b = svc
        .create_room_in_workspace(alice.id, ws_b.id, RoomKind::Channel, None)
        .await
        .unwrap();

    let in_a = rooms.rooms_for_in_workspace(alice.id, ws_a.id).await.unwrap();
    assert!(in_a.iter().any(|r| r.id == room_a.id));
    assert!(
        !in_a.iter().any(|r| r.id == room_b.id),
        "workspace A listing must not leak workspace B's room"
    );
}

// ---------------------------------------------------------------------------
// ROADMAP 第四版 — collaboration: #channel mentions + thread muting + per-room
// notification levels (all exercised through the dispatch_notifications fan-out).
// ---------------------------------------------------------------------------

/// Posting a message carrying a `Block::ChannelMention { room: B }` into room A
/// notifies the MEMBERS of room B (the mentioned channel), even though they are
/// not members of room A. The durable notification points at the posted message.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn channel_mention_notifies_target_room_members() {
    use aero_common::NotificationKind;
    use aero_storage::NotificationRepo;

    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let notifications = NotificationRepo::new(pool.clone());

    let alice = new_participant(&participants, "alice").await; // posts in room A
    let bob = new_participant(&participants, "bob").await; // member of room B only

    let bus = Arc::new(MockBus::default());
    let svc = ImService::new(
        RoomRepo::new(pool.clone()),
        MessageRepo::new(pool.clone()),
        ParticipantRepo::new(pool.clone()),
        ReceiptRepo::new(pool.clone()),
        ReactionRepo::new(pool.clone()),
        CallRepo::new(pool.clone()),
        AiJobRepo::new(pool.clone()),
        bus,
    )
    .with_workspaces(WorkspaceRepo::new(pool.clone()))
    .with_notifications(notifications.clone());

    // Room A (alice) and Room B (bob) — distinct, non-overlapping membership.
    let room_a = svc
        .create_room(alice.id, RoomKind::Channel, Some("room-a".into()))
        .await
        .unwrap();
    let room_b = svc
        .create_room(bob.id, RoomKind::Channel, Some("room-b".into()))
        .await
        .unwrap();

    // alice mentions #room-b inside room-a. Dispatch is awaited under cfg(test).
    let msg = svc
        .send_message(
            alice.id,
            room_a.id,
            vec![
                Block::text("see this in"),
                Block::ChannelMention { room: room_b.id },
            ],
            None,
        )
        .await
        .unwrap();

    // bob (a member of the mentioned room B, NOT of room A) got a notification.
    let bob_inbox = notifications.list(bob.id, None, false, Some(50)).await.unwrap();
    let hit = bob_inbox
        .iter()
        .find(|n| n.message_id == msg.id)
        .expect("bob is notified of the #channel mention");
    assert_eq!(hit.kind, NotificationKind::Mention, "channel mention is a mention-kind");
    assert_eq!(hit.actor_id, Some(alice.id), "actor is the poster");
    assert_eq!(hit.room_id, room_a.id, "notification points at the posting room");

    // The poster never notifies themselves.
    let alice_inbox = notifications.list(alice.id, None, false, Some(50)).await.unwrap();
    assert!(
        !alice_inbox.iter().any(|n| n.message_id == msg.id),
        "the poster does not notify themselves on a channel mention"
    );
}

/// A thread-muter is SUBTRACTED from the reply notification fan-out: the root
/// author who has muted their own thread gets no reply notification, while an
/// unmuted subscriber still does.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn thread_mute_suppresses_reply_notification() {
    use aero_storage::{NotificationRepo, ThreadMuteRepo};

    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let notifications = NotificationRepo::new(pool.clone());
    let mutes = ThreadMuteRepo::new(pool.clone());

    let alice = new_participant(&participants, "alice").await; // root author
    let bob = new_participant(&participants, "bob").await; // replier

    let bus = Arc::new(MockBus::default());
    let svc = ImService::new(
        RoomRepo::new(pool.clone()),
        MessageRepo::new(pool.clone()),
        ParticipantRepo::new(pool.clone()),
        ReceiptRepo::new(pool.clone()),
        ReactionRepo::new(pool.clone()),
        CallRepo::new(pool.clone()),
        AiJobRepo::new(pool.clone()),
        bus,
    )
    .with_workspaces(WorkspaceRepo::new(pool.clone()))
    .with_notifications(notifications.clone())
    .with_thread_mutes(mutes.clone());

    let room = svc
        .create_room(alice.id, RoomKind::Group, Some("thread-mute".into()))
        .await
        .unwrap();
    svc.add_member(alice.id, room.id, bob.id).await.unwrap();

    // alice posts a root; bob replies — normally alice (the root author) is notified.
    let root = svc
        .send_message(alice.id, room.id, vec![Block::text("root")], None)
        .await
        .unwrap();
    let reply1 = svc
        .send_message(bob.id, room.id, vec![Block::text("reply one")], Some(root.id))
        .await
        .unwrap();
    let after_first = notifications.list(alice.id, None, false, Some(50)).await.unwrap();
    assert!(
        after_first.iter().any(|n| n.message_id == reply1.id),
        "the root author is notified of a reply before muting"
    );

    // alice mutes her own thread, then bob replies again — no new notification.
    mutes.mute(alice.id, root.id).await.unwrap();
    let reply2 = svc
        .send_message(bob.id, room.id, vec![Block::text("reply two")], Some(root.id))
        .await
        .unwrap();
    let after_mute = notifications.list(alice.id, None, false, Some(50)).await.unwrap();
    assert!(
        !after_mute.iter().any(|n| n.message_id == reply2.id),
        "a muted thread yields no reply notification to the muter"
    );
}
