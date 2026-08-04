use super::{NewHuman, ParticipantDeleteError, ParticipantRepo};

use crate::{RoomMemberRole, RoomRepo, SessionRepo, WorkspaceRepo};
use aero_common::{ParticipantId, RoomKind, WorkspaceRole};
use sqlx::PgPool;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_lazy(&url)
        .expect("valid DATABASE_URL")
}

async fn human(pool: &PgPool, label: &str) -> ParticipantId {
    let nonce = uuid::Uuid::new_v4();
    ParticipantRepo::new(pool.clone())
        .create_human(NewHuman {
            email: format!("{label}-{nonce}@ownership.invalid"),
            display_name: format!("participant-ownership-{label}-{nonce}"),
            password_hash: "participant-ownership-test-hash".into(),
        })
        .await
        .unwrap()
        .id
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn account_erasure_preserves_workspace_and_channel_governance_atomically() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let workspaces = WorkspaceRepo::new(pool.clone());
    let rooms = RoomRepo::new(pool.clone());
    let sessions = SessionRepo::new(pool.clone());
    let workspace_owner = human(&pool, "workspace-owner").await;
    let channel_owner = human(&pool, "channel-owner").await;
    let successor = human(&pool, "successor").await;
    let workspace = workspaces
        .create(
            format!("participant-delete-{workspace_owner}"),
            format!("participant-delete-{workspace_owner}"),
            workspace_owner,
        )
        .await
        .unwrap()
        .id;
    for participant in [channel_owner, successor] {
        workspaces
            .add_member(workspace, participant, WorkspaceRole::Member)
            .await
            .unwrap();
    }
    let room = rooms
        .create_in_workspace(
            workspace,
            RoomKind::Channel,
            Some(format!("participant-delete-{channel_owner}")),
            channel_owner,
        )
        .await
        .unwrap()
        .id;
    rooms.add_member(room, successor).await.unwrap();
    sessions
        .record(
            channel_owner,
            &format!("participant-delete-session-{channel_owner}"),
            Some("ownership-test"),
        )
        .await
        .unwrap();

    assert!(matches!(
        participants
            .delete_participant(workspace_owner)
            .await
            .expect_err("workspace owners must transfer before erasure"),
        ParticipantDeleteError::WorkspaceOwnerProtected
    ));
    assert!(
        participants.get(workspace_owner).await.unwrap().is_some(),
        "a rejected erasure must leave the workspace owner active"
    );

    assert!(matches!(
        participants
            .delete_participant(channel_owner)
            .await
            .expect_err("a sole channel owner cannot be erased"),
        ParticipantDeleteError::ChannelOwnerProtected(protected) if protected == room
    ));
    assert!(
        participants.get(channel_owner).await.unwrap().is_some(),
        "the participant tombstone must roll back on governance conflict"
    );
    assert!(
        participants
            .find_credentials_by_participant_id(channel_owner)
            .await
            .unwrap()
            .is_some(),
        "PII deletion must not partially commit on governance conflict"
    );
    let active_sessions: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM auth_sessions
          WHERE participant_id = $1 AND revoked_at IS NULL",
    )
    .bind(channel_owner.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        active_sessions, 1,
        "session revocation must remain atomic with successful erasure"
    );

    rooms
        .change_channel_member_role_authorized(
            room,
            channel_owner,
            successor,
            RoomMemberRole::Owner,
        )
        .await
        .unwrap();
    sqlx::query(
        r"INSERT INTO workspace_deactivations
              (workspace_id, participant_id, deactivated_by)
           VALUES ($1, $2, $3)",
    )
    .bind(workspace.to_uuid())
    .bind(successor.to_uuid())
    .bind(workspace_owner.to_uuid())
    .execute(&pool)
    .await
    .unwrap();
    assert!(matches!(
        participants
            .delete_participant(channel_owner)
            .await
            .expect_err("an inactive owner cannot be the erasure successor"),
        ParticipantDeleteError::ChannelOwnerProtected(protected) if protected == room
    ));

    sqlx::query(
        "DELETE FROM workspace_deactivations
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(workspace.to_uuid())
    .bind(successor.to_uuid())
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        participants
            .delete_participant(channel_owner)
            .await
            .unwrap(),
        "erasure succeeds after an effective successor exists"
    );
    let (deleted_at, display_name): (Option<time::OffsetDateTime>, String) =
        sqlx::query_as("SELECT deleted_at, display_name FROM participants WHERE id = $1")
            .bind(channel_owner.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(deleted_at.is_some());
    assert_eq!(display_name, "[deleted]");
    let active_sessions: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM auth_sessions
          WHERE participant_id = $1 AND revoked_at IS NULL",
    )
    .bind(channel_owner.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active_sessions, 0);
}
