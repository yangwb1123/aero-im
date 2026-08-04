//! PostgreSQL-backed pin authorization and containment tests.

use std::time::Duration;

use aero_common::{RoomKind, WorkspaceId, WorkspaceRole};
use sqlx::PgPool;

use super::*;
use crate::{RoomRepo, WorkspaceRepo};

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect_lazy(&url)
        .expect("valid DATABASE_URL")
}

async fn participant(pool: &PgPool, label: &str) -> ParticipantId {
    let participant = ParticipantId::new();
    sqlx::query(
        "INSERT INTO participants (id, kind, display_name)
         VALUES ($1, 'human', $2)",
    )
    .bind(participant.to_uuid())
    .bind(format!("{label}-{participant}"))
    .execute(pool)
    .await
    .unwrap();
    participant
}

async fn workspace_room(pool: &PgPool, owner: ParticipantId, label: &str) -> (WorkspaceId, RoomId) {
    let workspace = WorkspaceRepo::new(pool.clone())
        .create(
            format!("{label}-{owner}"),
            format!("{label}-{owner}").to_ascii_lowercase(),
            owner,
        )
        .await
        .unwrap()
        .id;
    let room = RoomRepo::new(pool.clone())
        .create_in_workspace(
            workspace,
            RoomKind::Group,
            Some(format!("{label}-room")),
            owner,
        )
        .await
        .unwrap()
        .id;
    (workspace, room)
}

async fn add_room_member(
    pool: &PgPool,
    workspace: WorkspaceId,
    room: RoomId,
    participant: ParticipantId,
) {
    WorkspaceRepo::new(pool.clone())
        .add_member(workspace, participant, WorkspaceRole::Member)
        .await
        .unwrap();
    RoomRepo::new(pool.clone())
        .add_member(room, participant)
        .await
        .unwrap();
}

async fn message(pool: &PgPool, room: RoomId, sender: ParticipantId, label: &str) -> MessageId {
    let message = MessageId::new();
    sqlx::query(
        "INSERT INTO messages
             (id, room_id, sender_id, blocks, searchable_text, created_at)
         VALUES (
             $1,
             $2,
             $3,
             jsonb_build_array(jsonb_build_object('type', 'text', 'content', $4)),
             $4,
             CURRENT_TIMESTAMP
         )",
    )
    .bind(message.to_uuid())
    .bind(room.to_uuid())
    .bind(sender.to_uuid())
    .bind(label)
    .execute(pool)
    .await
    .unwrap();
    message
}

fn constraint(error: &sqlx::Error) -> Option<&str> {
    error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::constraint)
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn authorized_pin_is_idempotent_live_and_cross_room_opaque() {
    let pool = pool();
    let repo = PinRepo::new(pool.clone());
    let owner = participant(&pool, "pin-auth-owner").await;
    let outsider = participant(&pool, "pin-auth-outsider").await;
    let (workspace, room_a) = workspace_room(&pool, owner, "pin-auth").await;
    let room_b = RoomRepo::new(pool.clone())
        .create_in_workspace(
            workspace,
            RoomKind::Group,
            Some("pin-auth-room-b".into()),
            owner,
        )
        .await
        .unwrap()
        .id;
    let message_a = message(&pool, room_a, owner, "room-a").await;
    let message_b = message(&pool, room_b, owner, "room-b").await;
    let deleted = message(&pool, room_a, owner, "deleted").await;
    let expired = message(&pool, room_a, owner, "expired").await;
    sqlx::query("UPDATE messages SET deleted_at = CURRENT_TIMESTAMP WHERE id = $1")
        .bind(deleted.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE messages SET expires_at = CURRENT_TIMESTAMP - INTERVAL '1 second' WHERE id = $1",
    )
    .bind(expired.to_uuid())
    .execute(&pool)
    .await
    .unwrap();

    assert!(repo.pin_authorized(room_a, message_a, owner).await.unwrap());
    assert!(
        !repo.pin_authorized(room_a, message_a, owner).await.unwrap(),
        "re-pin keeps original provenance"
    );
    let listed = repo.list_authorized(room_a, owner).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].message.id, message_a);

    assert!(matches!(
        repo.pin_authorized(room_a, message_b, owner).await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        repo.pin_authorized(room_a, deleted, owner).await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        repo.pin_authorized(room_a, expired, owner).await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        repo.pin_authorized(room_a, message_a, outsider).await,
        Err(Error::Forbidden(_))
    ));
    assert!(matches!(
        repo.unpin_authorized(room_a, message_b, owner).await,
        Err(Error::NotFound(_))
    ));
    let cross_room_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM pins WHERE room_id = $1 AND message_id = $2",
    )
    .bind(room_a.to_uuid())
    .bind(message_b.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cross_room_count, 0);

    assert!(repo
        .unpin_authorized(room_a, message_a, owner)
        .await
        .unwrap());
    assert!(!repo
        .unpin_authorized(room_a, message_a, owner)
        .await
        .unwrap());
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn revocation_linearizes_pin_and_blocks_unpin_and_list() {
    let pool = pool();
    let repo = PinRepo::new(pool.clone());
    let owner = participant(&pool, "pin-revoke-owner").await;
    let actor = participant(&pool, "pin-revoke-actor").await;
    let (workspace, room) = workspace_room(&pool, owner, "pin-revoke").await;
    add_room_member(&pool, workspace, room, actor).await;
    let existing = message(&pool, room, owner, "existing").await;
    let raced_message = message(&pool, room, owner, "raced").await;
    repo.pin_authorized(room, existing, actor).await.unwrap();

    let mut revocation = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace.to_uuid())
        .execute(&mut *revocation)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspace_deactivations
             (workspace_id, participant_id, deactivated_by)
         VALUES ($1, $2, $3)",
    )
    .bind(workspace.to_uuid())
    .bind(actor.to_uuid())
    .bind(owner.to_uuid())
    .execute(&mut *revocation)
    .await
    .unwrap();

    let raced_repo = repo.clone();
    let mut raced =
        tokio::spawn(async move { raced_repo.pin_authorized(room, raced_message, actor).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut raced)
            .await
            .is_err(),
        "pin waits behind the workspace revocation fence"
    );
    revocation.commit().await.unwrap();
    assert!(matches!(raced.await.unwrap(), Err(Error::Forbidden(_))));
    let raced_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pins WHERE message_id = $1")
            .bind(raced_message.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(raced_count, 0);
    assert!(matches!(
        repo.unpin_authorized(room, existing, actor).await,
        Err(Error::Forbidden(_))
    ));
    assert!(matches!(
        repo.list_authorized(room, actor).await,
        Err(Error::Forbidden(_))
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pins WHERE message_id = $1")
            .bind(existing.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap(),
        1,
        "revoked unpin does not mutate the retained aggregate"
    );
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn raw_sql_enforces_containment_live_actor_immutability_and_cascade() {
    let pool = pool();
    let repo = PinRepo::new(pool.clone());
    let owner = participant(&pool, "pin-raw-owner").await;
    let outsider = participant(&pool, "pin-raw-outsider").await;
    let (workspace, room_a) = workspace_room(&pool, owner, "pin-raw").await;
    let room_b = RoomRepo::new(pool.clone())
        .create_in_workspace(
            workspace,
            RoomKind::Group,
            Some("pin-raw-room-b".into()),
            owner,
        )
        .await
        .unwrap()
        .id;
    let live = message(&pool, room_a, owner, "live").await;
    let other_room = message(&pool, room_b, owner, "other-room").await;
    let deleted = message(&pool, room_a, owner, "deleted").await;
    sqlx::query("UPDATE messages SET deleted_at = CURRENT_TIMESTAMP WHERE id = $1")
        .bind(deleted.to_uuid())
        .execute(&pool)
        .await
        .unwrap();

    let cross_room = sqlx::query(
        "INSERT INTO pins (room_id, message_id, pinned_by)
         VALUES ($1, $2, $3)",
    )
    .bind(room_a.to_uuid())
    .bind(other_room.to_uuid())
    .bind(owner.to_uuid())
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(constraint(&cross_room), Some("pins_message_room_scope_chk"));

    let stale = sqlx::query(
        "INSERT INTO pins (room_id, message_id, pinned_by)
         VALUES ($1, $2, $3)",
    )
    .bind(room_a.to_uuid())
    .bind(deleted.to_uuid())
    .bind(owner.to_uuid())
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(constraint(&stale), Some("pins_message_live_chk"));

    let forged_actor = sqlx::query(
        "INSERT INTO pins (room_id, message_id, pinned_by)
         VALUES ($1, $2, $3)",
    )
    .bind(room_a.to_uuid())
    .bind(live.to_uuid())
    .bind(outsider.to_uuid())
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&forged_actor),
        Some("pins_pinned_by_room_scope_chk")
    );

    assert!(repo.pin_authorized(room_a, live, owner).await.unwrap());
    let rewritten = sqlx::query(
        "UPDATE pins
            SET created_at = created_at + INTERVAL '1 second'
          WHERE room_id = $1 AND message_id = $2",
    )
    .bind(room_a.to_uuid())
    .bind(live.to_uuid())
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(constraint(&rewritten), Some("pins_identity_immutable_chk"));

    // A legacy pin can outlive a raw soft-delete, and authorized unpin remains
    // able to clean it. Hard deletion still cascades through the composite FK.
    sqlx::query("UPDATE messages SET deleted_at = CURRENT_TIMESTAMP WHERE id = $1")
        .bind(live.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(repo.unpin_authorized(room_a, live, owner).await.unwrap());

    let hard_deleted = message(&pool, room_a, owner, "hard-delete").await;
    assert!(repo
        .pin_authorized(room_a, hard_deleted, owner)
        .await
        .unwrap());
    sqlx::query("DELETE FROM messages WHERE id = $1")
        .bind(hard_deleted.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    let remaining = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pins WHERE message_id = $1")
        .bind(hard_deleted.to_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(remaining, 0);
}
