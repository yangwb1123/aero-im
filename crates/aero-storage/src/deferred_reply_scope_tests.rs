use aero_common::{Block, ParticipantId, RoomId, ScheduledMessageId, WorkspaceId};

use crate::message::NewMessage;
use crate::{DraftRepo, MessageRepo, ScheduledRepo};

fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("valid DATABASE_URL")
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn scheduled_and_draft_replies_are_room_contained_until_delivery() {
    let pool = pool();
    let actor = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id,kind,display_name) VALUES ($1,'human',$2)")
        .bind(actor.to_uuid())
        .bind(format!("deferred-reply-{actor}"))
        .execute(&pool)
        .await
        .expect("participant");
    let workspace = WorkspaceId::new();
    let mut workspace_tx = pool.begin().await.expect("begin workspace fixture");
    sqlx::query(
        "INSERT INTO workspaces (id,name,slug,created_by)
         VALUES ($1,$2,$3,$4)",
    )
    .bind(workspace.to_uuid())
    .bind("Deferred reply scope")
    .bind(format!("deferred-reply-{workspace}"))
    .bind(actor.to_uuid())
    .execute(&mut *workspace_tx)
    .await
    .expect("workspace");
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, 'owner')",
    )
    .bind(workspace.to_uuid())
    .bind(actor.to_uuid())
    .execute(&mut *workspace_tx)
    .await
    .expect("workspace owner");
    workspace_tx
        .commit()
        .await
        .expect("commit workspace fixture");

    let room_a = RoomId::new();
    let room_b = RoomId::new();
    for room in [room_a, room_b] {
        sqlx::query(
            "INSERT INTO rooms (id,kind,created_by,workspace_id)
             VALUES ($1,'group',$2,$3)",
        )
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .bind(workspace.to_uuid())
        .execute(&pool)
        .await
        .expect("room");
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'owner')",
        )
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .execute(&pool)
        .await
        .expect("room owner");
    }
    let messages = MessageRepo::new(pool.clone());
    let parent = messages
        .insert(NewMessage {
            room_id: room_a,
            sender_id: actor,
            blocks: vec![Block::text("thread root")],
            reply_to: None,
            metadata: serde_json::Value::Null,
            expires_at: None,
        })
        .await
        .expect("parent");
    let deliver_at = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let scheduled = ScheduledRepo::new(pool.clone());

    let repo_error = scheduled
        .create(
            room_b,
            actor,
            &[Block::text("cross-room scheduled reply")],
            Some(parent.id),
            deliver_at,
        )
        .await
        .expect_err("repo must reject a cross-room scheduled reply");
    assert!(matches!(
        repo_error,
        aero_common::Error::Database(sqlx::Error::Protocol(_))
    ));

    let direct_error = sqlx::query(
        "INSERT INTO scheduled_messages
             (id,room_id,sender_id,blocks,reply_to,scheduled_at,available_at)
         VALUES ($1,$2,$3,'[]'::jsonb,$4,$5,$5)",
    )
    .bind(ScheduledMessageId::new().to_uuid())
    .bind(room_b.to_uuid())
    .bind(actor.to_uuid())
    .bind(parent.id.to_uuid())
    .bind(deliver_at)
    .execute(&pool)
    .await
    .expect_err("database fence must reject a bypass");
    assert!(matches!(direct_error, sqlx::Error::Database(_)));

    let scheduled_id = scheduled
        .create(
            room_a,
            actor,
            &[Block::text("same-room scheduled reply")],
            Some(parent.id),
            deliver_at,
        )
        .await
        .expect("same-room scheduled reply");
    let drafts = DraftRepo::new(pool.clone());
    let draft_error = drafts
        .upsert(
            actor,
            room_b,
            &[Block::text("cross-room draft reply")],
            Some(parent.id),
        )
        .await
        .expect_err("repo must reject a cross-room draft reply");
    assert!(matches!(draft_error, sqlx::Error::Protocol(_)));
    let direct_draft_error = sqlx::query(
        "INSERT INTO message_drafts
             (participant_id,room_id,blocks,reply_to)
         VALUES ($1,$2,'[]'::jsonb,$3)",
    )
    .bind(actor.to_uuid())
    .bind(room_b.to_uuid())
    .bind(parent.id.to_uuid())
    .execute(&pool)
    .await
    .expect_err("draft database fence must reject a bypass");
    assert!(matches!(direct_draft_error, sqlx::Error::Database(_)));
    drafts
        .upsert(
            actor,
            room_a,
            &[Block::text("same-room draft reply")],
            Some(parent.id),
        )
        .await
        .expect("same-room draft reply");

    sqlx::query(
        "UPDATE scheduled_messages
            SET delivery_status = 'claimed',
                attempts = 1,
                claim_token = $2,
                claimed_at = now(),
                lease_expires_at = now() + interval '1 minute'
          WHERE id = $1",
    )
    .bind(scheduled_id.to_uuid())
    .bind(uuid::Uuid::new_v4())
    .execute(&pool)
    .await
    .expect("start a delivery attempt");
    let direct_clear = sqlx::query("UPDATE scheduled_messages SET reply_to = NULL WHERE id = $1")
        .bind(scheduled_id.to_uuid())
        .execute(&pool)
        .await
        .expect_err("a direct writer cannot mutate an attempted payload");
    assert_eq!(
        direct_clear
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("23514")
    );

    sqlx::query("DELETE FROM messages WHERE id = $1")
        .bind(parent.id.to_uuid())
        .execute(&pool)
        .await
        .expect("hard-delete parent through the nested FK action");
    let scheduled_state: (Option<uuid::Uuid>, i32, String) = sqlx::query_as(
        "SELECT reply_to, attempts, delivery_status
           FROM scheduled_messages
          WHERE id = $1",
    )
    .bind(scheduled_id.to_uuid())
    .fetch_one(&pool)
    .await
    .expect("scheduled row");
    let draft_reply: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT reply_to FROM message_drafts
         WHERE participant_id = $1 AND room_id = $2",
    )
    .bind(actor.to_uuid())
    .bind(room_a.to_uuid())
    .fetch_one(&pool)
    .await
    .expect("draft row");
    assert_eq!(scheduled_state, (None, 1, "claimed".to_owned()));
    assert!(draft_reply.is_none());
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn migration_cleanup_can_repair_an_attempted_historical_cross_room_reply() {
    let pool = pool();
    let actor = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id,kind,display_name) VALUES ($1,'human',$2)")
        .bind(actor.to_uuid())
        .bind(format!("deferred-migration-{actor}"))
        .execute(&pool)
        .await
        .expect("participant");
    let workspace = WorkspaceId::new();
    let mut workspace_tx = pool.begin().await.expect("begin workspace fixture");
    sqlx::query(
        "INSERT INTO workspaces (id,name,slug,created_by)
         VALUES ($1,$2,$3,$4)",
    )
    .bind(workspace.to_uuid())
    .bind("Deferred migration scope")
    .bind(format!("deferred-migration-{workspace}"))
    .bind(actor.to_uuid())
    .execute(&mut *workspace_tx)
    .await
    .expect("workspace");
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, 'owner')",
    )
    .bind(workspace.to_uuid())
    .bind(actor.to_uuid())
    .execute(&mut *workspace_tx)
    .await
    .expect("workspace owner");
    workspace_tx
        .commit()
        .await
        .expect("commit workspace fixture");
    let room_a = RoomId::new();
    let room_b = RoomId::new();
    for room in [room_a, room_b] {
        sqlx::query(
            "INSERT INTO rooms (id,kind,created_by,workspace_id)
             VALUES ($1,'group',$2,$3)",
        )
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .bind(workspace.to_uuid())
        .execute(&pool)
        .await
        .expect("room");
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'owner')",
        )
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .execute(&pool)
        .await
        .expect("room owner");
    }
    let parent = MessageRepo::new(pool.clone())
        .insert(NewMessage {
            room_id: room_a,
            sender_id: actor,
            blocks: vec![Block::text("historical parent")],
            reply_to: None,
            metadata: serde_json::Value::Null,
            expires_at: None,
        })
        .await
        .expect("parent");
    let scheduled_id = ScheduledMessageId::new();
    let deliver_at = time::OffsetDateTime::now_utc() + time::Duration::hours(1);

    // Recreate a pre-0192 cross-room row inside a rollback-only transaction,
    // then execute the migration's trigger-safe repair sequence.
    let mut tx = pool.begin().await.expect("transaction");
    sqlx::query(
        "ALTER TABLE scheduled_messages
         DROP CONSTRAINT scheduled_messages_reply_same_room_fkey",
    )
    .execute(&mut *tx)
    .await
    .expect("temporarily model pre-0192 schema");
    sqlx::query(
        "ALTER TABLE scheduled_messages
         DISABLE TRIGGER scheduled_messages_scope_state_guard",
    )
    .execute(&mut *tx)
    .await
    .expect("temporarily model pre-0215 trigger set");
    sqlx::query(
        "INSERT INTO scheduled_messages
             (id,room_id,sender_id,blocks,reply_to,scheduled_at,available_at,attempts)
         VALUES (
             $1,$2,$3,
             '[{\"type\":\"text\",\"content\":\"historical\"}]'::jsonb,
             $4,$5,$5,1
         )",
    )
    .bind(scheduled_id.to_uuid())
    .bind(room_b.to_uuid())
    .bind(actor.to_uuid())
    .bind(parent.id.to_uuid())
    .bind(deliver_at)
    .execute(&mut *tx)
    .await
    .expect("historical attempted cross-room reply");
    sqlx::query(
        "ALTER TABLE scheduled_messages
         DISABLE TRIGGER scheduled_messages_delivery_fence",
    )
    .execute(&mut *tx)
    .await
    .expect("disable payload fence under migration lock");
    sqlx::query(
        "UPDATE scheduled_messages AS scheduled
            SET reply_to = NULL
          WHERE scheduled.id = $1",
    )
    .bind(scheduled_id.to_uuid())
    .execute(&mut *tx)
    .await
    .expect("repair historical row");
    sqlx::query(
        "ALTER TABLE scheduled_messages
         ENABLE TRIGGER scheduled_messages_delivery_fence",
    )
    .execute(&mut *tx)
    .await
    .expect("restore payload fence");
    sqlx::query(
        "ALTER TABLE scheduled_messages
         ENABLE TRIGGER scheduled_messages_scope_state_guard",
    )
    .execute(&mut *tx)
    .await
    .expect("restore deferred scope fence");
    let repaired: (Option<uuid::Uuid>, i32) = sqlx::query_as(
        "SELECT reply_to, attempts
           FROM scheduled_messages
          WHERE id = $1",
    )
    .bind(scheduled_id.to_uuid())
    .fetch_one(&mut *tx)
    .await
    .expect("repaired row");
    assert_eq!(repaired, (None, 1));
    tx.rollback().await.expect("discard schema simulation");
}
