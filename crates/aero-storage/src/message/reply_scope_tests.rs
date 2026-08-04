use aero_common::{Block, Error, MessageId, ParticipantId, RoomId};

use super::{MessageRepo, NewMessage};

fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_lazy(&url)
        .expect("connect_lazy never fails for a well-formed URL")
}

async fn fixture(pool: &sqlx::PgPool) -> (ParticipantId, RoomId, RoomId) {
    let sender = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(sender.to_uuid())
        .bind(format!("reply-scope-{sender}"))
        .execute(pool)
        .await
        .expect("participant");
    sqlx::query(
        r"INSERT INTO workspace_members (workspace_id, participant_id, role)
          VALUES ('00000000-0000-0000-0000-000000000000', $1, 'member')",
    )
    .bind(sender.to_uuid())
    .execute(pool)
    .await
    .expect("workspace membership");

    let room_a = RoomId::new();
    let room_b = RoomId::new();
    for room in [room_a, room_b] {
        sqlx::query(
            r"INSERT INTO rooms (id, kind, name, created_by, workspace_id)
              VALUES ($1, 'group', $2, $3, '00000000-0000-0000-0000-000000000000')",
        )
        .bind(room.to_uuid())
        .bind(format!("reply-scope-{room}"))
        .bind(sender.to_uuid())
        .execute(pool)
        .await
        .expect("room");
        sqlx::query(
            r"INSERT INTO room_members (room_id, participant_id, role)
              VALUES ($1, $2, 'owner')",
        )
        .bind(room.to_uuid())
        .bind(sender.to_uuid())
        .execute(pool)
        .await
        .expect("room membership");
    }
    (sender, room_a, room_b)
}

fn message(room: RoomId, sender: ParticipantId, reply_to: Option<MessageId>) -> NewMessage {
    NewMessage {
        room_id: room,
        sender_id: sender,
        blocks: vec![Block::text("reply scope regression")],
        reply_to,
        metadata: serde_json::json!({}),
        expires_at: None,
    }
}

#[tokio::test]
#[ignore = "requires live Postgres with migration 0190 applied"]
async fn reply_parent_is_database_contained_by_room() {
    let pool = pool();
    let (sender, room_a, room_b) = fixture(&pool).await;
    let repo = MessageRepo::new(pool.clone());

    let parent = repo
        .insert(message(room_a, sender, None))
        .await
        .expect("parent");
    let valid = repo
        .insert(message(room_a, sender, Some(parent.id)))
        .await
        .expect("same-room reply");
    assert_eq!(valid.reply_to, Some(parent.id));

    let rejected = repo
        .insert(message(room_b, sender, Some(parent.id)))
        .await
        .expect_err("repository rejects a cross-room parent");
    assert!(matches!(rejected, sqlx::Error::Protocol(_)));

    let outboxed_error = repo
        .insert_outboxed(
            message(room_b, sender, Some(parent.id)),
            None,
            Vec::new(),
            None,
        )
        .await
        .expect_err("outbox transaction rejects a cross-room parent");
    assert!(matches!(outboxed_error, Error::Invalid(_)));

    let direct_id = MessageId::new();
    let direct_error = sqlx::query(
        r"INSERT INTO messages
              (id, room_id, sender_id, blocks, reply_to, searchable_text)
          VALUES ($1, $2, $3, '[]'::jsonb, $4, 'cross-room')",
    )
    .bind(direct_id.to_uuid())
    .bind(room_b.to_uuid())
    .bind(sender.to_uuid())
    .bind(parent.id.to_uuid())
    .execute(&pool)
    .await
    .expect_err("composite FK rejects direct cross-room writes");
    assert_eq!(
        direct_error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("messages_reply_same_room_fkey")
    );

    sqlx::query("DELETE FROM messages WHERE id = $1")
        .bind(parent.id.to_uuid())
        .execute(&pool)
        .await
        .expect("hard delete parent");
    let child: (uuid::Uuid, Option<uuid::Uuid>) =
        sqlx::query_as("SELECT room_id, reply_to FROM messages WHERE id = $1")
            .bind(valid.id.to_uuid())
            .fetch_one(&pool)
            .await
            .expect("child remains");
    assert_eq!(child, (room_a.to_uuid(), None));
}

#[tokio::test]
#[ignore = "requires live Postgres with migration 0190 applied"]
async fn defensive_thread_queries_hide_deferred_historical_cross_room_edges() {
    let pool = pool();
    let (sender, room_a, room_b) = fixture(&pool).await;
    let repo = MessageRepo::new(pool.clone());
    let parent = repo
        .insert(message(room_a, sender, None))
        .await
        .expect("parent");

    // The production constraint is immediate. Deferring it inside this
    // rollback-only transaction gives the read predicates a representative
    // historical dirty row without ever committing invalid state.
    let mut tx = pool.begin().await.expect("transaction");
    sqlx::query("SET CONSTRAINTS messages_reply_same_room_fkey DEFERRED")
        .execute(&mut *tx)
        .await
        .expect("defer containment FK");
    sqlx::query(
        r"INSERT INTO messages
              (id, room_id, sender_id, blocks, reply_to, searchable_text)
          VALUES ($1, $2, $3, '[]'::jsonb, $4, 'historical-cross-room')",
    )
    .bind(MessageId::new().to_uuid())
    .bind(room_b.to_uuid())
    .bind(sender.to_uuid())
    .bind(parent.id.to_uuid())
    .execute(&mut *tx)
    .await
    .expect("temporary deferred violation");

    let visible: i64 = sqlx::query_scalar(
        r"SELECT COUNT(*)
            FROM messages AS reply
            JOIN messages AS root
              ON root.id = $1 AND root.room_id = reply.room_id
           WHERE reply.reply_to = $1",
    )
    .bind(parent.id.to_uuid())
    .fetch_one(&mut *tx)
    .await
    .expect("scoped thread read");
    assert_eq!(visible, 0, "cross-room historical edge stays invisible");

    tx.rollback().await.expect("discard deferred violation");
}
