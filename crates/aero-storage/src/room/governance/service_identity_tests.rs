use aero_common::{ParticipantId, ParticipantKind, RoomKind, WorkspaceId, WorkspaceRole};
use sqlx::PgPool;
use uuid::Uuid;

use super::*;
use crate::{DmRepo, WorkspaceRepo};

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_lazy(&url)
        .expect("valid DATABASE_URL")
}

async fn human(pool: &PgPool, label: &str) -> ParticipantId {
    let participant = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(participant.to_uuid())
        .bind(format!("{label}-{participant}"))
        .execute(pool)
        .await
        .unwrap();
    participant
}

async fn workspace_room(
    pool: &PgPool,
    owner: ParticipantId,
    label: &str,
) -> (WorkspaceId, aero_common::RoomId) {
    let workspace = WorkspaceRepo::new(pool.clone())
        .create(
            format!("Agent install {label}"),
            format!("agent-install-{label}-{}", Uuid::new_v4()),
            owner,
        )
        .await
        .unwrap()
        .id;
    let room = RoomRepo::new(pool.clone())
        .create_in_workspace_authorized(
            workspace,
            RoomKind::Group,
            Some(format!("Agent install {label}")),
            owner,
        )
        .await
        .unwrap()
        .id;
    (workspace, room)
}

async fn count_named(pool: &PgPool, creator: ParticipantId, name: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM participants WHERE created_by = $1 AND display_name = $2",
    )
    .bind(creator.to_uuid())
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn room_service_identity_install_is_atomic_and_tenant_scoped() {
    let pool = pool();
    let owner = human(&pool, "agent-install-owner").await;
    let (workspace, room) = workspace_room(&pool, owner, "success").await;
    let rooms = RoomRepo::new(pool.clone());

    let agent = rooms
        .create_service_identity_authorized(
            room,
            owner,
            "Room Agent",
            ParticipantKind::Agent,
            Some("https://example.test/agent.png"),
        )
        .await
        .unwrap();
    assert_eq!(agent.kind, ParticipantKind::Agent);
    assert_eq!(
        agent.avatar_url.as_deref(),
        Some("https://example.test/agent.png")
    );
    let edges = sqlx::query_as::<_, (String, bool, String)>(
        r"SELECT workspace_member.role, workspace_member.is_guest, room_member.role
            FROM workspace_members workspace_member
            JOIN room_members room_member
              ON room_member.participant_id = workspace_member.participant_id
             AND room_member.room_id = $2
           WHERE workspace_member.workspace_id = $1
             AND workspace_member.participant_id = $3",
    )
    .bind(workspace.to_uuid())
    .bind(room.to_uuid())
    .bind(agent.id.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(edges, ("member".into(), false, "member".into()));
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT aero_effective_workspace_access($1, $2)")
            .bind(workspace.to_uuid())
            .bind(agent.id.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap()
    );

    let bot = rooms
        .create_service_identity_authorized(room, owner, "Room Bot", ParticipantKind::Bot, None)
        .await
        .unwrap();
    assert_eq!(bot.kind, ParticipantKind::Bot);

    let other_owner = human(&pool, "agent-install-other-owner").await;
    let (_, other_room) = workspace_room(&pool, other_owner, "other").await;
    assert!(matches!(
        rooms
            .add_member_authorized(other_room, other_owner, agent.id)
            .await,
        Err(RoomMembershipWriteError::TargetNotEligible)
    ));
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn room_service_identity_install_rejects_non_managers_and_fixed_rooms_without_writes() {
    let pool = pool();
    let owner = human(&pool, "agent-policy-owner").await;
    let (workspace, room) = workspace_room(&pool, owner, "policy").await;
    let ordinary = human(&pool, "agent-policy-ordinary").await;
    let peer = human(&pool, "agent-policy-peer").await;
    let workspaces = WorkspaceRepo::new(pool.clone());
    for participant in [ordinary, peer] {
        workspaces
            .add_member(workspace, participant, WorkspaceRole::Member)
            .await
            .unwrap();
    }
    let rooms = RoomRepo::new(pool.clone());
    rooms.add_member(room, ordinary).await.unwrap();

    let denied_name = format!("denied-agent-{ordinary}");
    assert!(matches!(
        rooms
            .create_service_identity_authorized(
                room,
                ordinary,
                &denied_name,
                ParticipantKind::Agent,
                None,
            )
            .await,
        Err(RoomMembershipWriteError::NotAuthorized)
    ));
    assert_eq!(count_named(&pool, ordinary, &denied_name).await, 0);

    let direct = DmRepo::new(pool.clone())
        .find_or_create_in_workspace(workspace, owner, peer)
        .await
        .unwrap();
    let fixed_name = format!("fixed-agent-{owner}");
    assert!(matches!(
        rooms
            .create_service_identity_authorized(
                direct.id,
                owner,
                &fixed_name,
                ParticipantKind::Bot,
                None,
            )
            .await,
        Err(RoomMembershipWriteError::FixedMembership)
    ));
    assert_eq!(count_named(&pool, owner, &fixed_name).await, 0);
    assert!(matches!(
        rooms
            .create_service_identity_authorized(
                room,
                owner,
                "human-is-not-an-agent",
                ParticipantKind::Human,
                None,
            )
            .await,
        Err(RoomMembershipWriteError::InvalidInput(_))
    ));
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn room_service_identity_install_rolls_back_every_edge_after_late_failure() {
    let pool = pool();
    let owner = human(&pool, "agent-rollback-owner").await;
    let (_, room) = workspace_room(&pool, owner, "rollback").await;
    let failure_name = format!("rollback-agent-{owner}");
    let function = format!("aero_fail_agent_install_{}", Uuid::new_v4().simple());
    let trigger = format!("fail_agent_install_{}", Uuid::new_v4().simple());
    sqlx::query(&format!(
        "CREATE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             IF EXISTS (
                 SELECT 1 FROM participants
                  WHERE id = NEW.participant_id
                    AND display_name = '{failure_name}'
             ) THEN
                 RAISE EXCEPTION 'injected room membership failure';
             END IF;
             RETURN NEW;
         END $$"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER {trigger}
         BEFORE INSERT ON room_members
         FOR EACH ROW EXECUTE FUNCTION {function}()"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let result = RoomRepo::new(pool.clone())
        .create_service_identity_authorized(
            room,
            owner,
            &failure_name,
            ParticipantKind::Agent,
            None,
        )
        .await;
    assert!(matches!(result, Err(RoomMembershipWriteError::Storage(_))));
    assert_eq!(count_named(&pool, owner, &failure_name).await, 0);

    sqlx::query(&format!("DROP TRIGGER {trigger} ON room_members"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("DROP FUNCTION {function}()"))
        .execute(&pool)
        .await
        .unwrap();
}
