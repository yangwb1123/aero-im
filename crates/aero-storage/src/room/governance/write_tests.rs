use std::time::Duration;

use super::*;
use crate::{DmRepo, WorkspaceRepo};
use sqlx::PgPool;

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
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(participant.to_uuid())
        .bind(format!("room-write-{label}-{participant}"))
        .execute(pool)
        .await
        .unwrap();
    participant
}

async fn workspace(pool: &PgPool, label: &str) -> (WorkspaceId, ParticipantId) {
    let owner = participant(pool, &format!("{label}-owner")).await;
    let workspace = WorkspaceRepo::new(pool.clone())
        .create(
            format!("Room write {label}"),
            format!("room-write-{label}-{owner}"),
            owner,
        )
        .await
        .unwrap()
        .id;
    (workspace, owner)
}

#[tokio::test]
#[ignore = "requires live PostgreSQL with migrations"]
async fn create_rechecks_membership_after_waiting_for_revocation() {
    let pool = pool();
    let (workspace, owner) = workspace(&pool, "create-revoke").await;
    let creator = participant(&pool, "creator").await;
    WorkspaceRepo::new(pool.clone())
        .add_member(workspace, creator, WorkspaceRole::Member)
        .await
        .unwrap();

    let mut revocation = pool.begin().await.unwrap();
    crate::ownership::lock_membership_governance(&mut revocation)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace.to_uuid())
        .execute(&mut *revocation)
        .await
        .unwrap();
    sqlx::query("DELETE FROM workspace_members WHERE workspace_id = $1 AND participant_id = $2")
        .bind(workspace.to_uuid())
        .bind(creator.to_uuid())
        .execute(&mut *revocation)
        .await
        .unwrap();

    let repo = RoomRepo::new(pool.clone());
    let mut raced = tokio::spawn(async move {
        repo.create_in_workspace_authorized(
            workspace,
            RoomKind::Channel,
            Some("must-not-exist".into()),
            creator,
        )
        .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut raced)
            .await
            .is_err(),
        "create must wait for the workspace revocation boundary"
    );
    revocation.commit().await.unwrap();
    assert!(matches!(
        raced.await.unwrap(),
        Err(RoomMembershipWriteError::NotAuthorized)
    ));
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM rooms WHERE workspace_id = $1 AND name = 'must-not-exist'",
    )
    .bind(workspace.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 0);

    // Keep the fixture's owner live so the workspace-owner invariant remains
    // explicit even if this test is run inside a surrounding transaction.
    assert!(WorkspaceRepo::new(pool)
        .is_member(workspace, owner)
        .await
        .unwrap());
}

#[tokio::test]
#[ignore = "requires live PostgreSQL with migrations"]
async fn add_member_requires_current_manager_and_rejects_fixed_or_guest_targets() {
    let pool = pool();
    let (workspace, owner) = workspace(&pool, "member-policy").await;
    let ordinary = participant(&pool, "ordinary").await;
    let target = participant(&pool, "target").await;
    let guest = participant(&pool, "guest").await;
    let peer = participant(&pool, "peer").await;
    let workspaces = WorkspaceRepo::new(pool.clone());
    for member in [ordinary, target, peer] {
        workspaces
            .add_member(workspace, member, WorkspaceRole::Member)
            .await
            .unwrap();
    }
    let rooms = RoomRepo::new(pool.clone());
    let room = rooms
        .create_in_workspace_authorized(workspace, RoomKind::Channel, Some("managed".into()), owner)
        .await
        .unwrap()
        .id;
    rooms.add_member(room, ordinary).await.unwrap();

    assert!(matches!(
        rooms
            .add_member_authorized(room, ordinary, target)
            .await
            .expect_err("ordinary members cannot pull arbitrary users into a room"),
        RoomMembershipWriteError::NotAuthorized
    ));
    assert!(rooms
        .add_member_authorized(room, owner, target)
        .await
        .unwrap());
    assert!(!rooms
        .add_member_authorized(room, owner, target)
        .await
        .unwrap());

    workspaces
        .add_guest_authorized(workspace, owner, guest, room)
        .await
        .unwrap();
    let other = rooms
        .create_in_workspace_authorized(workspace, RoomKind::Channel, Some("other".into()), owner)
        .await
        .unwrap()
        .id;
    assert!(matches!(
        rooms
            .add_member_authorized(other, owner, guest)
            .await
            .expect_err("single-channel guests cannot be spread to another room"),
        RoomMembershipWriteError::TargetNotEligible
    ));

    let direct = DmRepo::new(pool.clone())
        .find_or_create_in_workspace(workspace, owner, peer)
        .await
        .unwrap();
    assert!(matches!(
        rooms
            .add_member_authorized(direct.id, owner, target)
            .await
            .expect_err("direct membership is fixed"),
        RoomMembershipWriteError::FixedMembership
    ));
}

#[tokio::test]
#[ignore = "requires live PostgreSQL with migrations"]
async fn join_observes_channel_privacy_committed_while_waiting() {
    let pool = pool();
    let (workspace, owner) = workspace(&pool, "join-race").await;
    let joiner = participant(&pool, "joiner").await;
    WorkspaceRepo::new(pool.clone())
        .add_member(workspace, joiner, WorkspaceRole::Member)
        .await
        .unwrap();
    let rooms = RoomRepo::new(pool.clone());
    let room = rooms
        .create_in_workspace_authorized(
            workspace,
            RoomKind::Channel,
            Some("public-until-locked".into()),
            owner,
        )
        .await
        .unwrap()
        .id;

    let mut privacy = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace.to_uuid())
        .execute(&mut *privacy)
        .await
        .unwrap();
    sqlx::query("UPDATE rooms SET is_private = true WHERE id = $1")
        .bind(room.to_uuid())
        .execute(&mut *privacy)
        .await
        .unwrap();

    let raced_repo = rooms.clone();
    let mut raced = tokio::spawn(async move {
        raced_repo
            .join_public_channel_authorized(room, joiner)
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut raced)
            .await
            .is_err(),
        "join must wait behind the workspace/channel policy change"
    );
    privacy.commit().await.unwrap();
    assert!(matches!(
        raced.await.unwrap(),
        Err(RoomMembershipWriteError::NotJoinable)
    ));
    assert!(!rooms.is_member(room, joiner).await.unwrap());
}

#[tokio::test]
#[ignore = "requires live PostgreSQL with migrations"]
async fn add_member_observes_workspace_admin_demotion_after_waiting() {
    let pool = pool();
    let (workspace, owner) = workspace(&pool, "add-demotion").await;
    let admin = participant(&pool, "admin").await;
    let target = participant(&pool, "target").await;
    let workspaces = WorkspaceRepo::new(pool.clone());
    workspaces
        .add_member(workspace, admin, WorkspaceRole::Admin)
        .await
        .unwrap();
    workspaces
        .add_member(workspace, target, WorkspaceRole::Member)
        .await
        .unwrap();
    let rooms = RoomRepo::new(pool.clone());
    let room = rooms
        .create_in_workspace_authorized(
            workspace,
            RoomKind::Channel,
            Some("admin-demotion".into()),
            owner,
        )
        .await
        .unwrap()
        .id;
    rooms.add_member(room, admin).await.unwrap();

    let mut demotion = pool.begin().await.unwrap();
    crate::ownership::lock_membership_governance(&mut demotion)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace.to_uuid())
        .execute(&mut *demotion)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE workspace_members SET role = 'member'
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(workspace.to_uuid())
    .bind(admin.to_uuid())
    .execute(&mut *demotion)
    .await
    .unwrap();

    let raced_repo = rooms.clone();
    let mut raced =
        tokio::spawn(async move { raced_repo.add_member_authorized(room, admin, target).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut raced)
            .await
            .is_err(),
        "membership write must wait behind current authority changes"
    );
    demotion.commit().await.unwrap();
    assert!(matches!(
        raced.await.unwrap(),
        Err(RoomMembershipWriteError::NotAuthorized)
    ));
    assert!(!rooms.is_member(room, target).await.unwrap());
    assert!(matches!(
        rooms
            .set_channel_retention_authorized(room, admin, Some(30))
            .await
            .expect_err("a demoted workspace admin cannot change channel retention"),
        RoomMembershipWriteError::NotAuthorized
    ));
    assert_eq!(rooms.retention_days(room).await.unwrap(), None);
}
