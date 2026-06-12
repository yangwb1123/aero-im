//! Pinned-messages repository (To-B collaboration).
//!
//! Backs `migrations/0011_pins.sql`. A room can pin a small set of important
//! messages; listing joins back to `messages` so callers render the pinned
//! content directly. Purely additive: a NEW [`PinRepo`]; no existing repo is
//! touched.

use aero_common::{Block, Message, MessageId, ParticipantId, PinnedMessage, RoomId};
use sqlx::PgPool;

/// Largest number of pins a room listing returns. A room with more than this many
/// pins is unusual; the cap keeps the panel bounded.
const MAX_PINS: i64 = 200;

#[derive(Clone)]
pub struct PinRepo {
    pool: PgPool,
}

impl PinRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Pin a message in a room. Idempotent: re-pinning an already-pinned message
    /// keeps the original provenance. Returns `true` if a new pin was created.
    /// Caller has already checked the actor's room access and that the message
    /// belongs to the room.
    pub async fn pin(
        &self,
        room: RoomId,
        message: MessageId,
        by: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"INSERT INTO pins (room_id, message_id, pinned_by, created_at)
               VALUES ($1, $2, $3, now())
               ON CONFLICT (room_id, message_id) DO NOTHING",
        )
        .bind(room.to_uuid())
        .bind(message.to_uuid())
        .bind(by.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Unpin a message. Returns `true` if a pin was removed.
    pub async fn unpin(
        &self,
        room: RoomId,
        message: MessageId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(r"DELETE FROM pins WHERE room_id = $1 AND message_id = $2")
            .bind(room.to_uuid())
            .bind(message.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Whether a message is currently pinned in a room.
    pub async fn is_pinned(
        &self,
        room: RoomId,
        message: MessageId,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (i64,)>(
            r"SELECT COUNT(*) FROM pins WHERE room_id = $1 AND message_id = $2",
        )
        .bind(room.to_uuid())
        .bind(message.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0 > 0)
    }

    /// List a room's pins, newest first, each joined to its (non-deleted)
    /// message. Pins whose message has since been soft-deleted are skipped.
    pub async fn list_for_room(&self, room: RoomId) -> Result<Vec<PinnedMessage>, sqlx::Error> {
        let rows = sqlx::query_as::<_, PinnedRow>(
            r"SELECT
                 p.room_id, p.pinned_by, p.created_at AS pinned_at,
                 m.id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                 m.created_at, m.edited_at, m.deleted_at, m.expires_at
               FROM pins p
               JOIN messages m ON m.id = p.message_id
               WHERE p.room_id = $1 AND m.deleted_at IS NULL
               ORDER BY p.created_at DESC
               LIMIT $2",
        )
        .bind(room.to_uuid())
        .bind(MAX_PINS)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(PinnedMessage::from).collect())
    }
}

#[derive(sqlx::FromRow)]
struct PinnedRow {
    room_id: uuid::Uuid,
    pinned_by: uuid::Uuid,
    pinned_at: time::OffsetDateTime,
    id: uuid::Uuid,
    sender_id: uuid::Uuid,
    blocks: serde_json::Value,
    reply_to: Option<uuid::Uuid>,
    metadata: serde_json::Value,
    created_at: time::OffsetDateTime,
    edited_at: Option<time::OffsetDateTime>,
    deleted_at: Option<time::OffsetDateTime>,
    expires_at: Option<time::OffsetDateTime>,
}

impl From<PinnedRow> for PinnedMessage {
    fn from(r: PinnedRow) -> Self {
        let blocks: Vec<Block> = serde_json::from_value(r.blocks).unwrap_or_default();
        let room_id = RoomId::from_uuid(r.room_id);
        Self {
            room_id,
            message: Message {
                id: MessageId::from_uuid(r.id),
                room_id,
                sender_id: ParticipantId::from_uuid(r.sender_id),
                blocks,
                reply_to: r.reply_to.map(MessageId::from_uuid),
                metadata: r.metadata,
                created_at: r.created_at,
                edited_at: r.edited_at,
                deleted_at: r.deleted_at,
                expires_at: r.expires_at,
            },
            pinned_by: ParticipantId::from_uuid(r.pinned_by),
            pinned_at: r.pinned_at,
        }
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored pin_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{MessageId, ParticipantId, RoomId};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn fixture(p: &PgPool) -> (RoomId, MessageId, ParticipantId) {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(actor.to_uuid())
            .bind(format!("pin-actor-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");
        let room = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
             VALUES ($1,'channel',$2,$3, now(), '00000000-0000-0000-0000-000000000000')",
        )
        .bind(room.to_uuid())
        .bind("pin-room")
        .bind(actor.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        let message = MessageId::new();
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks, searchable_text, created_at)
             VALUES ($1,$2,$3,'[{\"type\":\"text\",\"content\":\"hi\"}]'::jsonb,'hi', now())",
        )
        .bind(message.to_uuid())
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .execute(p)
        .await
        .expect("insert message");
        (room, message, actor)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn pin_then_list_then_unpin_roundtrip() {
        let p = pool();
        let repo = PinRepo::new(p.clone());
        let (room, message, actor) = fixture(&p).await;

        assert!(repo.pin(room, message, actor).await.unwrap(), "first pin created");
        assert!(!repo.pin(room, message, actor).await.unwrap(), "re-pin is idempotent");
        assert!(repo.is_pinned(room, message).await.unwrap());

        let pins = repo.list_for_room(room).await.unwrap();
        assert_eq!(pins.len(), 1, "one pin in the room");
        assert_eq!(pins[0].message.id, message);
        assert_eq!(pins[0].pinned_by, actor);
        assert!(!pins[0].message.blocks.is_empty(), "joined message content present");

        assert!(repo.unpin(room, message).await.unwrap(), "unpin removed it");
        assert!(!repo.is_pinned(room, message).await.unwrap());
        assert!(repo.list_for_room(room).await.unwrap().is_empty());
    }
}
