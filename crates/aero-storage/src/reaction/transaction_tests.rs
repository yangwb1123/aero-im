use std::time::Duration;

use aero_common::{
    Block, Error, MessageId, ParticipantId, ReactionOp, RoomEvent, RoomId, RoomKind, WorkspaceId,
    WorkspaceRole,
};
use sqlx::PgPool;

use super::ReactionRepo;
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

async fn outboxed_message(
    pool: &PgPool,
    room: RoomId,
    sender: ParticipantId,
    label: &str,
) -> MessageId {
    MessageRepo::new(pool.clone())
        .insert_outboxed(
            NewMessage {
                room_id: room,
                sender_id: sender,
                blocks: vec![Block::text(label)],
                reply_to: None,
                metadata: serde_json::Value::Null,
                expires_at: None,
            },
            None,
            Vec::new(),
            None,
        )
        .await
        .unwrap()
        .message()
        .id
}

struct Fixture {
    pool: PgPool,
    repo: ReactionRepo,
    workspace: WorkspaceId,
    other_workspace: WorkspaceId,
    room: RoomId,
    message: MessageId,
    other_message: MessageId,
    owner: ParticipantId,
    actor: ParticipantId,
    outsider: ParticipantId,
}

impl Fixture {
    async fn create() -> Self {
        let pool = pool();
        let workspaces = WorkspaceRepo::new(pool.clone());
        let rooms = RoomRepo::new(pool.clone());
        let owner = participant(&pool, "reaction-owner").await;
        let actor = participant(&pool, "reaction-actor").await;
        let outsider = participant(&pool, "reaction-outsider").await;
        let workspace = workspaces
            .create(
                format!("Reaction {owner}"),
                format!("reaction-{owner}"),
                owner,
            )
            .await
            .unwrap()
            .id;
        workspaces
            .add_member(workspace, actor, WorkspaceRole::Member)
            .await
            .unwrap();
        let room = rooms
            .create_in_workspace(
                workspace,
                RoomKind::Channel,
                Some(format!("reaction-{owner}")),
                owner,
            )
            .await
            .unwrap()
            .id;
        rooms.add_member(room, actor).await.unwrap();

        let other_workspace = workspaces
            .create(
                format!("Reaction other {outsider}"),
                format!("reaction-other-{outsider}"),
                outsider,
            )
            .await
            .unwrap()
            .id;
        let other_room = rooms
            .create_in_workspace(
                other_workspace,
                RoomKind::Channel,
                Some(format!("reaction-other-{outsider}")),
                outsider,
            )
            .await
            .unwrap()
            .id;
        let message = outboxed_message(&pool, room, owner, "react here").await;
        let other_message =
            outboxed_message(&pool, other_room, outsider, "foreign reaction target").await;
        Self {
            repo: ReactionRepo::new(pool.clone()),
            pool,
            workspace,
            other_workspace,
            room,
            message,
            other_message,
            owner,
            actor,
            outsider,
        }
    }

    async fn cleanup(&self) {
        sqlx::query("DELETE FROM event_outbox WHERE message_id = ANY($1)")
            .bind(vec![self.message.to_uuid(), self.other_message.to_uuid()])
            .execute(&self.pool)
            .await
            .ok();
        sqlx::query("DELETE FROM workspaces WHERE id = ANY($1)")
            .bind(vec![
                self.workspace.to_uuid(),
                self.other_workspace.to_uuid(),
            ])
            .execute(&self.pool)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
            .bind(vec![
                self.owner.to_uuid(),
                self.actor.to_uuid(),
                self.outsider.to_uuid(),
            ])
            .execute(&self.pool)
            .await
            .ok();
    }
}

fn constraint(error: &sqlx::Error) -> Option<&str> {
    error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::constraint)
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn authorized_toggle_commits_projection_and_reaction_outbox_in_order() {
    let fixture = Fixture::create().await;
    // Announcement-only channels still allow member reactions; post_policy only
    // gates new/edited message content.
    sqlx::query("UPDATE rooms SET post_policy = 'admins' WHERE id = $1")
        .bind(fixture.room.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();

    let added = fixture
        .repo
        .toggle_authorized_outboxed(
            fixture.message,
            fixture.actor,
            "👍",
            Some("00-reaction-test"),
        )
        .await
        .unwrap();
    assert_eq!(added.op, ReactionOp::Add);
    assert_eq!(added.room_id, fixture.room);
    assert_eq!(added.message_sender, fixture.owner);
    let row: (String, i64, serde_json::Value, Option<String>) = sqlx::query_as(
        "SELECT event_kind, aggregate_version, payload, traceparent
           FROM event_outbox
          WHERE id = $1",
    )
    .bind(added.outbox_id)
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(row.0, "reaction");
    assert!(
        row.1 > 1,
        "reaction follows the message-created aggregate event"
    );
    assert_eq!(row.3.as_deref(), Some("00-reaction-test"));
    let event: RoomEvent = serde_json::from_value(row.2).unwrap();
    assert!(matches!(
        event,
        RoomEvent::Reaction {
            room_id,
            message_id,
            participant,
            op: ReactionOp::Add,
            ..
        } if room_id == fixture.room
            && message_id == fixture.message
            && participant == fixture.actor
    ));

    let removed = fixture
        .repo
        .toggle_authorized_outboxed(fixture.message, fixture.actor, "👍", None)
        .await
        .unwrap();
    assert_eq!(removed.op, ReactionOp::Remove);
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM reactions
          WHERE message_id = $1 AND participant_id = $2 AND emoji = '👍'",
    )
    .bind(fixture.message.to_uuid())
    .bind(fixture.actor.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(remaining, 0);
    let versions: Vec<i64> = sqlx::query_scalar(
        "SELECT aggregate_version
           FROM event_outbox
          WHERE message_id = $1 AND event_kind = 'reaction'
          ORDER BY aggregate_version",
    )
    .bind(fixture.message.to_uuid())
    .fetch_all(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(versions, vec![row.1, row.1 + 1]);
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn concurrent_duplicate_toggles_serialize_without_duplicate_projection() {
    let fixture = Fixture::create().await;
    let first_repo = fixture.repo.clone();
    let second_repo = fixture.repo.clone();
    let message = fixture.message;
    let actor = fixture.actor;
    let (first, second) = tokio::join!(
        async move {
            first_repo
                .toggle_authorized_outboxed(message, actor, "🔁", None)
                .await
                .unwrap()
                .op
        },
        async move {
            second_repo
                .toggle_authorized_outboxed(message, actor, "🔁", None)
                .await
                .unwrap()
                .op
        }
    );
    assert!(
        matches!(
            (first, second),
            (ReactionOp::Add, ReactionOp::Remove) | (ReactionOp::Remove, ReactionOp::Add)
        ),
        "the two committed toggles are one Add and one Remove"
    );
    let projection_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM reactions
          WHERE message_id = $1 AND participant_id = $2 AND emoji = '🔁'",
    )
    .bind(fixture.message.to_uuid())
    .bind(fixture.actor.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    let event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM event_outbox
          WHERE message_id = $1 AND event_kind = 'reaction'",
    )
    .bind(fixture.message.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(projection_count, 0);
    assert_eq!(event_count, 2);
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn cross_room_and_deleted_targets_have_stable_errors_and_zero_writes() {
    let fixture = Fixture::create().await;
    assert!(matches!(
        fixture
            .repo
            .toggle_authorized_outboxed(fixture.other_message, fixture.actor, "🚫", None)
            .await,
        Err(Error::Forbidden(_))
    ));
    let foreign_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM event_outbox
          WHERE message_id = $1 AND event_kind = 'reaction'",
    )
    .bind(fixture.other_message.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(foreign_events, 0);

    sqlx::query("UPDATE messages SET deleted_at = now() WHERE id = $1")
        .bind(fixture.message.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .repo
            .toggle_authorized_outboxed(fixture.message, fixture.actor, "🚫", None)
            .await,
        Err(Error::NotFound(_))
    ));
    let local_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM reactions WHERE message_id = $1")
            .bind(fixture.message.to_uuid())
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert_eq!(local_rows, 0);
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn revocation_wins_waiting_toggle_and_leaves_no_projection_or_event() {
    let fixture = Fixture::create().await;
    let mut revocation = fixture.pool.begin().await.unwrap();
    crate::ownership::lock_membership_governance(&mut revocation)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(fixture.workspace.to_uuid())
        .execute(&mut *revocation)
        .await
        .unwrap();
    sqlx::query(
        "DELETE FROM workspace_members
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(fixture.workspace.to_uuid())
    .bind(fixture.actor.to_uuid())
    .execute(&mut *revocation)
    .await
    .unwrap();

    let repo = fixture.repo.clone();
    let message = fixture.message;
    let actor = fixture.actor;
    let mut toggle = tokio::spawn(async move {
        repo.toggle_authorized_outboxed(message, actor, "⏳", None)
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut toggle)
            .await
            .is_err(),
        "toggle waits behind the workspace revocation fence"
    );
    revocation.commit().await.unwrap();
    assert!(matches!(toggle.await.unwrap(), Err(Error::Forbidden(_))));

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reactions WHERE message_id = $1")
        .bind(fixture.message.to_uuid())
        .fetch_one(&fixture.pool)
        .await
        .unwrap();
    let events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM event_outbox
          WHERE message_id = $1 AND event_kind = 'reaction'",
    )
    .bind(fixture.message.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!((rows, events), (0, 0));
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn outbox_failure_rolls_back_reaction_projection() {
    let fixture = Fixture::create().await;
    let suffix = fixture.message.to_uuid().simple().to_string();
    let function = format!("test_reject_reaction_outbox_{suffix}");
    let trigger = format!("test_reject_reaction_outbox_trigger_{suffix}");
    let create_function = format!(
        "CREATE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             IF NEW.event_kind = 'reaction'
                AND NEW.message_id = '{}'::uuid THEN
                 RAISE EXCEPTION 'injected reaction outbox failure';
             END IF;
             RETURN NEW;
         END
         $$",
        fixture.message.to_uuid()
    );
    sqlx::query(&create_function)
        .execute(&fixture.pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER {trigger}
         BEFORE INSERT ON event_outbox
         FOR EACH ROW EXECUTE FUNCTION {function}()"
    ))
    .execute(&fixture.pool)
    .await
    .unwrap();

    let result = fixture
        .repo
        .toggle_authorized_outboxed(fixture.message, fixture.actor, "💥", None)
        .await;
    sqlx::query(&format!("DROP TRIGGER {trigger} ON event_outbox"))
        .execute(&fixture.pool)
        .await
        .unwrap();
    sqlx::query(&format!("DROP FUNCTION {function}()"))
        .execute(&fixture.pool)
        .await
        .unwrap();
    assert!(matches!(result, Err(Error::Database(_))));
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reactions WHERE message_id = $1")
        .bind(fixture.message.to_uuid())
        .fetch_one(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "failed outbox append rolls back reaction insert");
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn raw_sql_guards_scope_identity_input_and_tombstone_cleanup() {
    let fixture = Fixture::create().await;
    let no_access = sqlx::query(
        "INSERT INTO reactions (message_id, participant_id, emoji)
         VALUES ($1, $2, 'forged')",
    )
    .bind(fixture.message.to_uuid())
    .bind(fixture.outsider.to_uuid())
    .execute(&fixture.pool)
    .await
    .expect_err("cross-tenant raw reaction must fail");
    assert_eq!(
        constraint(&no_access),
        Some("reactions_participant_scope_chk")
    );

    let empty = sqlx::query(
        "INSERT INTO reactions (message_id, participant_id, emoji)
         VALUES ($1, $2, '')",
    )
    .bind(fixture.message.to_uuid())
    .bind(fixture.actor.to_uuid())
    .execute(&fixture.pool)
    .await
    .expect_err("empty raw emoji must fail");
    assert_eq!(constraint(&empty), Some("reactions_emoji_bytes_chk"));

    let malformed_event = serde_json::json!({
        "kind": "reaction",
        "room_id": fixture.room,
        "message_id": fixture.message,
        "participant": fixture.actor,
        "op": "add"
    });
    let malformed_outbox = sqlx::query(
        "INSERT INTO event_outbox
             (id, event_id, message_id, event_kind, aggregate_version, subject, payload)
         SELECT $1, $2, $3, 'reaction',
                COALESCE(MAX(aggregate_version), 0) + 1,
                $4, $5
           FROM event_outbox
          WHERE message_id = $3",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(uuid::Uuid::new_v4())
    .bind(fixture.message.to_uuid())
    .bind(format!("im.room.{}", fixture.room))
    .bind(malformed_event)
    .execute(&fixture.pool)
    .await
    .expect_err("malformed raw reaction outbox must fail");
    assert_eq!(
        constraint(&malformed_outbox),
        Some("event_outbox_reaction_payload_chk")
    );

    let forged_event = serde_json::json!({
        "kind": "reaction",
        "room_id": fixture.room,
        "message_id": fixture.message,
        "participant": fixture.outsider,
        "emoji": "forged",
        "op": "add"
    });
    let forged_outbox = sqlx::query(
        "INSERT INTO event_outbox
             (id, event_id, message_id, event_kind, aggregate_version, subject, payload)
         SELECT $1, $2, $3, 'reaction',
                COALESCE(MAX(aggregate_version), 0) + 1,
                $4, $5
           FROM event_outbox
          WHERE message_id = $3",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(uuid::Uuid::new_v4())
    .bind(fixture.message.to_uuid())
    .bind(format!("im.room.{}", fixture.room))
    .bind(forged_event)
    .execute(&fixture.pool)
    .await
    .expect_err("cross-tenant raw reaction outbox must fail");
    assert_eq!(
        constraint(&forged_outbox),
        Some("event_outbox_reaction_participant_scope_chk")
    );

    fixture
        .repo
        .toggle_authorized_outboxed(fixture.message, fixture.actor, "✅", None)
        .await
        .unwrap();
    let rewrite = sqlx::query(
        "UPDATE reactions
            SET emoji = 'rewritten'
          WHERE message_id = $1 AND participant_id = $2 AND emoji = '✅'",
    )
    .bind(fixture.message.to_uuid())
    .bind(fixture.actor.to_uuid())
    .execute(&fixture.pool)
    .await
    .expect_err("reaction identity is immutable");
    assert_eq!(
        constraint(&rewrite),
        Some("reactions_identity_immutable_chk")
    );

    sqlx::query("UPDATE messages SET deleted_at = now() WHERE id = $1")
        .bind(fixture.message.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();
    let deleted_event = serde_json::json!({
        "kind": "reaction",
        "room_id": fixture.room,
        "message_id": fixture.message,
        "participant": fixture.actor,
        "emoji": "late",
        "op": "add"
    });
    let deleted_outbox = sqlx::query(
        "INSERT INTO event_outbox
             (id, event_id, message_id, event_kind, aggregate_version, subject, payload)
         SELECT $1, $2, $3, 'reaction',
                COALESCE(MAX(aggregate_version), 0) + 1,
                $4, $5
           FROM event_outbox
          WHERE message_id = $3",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(uuid::Uuid::new_v4())
    .bind(fixture.message.to_uuid())
    .bind(format!("im.room.{}", fixture.room))
    .bind(deleted_event)
    .execute(&fixture.pool)
    .await
    .expect_err("deleted-message raw reaction outbox must fail");
    assert_eq!(
        constraint(&deleted_outbox),
        Some("event_outbox_reaction_scope_chk")
    );
    let after_tombstone: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM reactions WHERE message_id = $1")
            .bind(fixture.message.to_uuid())
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert_eq!(after_tombstone, 0);
    fixture.cleanup().await;
}
