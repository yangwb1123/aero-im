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

use aero_common::{Block, RoomKind};
use aero_storage::{db::PgPool, MessageRepo, ParticipantRepo, RoomRepo};

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
        ParticipantRepo::new(pool),
        bus,
    )
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
        ParticipantRepo::new(pool),
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
