use std::time::Duration;

use aero_common::{
    Block, Error, MessageId, ParticipantId, RoomId, RoomKind, ScheduledMessageId, WorkspaceId,
    WorkspaceRole,
};
use sqlx::PgPool;

use super::ScheduledRepo;
use crate::{MessageRepo, NewMessage, RoomRepo, WorkspaceRepo};

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
    owner: ParticipantId,
    actor: ParticipantId,
    peer: ParticipantId,
    outsider: ParticipantId,
    room_a: RoomId,
    room_b: RoomId,
}

impl Fixture {
    async fn create() -> Self {
        let pool = pool();
        let owner = participant(&pool, "scheduled-owner").await;
        let actor = participant(&pool, "scheduled-actor").await;
        let peer = participant(&pool, "scheduled-peer").await;
        let outsider = participant(&pool, "scheduled-outsider").await;
        let workspaces = WorkspaceRepo::new(pool.clone());
        let workspace = workspaces
            .create(
                format!("Scheduled auth {owner}"),
                format!("scheduled-auth-{owner}"),
                owner,
            )
            .await
            .unwrap()
            .id;
        for member in [actor, peer, outsider] {
            workspaces
                .add_member(workspace, member, WorkspaceRole::Member)
                .await
                .unwrap();
        }
        let rooms = RoomRepo::new(pool.clone());
        let room_a = rooms
            .create_in_workspace(
                workspace,
                RoomKind::Channel,
                Some(format!("scheduled-a-{actor}")),
                owner,
            )
            .await
            .unwrap()
            .id;
        let room_b = rooms
            .create_in_workspace(
                workspace,
                RoomKind::Channel,
                Some(format!("scheduled-b-{actor}")),
                owner,
            )
            .await
            .unwrap()
            .id;
        for room in [room_a, room_b] {
            rooms.add_member(room, actor).await.unwrap();
            rooms.add_member(room, peer).await.unwrap();
        }
        Self {
            pool,
            workspace,
            owner,
            actor,
            peer,
            outsider,
            room_a,
            room_b,
        }
    }

    async fn message(&self, room: RoomId, body: &str) -> MessageId {
        MessageRepo::new(self.pool.clone())
            .insert(NewMessage {
                room_id: room,
                sender_id: self.actor,
                blocks: vec![Block::text(body)],
                reply_to: None,
                metadata: serde_json::Value::Null,
                expires_at: None,
            })
            .await
            .unwrap()
            .id
    }

    async fn cleanup(&self) {
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(self.workspace.to_uuid())
            .execute(&self.pool)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
            .bind(vec![
                self.owner.to_uuid(),
                self.actor.to_uuid(),
                self.peer.to_uuid(),
                self.outsider.to_uuid(),
            ])
            .execute(&self.pool)
            .await
            .ok();
    }
}

fn constraint(error: &sqlx::Error) -> Option<&str> {
    match error {
        sqlx::Error::Database(database) => database.constraint(),
        _ => None,
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn scheduled_path_room_and_non_owner_ids_are_opaque() {
    let fixture = Fixture::create().await;
    let repo = ScheduledRepo::new(fixture.pool.clone());
    let id = repo
        .create_authorized(
            fixture.room_a,
            fixture.actor,
            &[Block::text("keep")],
            None,
            time::OffsetDateTime::now_utc() + time::Duration::hours(1),
        )
        .await
        .unwrap();

    assert!(matches!(
        repo.cancel_authorized(Some(fixture.room_b), id, fixture.actor)
            .await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        repo.cancel_authorized(Some(fixture.room_a), id, fixture.peer)
            .await,
        Err(Error::NotFound(_))
    ));
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT delivery_status FROM scheduled_messages WHERE id = $1",
        )
        .bind(id.to_uuid())
        .fetch_one(&fixture.pool)
        .await
        .unwrap(),
        "pending"
    );
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn scheduled_update_waits_for_revocation_then_writes_nothing_and_list_hides_room() {
    let fixture = Fixture::create().await;
    let repo = ScheduledRepo::new(fixture.pool.clone());
    let future = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let hidden = repo
        .create_authorized(
            fixture.room_a,
            fixture.actor,
            &[Block::text("before")],
            None,
            future,
        )
        .await
        .unwrap();
    let visible = repo
        .create_authorized(
            fixture.room_b,
            fixture.actor,
            &[Block::text("visible")],
            None,
            future,
        )
        .await
        .unwrap();

    let mut revoke = fixture.pool.begin().await.unwrap();
    sqlx::query("DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
        .bind(fixture.room_a.to_uuid())
        .bind(fixture.actor.to_uuid())
        .execute(&mut *revoke)
        .await
        .unwrap();
    let update_repo = repo.clone();
    let room = fixture.room_a;
    let actor = fixture.actor;
    let update = tokio::spawn(async move {
        update_repo
            .update_authorized(
                Some(room),
                hidden,
                actor,
                future + time::Duration::hours(1),
                &[Block::text("after")],
                None,
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(75)).await;
    assert!(
        !update.is_finished(),
        "scheduled update must wait behind membership revocation"
    );
    revoke.commit().await.unwrap();
    assert!(matches!(update.await.unwrap(), Err(Error::Forbidden(_))));

    let blocks: serde_json::Value =
        sqlx::query_scalar("SELECT blocks FROM scheduled_messages WHERE id = $1")
            .bind(hidden.to_uuid())
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert_eq!(
        blocks,
        serde_json::to_value(vec![Block::text("before")]).unwrap()
    );
    let global = repo
        .list_actionable_authorized(fixture.actor, None)
        .await
        .unwrap();
    assert!(global.iter().any(|row| row.id == visible));
    assert!(global.iter().all(|row| row.id != hidden));
    assert!(matches!(
        repo.list_actionable_authorized(fixture.actor, Some(fixture.room_a))
            .await,
        Err(Error::Forbidden(_))
    ));
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn scheduled_raw_sql_guard_rejects_scope_identity_live_targets_and_bad_state() {
    let fixture = Fixture::create().await;
    let repo = ScheduledRepo::new(fixture.pool.clone());
    let future = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let raw_id = ScheduledMessageId::new();
    let error = sqlx::query(
        r"INSERT INTO scheduled_messages
              (id, room_id, sender_id, blocks, scheduled_at, available_at)
           VALUES ($1, $2, $3, $4, $5, $5)",
    )
    .bind(raw_id.to_uuid())
    .bind(fixture.room_a.to_uuid())
    .bind(fixture.outsider.to_uuid())
    .bind(serde_json::json!([{"type":"text","content":"raw"}]))
    .bind(future)
    .execute(&fixture.pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&error),
        Some("scheduled_messages_sender_scope_chk")
    );

    let id = repo
        .create_authorized(
            fixture.room_a,
            fixture.actor,
            &[Block::text("guarded")],
            None,
            future,
        )
        .await
        .unwrap();
    let error = sqlx::query("UPDATE scheduled_messages SET room_id = $2 WHERE id = $1")
        .bind(id.to_uuid())
        .bind(fixture.room_b.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap_err();
    assert_eq!(
        constraint(&error),
        Some("scheduled_messages_identity_immutable_chk")
    );
    let error = sqlx::query(
        "UPDATE scheduled_messages
            SET delivery_status = 'delivered', delivered_at = now()
          WHERE id = $1",
    )
    .bind(id.to_uuid())
    .execute(&fixture.pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&error),
        Some("scheduled_messages_state_transition_chk")
    );

    let deleted_parent = fixture.message(fixture.room_a, "deleted").await;
    sqlx::query("UPDATE messages SET deleted_at = now() WHERE id = $1")
        .bind(deleted_parent.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();
    let error = sqlx::query(
        r"INSERT INTO scheduled_messages
              (id, room_id, sender_id, blocks, reply_to, scheduled_at, available_at)
           VALUES ($1, $2, $3, $4, $5, $6, $6)",
    )
    .bind(ScheduledMessageId::new().to_uuid())
    .bind(fixture.room_a.to_uuid())
    .bind(fixture.actor.to_uuid())
    .bind(serde_json::json!([{"type":"text","content":"reply"}]))
    .bind(deleted_parent.to_uuid())
    .bind(future)
    .execute(&fixture.pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&error),
        Some("scheduled_messages_live_reply_chk")
    );

    let other_parent = fixture.message(fixture.room_a, "other room").await;
    let error = sqlx::query(
        r"INSERT INTO scheduled_messages
              (id, room_id, sender_id, blocks, scheduled_at, available_at)
           VALUES ($1, $2, $3, $4, $5, $5)",
    )
    .bind(ScheduledMessageId::new().to_uuid())
    .bind(fixture.room_b.to_uuid())
    .bind(fixture.actor.to_uuid())
    .bind(serde_json::json!([{
        "type":"card",
        "schema":"message_reminder",
        "payload":{
            "message_id":other_parent,
            "by":fixture.actor
        }
    }]))
    .bind(future)
    .execute(&fixture.pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&error),
        Some("scheduled_messages_live_reminder_chk")
    );
    fixture.cleanup().await;
}
