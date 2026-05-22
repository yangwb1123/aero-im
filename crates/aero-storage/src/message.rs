//! Message repository.

use aero_common::{Block, Message, MessageId, ParticipantId, RoomId};
use sqlx::PgPool;

#[derive(Clone)]
pub struct MessageRepo {
    pool: PgPool,
}

#[derive(Debug, Clone)]
pub struct NewMessage {
    pub room_id: RoomId,
    pub sender_id: ParticipantId,
    pub blocks: Vec<Block>,
    pub reply_to: Option<MessageId>,
    pub metadata: serde_json::Value,
}

impl MessageRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn insert(&self, new: NewMessage) -> Result<Message, sqlx::Error> {
        let id = MessageId::new();
        let created_at = time::OffsetDateTime::now_utc();
        let blocks_json = serde_json::to_value(&new.blocks)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        sqlx::query(
            r#"INSERT INTO messages (id, room_id, sender_id, blocks, reply_to, metadata, created_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7)"#,
        )
        .bind(id.to_uuid())
        .bind(new.room_id.to_uuid())
        .bind(new.sender_id.to_uuid())
        .bind(&blocks_json)
        .bind(new.reply_to.map(|m| m.to_uuid()))
        .bind(&new.metadata)
        .bind(created_at)
        .execute(&self.pool)
        .await?;

        Ok(Message {
            id,
            room_id: new.room_id,
            sender_id: new.sender_id,
            blocks: new.blocks,
            reply_to: new.reply_to,
            metadata: new.metadata,
            created_at,
            edited_at: None,
            deleted_at: None,
        })
    }

    /// Fetch the most recent `limit` messages in a room, optionally before a cursor.
    pub async fn list_recent(
        &self,
        room: RoomId,
        before: Option<MessageId>,
        limit: i64,
    ) -> Result<Vec<Message>, sqlx::Error> {
        let limit = limit.clamp(1, 200);
        let rows = if let Some(b) = before {
            sqlx::query_as::<_, MessageRow>(
                r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at
                   FROM messages
                   WHERE room_id = $1 AND id < $2 AND deleted_at IS NULL
                   ORDER BY id DESC
                   LIMIT $3"#,
            )
            .bind(room.to_uuid())
            .bind(b.to_uuid())
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, MessageRow>(
                r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at
                   FROM messages
                   WHERE room_id = $1 AND deleted_at IS NULL
                   ORDER BY id DESC
                   LIMIT $2"#,
            )
            .bind(room.to_uuid())
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };

        Ok(rows.into_iter().map(Message::from).collect())
    }
}

#[derive(sqlx::FromRow)]
struct MessageRow {
    id: uuid::Uuid,
    room_id: uuid::Uuid,
    sender_id: uuid::Uuid,
    blocks: serde_json::Value,
    reply_to: Option<uuid::Uuid>,
    metadata: serde_json::Value,
    created_at: time::OffsetDateTime,
    edited_at: Option<time::OffsetDateTime>,
    deleted_at: Option<time::OffsetDateTime>,
}

impl From<MessageRow> for Message {
    fn from(r: MessageRow) -> Self {
        let blocks: Vec<Block> = serde_json::from_value(r.blocks).unwrap_or_default();
        Self {
            id: MessageId::from_uuid(r.id),
            room_id: RoomId::from_uuid(r.room_id),
            sender_id: ParticipantId::from_uuid(r.sender_id),
            blocks,
            reply_to: r.reply_to.map(MessageId::from_uuid),
            metadata: r.metadata,
            created_at: r.created_at,
            edited_at: r.edited_at,
            deleted_at: r.deleted_at,
        }
    }
}
