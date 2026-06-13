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

use aero_common::{Block, CallKind, CallMode, RoomKind, WorkspaceRole};
use aero_storage::{
    db::PgPool, AiJobRepo, BlockRepo, CallRepo, MessageRepo, ParticipantRepo, ReactionRepo,
    ReceiptRepo, RoomRepo, WorkspaceRepo,
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
    .with_workspaces(WorkspaceRepo::new(pool.clone()))
    // Blocking guard for 1:1 call/DM (no-op unless a block exists).
    .with_block_repo(BlockRepo::new(pool))
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
        .send_message(alice.id, room.id, vec![Block::text("hi")], None, None)
        .await
        .unwrap();
    assert_eq!(msg.room_id, room.id);

    let history = svc.history(bob.id, room.id, None, 50).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, msg.id);
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn pii_guard_blocks_message_with_sensitive_data() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let alice = new_participant(&participants, "alice").await;

    // Wire the PII guard onto the standard service (ROADMAP5 方向五).
    let svc = service(pool).with_pii_detector(std::sync::Arc::new(crate::PiiDetector::new(
        crate::PiiConfig::default(),
    )));
    let room = svc
        .create_room(alice.id, RoomKind::Group, Some("pii".into()))
        .await
        .unwrap();

    // A message carrying a Luhn-valid card number is rejected as Invalid before
    // it can reach the FTS index / embeddings / exports.
    let err = svc
        .send_message(
            alice.id,
            room.id,
            vec![Block::text("here is my card 4111 1111 1111 1111 thanks")],
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, aero_common::Error::Invalid(_)),
        "PII message must be blocked, got {err:?}"
    );

    // A clean message still sends, and nothing was persisted for the blocked one.
    let ok = svc
        .send_message(alice.id, room.id, vec![Block::text("ship it at 3pm")], None, None)
        .await
        .expect("clean message sends");
    assert_eq!(ok.room_id, room.id);
    let history = svc.history(alice.id, room.id, None, 50).await.unwrap();
    assert_eq!(history.len(), 1, "only the clean message persisted");
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
        .send_message(intruder.id, room.id, vec![Block::text("hi")], None, None)
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
        .send_message(alice.id, room.id, vec![Block::text("hi")], None, None)
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

/// A 1:1 direct-message call must not connect users who have blocked each other
/// (either direction), closing the gap where a pre-existing DM could be used to
/// ring someone after a block (notification suppression can't catch a live call).
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn blocked_user_cannot_call_in_direct_room() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let blocks = BlockRepo::new(pool.clone());
    let svc = service(pool.clone());

    let alice = new_participant(&participants, "alice-callblk").await;
    let bob = new_participant(&participants, "bob-callblk").await;

    let room = svc.create_room(alice.id, RoomKind::Direct, Some("dm".into())).await.unwrap();
    svc.add_member(alice.id, room.id, bob.id).await.unwrap();

    blocks.block(alice.id, bob.id).await.unwrap();

    // Neither direction may ring the other while the block stands.
    let err = svc
        .start_call(bob.id, room.id, CallKind::Audio, CallMode::P2p, "sdp".into())
        .await
        .unwrap_err();
    assert!(matches!(err, aero_common::Error::Forbidden(_)), "blocked → forbidden, got {err:?}");
    let err2 = svc
        .start_call(alice.id, room.id, CallKind::Audio, CallMode::P2p, "sdp".into())
        .await
        .unwrap_err();
    assert!(matches!(err2, aero_common::Error::Forbidden(_)));

    // After unblock the call is permitted again (reaches the call machinery).
    blocks.unblock(alice.id, bob.id).await.unwrap();
    svc.start_call(bob.id, room.id, CallKind::Audio, CallMode::P2p, "sdp".into())
        .await
        .expect("call allowed after unblock");
}
