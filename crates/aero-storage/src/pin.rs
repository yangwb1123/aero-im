//! Pinned-messages repository (To-B collaboration).
//!
//! Backs `migrations/0011_pins.sql`. A room can pin a small set of important
//! messages; listing joins back to `messages` so callers render the pinned
//! content directly. Production reads/writes own current room authorization,
//! message containment and the pin mutation in one transaction.

use aero_common::{Block, Error, Message, MessageId, ParticipantId, PinnedMessage, RoomId};
use sqlx::{PgPool, Postgres, Transaction};

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

    /// Pin a live message while current room access and message containment are
    /// locked through commit.
    ///
    /// Re-pinning preserves the original provenance and returns `false`.
    /// Missing, deleted, expired, or cross-room message ids share one opaque
    /// not-found result.
    pub async fn pin_authorized(
        &self,
        room: RoomId,
        message: MessageId,
        actor: ParticipantId,
    ) -> Result<bool, Error> {
        let mut tx = self.pool.begin().await?;
        assert_effective_room_access_in_tx(&mut tx, room, actor).await?;
        let Some(locked_message) = lock_message_in_tx(&mut tx, message).await? else {
            return Err(Error::NotFound(format!("message {message}")));
        };
        if locked_message.room_id != room
            || locked_message.deleted_at.is_some()
            || locked_message
                .expires_at
                .is_some_and(|expires_at| expires_at <= time::OffsetDateTime::now_utc())
        {
            return Err(Error::NotFound(format!("message {message}")));
        }

        let result = sqlx::query(
            r"INSERT INTO pins (room_id, message_id, pinned_by, created_at)
               VALUES ($1, $2, $3, now())
               ON CONFLICT (room_id, message_id) DO NOTHING",
        )
        .bind(room.to_uuid())
        .bind(message.to_uuid())
        .bind(actor.to_uuid())
        .execute(&mut *tx)
        .await?;
        let created = result.rows_affected() > 0;
        tx.commit().await?;
        Ok(created)
    }

    /// Unpin under current room access. A same-room soft-deleted message remains
    /// removable so historical projections can be cleaned; a hard-deleted
    /// message has already cascaded its pin and returns the idempotent `false`.
    pub async fn unpin_authorized(
        &self,
        room: RoomId,
        message: MessageId,
        actor: ParticipantId,
    ) -> Result<bool, Error> {
        let mut tx = self.pool.begin().await?;
        assert_effective_room_access_in_tx(&mut tx, room, actor).await?;
        let Some(locked_message) = lock_message_in_tx(&mut tx, message).await? else {
            tx.commit().await?;
            return Ok(false);
        };
        if locked_message.room_id != room {
            return Err(Error::NotFound(format!("message {message}")));
        }

        let result = sqlx::query(r"DELETE FROM pins WHERE room_id = $1 AND message_id = $2")
            .bind(room.to_uuid())
            .bind(message.to_uuid())
            .execute(&mut *tx)
            .await?;
        let removed = result.rows_affected() > 0;
        tx.commit().await?;
        Ok(removed)
    }

    /// List a room's current live pins while effective room access is held
    /// through the joined message read.
    pub async fn list_authorized(
        &self,
        room: RoomId,
        actor: ParticipantId,
    ) -> Result<Vec<PinnedMessage>, Error> {
        let mut tx = self.pool.begin().await?;
        assert_effective_room_access_in_tx(&mut tx, room, actor).await?;
        let pins = list_for_room_in_tx(&mut tx, room).await?;
        tx.commit().await?;
        Ok(pins)
    }

    /// Low-level compatibility seam used only by storage tests.
    #[cfg(test)]
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

    /// Low-level compatibility seam used only by storage tests.
    #[cfg(test)]
    pub async fn unpin(&self, room: RoomId, message: MessageId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(r"DELETE FROM pins WHERE room_id = $1 AND message_id = $2")
            .bind(room.to_uuid())
            .bind(message.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Low-level compatibility seam used only by storage tests.
    #[cfg(test)]
    pub async fn is_pinned(&self, room: RoomId, message: MessageId) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (i64,)>(
            r"SELECT COUNT(*) FROM pins WHERE room_id = $1 AND message_id = $2",
        )
        .bind(room.to_uuid())
        .bind(message.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0 > 0)
    }

    /// Low-level compatibility seam used only by storage tests.
    #[cfg(test)]
    pub async fn list_for_room(&self, room: RoomId) -> Result<Vec<PinnedMessage>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let pins = list_for_room_in_tx(&mut tx, room).await?;
        tx.commit().await?;
        Ok(pins)
    }
}

struct LockedMessage {
    room_id: RoomId,
    deleted_at: Option<time::OffsetDateTime>,
    expires_at: Option<time::OffsetDateTime>,
}

async fn assert_effective_room_access_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    room: RoomId,
    actor: ParticipantId,
) -> Result<(), Error> {
    let allowed: bool = sqlx::query_scalar("SELECT aero_effective_room_access($1, $2, NULL)")
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .fetch_one(&mut **tx)
        .await?;
    if !allowed {
        return Err(Error::Forbidden("current room access required".into()));
    }
    Ok(())
}

async fn lock_message_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    message: MessageId,
) -> Result<Option<LockedMessage>, sqlx::Error> {
    sqlx::query_as::<
        _,
        (
            uuid::Uuid,
            Option<time::OffsetDateTime>,
            Option<time::OffsetDateTime>,
        ),
    >(
        "SELECT room_id, deleted_at, expires_at
           FROM messages
          WHERE id = $1
          FOR UPDATE",
    )
    .bind(message.to_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map(|row| {
        row.map(|(room_id, deleted_at, expires_at)| LockedMessage {
            room_id: RoomId::from_uuid(room_id),
            deleted_at,
            expires_at,
        })
    })
}

async fn list_for_room_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    room: RoomId,
) -> Result<Vec<PinnedMessage>, sqlx::Error> {
    let rows = sqlx::query_as::<_, PinnedRow>(
        r"SELECT
             p.room_id, p.pinned_by, p.created_at AS pinned_at,
             m.id, m.sender_id, m.blocks, m.reply_to, m.metadata,
             m.created_at, m.edited_at, m.deleted_at, m.expires_at, m.version
           FROM pins p
           JOIN messages m
             ON m.id = p.message_id
            AND m.room_id = p.room_id
           WHERE p.room_id = $1
             AND m.deleted_at IS NULL
             AND (m.expires_at IS NULL OR m.expires_at > CURRENT_TIMESTAMP)
           ORDER BY p.created_at DESC
           LIMIT $2",
    )
    .bind(room.to_uuid())
    .bind(MAX_PINS)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows.into_iter().map(PinnedMessage::from).collect())
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
    version: i32,
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
                version: r.version,
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
    use aero_common::{MessageId, ParticipantId, RoomId, RoomKind};

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
        let workspace = crate::WorkspaceRepo::new(p.clone())
            .create(
                format!("pin-workspace-{actor}"),
                format!("pin-workspace-{actor}").to_ascii_lowercase(),
                actor,
            )
            .await
            .expect("insert workspace")
            .id;
        let room = crate::RoomRepo::new(p.clone())
            .create_in_workspace(
                workspace,
                RoomKind::Group,
                Some(format!("pin-room-{actor}")),
                actor,
            )
            .await
            .expect("insert room")
            .id;
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

        assert!(
            repo.pin_authorized(room, message, actor).await.unwrap(),
            "first pin created"
        );
        assert!(
            !repo.pin_authorized(room, message, actor).await.unwrap(),
            "re-pin is idempotent"
        );
        assert!(repo.is_pinned(room, message).await.unwrap());

        let pins = repo.list_authorized(room, actor).await.unwrap();
        assert_eq!(pins.len(), 1, "one pin in the room");
        assert_eq!(pins[0].message.id, message);
        assert_eq!(pins[0].pinned_by, actor);
        assert!(
            !pins[0].message.blocks.is_empty(),
            "joined message content present"
        );

        assert!(
            repo.unpin_authorized(room, message, actor).await.unwrap(),
            "unpin removed it"
        );
        assert!(!repo.is_pinned(room, message).await.unwrap());
        assert!(repo.list_authorized(room, actor).await.unwrap().is_empty());
    }
}

#[cfg(test)]
#[path = "pin/security_tests.rs"]
mod security_tests;
