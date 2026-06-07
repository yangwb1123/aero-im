//! Message edit-history repository (prior versions of an edited message).
//!
//! Backs `migrations/0036_message_edits.sql`. Each time a message is edited, the
//! OLD block content is captured here BEFORE the live `messages` row is
//! overwritten, so members can review what a message previously said. One row
//! per prior version, surfaced newest-first.
//!
//! Purely additive: a NEW [`MessageEditRepo`]; no existing repo is touched. The
//! capture-on-edit hook ([`MessageEditRepo::record`]) is the seam — it is called
//! from the edit path (`ImService::edit_message`) just before the overwrite,
//! capturing the OLD blocks. The [`MessageEdit`] model lives here (and is
//! re-exported from the crate root) rather than in `aero-common`, since it is a
//! storage-layer projection — mirroring [`SavedSearch`](crate::SavedSearch).
//!
//! Read access is membership-gated by the caller: [`MessageEditRepo::message_room`]
//! resolves the edited message's room so the read route can assert room access
//! before returning any history.

use aero_common::{MessageEditId, MessageId, ParticipantId, RoomId};
use serde::Serialize;
use sqlx::PgPool;

/// One captured prior version of an edited message.
///
/// A storage-layer projection of a `message_edits` row. `Serialize` so a handler
/// can hand the row straight back as JSON; `recorded_at` renders as RFC 3339 and
/// `blocks` carries the verbatim block array the message held before this edit.
#[derive(Debug, Clone, Serialize)]
pub struct MessageEdit {
    /// The edit record's unique id.
    pub id: MessageEditId,
    /// The message this prior version belongs to.
    pub message_id: MessageId,
    /// The participant who performed the edit that retired this version.
    pub editor_id: ParticipantId,
    /// The message's block content as it stood before the edit (a JSON array).
    pub blocks: serde_json::Value,
    /// When this version was captured (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub recorded_at: time::OffsetDateTime,
}

/// The columns a [`MessageEdit`] is built from, in select order. Shared by every
/// query so the row decoding stays in one place.
const COLUMNS: &str = "id, message_id, editor_id, blocks, recorded_at";

type Row = (
    uuid::Uuid,
    uuid::Uuid,
    uuid::Uuid,
    serde_json::Value,
    time::OffsetDateTime,
);

fn row_to_model(r: Row) -> MessageEdit {
    let (id, message_id, editor_id, blocks, recorded_at) = r;
    MessageEdit {
        id: MessageEditId::from_uuid(id),
        message_id: MessageId::from_uuid(message_id),
        editor_id: ParticipantId::from_uuid(editor_id),
        blocks,
        recorded_at,
    }
}

/// Repository over the `message_edits` table (per-message edit history).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`MessageEditRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct MessageEditRepo {
    pool: PgPool,
}

impl MessageEditRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Capture a prior version of `message` — the `blocks` it held before an edit
    /// performed by `editor` — returning the generated id. The caller records the
    /// OLD blocks just before overwriting the live message row; this repo never
    /// inspects or validates the block contents.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn record(
        &self,
        message: MessageId,
        editor: ParticipantId,
        blocks: &serde_json::Value,
    ) -> Result<MessageEditId, sqlx::Error> {
        let id = MessageEditId::new();
        sqlx::query(
            r"INSERT INTO message_edits (id, message_id, editor_id, blocks)
               VALUES ($1, $2, $3, $4)",
        )
        .bind(id.to_uuid())
        .bind(message.to_uuid())
        .bind(editor.to_uuid())
        .bind(blocks)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List every captured prior version of `message`, newest first. The caller
    /// is responsible for asserting the requester may read the message's room
    /// (see [`MessageEditRepo::message_room`]).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_message(
        &self,
        message: MessageId,
    ) -> Result<Vec<MessageEdit>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM message_edits
              WHERE message_id = $1
              ORDER BY recorded_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(message.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Resolve the room a message belongs to, or `None` if no such message exists.
    /// Used by the read route to membership-gate edit history: the caller asserts
    /// access to the returned room before surfacing any prior versions.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn message_room(
        &self,
        message: MessageId,
    ) -> Result<Option<RoomId>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid,)>("SELECT room_id FROM messages WHERE id = $1")
            .bind(message.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(room_id,)| RoomId::from_uuid(room_id)))
    }
}

/// Hermetic unit tests for the pure row→model mapping (no database needed).
#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn row_to_model_maps_every_field() {
        let id = MessageEditId::new();
        let message = MessageId::new();
        let editor = ParticipantId::new();
        let blocks = serde_json::json!([{ "type": "text", "text": "hello" }]);
        let recorded_at = time::OffsetDateTime::now_utc();

        let model = row_to_model((
            id.to_uuid(),
            message.to_uuid(),
            editor.to_uuid(),
            blocks.clone(),
            recorded_at,
        ));

        assert_eq!(model.id, id);
        assert_eq!(model.message_id, message);
        assert_eq!(model.editor_id, editor);
        assert_eq!(model.blocks, blocks);
        assert_eq!(model.recorded_at, recorded_at);
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored message_edit
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    /// The reserved all-zero default workspace, guaranteed to exist by migration
    /// 0006's backfill — reused so the seeded room is well-scoped.
    const DEFAULT_WS: &str = "00000000-0000-0000-0000-000000000000";

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway participant so the test is self-contained.
    async fn participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("message-edit-actor-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    /// Seed a minimal room + message owned by `owner`, returning the message id and
    /// its room id, so `record`/`list_for_message`/`message_room` are exercised
    /// end-to-end against real rows.
    async fn seed_message(p: &PgPool, owner: ParticipantId) -> (MessageId, RoomId) {
        let room = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id)
               VALUES ($1, 'group', $2, $3, $4)",
        )
        .bind(room.to_uuid())
        .bind(format!("edit-history-room-{room}"))
        .bind(owner.to_uuid())
        .bind(uuid::Uuid::parse_str(DEFAULT_WS).expect("valid uuid"))
        .execute(p)
        .await
        .expect("insert room");

        let message = MessageId::new();
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks)
               VALUES ($1, $2, $3, '[]'::jsonb)",
        )
        .bind(message.to_uuid())
        .bind(room.to_uuid())
        .bind(owner.to_uuid())
        .execute(p)
        .await
        .expect("insert message");
        (message, room)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn message_edit_record_list_and_resolve_room() {
        let p = pool();
        let repo = MessageEditRepo::new(p.clone());
        let owner = participant(&p).await;
        let (message, room) = seed_message(&p, owner).await;

        // message_room resolves the seeded message's room; an unknown id is None.
        let resolved = repo.message_room(message).await.unwrap();
        assert_eq!(resolved, Some(room), "message_room resolves the seeded room");
        assert!(
            repo.message_room(MessageId::new()).await.unwrap().is_none(),
            "unknown message resolves to None"
        );

        // No edits yet.
        assert!(
            repo.list_for_message(message).await.unwrap().is_empty(),
            "a message with no edits has empty history"
        );

        // Record two prior versions; the second is recorded later.
        let v1 = serde_json::json!([{ "type": "text", "text": "first" }]);
        let v2 = serde_json::json!([{ "type": "text", "text": "second" }]);
        let first = repo.record(message, owner, &v1).await.unwrap();
        let second = repo.record(message, owner, &v2).await.unwrap();

        // Both surface, newest first.
        let history = repo.list_for_message(message).await.unwrap();
        assert_eq!(history.len(), 2, "both prior versions are listed");
        assert_eq!(history[0].id, second, "newest edit sorts first");
        assert_eq!(history[1].id, first, "oldest edit sorts last");
        assert_eq!(history[0].blocks, v2);
        assert_eq!(history[1].blocks, v1);
        assert_eq!(history[0].message_id, message);
        assert_eq!(history[0].editor_id, owner);

        // Cleanup so reruns stay self-contained (FK cascade clears edits/messages).
        sqlx::query("DELETE FROM message_edits WHERE message_id = $1")
            .bind(message.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(message.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
