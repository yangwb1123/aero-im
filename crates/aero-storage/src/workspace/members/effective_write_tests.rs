use super::*;

use crate::{RoomMemberRole, RoomRepo};
use aero_common::RoomKind;
use sqlx::PgPool;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_lazy(&url)
        .expect("valid DATABASE_URL")
}

async fn participant(pool: &PgPool) -> ParticipantId {
    let participant = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(participant.to_uuid())
        .bind(format!("effective-workspace-write-{participant}"))
        .execute(pool)
        .await
        .unwrap();
    participant
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn authorized_membership_writes_revalidate_effective_caller_in_transaction() {
    let pool = pool();
    let repo = WorkspaceRepo::new(pool.clone());
    let owner = participant(&pool).await;
    let admin = participant(&pool).await;
    let target = participant(&pool).await;
    let invitee = participant(&pool).await;
    let workspace = repo
        .create(
            format!("effective-write-{owner}"),
            format!("effective-write-{owner}"),
            owner,
        )
        .await
        .unwrap()
        .id;
    repo.add_member(workspace, admin, WorkspaceRole::Admin)
        .await
        .unwrap();
    repo.add_member(workspace, target, WorkspaceRole::Member)
        .await
        .unwrap();

    sqlx::query(
        r"INSERT INTO workspace_deactivations
              (workspace_id, participant_id, deactivated_by)
           VALUES ($1, $2, $3)",
    )
    .bind(workspace.to_uuid())
    .bind(admin.to_uuid())
    .bind(owner.to_uuid())
    .execute(&pool)
    .await
    .unwrap();
    assert!(matches!(
        repo.add_member_authorized(workspace, admin, invitee, WorkspaceRole::Member)
            .await
            .expect_err("a deactivated admin cannot invite"),
        WorkspaceMemberWriteError::NotAuthorized
    ));

    sqlx::query(
        "DELETE FROM workspace_deactivations
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(workspace.to_uuid())
    .bind(admin.to_uuid())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r"INSERT INTO totp_secrets
              (participant_id, secret, activated, activated_at)
           VALUES ($1, 'effective-write-owner', true, NOW())",
    )
    .bind(owner.to_uuid())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE workspaces SET require_2fa = true WHERE id = $1")
        .bind(workspace.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        repo.change_member_role_authorized(workspace, admin, target, WorkspaceRole::Admin,)
            .await
            .expect_err("an admin missing mandatory 2FA cannot change roles"),
        WorkspaceMemberWriteError::NotAuthorized
    ));

    sqlx::query(
        r"INSERT INTO totp_secrets
              (participant_id, secret, activated, activated_at)
           VALUES ($1, 'effective-write-test', true, NOW())",
    )
    .bind(admin.to_uuid())
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        repo.change_member_role_authorized(workspace, admin, target, WorkspaceRole::Admin,)
            .await
            .unwrap(),
        WorkspaceRole::Member
    );

    sqlx::query("UPDATE participants SET deleted_at = NOW() WHERE id = $1")
        .bind(admin.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        repo.remove_member_authorized(workspace, admin, target)
            .await
            .expect_err("a deleted admin cannot remove members"),
        WorkspaceMemberWriteError::NotAuthorized
    ));
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn effective_access_refreshes_after_waiting_for_deactivation_commit() {
    let pool = pool();
    let repo = WorkspaceRepo::new(pool.clone());
    let owner = participant(&pool).await;
    let member = participant(&pool).await;
    let workspace = repo
        .create(
            format!("effective-wait-{owner}"),
            format!("effective-wait-{owner}"),
            owner,
        )
        .await
        .unwrap()
        .id;
    repo.add_member(workspace, member, WorkspaceRole::Member)
        .await
        .unwrap();

    let mut deactivation = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace.to_uuid())
        .fetch_one(&mut *deactivation)
        .await
        .unwrap();
    sqlx::query(
        r"INSERT INTO workspace_deactivations
              (workspace_id, participant_id, deactivated_by)
           VALUES ($1, $2, $3)",
    )
    .bind(workspace.to_uuid())
    .bind(member.to_uuid())
    .bind(owner.to_uuid())
    .execute(&mut *deactivation)
    .await
    .unwrap();

    let mut connection = pool.acquire().await.unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let contender = tokio::spawn(async move {
        let _ = started_tx.send(());
        sqlx::query_scalar::<_, bool>("SELECT aero_effective_workspace_access($1, $2)")
            .bind(workspace.to_uuid())
            .bind(member.to_uuid())
            .fetch_one(&mut *connection)
            .await
    });
    started_rx.await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(75)).await;
    assert!(
        !contender.is_finished(),
        "effective-access query must wait behind the workspace governance lock"
    );

    deactivation.commit().await.unwrap();
    assert!(
        !contender.await.unwrap().unwrap(),
        "the post-wait READ COMMITTED snapshot must observe the committed deactivation"
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn removing_workspace_membership_cannot_orphan_an_owned_channel() {
    let pool = pool();
    let repo = WorkspaceRepo::new(pool.clone());
    let rooms = RoomRepo::new(pool.clone());
    let workspace_owner = participant(&pool).await;
    let channel_owner = participant(&pool).await;
    let successor = participant(&pool).await;
    let workspace = repo
        .create(
            format!("workspace-remove-channel-owner-{workspace_owner}"),
            format!("workspace-remove-channel-owner-{workspace_owner}"),
            workspace_owner,
        )
        .await
        .unwrap()
        .id;
    for member in [channel_owner, successor] {
        repo.add_member(workspace, member, WorkspaceRole::Member)
            .await
            .unwrap();
    }
    let room = rooms
        .create_in_workspace(
            workspace,
            RoomKind::Channel,
            Some(format!("workspace-remove-{channel_owner}")),
            channel_owner,
        )
        .await
        .unwrap()
        .id;
    rooms.add_member(room, successor).await.unwrap();

    assert!(matches!(
        repo.remove_member_authorized(workspace, workspace_owner, channel_owner)
            .await
            .expect_err("sole channel owner cannot lose workspace membership"),
        WorkspaceMemberWriteError::ChannelOwnerProtected
    ));
    assert_eq!(
        repo.member_role(workspace, channel_owner).await.unwrap(),
        Some(WorkspaceRole::Member)
    );
    assert!(rooms.is_member(room, channel_owner).await.unwrap());

    rooms
        .change_channel_member_role_authorized(
            room,
            channel_owner,
            successor,
            RoomMemberRole::Owner,
        )
        .await
        .unwrap();
    assert_eq!(
        repo.remove_member_authorized(workspace, workspace_owner, channel_owner)
            .await
            .unwrap(),
        WorkspaceRole::Member
    );
    assert_eq!(
        repo.member_role(workspace, channel_owner).await.unwrap(),
        None
    );
    assert!(!rooms.is_member(room, channel_owner).await.unwrap());
}
