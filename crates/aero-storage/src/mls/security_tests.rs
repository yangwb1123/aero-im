//! PostgreSQL-backed MLS transaction and tenant-boundary tests.

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
            RoomKind::Channel,
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

fn group(group_id: &[u8], room: RoomId, epoch: u64, state: &[u8]) -> MlsGroupState {
    MlsGroupState {
        group_id: MlsGroupId::new(group_id.to_vec()),
        room_id: Some(room),
        ciphersuite: "MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519".into(),
        epoch,
        state: state.to_vec(),
        updated_at: time::OffsetDateTime::now_utc(),
    }
}

fn constraint(error: &sqlx::Error) -> Option<&str> {
    error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::constraint)
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn group_cross_room_nonmember_and_epoch_rollback_are_rejected() {
    let pool = pool();
    let repo = MlsGroupRepo::new(pool.clone());
    let owner = participant(&pool, "mls-owner").await;
    let outsider = participant(&pool, "mls-outsider").await;
    let (workspace, room_a) = workspace_room(&pool, owner, "mls-left").await;
    let room_b = RoomRepo::new(pool.clone())
        .create_in_workspace(
            workspace,
            RoomKind::Channel,
            Some("mls-right-room".into()),
            owner,
        )
        .await
        .unwrap()
        .id;
    let group_id = uuid::Uuid::new_v4().as_bytes().to_vec();
    let original = group(&group_id, room_a, 7, b"epoch-seven");
    repo.upsert_authorized(owner, &original).await.unwrap();

    let moved = group(&group_id, room_b, 8, b"moved");
    assert!(matches!(
        repo.upsert_authorized(owner, &moved).await,
        Err(Error::Conflict(_))
    ));

    let rollback = group(&group_id, room_a, 6, b"rollback");
    assert!(matches!(
        repo.upsert_authorized(owner, &rollback).await,
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        repo.get_authorized(outsider, &original.group_id).await,
        Err(Error::Forbidden(_))
    ));
    assert!(matches!(
        repo.upsert_authorized(outsider, &original).await,
        Err(Error::Forbidden(_))
    ));

    let retained = repo
        .get_authorized(owner, &original.group_id)
        .await
        .unwrap();
    assert_eq!(retained.room_id, Some(room_a));
    assert_eq!(retained.epoch, 7);
    assert_eq!(retained.state, b"epoch-seven");
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn concurrent_first_create_cannot_bind_one_group_to_two_rooms() {
    let pool = pool();
    let repo = MlsGroupRepo::new(pool.clone());
    let owner = participant(&pool, "mls-create-race-owner").await;
    let (_, room_a) = workspace_room(&pool, owner, "mls-create-race-a").await;
    let (_, room_b) = workspace_room(&pool, owner, "mls-create-race-b").await;
    let group_id = uuid::Uuid::new_v4().as_bytes().to_vec();
    let left = group(&group_id, room_a, 1, b"left");
    let right = group(&group_id, room_b, 1, b"right");

    let (left_result, right_result) = tokio::join!(
        repo.upsert_authorized(owner, &left),
        repo.upsert_authorized(owner, &right)
    );
    let outcomes = [left_result, right_result];
    assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| matches!(result, Err(Error::Conflict(_))))
            .count(),
        1
    );
    let stored = sqlx::query_as::<_, (uuid::Uuid, i64, Vec<u8>)>(
        "SELECT room_id, epoch, state
           FROM mls_groups
          WHERE group_id = $1",
    )
    .bind(&group_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(stored.0 == room_a.to_uuid() || stored.0 == room_b.to_uuid());
    assert_eq!(stored.1, 1);
    assert!(stored.2 == b"left" || stored.2 == b"right");
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn group_write_waits_for_workspace_revocation_and_writes_nothing() {
    let pool = pool();
    let repo = MlsGroupRepo::new(pool.clone());
    let owner = participant(&pool, "mls-revoke-admin").await;
    let actor = participant(&pool, "mls-revoke-actor").await;
    let (workspace, room) = workspace_room(&pool, owner, "mls-revoke").await;
    add_room_member(&pool, workspace, room, actor).await;
    let group_id = uuid::Uuid::new_v4().as_bytes().to_vec();
    let first = group(&group_id, room, 1, b"first");
    repo.upsert_authorized(actor, &first).await.unwrap();

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
    let second = group(&group_id, room, 2, b"second");
    let mut raced = tokio::spawn(async move { raced_repo.upsert_authorized(actor, &second).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut raced)
            .await
            .is_err(),
        "MLS write waits behind the workspace revocation fence"
    );
    revocation.commit().await.unwrap();
    assert!(matches!(raced.await.unwrap(), Err(Error::Forbidden(_))));

    let retained = repo.get_authorized(owner, &first.group_id).await.unwrap();
    assert_eq!(retained.epoch, 1);
    assert_eq!(retained.state, b"first");
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn raw_sql_cannot_rewrite_group_identity_epoch_or_actor_scope() {
    let pool = pool();
    let repo = MlsGroupRepo::new(pool.clone());
    let owner = participant(&pool, "mls-raw-owner").await;
    let outsider = participant(&pool, "mls-raw-outsider").await;
    let (workspace, room_a) = workspace_room(&pool, owner, "mls-raw").await;
    let room_b = RoomRepo::new(pool.clone())
        .create_in_workspace(
            workspace,
            RoomKind::Channel,
            Some("mls-raw-room-b".into()),
            owner,
        )
        .await
        .unwrap()
        .id;
    let group_id = uuid::Uuid::new_v4().as_bytes().to_vec();
    let original = group(&group_id, room_a, 4, b"opaque");
    repo.upsert_authorized(owner, &original).await.unwrap();

    for query in [
        "UPDATE mls_groups SET room_id = $2 WHERE group_id = $1",
        "UPDATE mls_groups SET ciphersuite = 'forged' WHERE group_id = $1",
    ] {
        let mut statement = sqlx::query(query).bind(original.group_id.as_bytes());
        if query.contains("$2") {
            statement = statement.bind(room_b.to_uuid());
        }
        let error = statement.execute(&pool).await.unwrap_err();
        assert_eq!(
            constraint(&error),
            Some("mls_groups_identity_immutable_chk")
        );
    }

    let rollback = sqlx::query("UPDATE mls_groups SET epoch = 3 WHERE group_id = $1")
        .bind(original.group_id.as_bytes())
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(
        constraint(&rollback),
        Some("mls_groups_epoch_nondecreasing_chk")
    );

    let forged_actor = sqlx::query(
        "UPDATE mls_groups
            SET state = $2, updated_by = $3
          WHERE group_id = $1",
    )
    .bind(original.group_id.as_bytes())
    .bind(b"forged".as_slice())
    .bind(outsider.to_uuid())
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&forged_actor),
        Some("mls_groups_updated_by_room_scope_chk")
    );

    let legacy_group_id = uuid::Uuid::new_v4().as_bytes().to_vec();
    sqlx::query(
        "INSERT INTO mls_groups
             (group_id, room_id, ciphersuite, epoch, state)
         VALUES ($1, $2, 'MLS_LEGACY', 0, $3)",
    )
    .bind(&legacy_group_id)
    .bind(room_a.to_uuid())
    .bind(b"legacy-opaque".as_slice())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE mls_groups
            SET epoch = 1, state = $2, updated_at = CURRENT_TIMESTAMP
          WHERE group_id = $1",
    )
    .bind(&legacy_group_id)
    .bind(b"legacy-advanced".as_slice())
    .execute(&pool)
    .await
    .unwrap();
    let legacy = sqlx::query_as::<_, (i64, Vec<u8>, Option<uuid::Uuid>)>(
        "SELECT epoch, state, updated_by
           FROM mls_groups
          WHERE group_id = $1",
    )
    .bind(&legacy_group_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(legacy.0, 1);
    assert_eq!(legacy.1, b"legacy-advanced");
    assert_eq!(legacy.2, None, "old-pod writes retain the nullable seam");
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn key_package_claim_requires_both_room_members_and_consumes_once() {
    let pool = pool();
    let repo = KeyPackageRepo::new(pool.clone());
    let requester = participant(&pool, "mls-kp-requester").await;
    let target = participant(&pool, "mls-kp-target").await;
    let outsider = participant(&pool, "mls-kp-outsider").await;
    let (workspace, room) = workspace_room(&pool, requester, "mls-kp").await;
    add_room_member(&pool, workspace, room, target).await;
    repo.publish(target, "MLS_TEST", b"one-package".to_vec())
        .await
        .unwrap();

    assert!(matches!(
        repo.consume_one_authorized(outsider, target, room).await,
        Err(Error::Forbidden(_))
    ));
    assert!(matches!(
        repo.consume_own(outsider, target).await,
        Err(Error::Forbidden(_))
    ));
    assert_eq!(repo.pending_count(target).await.unwrap(), 1);

    let (first, second) = tokio::join!(
        repo.consume_one_authorized(requester, target, room),
        repo.consume_one_authorized(requester, target, room)
    );
    let claims = [first.unwrap(), second.unwrap()];
    assert_eq!(claims.iter().filter(|claim| claim.is_some()).count(), 1);
    assert_eq!(claims.iter().filter(|claim| claim.is_none()).count(), 1);
    assert_eq!(repo.pending_count(target).await.unwrap(), 0);
}
