//! Per-room composer-draft repository (server-persisted message drafts).
//!
//! Backs `migrations/0028_drafts.sql`. A user's in-progress composer message for
//! a room is saved server-side so it follows them across devices/reloads (Slack
//! drafts). A draft is PRIVATE to the authoring participant, and there is exactly
//! one per `(participant, room)`:
//! [`upsert_authorized`](DraftRepo::upsert_authorized) replaces the previous
//! draft for that pair. Saving a draft sends nothing — it only stages the
//! composer's [`Block`]s and an optional reply target.
//!
//! Purely additive: a NEW [`DraftRepo`]; no existing repo is touched. The
//! [`Draft`] model lives here (and is re-exported from the crate root) rather
//! than in `aero-common`, since it is a storage-layer projection — mirroring
//! [`ScheduledMessage`](crate::ScheduledMessage). Blocks are stored as JSONB via
//! `sqlx::types::Json`, the same way the scheduled repo persists its blocks.

use aero_common::{Block, Error, MessageId, ParticipantId, RoomId};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};

#[cfg(test)]
use crate::MessageRepo;

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

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    room_id: uuid::Uuid,
    blocks: serde_json::Value,
    reply_to: Option<uuid::Uuid>,
    updated_at: time::OffsetDateTime,
}

fn row_to_model(r: Row) -> Draft {
    Draft {
        room_id: RoomId::from_uuid(r.room_id),
        blocks: serde_json::from_value(r.blocks).unwrap_or_default(),
        reply_to: r.reply_to.map(MessageId::from_uuid),
        updated_at: r.updated_at,
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
    #[cfg(test)]
    pub(crate) async fn upsert(
        &self,
        participant: ParticipantId,
        room: RoomId,
        blocks: &[Block],
        reply_to: Option<MessageId>,
    ) -> Result<(), sqlx::Error> {
        let blocks_json =
            serde_json::to_value(blocks).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let mut tx = self.pool.begin().await?;
        if !MessageRepo::lock_reply_parent_in_tx(&mut tx, reply_to, room).await? {
            return Err(sqlx::Error::Protocol(
                "reply_to must reference an existing message in the same room".into(),
            ));
        }
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
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Fetch a participant's draft for a single room, if one is saved. Always
    /// scoped to `participant`, so a user can never read another's draft.
    ///
    /// # Errors
    /// Propagates any `sqlx` error.
    #[cfg(test)]
    pub(crate) async fn get(
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
    #[cfg(test)]
    pub(crate) async fn list_for(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<Draft>, sqlx::Error> {
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
    #[cfg(test)]
    pub(crate) async fn delete(
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

    /// Save a draft after acquiring a current effective-access fence in the same
    /// transaction. A reply target must still be live and in `room` when the
    /// upsert commits.
    ///
    /// # Errors
    /// Returns an opaque not-found/forbidden error for inaccessible rooms,
    /// [`Error::Invalid`] for an invalid reply target, or a database error.
    pub async fn upsert_authorized(
        &self,
        participant: ParticipantId,
        room: RoomId,
        blocks: &[Block],
        reply_to: Option<MessageId>,
    ) -> Result<(), Error> {
        let blocks_json = serde_json::to_value(blocks)?;
        let mut tx = self.pool.begin().await?;
        lock_effective_room_access_in_tx(&mut tx, participant, room).await?;
        if let Some(parent) = reply_to {
            let valid = sqlx::query_scalar::<_, bool>(
                r"SELECT true
                    FROM messages
                   WHERE id = $1
                     AND room_id = $2
                     AND deleted_at IS NULL
                   FOR SHARE",
            )
            .bind(parent.to_uuid())
            .bind(room.to_uuid())
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or(false);
            if !valid {
                return Err(Error::Invalid(
                    "reply target is not a live message in this room".into(),
                ));
            }
        }
        sqlx::query(
            r"INSERT INTO message_drafts
                  (participant_id, room_id, blocks, reply_to, updated_at)
               VALUES ($1, $2, $3, $4, now())
               ON CONFLICT (participant_id, room_id) DO UPDATE
                 SET blocks = EXCLUDED.blocks,
                     reply_to = EXCLUDED.reply_to,
                     updated_at = now()",
        )
        .bind(participant.to_uuid())
        .bind(room.to_uuid())
        .bind(sqlx::types::Json(blocks_json))
        .bind(reply_to.map(|message| message.to_uuid()))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Fetch the caller's draft only after fencing their current room access.
    ///
    /// # Errors
    /// Returns an opaque access error or a database error.
    pub async fn get_authorized(
        &self,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<Option<Draft>, Error> {
        let mut tx = self.pool.begin().await?;
        lock_effective_room_access_in_tx(&mut tx, participant, room).await?;
        let sql = format!(
            "SELECT {COLUMNS}
               FROM message_drafts
              WHERE participant_id = $1 AND room_id = $2"
        );
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(participant.to_uuid())
            .bind(room.to_uuid())
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(row.map(row_to_model))
    }

    /// List only drafts whose rooms the caller can currently access.
    ///
    /// # Errors
    /// Propagates database errors.
    pub async fn list_accessible(&self, participant: ParticipantId) -> Result<Vec<Draft>, Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM message_drafts
              WHERE participant_id = $1
                AND aero_effective_room_access(room_id, $1, NULL)
              ORDER BY updated_at DESC, room_id ASC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(participant.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Delete the caller's draft after fencing current effective room access.
    ///
    /// # Errors
    /// Returns an opaque access error or a database error.
    pub async fn delete_authorized(
        &self,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<bool, Error> {
        let mut tx = self.pool.begin().await?;
        lock_effective_room_access_in_tx(&mut tx, participant, room).await?;
        let result =
            sqlx::query("DELETE FROM message_drafts WHERE participant_id = $1 AND room_id = $2")
                .bind(participant.to_uuid())
                .bind(room.to_uuid())
                .execute(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }
}

/// Acquire the canonical workspace -> room -> membership access fence. This is
/// shared by private room-state repositories so authorization and mutation live
/// in one transaction.
pub(crate) async fn lock_effective_room_access_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    participant: ParticipantId,
    room: RoomId,
) -> Result<(), Error> {
    let allowed: bool = sqlx::query_scalar("SELECT aero_effective_room_access($1, $2, NULL)")
        .bind(room.to_uuid())
        .bind(participant.to_uuid())
        .fetch_one(&mut **tx)
        .await?;
    if allowed {
        return Ok(());
    }
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM rooms WHERE id = $1)")
        .bind(room.to_uuid())
        .fetch_one(&mut **tx)
        .await?;
    if exists {
        Err(Error::Forbidden("room access required".into()))
    } else {
        Err(Error::NotFound("room".into()))
    }
}

#[cfg(test)]
#[path = "personal_state_security_tests.rs"]
mod personal_state_security_tests;

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored draft
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{Block, WorkspaceId};

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
        let workspace = WorkspaceId::new();
        let room = RoomId::new();
        let mut tx = p.begin().await.expect("begin draft fixture");
        sqlx::query(
            "INSERT INTO workspaces (id, name, slug, created_by)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(workspace.to_uuid())
        .bind(format!("draft-workspace-{workspace}"))
        .bind(format!("draft-{workspace}"))
        .bind(actor.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert draft workspace");
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1, $2, 'owner')",
        )
        .bind(workspace.to_uuid())
        .bind(actor.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert draft workspace owner");
        sqlx::query(
            "INSERT INTO rooms (id, kind, created_by, workspace_id)
             VALUES ($1, 'group', $2, $3)",
        )
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .bind(workspace.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert room");
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'member')",
        )
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert draft room member");
        tx.commit().await.expect("commit draft fixture");
        (room, actor)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn draft_upsert_get_replace_list_delete() {
        let p = pool();
        let repo = DraftRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;

        // No draft initially.
        assert!(
            repo.get(actor, room).await.unwrap().is_none(),
            "no draft to start"
        );

        // First upsert: get returns it.
        let first = vec![Block::text("draft v1")];
        repo.upsert(actor, room, &first, None).await.unwrap();
        let got = repo
            .get(actor, room)
            .await
            .unwrap()
            .expect("draft present after upsert");
        assert_eq!(got.room_id, room);
        assert_eq!(got.blocks.len(), 1, "first draft has one block");

        // Second upsert REPLACES (still one row, new blocks + reply target).
        let reply = MessageId::new();
        sqlx::query(
            "INSERT INTO messages
                 (id,room_id,sender_id,blocks,searchable_text)
             VALUES ($1,$2,$3,'[]'::jsonb,'thread root')",
        )
        .bind(reply.to_uuid())
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .execute(&p)
        .await
        .expect("same-room reply parent");
        let second = vec![Block::text("draft v2"), Block::text("more")];
        repo.upsert(actor, room, &second, Some(reply))
            .await
            .unwrap();
        let got = repo
            .get(actor, room)
            .await
            .unwrap()
            .expect("draft still present");
        assert_eq!(got.blocks.len(), 2, "replaced draft has the new blocks");
        assert_eq!(got.reply_to, Some(reply), "reply target persisted");

        // list_for returns exactly the one (upserted) draft.
        let all = repo.list_for(actor).await.unwrap();
        assert_eq!(
            all.len(),
            1,
            "upsert keeps a single row per (participant, room)"
        );
        assert_eq!(all[0].room_id, room);
        assert_eq!(
            all[0].blocks.len(),
            2,
            "listed draft reflects the replacement"
        );

        // A different participant sees none of it (drafts are private).
        let other = ParticipantId::new();
        assert!(
            repo.list_for(other).await.unwrap().is_empty(),
            "drafts are per-user"
        );
        assert!(
            repo.get(other, room).await.unwrap().is_none(),
            "other user has no draft here"
        );

        // Delete removes it; a second delete is a no-op.
        assert!(
            repo.delete(actor, room).await.unwrap(),
            "delete removed the draft"
        );
        assert!(
            !repo.delete(actor, room).await.unwrap(),
            "second delete returns false"
        );
        assert!(
            repo.get(actor, room).await.unwrap().is_none(),
            "draft gone after delete"
        );
        assert!(
            repo.list_for(actor).await.unwrap().is_empty(),
            "listing empty after delete"
        );
    }
}
