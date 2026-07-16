//! Saved-items / bookmarks repository (personal save-for-later).
//!
//! Backs `migrations/0020_bookmarks.sql`. A user privately saves messages to
//! revisit later (Slack "Saved items"). Unlike pins (per-room, visible to every
//! member), a bookmark is per-user and the saved list crosses rooms. Listing
//! joins back to `messages` so callers render the saved content directly,
//! skipping any message that has since been soft-deleted. Purely additive: a NEW
//! [`BookmarkRepo`]; no existing repo is touched.

use aero_common::{Block, BookmarkCollectionId, Message, MessageId, ParticipantId, RoomId};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use time::OffsetDateTime;

/// A saved message record: the joined [`Message`] plus where it lives, the
/// owner's optional note, and when they saved it. The bookmarks-equivalent of
/// `PinnedMessage` (which this mirrors), but per-user and cross-room.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedMessage {
    /// The room the saved message belongs to (carried for access checks + UI).
    pub room_id: RoomId,
    /// The saved message (joined in on listing).
    pub message: Message,
    /// Optional free-text note the saver attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub saved_at: OffsetDateTime,
}

/// Largest number of saved items a single listing returns when the caller does
/// not specify (or over-specifies) a limit.
const DEFAULT_LIMIT: i64 = 100;
/// Hard ceiling on a saved-items page, keeping the cross-room scan bounded.
const MAX_LIMIT: i64 = 200;

#[derive(Clone)]
pub struct BookmarkRepo {
    pool: PgPool,
}

impl BookmarkRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Save a message for a user. Idempotent: re-saving an already-saved message
    /// keeps the original `created_at`/`note` (provenance). Returns `true` if a
    /// new bookmark was created. Caller has already checked the actor's access to
    /// the message's room.
    pub async fn save(
        &self,
        participant: ParticipantId,
        message: MessageId,
        room: RoomId,
        note: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"INSERT INTO bookmarks (participant_id, message_id, room_id, note, created_at)
               VALUES ($1, $2, $3, $4, now())
               ON CONFLICT (participant_id, message_id) DO NOTHING",
        )
        .bind(participant.to_uuid())
        .bind(message.to_uuid())
        .bind(room.to_uuid())
        .bind(note)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Remove a saved message. Returns `true` if a bookmark was removed.
    pub async fn unsave(
        &self,
        participant: ParticipantId,
        message: MessageId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"DELETE FROM bookmarks WHERE participant_id = $1 AND message_id = $2",
        )
        .bind(participant.to_uuid())
        .bind(message.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Whether a message is currently saved by a user.
    pub async fn is_saved(
        &self,
        participant: ParticipantId,
        message: MessageId,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (i64,)>(
            r"SELECT COUNT(*) FROM bookmarks WHERE participant_id = $1 AND message_id = $2",
        )
        .bind(participant.to_uuid())
        .bind(message.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0 > 0)
    }

    /// List a user's saved items, newest-saved first, each joined to its
    /// (non-deleted) message. Saved rows whose message has since been
    /// soft-deleted are skipped. Crosses rooms. `limit` is clamped into
    /// `[1, MAX_LIMIT]`, defaulting to [`DEFAULT_LIMIT`] when `None`.
    pub async fn list(
        &self,
        participant: ParticipantId,
        limit: Option<i64>,
    ) -> Result<Vec<SavedMessage>, sqlx::Error> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let rows = sqlx::query_as::<_, SavedRow>(
            r"SELECT
                 b.room_id, b.note, b.created_at AS saved_at,
                 m.id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                 m.created_at, m.edited_at, m.deleted_at, m.expires_at, m.version
               FROM bookmarks b
               JOIN messages m ON m.id = b.message_id
               WHERE b.participant_id = $1 AND m.deleted_at IS NULL
               ORDER BY b.created_at DESC
               LIMIT $2",
        )
        .bind(participant.to_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(SavedMessage::from).collect())
    }

    /// List a user's saved items, optionally restricted to one collection
    /// (folder). `collection = Some(id)` returns only items filed in that
    /// collection; `collection = None` returns the WHOLE saved list (every item,
    /// foldered or not) — the same set as [`list`](Self::list). Same join-out of
    /// soft-deleted messages, same ordering and `limit` clamping as `list`.
    /// Always owner-scoped.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_in_collection(
        &self,
        participant: ParticipantId,
        collection: Option<BookmarkCollectionId>,
        limit: Option<i64>,
    ) -> Result<Vec<SavedMessage>, sqlx::Error> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let rows = sqlx::query_as::<_, SavedRow>(
            r"SELECT
                 b.room_id, b.note, b.created_at AS saved_at,
                 m.id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                 m.created_at, m.edited_at, m.deleted_at, m.expires_at, m.version
               FROM bookmarks b
               JOIN messages m ON m.id = b.message_id
               WHERE b.participant_id = $1
                 AND m.deleted_at IS NULL
                 AND ($2::uuid IS NULL OR b.collection_id = $2)
               ORDER BY b.created_at DESC
               LIMIT $3",
        )
        .bind(participant.to_uuid())
        .bind(collection.map(|c| c.to_uuid()))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(SavedMessage::from).collect())
    }
}

#[derive(sqlx::FromRow)]
struct SavedRow {
    room_id: uuid::Uuid,
    note: Option<String>,
    saved_at: time::OffsetDateTime,
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

impl From<SavedRow> for SavedMessage {
    fn from(r: SavedRow) -> Self {
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
            note: r.note,
            saved_at: r.saved_at,
        }
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored bookmark_
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

    /// A self-contained (participant, room, message) triple. `deleted` soft-deletes
    /// the message so the listing-exclusion case can be exercised.
    async fn fixture(p: &PgPool, deleted: bool) -> (RoomId, MessageId, ParticipantId) {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(actor.to_uuid())
            .bind(format!("bm-actor-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");
        let room = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
             VALUES ($1,'channel',$2,$3, now(), '00000000-0000-0000-0000-000000000000')",
        )
        .bind(room.to_uuid())
        .bind("bm-room")
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
        if deleted {
            // Soft-delete so the listing-exclusion path can be exercised.
            sqlx::query("UPDATE messages SET deleted_at = now() WHERE id = $1")
                .bind(message.to_uuid())
                .execute(p)
                .await
                .expect("soft-delete message");
        }
        (room, message, actor)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn bookmark_save_then_list_then_unsave_roundtrip() {
        let p = pool();
        let repo = BookmarkRepo::new(p.clone());
        let (room, message, actor) = fixture(&p, false).await;

        assert!(
            repo.save(actor, message, room, Some("read later")).await.unwrap(),
            "first save created"
        );
        assert!(
            !repo.save(actor, message, room, Some("ignored")).await.unwrap(),
            "re-save is idempotent"
        );
        assert!(repo.is_saved(actor, message).await.unwrap());

        let saved = repo.list(actor, None).await.unwrap();
        assert_eq!(saved.len(), 1, "one saved item");
        assert_eq!(saved[0].message.id, message);
        assert_eq!(saved[0].room_id, room);
        assert_eq!(saved[0].note.as_deref(), Some("read later"), "original note kept");
        assert!(!saved[0].message.blocks.is_empty(), "joined message content present");

        assert!(repo.unsave(actor, message).await.unwrap(), "unsave removed it");
        assert!(!repo.is_saved(actor, message).await.unwrap());
        assert!(repo.list(actor, None).await.unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn bookmark_list_excludes_soft_deleted_message() {
        let p = pool();
        let repo = BookmarkRepo::new(p.clone());
        let (room, message, actor) = fixture(&p, true).await;

        // The row saves fine (FK still resolves to the soft-deleted message)...
        assert!(repo.save(actor, message, room, None).await.unwrap(), "save created");
        assert!(repo.is_saved(actor, message).await.unwrap(), "row exists");

        // ...but the listing JOINs out soft-deleted messages.
        assert!(
            repo.list(actor, None).await.unwrap().is_empty(),
            "soft-deleted message excluded from saved list"
        );
    }
}
