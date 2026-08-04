//! Core CRUD operations for messages: insert, get, edit, soft-delete (various
//! flavours), voice-transcript patch, embedding update.
//!
//! Extracted from `message.rs` as part of REFACTOR_PLAN.md Step 2.

use aero_common::{Block, Message, MessageId, ParticipantId, WorkspaceId};
use pgvector::Vector;
use sqlx::{Postgres, Transaction};

use super::{MessageRepo, NewMessage};
use crate::message::orig::{attached_blob_ids, searchable_of, MessageRow};

/// Message-scoped rows that only make sense while the message is visible.
///
/// `message_edits` and `message_reports` are deliberately absent: they are
/// audit/moderation evidence and must survive a soft delete. Room-level
/// `read_receipts` and `delivery_cursors` are monotonic cursors rather than
/// message-owned projections, so deleting their current cursor target would
/// incorrectly make already-seen content unread again.
const VISIBLE_ASSOCIATION_DELETE_QUERIES: &[&str] = &[
    "DELETE FROM reactions WHERE message_id = ANY($1)",
    "DELETE FROM message_receipts WHERE message_id = ANY($1)",
    "DELETE FROM pins WHERE message_id = ANY($1)",
    "DELETE FROM bookmarks WHERE message_id = ANY($1)",
    "DELETE FROM notifications WHERE message_id = ANY($1)",
    "DELETE FROM notification_bundles WHERE message_id = ANY($1)",
    "DELETE FROM block_interactions WHERE message_id = ANY($1)",
];

impl MessageRepo {
    pub async fn insert(&self, new: NewMessage) -> Result<Message, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        if !Self::lock_reply_parent_in_tx(&mut tx, new.reply_to, new.room_id).await? {
            return Err(sqlx::Error::Protocol(
                "reply_to must reference an existing message in the same room".into(),
            ));
        }
        let message = Self::insert_row_in_tx(&mut tx, new).await?;
        tx.commit().await?;
        Ok(message)
    }

    /// Validate an optional reply parent under the message write transaction.
    ///
    /// `FOR KEY SHARE` composes with migration 0190's composite foreign key:
    /// a concurrent hard delete cannot open a check/insert race.
    pub(crate) async fn lock_reply_parent_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        reply_to: Option<MessageId>,
        room: aero_common::RoomId,
    ) -> Result<bool, sqlx::Error> {
        let Some(reply_to) = reply_to else {
            return Ok(true);
        };
        Ok(sqlx::query_scalar::<_, bool>(
            r"SELECT true
                FROM messages
               WHERE id = $1 AND room_id = $2
               FOR KEY SHARE",
        )
        .bind(reply_to.to_uuid())
        .bind(room.to_uuid())
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or(false))
    }

    pub(crate) async fn insert_row_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        new: NewMessage,
    ) -> Result<Message, sqlx::Error> {
        let id = MessageId::new();
        let created_at = time::OffsetDateTime::now_utc();
        let blocks_json =
            serde_json::to_value(&new.blocks).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let searchable = searchable_of(&new.blocks);

        sqlx::query(
            r#"INSERT INTO messages
                 (id, room_id, sender_id, blocks, reply_to, metadata, searchable_text, created_at, expires_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"#,
        )
        .bind(id.to_uuid())
        .bind(new.room_id.to_uuid())
        .bind(new.sender_id.to_uuid())
        .bind(&blocks_json)
        .bind(new.reply_to.map(|m| m.to_uuid()))
        .bind(&new.metadata)
        .bind(&searchable)
        .bind(created_at)
        .bind(new.expires_at)
        .execute(&mut **tx)
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
            expires_at: new.expires_at,
            version: 1,
        })
    }

    /// Cheap preflight for a user-facing validation error.
    ///
    /// Every repository write repeats this check under `FOR KEY SHARE`; the
    /// composite FK is the final race-proof fence.
    pub async fn reply_parent_exists_in_room(
        &self,
        reply_to: MessageId,
        room: aero_common::RoomId,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE id = $1 AND room_id = $2)",
        )
        .bind(reply_to.to_uuid())
        .bind(room.to_uuid())
        .fetch_one(&self.pool)
        .await
    }

    /// Fetch a single message by id (including soft-deleted, caller must filter).
    pub async fn get(&self, id: MessageId) -> Result<Option<Message>, sqlx::Error> {
        let row = sqlx::query_as::<_, MessageRow>(
            r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at, version
               FROM messages WHERE id = $1"#,
        )
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Message::from))
    }

    /// Whether a message already has a stored embedding.
    ///
    /// A single-row presence read (the `Message` struct deliberately omits the
    /// heavy `embedding` column, so this is the cheap way to ask). Used by the
    /// AI worker to skip re-embedding — and re-paying for — a message that is
    /// already embedded. A missing id returns `false` (nothing embedded),
    /// mirroring `get`'s "missing ⇒ `None`/absent" convention; callers that
    /// need to distinguish missing from un-embedded use `get` separately.
    pub async fn has_embedding(&self, id: MessageId) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (bool,)>(
            r#"SELECT embedding IS NOT NULL FROM messages WHERE id = $1"#,
        )
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map_or(false, |(present,)| present))
    }

    /// Update message blocks with optimistic locking (migration 0157).
    /// Caller has already checked authorization.
    /// `expected_version` is the version the caller read — the UPDATE atomically
    /// increments it. Returns `Err(Conflict)` when another writer got there first.
    /// Returns `Ok(None)` when the row is missing/soft-deleted.
    pub async fn edit(
        &self,
        id: MessageId,
        blocks: Vec<Block>,
        expected_version: i32,
    ) -> Result<Option<Message>, aero_common::Error> {
        let blocks_json = serde_json::to_value(&blocks)?;
        let searchable = searchable_of(&blocks);
        let edited_at = time::OffsetDateTime::now_utc();

        let row = sqlx::query_as::<_, MessageRow>(
            r#"UPDATE messages
                  SET blocks = $1, searchable_text = $2, edited_at = $3, embedding = NULL,
                      version = version + 1
               WHERE id = $4 AND deleted_at IS NULL AND version = $5
            RETURNING id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at, version"#,
        )
        .bind(&blocks_json)
        .bind(&searchable)
        .bind(edited_at)
        .bind(id.to_uuid())
        .bind(expected_version)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(r) => Ok(Some(r.into())),
            // Two possibilities: message is missing/deleted, or version mismatch.
            // Check existence to distinguish.
            None => {
                let exists = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS(SELECT 1 FROM messages WHERE id = $1 AND deleted_at IS NULL)",
                )
                .bind(id.to_uuid())
                .fetch_one(&self.pool)
                .await?;
                if exists {
                    Err(aero_common::Error::Conflict(
                        "message was edited concurrently; reload and retry".into(),
                    ))
                } else {
                    Ok(None)
                }
            }
        }
    }

    /// Read the current version of a non-deleted message. Returns `None` when
    /// the message is missing or soft-deleted. Used by callers to obtain the
    /// expected_version for [`edit`](Self::edit).
    pub async fn get_version(&self, id: MessageId) -> Result<Option<i32>, sqlx::Error> {
        sqlx::query_scalar::<_, i32>(
            "SELECT version FROM messages WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await
    }

    /// Soft-delete a message. Caller has already checked authorization.
    ///
    /// GDPR 方向四: before clearing the blocks, any blob the message attached
    /// (`File` / `Voice` blocks) is enqueued into `blob_gc_queue` so the
    /// background GC job deletes its storage object — UNLESS another *live*
    /// message still references the same blob. The shared-blob case is real
    /// because owner-scoped SHA-256 dedup lets two messages point at one blob;
    /// the reference check (a containment scan over remaining live blocks) runs
    /// AFTER this row's blocks are cleared, so it never counts the row being
    /// deleted. All in one transaction.
    pub async fn soft_delete(&self, id: MessageId) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let deleted = Self::soft_delete_in_tx(&mut tx, id).await?;
        tx.commit().await?;
        Ok(deleted)
    }

    /// Transaction-scoped body of [`soft_delete`](Self::soft_delete): the
    /// delete + guarded blob-GC enqueue run on the caller's transaction, so a
    /// caller can compose further writes (e.g. an audit row) that commit or
    /// roll back together with the delete. Returns `false` (without writing
    /// anything) when the message is already deleted or missing.
    pub async fn soft_delete_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        id: MessageId,
    ) -> Result<bool, sqlx::Error> {
        // Read + lock the row; capture its blocks so we can extract blob ids.
        let row = sqlx::query_as::<_, (serde_json::Value,)>(
            "SELECT blocks FROM messages WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut **tx)
        .await?;
        let Some((blocks_json,)) = row else {
            return Ok(false);
        };
        let blob_ids = attached_blob_ids(&blocks_json);

        sqlx::query(
            r#"UPDATE messages
                  SET deleted_at = NOW(), blocks = '[]'::jsonb, searchable_text = '', embedding = NULL
               WHERE id = $1 AND deleted_at IS NULL"#,
        )
        .bind(id.to_uuid())
        .execute(&mut **tx)
        .await?;

        Self::cleanup_visible_associations_in_tx(tx, &[id.to_uuid()]).await?;

        Self::enqueue_unreferenced_blobs_in_tx(tx, &blob_ids).await?;

        Ok(true)
    }

    /// Remove message-owned, user-visible projections in the caller's
    /// transaction.
    ///
    /// This is shared by the single-message delete path and the set-based
    /// retention sweep. Keeping the cleanup transaction-scoped prevents pins,
    /// reactions, inbox rows, precise read receipts, saved items, or interactive
    /// payloads from surviving a committed tombstone.
    pub(crate) async fn cleanup_visible_associations_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        message_ids: &[uuid::Uuid],
    ) -> Result<(), sqlx::Error> {
        if message_ids.is_empty() {
            return Ok(());
        }
        for query in VISIBLE_ASSOCIATION_DELETE_QUERIES {
            sqlx::query(query)
                .bind(message_ids)
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
    }

    /// Enqueue attachment bytes that no currently visible message references.
    ///
    /// Shared by explicit deletion, workspace/channel retention, and ephemeral
    /// expiry. The exact JSONB containment predicate avoids substring matches;
    /// already-expired messages do not keep bytes alive while waiting for their
    /// eventual hard-delete sweep.
    pub(crate) async fn enqueue_unreferenced_blobs_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        blob_ids: &[aero_common::BlobId],
    ) -> Result<(), sqlx::Error> {
        let mut unique: Vec<_> = blob_ids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        unique.sort_unstable_by_key(|blob| blob.to_uuid());
        for blob in unique {
            let exists = sqlx::query_scalar::<_, uuid::Uuid>(
                "SELECT id FROM blobs WHERE id = $1 FOR UPDATE",
            )
            .bind(blob.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .is_some();
            if !exists {
                continue;
            }
            let reference = serde_json::json!([{ "blob_id": blob.to_string() }]);
            let still_referenced: bool = sqlx::query_scalar(
                r"SELECT EXISTS(
                     SELECT 1
                       FROM messages
                      WHERE deleted_at IS NULL
                        AND (expires_at IS NULL OR expires_at > now())
                        AND blocks @> $1
                   )",
            )
            .bind(reference)
            .fetch_one(&mut **tx)
            .await?;
            if !still_referenced {
                sqlx::query(
                    r"INSERT INTO blob_gc_queue (blob_id, force_delete)
                      VALUES ($1, FALSE)
                      ON CONFLICT (blob_id) DO NOTHING",
                )
                .bind(blob.to_uuid())
                .execute(&mut **tx)
                .await?;
            }
        }
        Ok(())
    }

    /// Soft-delete `id` AND append a `message.deleted` audit row to `workspace`'s
    /// trail in ONE transaction (ROADMAP 第三版 方向五 审计事务化): if the audit
    /// insert fails the delete rolls back too — a message is never silently
    /// deleted unaudited. Returns whether the message was actually deleted; an
    /// already-deleted/missing message writes no audit row (`false`).
    pub async fn soft_delete_audited(
        &self,
        id: MessageId,
        workspace: WorkspaceId,
        actor: Option<ParticipantId>,
        detail: serde_json::Value,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let deleted = Self::soft_delete_in_tx(&mut tx, id).await?;
        if deleted {
            crate::audit::AuditRepo::append_in_tx(
                &mut tx,
                workspace,
                actor,
                "message.deleted",
                Some(&id.to_string()),
                detail,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(deleted)
    }

    /// Soft-delete a moderation-flagged `id` AND append a `message.moderated`
    /// audit row to `workspace`'s trail in ONE transaction (ROADMAP 第三版 方向五
    /// 审计事务化, moderation path): a moderation deletion can never commit while
    /// its audit row is lost, so every removed message stays independently
    /// reviewable. The actor is system-initiated (`None`). Mirrors
    /// [`soft_delete_audited`](Self::soft_delete_audited) but records the
    /// `message.moderated` action so moderation deletions are distinguishable from
    /// user-initiated `message.deleted` ones in the audit trail. Returns whether
    /// the message was actually deleted; an already-deleted/missing message writes
    /// no audit row (`false`).
    pub async fn soft_delete_moderated(
        &self,
        id: MessageId,
        workspace: WorkspaceId,
        detail: serde_json::Value,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let deleted = Self::soft_delete_in_tx(&mut tx, id).await?;
        if deleted {
            crate::audit::AuditRepo::append_in_tx(
                &mut tx,
                workspace,
                None,
                "message.moderated",
                Some(&id.to_string()),
                detail,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(deleted)
    }

    /// Patch transcripts onto the Voice blocks of a message that don't have
    /// one yet. Called by the transcribe bot — bypasses the sender-only edit
    /// check because the AI is acting on behalf of the system.
    /// Returns the updated message (or None if the row is missing/deleted).
    pub async fn update_voice_transcript(
        &self,
        id: MessageId,
        transcript: &str,
    ) -> Result<Option<Message>, sqlx::Error> {
        let row = sqlx::query_as::<_, MessageRow>(
            r#"UPDATE messages SET
                 blocks = (
                   SELECT jsonb_agg(
                     CASE WHEN elem->>'type' = 'voice'
                              AND (elem->>'transcript') IS NULL
                          THEN elem || jsonb_build_object('transcript', $2::text)
                          ELSE elem
                     END
                   )
                   FROM jsonb_array_elements(blocks) AS elem
                 ),
                 searchable_text = searchable_text || E'\n' || $2,
                 edited_at = NOW(),
                 embedding = NULL
               WHERE id = $1 AND deleted_at IS NULL
            RETURNING id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at, version"#,
        )
        .bind(id.to_uuid())
        .bind(transcript)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Message::from))
    }

    /// Update the embedding column for a message. Called by the AI worker.
    pub async fn update_embedding(
        &self,
        id: MessageId,
        embedding: Vec<f32>,
    ) -> Result<bool, sqlx::Error> {
        let v = Vector::from(embedding);
        let result = sqlx::query(
            r#"UPDATE messages SET embedding = $1 WHERE id = $2 AND deleted_at IS NULL"#,
        )
        .bind(v)
        .bind(id.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Overwrite a message's `searchable_text` (the FTS source column). Called by
    /// the AI worker when it folds an attached document's *extracted body text*
    /// into the message's indexed text (方向三-2 file content search), so a
    /// `report.pdf`'s prose is full-text-searchable, not just its file name.
    ///
    /// The `search_tsv` tsvector is a STORED generated column over this column, so
    /// a single UPDATE here re-derives the FTS index automatically (no separate
    /// trigger to fire). Does NOT touch `embedding`: the caller (the worker)
    /// writes the embedding for the *same* combined text in the immediately
    /// following step, so nulling it here would be redundant churn. Returns
    /// whether a live row was updated (`false` if missing / soft-deleted).
    pub async fn update_searchable_text(
        &self,
        id: MessageId,
        searchable_text: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r#"UPDATE messages SET searchable_text = $1 WHERE id = $2 AND deleted_at IS NULL"#,
        )
        .bind(searchable_text)
        .bind(id.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod association_tests {
    use super::*;
    use aero_common::RoomId;
    use sqlx::PgPool;

    const DEFAULT_WORKSPACE: &str = "00000000-0000-0000-0000-000000000000";

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy accepts a valid URL")
    }

    async fn count_for(p: &PgPool, table: &str, message: MessageId) -> i64 {
        let sql = format!("SELECT COUNT(*) FROM {table} WHERE message_id = $1");
        sqlx::query_scalar(&sql)
            .bind(message.to_uuid())
            .fetch_one(p)
            .await
            .expect("count association")
    }

    #[test]
    fn visible_cleanup_classification_preserves_evidence_and_cursors() {
        let sql = VISIBLE_ASSOCIATION_DELETE_QUERIES.join("\n");
        for table in [
            "reactions",
            "message_receipts",
            "pins",
            "bookmarks",
            "notifications",
            "notification_bundles",
            "block_interactions",
        ] {
            assert!(
                sql.contains(table),
                "{table} is a visible message projection"
            );
        }
        for retained in [
            "message_edits",
            "message_reports",
            "read_receipts",
            "delivery_cursors",
            "message_send_keys",
            "event_outbox",
        ] {
            assert!(
                !sql.contains(retained),
                "{retained} is evidence, a monotonic cursor, or a delivery ledger"
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with applied migrations"]
    async fn soft_delete_cleans_visible_associations_but_retains_evidence() {
        let p = pool();
        let sender = ParticipantId::new();
        let room = RoomId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(sender.to_uuid())
            .bind(format!("soft-delete-associations-{sender}"))
            .execute(&p)
            .await
            .expect("participant");
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id)
             VALUES ($1,'group',$2,$3,$4::uuid)",
        )
        .bind(room.to_uuid())
        .bind(format!("soft-delete-associations-{room}"))
        .bind(sender.to_uuid())
        .bind(DEFAULT_WORKSPACE)
        .execute(&p)
        .await
        .expect("room");
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1::uuid,$2,'member')
             ON CONFLICT DO NOTHING",
        )
        .bind(DEFAULT_WORKSPACE)
        .bind(sender.to_uuid())
        .execute(&p)
        .await
        .expect("workspace membership");
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1,$2,'owner')",
        )
        .bind(room.to_uuid())
        .bind(sender.to_uuid())
        .execute(&p)
        .await
        .expect("room membership");

        let repo = MessageRepo::new(p.clone());
        let message = repo
            .insert(NewMessage {
                room_id: room,
                sender_id: sender,
                blocks: vec![Block::text("retire me")],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("message")
            .id;

        sqlx::query(
            "INSERT INTO reactions (message_id, participant_id, emoji)
             VALUES ($1,$2,'👍')",
        )
        .bind(message.to_uuid())
        .bind(sender.to_uuid())
        .execute(&p)
        .await
        .expect("reaction");
        sqlx::query("INSERT INTO message_receipts (message_id, participant_id) VALUES ($1,$2)")
            .bind(message.to_uuid())
            .bind(sender.to_uuid())
            .execute(&p)
            .await
            .expect("message receipt");
        sqlx::query("INSERT INTO pins (room_id, message_id, pinned_by) VALUES ($1,$2,$3)")
            .bind(room.to_uuid())
            .bind(message.to_uuid())
            .bind(sender.to_uuid())
            .execute(&p)
            .await
            .expect("pin");
        sqlx::query(
            "INSERT INTO bookmarks (participant_id, message_id, room_id, note)
             VALUES ($1,$2,$3,'private note')",
        )
        .bind(sender.to_uuid())
        .bind(message.to_uuid())
        .bind(room.to_uuid())
        .execute(&p)
        .await
        .expect("bookmark");
        sqlx::query(
            "INSERT INTO notifications
                 (id, participant_id, room_id, message_id, kind, actor_id)
             VALUES ($1,$2,$3,$4,'mention',$2)",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(sender.to_uuid())
        .bind(room.to_uuid())
        .bind(message.to_uuid())
        .execute(&p)
        .await
        .expect("notification");
        sqlx::query(
            "INSERT INTO notification_bundles
                 (participant_id, room_id, message_id, kind, actor_id)
             VALUES ($1,$2,$3,'reply',$1)",
        )
        .bind(sender.to_uuid())
        .bind(room.to_uuid())
        .bind(message.to_uuid())
        .execute(&p)
        .await
        .expect("notification bundle");
        sqlx::query(
            "INSERT INTO block_interactions
                 (id, message_id, room_id, participant_id, action_id, value)
             VALUES ($1,$2,$3,$4,'approve','yes')",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(message.to_uuid())
        .bind(room.to_uuid())
        .bind(sender.to_uuid())
        .execute(&p)
        .await
        .expect("interaction");

        sqlx::query(
            "INSERT INTO message_edits (id, message_id, editor_id, blocks)
             VALUES ($1,$2,$3,$4)",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(message.to_uuid())
        .bind(sender.to_uuid())
        .bind(serde_json::json!([{ "type": "text", "text": "prior body" }]))
        .execute(&p)
        .await
        .expect("edit evidence");
        sqlx::query(
            "INSERT INTO message_reports
                 (id, workspace_id, message_id, reporter_id, reason)
             VALUES ($1,$2::uuid,$3,$4,'policy violation')",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(DEFAULT_WORKSPACE)
        .bind(message.to_uuid())
        .bind(sender.to_uuid())
        .execute(&p)
        .await
        .expect("report evidence");
        sqlx::query(
            "INSERT INTO read_receipts (room_id, participant_id, last_read_message_id)
             VALUES ($1,$2,$3)",
        )
        .bind(room.to_uuid())
        .bind(sender.to_uuid())
        .bind(message.to_uuid())
        .execute(&p)
        .await
        .expect("room read cursor");

        assert!(repo.soft_delete(message).await.expect("soft delete"));
        for table in [
            "reactions",
            "message_receipts",
            "pins",
            "bookmarks",
            "notifications",
            "notification_bundles",
            "block_interactions",
        ] {
            assert_eq!(
                count_for(&p, table, message).await,
                0,
                "{table} was transactionally removed"
            );
        }
        assert_eq!(count_for(&p, "message_edits", message).await, 1);
        assert_eq!(count_for(&p, "message_reports", message).await, 1);
        let room_cursor: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM read_receipts
              WHERE room_id = $1 AND participant_id = $2 AND last_read_message_id = $3",
        )
        .bind(room.to_uuid())
        .bind(sender.to_uuid())
        .bind(message.to_uuid())
        .fetch_one(&p)
        .await
        .expect("room cursor count");
        assert_eq!(room_cursor, 1, "monotonic room read cursor is retained");

        let edits = crate::MessageEditRepo::new(p.clone());
        assert_eq!(
            edits.message_room(message).await.expect("history room"),
            None,
            "ordinary history route cannot resolve a deleted message"
        );
        assert!(
            edits
                .list_for_message(message)
                .await
                .expect("history list")
                .is_empty(),
            "retained prior bodies do not surface on ordinary reads"
        );

        sqlx::query("DELETE FROM message_reports WHERE message_id = $1")
            .bind(message.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM message_edits WHERE message_id = $1")
            .bind(message.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(sender.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
