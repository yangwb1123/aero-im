use super::{ScimRepo, ScimUserWriteError};
use crate::{RoomMemberRole, RoomRepo, WorkspaceRepo};
use aero_common::{ParticipantId, RoomKind};
use sqlx::PgPool;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_lazy(&url)
        .expect("valid DATABASE_URL")
}

async fn participant(pool: &PgPool, label: &str) -> ParticipantId {
    let participant = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(participant.to_uuid())
        .bind(format!("scim-{label}-{participant}"))
        .execute(pool)
        .await
        .unwrap();
    participant
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn scim_cannot_deprovision_a_sole_effective_channel_owner() {
    let pool = pool();
    let scim = ScimRepo::new(pool.clone());
    let workspaces = WorkspaceRepo::new(pool.clone());
    let rooms = RoomRepo::new(pool.clone());
    let workspace_owner = participant(&pool, "workspace-owner").await;
    let workspace = workspaces
        .create(
            "SCIM owner guard".into(),
            format!("scim-owner-guard-{workspace_owner}"),
            workspace_owner,
        )
        .await
        .unwrap()
        .id;
    let owner = scim
        .provision_user(
            workspace,
            "SCIM channel owner",
            &format!("channel-owner-{}@example.com", uuid::Uuid::new_v4()),
            None,
            true,
        )
        .await
        .unwrap();
    let successor = scim
        .provision_user(
            workspace,
            "SCIM successor",
            &format!("channel-successor-{}@example.com", uuid::Uuid::new_v4()),
            None,
            true,
        )
        .await
        .unwrap();
    let room = rooms
        .create_in_workspace(
            workspace,
            RoomKind::Channel,
            Some("SCIM owner guard".into()),
            owner.participant_id,
        )
        .await
        .unwrap()
        .id;
    rooms
        .add_member(room, successor.participant_id)
        .await
        .unwrap();

    assert!(matches!(
        scim.set_active(workspace, owner.participant_id, false)
            .await
            .expect_err("active=false cannot orphan a channel"),
        ScimUserWriteError::ChannelOwnerDeprovision
    ));
    assert!(matches!(
        scim.delete_user(workspace, owner.participant_id)
            .await
            .expect_err("SCIM DELETE cannot orphan a channel"),
        ScimUserWriteError::ChannelOwnerDeprovision
    ));
    assert!(
        scim.get_user(workspace, owner.participant_id)
            .await
            .unwrap()
            .expect("failed deprovision rolls mapping back")
            .active
    );

    rooms
        .change_channel_member_role_authorized(
            room,
            owner.participant_id,
            successor.participant_id,
            RoomMemberRole::Owner,
        )
        .await
        .unwrap();
    assert!(scim
        .delete_user(workspace, owner.participant_id)
        .await
        .unwrap());
}
