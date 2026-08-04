use std::time::Duration;

use aero_common::{Block, Error, ParticipantId, RoomId, RoomKind, WorkspaceId, WorkspaceRole};
use sqlx::PgPool;

use super::{MessageRepo, NewMessage};
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
    let id = ParticipantId::new();
    sqlx::query(
        "INSERT INTO participants (id, kind, display_name)
         VALUES ($1, 'human', $2)",
    )
    .bind(id.to_uuid())
    .bind(format!("{label}-{id}"))
    .execute(pool)
    .await
    .unwrap();
    id
}

struct Fixture {
    pool: PgPool,
    workspace: WorkspaceId,
    room: RoomId,
    owner: ParticipantId,
    actor: ParticipantId,
}

impl Fixture {
    async fn create(actor_role: WorkspaceRole) -> Self {
        let pool = pool();
        let owner = participant(&pool, "message-auth-owner").await;
        let actor = participant(&pool, "message-auth-actor").await;
        let workspaces = WorkspaceRepo::new(pool.clone());
        let workspace = workspaces
            .create(
                format!("Message auth {owner}"),
                format!("message-auth-{owner}"),
                owner,
            )
            .await
            .unwrap()
            .id;
        workspaces
            .add_member(workspace, actor, actor_role)
            .await
            .unwrap();
        let rooms = RoomRepo::new(pool.clone());
        let room = rooms
            .create_in_workspace(
                workspace,
                RoomKind::Channel,
                Some(format!("message-auth-{actor}")),
                owner,
            )
            .await
            .unwrap()
            .id;
        rooms.add_member(room, actor).await.unwrap();
        Self {
            pool,
            workspace,
            room,
            owner,
            actor,
        }
    }

    async fn insert_message(&self, body: &str) -> aero_common::Message {
        MessageRepo::new(self.pool.clone())
            .insert_outboxed(
                NewMessage {
                    room_id: self.room,
                    sender_id: self.actor,
                    blocks: vec![Block::text(body)],
                    reply_to: None,
                    metadata: serde_json::Value::Null,
                    expires_at: None,
                },
                None,
                vec![],
                None,
            )
            .await
            .unwrap()
            .message()
            .clone()
    }

    async fn cleanup(&self) {
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(self.workspace.to_uuid())
            .execute(&self.pool)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
            .bind(vec![self.owner.to_uuid(), self.actor.to_uuid()])
            .execute(&self.pool)
            .await
            .ok();
    }
}

async fn lock_workspace(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, workspace: WorkspaceId) {
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace.to_uuid())
        .fetch_one(&mut **tx)
        .await
        .unwrap();
    sqlx::query("SET LOCAL lock_timeout = '500ms'")
        .execute(&mut **tx)
        .await
        .unwrap();
}

async fn assert_waiting<T>(task: &tokio::task::JoinHandle<T>) {
    tokio::time::sleep(Duration::from_millis(75)).await;
    assert!(
        !task.is_finished(),
        "message mutation must wait behind the workspace governance fence"
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn send_rechecks_policy_after_waiting_without_locking_room_first() {
    let fixture = Fixture::create(WorkspaceRole::Member).await;
    let mut governance = fixture.pool.begin().await.unwrap();
    crate::ownership::lock_membership_governance(&mut governance)
        .await
        .unwrap();
    lock_workspace(&mut governance, fixture.workspace).await;

    let messages = MessageRepo::new(fixture.pool.clone());
    let room = fixture.room;
    let actor = fixture.actor;
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let send = tokio::spawn(async move {
        let _ = started_tx.send(());
        messages
            .insert_outboxed(
                NewMessage {
                    room_id: room,
                    sender_id: actor,
                    blocks: vec![Block::text("must not commit")],
                    reply_to: None,
                    metadata: serde_json::Value::Null,
                    expires_at: None,
                },
                None,
                vec![],
                None,
            )
            .await
    });
    started_rx.await.unwrap();
    assert_waiting(&send).await;

    // This UPDATE must not wait on a room lock held by the blocked send.
    sqlx::query("UPDATE rooms SET post_policy = 'admins' WHERE id = $1")
        .bind(fixture.room.to_uuid())
        .execute(&mut *governance)
        .await
        .unwrap();
    governance.commit().await.unwrap();

    let error = tokio::time::timeout(Duration::from_secs(3), send)
        .await
        .expect("send unblocked")
        .unwrap()
        .expect_err("member lost posting authority");
    assert!(matches!(error, Error::Forbidden(_)));
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE room_id = $1 AND sender_id = $2")
            .bind(fixture.room.to_uuid())
            .bind(fixture.actor.to_uuid())
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn send_rechecks_admin_role_after_waiting_for_demotion() {
    let fixture = Fixture::create(WorkspaceRole::Admin).await;
    RoomRepo::new(fixture.pool.clone())
        .set_post_policy(fixture.room, "admins")
        .await
        .unwrap();
    let mut governance = fixture.pool.begin().await.unwrap();
    crate::ownership::lock_membership_governance(&mut governance)
        .await
        .unwrap();
    lock_workspace(&mut governance, fixture.workspace).await;

    let messages = MessageRepo::new(fixture.pool.clone());
    let room = fixture.room;
    let actor = fixture.actor;
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let send = tokio::spawn(async move {
        let _ = started_tx.send(());
        messages
            .insert_outboxed(
                NewMessage {
                    room_id: room,
                    sender_id: actor,
                    blocks: vec![Block::text("demoted admin")],
                    reply_to: None,
                    metadata: serde_json::Value::Null,
                    expires_at: None,
                },
                None,
                vec![],
                None,
            )
            .await
    });
    started_rx.await.unwrap();
    assert_waiting(&send).await;

    // A blocked send has not inverted workspace → room lock order.
    sqlx::query("SELECT id FROM rooms WHERE id = $1 FOR UPDATE")
        .bind(fixture.room.to_uuid())
        .fetch_one(&mut *governance)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE workspace_members
            SET role = 'member'
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(fixture.workspace.to_uuid())
    .bind(fixture.actor.to_uuid())
    .execute(&mut *governance)
    .await
    .unwrap();
    governance.commit().await.unwrap();

    let error = tokio::time::timeout(Duration::from_secs(3), send)
        .await
        .expect("send unblocked")
        .unwrap()
        .expect_err("demoted admin lost posting authority");
    assert!(matches!(error, Error::Forbidden(_)));
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn edit_rechecks_membership_before_locking_message() {
    let fixture = Fixture::create(WorkspaceRole::Member).await;
    let message = fixture.insert_message("before membership removal").await;
    let mut governance = fixture.pool.begin().await.unwrap();
    lock_workspace(&mut governance, fixture.workspace).await;

    let messages = MessageRepo::new(fixture.pool.clone());
    let actor = fixture.actor;
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let edit = tokio::spawn(async move {
        let _ = started_tx.send(());
        messages
            .edit_outboxed_authorized(
                message.id,
                actor,
                vec![Block::text("must not replace")],
                message.version,
                true,
                None,
            )
            .await
    });
    started_rx.await.unwrap();
    assert_waiting(&edit).await;

    sqlx::query("SELECT id FROM rooms WHERE id = $1 FOR UPDATE")
        .bind(fixture.room.to_uuid())
        .fetch_one(&mut *governance)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM messages WHERE id = $1 FOR UPDATE")
        .bind(message.id.to_uuid())
        .fetch_one(&mut *governance)
        .await
        .unwrap();
    sqlx::query("DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
        .bind(fixture.room.to_uuid())
        .bind(fixture.actor.to_uuid())
        .execute(&mut *governance)
        .await
        .unwrap();
    governance.commit().await.unwrap();

    let error = tokio::time::timeout(Duration::from_secs(3), edit)
        .await
        .expect("edit unblocked")
        .unwrap()
        .expect_err("removed member lost edit authority");
    assert!(matches!(error, Error::Forbidden(_)));
    let current = MessageRepo::new(fixture.pool.clone())
        .get(message.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.version, message.version);
    assert_eq!(
        serde_json::to_value(current.blocks).unwrap(),
        serde_json::to_value(message.blocks).unwrap()
    );
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn delete_rechecks_deactivation_before_locking_message() {
    let fixture = Fixture::create(WorkspaceRole::Member).await;
    let message = fixture.insert_message("before deactivation").await;
    let mut governance = fixture.pool.begin().await.unwrap();
    lock_workspace(&mut governance, fixture.workspace).await;

    let messages = MessageRepo::new(fixture.pool.clone());
    let actor = fixture.actor;
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let delete = tokio::spawn(async move {
        let _ = started_tx.send(());
        messages
            .soft_delete_outboxed_authorized(message.id, actor, None)
            .await
    });
    started_rx.await.unwrap();
    assert_waiting(&delete).await;

    sqlx::query("SELECT id FROM rooms WHERE id = $1 FOR UPDATE")
        .bind(fixture.room.to_uuid())
        .fetch_one(&mut *governance)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM messages WHERE id = $1 FOR UPDATE")
        .bind(message.id.to_uuid())
        .fetch_one(&mut *governance)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspace_deactivations
             (workspace_id, participant_id, deactivated_by)
         VALUES ($1, $2, $3)",
    )
    .bind(fixture.workspace.to_uuid())
    .bind(fixture.actor.to_uuid())
    .bind(fixture.owner.to_uuid())
    .execute(&mut *governance)
    .await
    .unwrap();
    governance.commit().await.unwrap();

    let error = tokio::time::timeout(Duration::from_secs(3), delete)
        .await
        .expect("delete unblocked")
        .unwrap()
        .expect_err("deactivated sender lost delete authority");
    assert!(matches!(error, Error::Forbidden(_)));
    let current = MessageRepo::new(fixture.pool.clone())
        .get(message.id)
        .await
        .unwrap()
        .unwrap();
    assert!(current.deleted_at.is_none());
    fixture.cleanup().await;
}
