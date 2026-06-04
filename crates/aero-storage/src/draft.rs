//! Per-room composer-draft repository (server-persisted message drafts).
//!
//! Backs `migrations/0028_drafts.sql`. A user's in-progress composer message for
//! a room is saved server-side so it follows them across devices/reloads (Slack
//! drafts). A draft is PRIVATE to the authoring participant, and there is exactly
//! one per `(participant, room)`: [`upsert`](DraftRepo::upsert) replaces the
//! previous draft for that pair. Saving a draft sends nothing — it only stages
//! the composer's [`Block`]s and an optional reply target.
//!
//! Purely additive: a NEW [`DraftRepo`]; no existing repo is touched. The
//! [`Draft`] model lives here (and is re-exported from the crate root) rather
//! than in `aero-common`, since it is a storage-layer projection — mirroring
//! [`ScheduledMessage`](crate::ScheduledMessage). Blocks are stored as JSONB via
//! `sqlx::types::Json`, the same way the scheduled repo persists its blocks.

use aero_common::{Block, MessageId, ParticipantId, RoomId};
use serde::Serialize;
use sqlx::PgPool;

/// One participant's saved composer draft for a single room. Serialized back to
/// the client so it can rehydrate the composer (blocks + optional reply target)
/// on reload. The owning participant is implicit (the draft is always fetched
/// scoped to the caller) and so is not carried on the wire.
#[derive(Debug, Clone, Serialize)]
pub struct Draft {
    /// The room this draft composes a message for.
    pub room_id: RoomId,
    /// The staged composer content (rehydrates the editor verbatim).
    pub blocks: Vec<Block>,
    /// Optional message this draft is a threaded/inline reply to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<MessageId>,
    /// When the draft was last upserted (most-recent-edit ordering key).
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: time::OffsetDateTime,
}

/// The columns a [`Draft`] is built from, in select order. Shared by every query
/// so the row decoding stays in one place.
const COLUMNS: &str = "room_id, blocks, reply_to, updated_at";

type Row = (
    uuid::Uuid,
    serde_json::Value,
    Option<uuid::Uuid>,
    time::OffsetDateTime,
);

fn row_to_model(r: Row) -> Draft {
    let (room_id, blocks, reply_to, updated_at) = r;
    Draft {
        room_id: RoomId::from_uuid(room_id),
        blocks: serde_json::from_value(blocks).unwrap_or_default(),
        reply_to: reply_to.map(MessageId::from_uuid),
        updated_at,
    }
}

/// Per-room composer-draft store. Cheap to clone (wraps an `Arc<PgPool>`), so
/// feature modules construct one inline rather than threading it through state.
#[derive(Clone)]
pub struct DraftRepo {
    pool: PgPool,
}

impl DraftRepo {
    /// Build a draft repository over the shared connection pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Save (or replace) a participant's draft for a room. One row per
    /// `(participant, room)`: an existing draft is overwritten in place and its
    /// `updated_at` bumped to now. The caller is responsible for asserting the
    /// participant's access to the room before staging a draft for it.
    ///
    /// # Errors
    /// Propagates any `sqlx` error (including a JSON-encode failure for `blocks`).
    pub async fn upsert(
        &self,
        participant: ParticipantId,
        room: RoomId,
        blocks: &[Block],
        reply_to: Option<MessageId>,
    ) -> Result<(), sqlx::Error> {
        let blocks_json =
            serde_json::to_value(blocks).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        sqlx::query(
            r"INSERT INTO message_drafts (participant_id, room_id, blocks, reply_to, updated_at)
               VALUES ($1, $2, $3, $4, now())
               ON CONFLICT (participant_id, room_id) DO UPDATE
                 SET blocks = EXCLUDED.blocks,
                     reply_to = EXCLUDED.reply_to,
                     updated_at = now()",
        )
        .bind(participant.to_uuid())
        .bind(room.to_uuid())
        .bind(sqlx::types::Json(blocks_json))
        .bind(reply_to.map(|m| m.to_uuid()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch a participant's draft for a single room, if one is saved. Always
    /// scoped to `participant`, so a user can never read another's draft.
    ///
    /// # Errors
    /// Propagates any `sqlx` error.
    pub async fn get(
        &self,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<Option<Draft>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM message_drafts
              WHERE participant_id = $1 AND room_id = $2"
        );
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(participant.to_uuid())
            .bind(room.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// List all of a participant's drafts across rooms, most-recently-edited
    /// first. Always scoped to `participant` (drafts are private).
    ///
    /// # Errors
    /// Propagates any `sqlx` error.
    pub async fn list_for(&self, participant: ParticipantId) -> Result<Vec<Draft>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM message_drafts
              WHERE participant_id = $1
              ORDER BY updated_at DESC, room_id ASC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(participant.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Discard a participant's draft for a room (e.g. after sending or clearing
    /// the composer). Returns `true` iff a draft was removed. Sender-scoped, so a
    /// user can never delete another's draft.
    ///
    /// # Errors
    /// Propagates any `sqlx` error.
    pub async fn delete(
        &self,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<bool, sqlx::Error> {
        let result =
            sqlx::query("DELETE FROM message_drafts WHERE participant_id = $1 AND room_id = $2")
                .bind(participant.to_uuid())
                .bind(room.to_uuid())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored draft
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{Block, RoomKind};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    // Create a throwaway participant + room so the test is self-contained.
    async fn fixture(p: &PgPool) -> (RoomId, ParticipantId) {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(actor.to_uuid())
            .bind(format!("draft-actor-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");
        let room = RoomId::new();
        // `rooms.workspace_id` is NOT NULL (migration 0006); reuse the reserved
        // all-zero default workspace, guaranteed to exist by that migration's
        // backfill, so the fixture row satisfies the FK + NOT NULL constraint.
        sqlx::query(
            "INSERT INTO rooms (id, kind, created_by, workspace_id)
             VALUES ($1, $2, $3, '00000000-0000-0000-0000-000000000000'::uuid)",
        )
        .bind(room.to_uuid())
        .bind(format!("{:?}", RoomKind::Group).to_lowercase())
        .bind(actor.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        (room, actor)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn draft_upsert_get_replace_list_delete() {
        let p = pool();
        let repo = DraftRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;

        // No draft initially.
        assert!(repo.get(actor, room).await.unwrap().is_none(), "no draft to start");

        // First upsert: get returns it.
        let first = vec![Block::text("draft v1")];
        repo.upsert(actor, room, &first, None).await.unwrap();
        let got = repo.get(actor, room).await.unwrap().expect("draft present after upsert");
        assert_eq!(got.room_id, room);
        assert_eq!(got.blocks.len(), 1, "first draft has one block");

        // Second upsert REPLACES (still one row, new blocks + reply target).
        let reply = MessageId::new();
        let second = vec![Block::text("draft v2"), Block::text("more")];
        repo.upsert(actor, room, &second, Some(reply)).await.unwrap();
        let got = repo.get(actor, room).await.unwrap().expect("draft still present");
        assert_eq!(got.blocks.len(), 2, "replaced draft has the new blocks");
        assert_eq!(got.reply_to, Some(reply), "reply target persisted");

        // list_for returns exactly the one (upserted) draft.
        let all = repo.list_for(actor).await.unwrap();
        assert_eq!(all.len(), 1, "upsert keeps a single row per (participant, room)");
        assert_eq!(all[0].room_id, room);
        assert_eq!(all[0].blocks.len(), 2, "listed draft reflects the replacement");

        // A different participant sees none of it (drafts are private).
        let other = ParticipantId::new();
        assert!(repo.list_for(other).await.unwrap().is_empty(), "drafts are per-user");
        assert!(repo.get(other, room).await.unwrap().is_none(), "other user has no draft here");

        // Delete removes it; a second delete is a no-op.
        assert!(repo.delete(actor, room).await.unwrap(), "delete removed the draft");
        assert!(!repo.delete(actor, room).await.unwrap(), "second delete returns false");
        assert!(repo.get(actor, room).await.unwrap().is_none(), "draft gone after delete");
        assert!(repo.list_for(actor).await.unwrap().is_empty(), "listing empty after delete");
    }
}
