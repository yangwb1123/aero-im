//! Core CRUD operations for messages: insert, get, edit, soft-delete (various
//! flavours), voice-transcript patch, embedding update.
//!
//! Extracted from `message.rs` as part of REFACTOR_PLAN.md Step 2.

use aero_common::{Block, Message, MessageId, ParticipantId, WorkspaceId};
use pgvector::Vector;
use sqlx::{Postgres, Transaction};

use super::{MessageRepo, NewMessage};
use crate::message::orig::{attached_blob_ids, searchable_of, MessageRow};

impl MessageRepo {
    pub async fn insert(&self, new: NewMessage) -> Result<Message, sqlx::Error> {
        let id = MessageId::new();
        let created_at = time::OffsetDateTime::now_utc();
        let blocks_json = serde_json::to_value(&new.blocks)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
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
            expires_at: new.expires_at,
            version: 1,
        })
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
                    "SELECT EXISTS(SELECT 1 FROM messages WHERE id = $1 AND deleted_at IS NULL)"
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
            "SELECT version FROM messages WHERE id = $1 AND deleted_at IS NULL"
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

        // Enqueue each attached blob for GC unless another live message still
        // references it. The ULID string is globally unique, so a substring
        // containment test over the remaining live blocks is a safe reference
        // check (this row's blocks are already cleared above).
        for blob in blob_ids {
            let still_referenced = sqlx::query_as::<_, (bool,)>(
                r"SELECT EXISTS(
                    SELECT 1 FROM messages
                     WHERE deleted_at IS NULL AND blocks::text LIKE $1
                  )",
            )
            .bind(format!("%{blob}%"))
            .fetch_one(&mut **tx)
            .await?
            .0;
            if !still_referenced {
                sqlx::query(
                    r"INSERT INTO blob_gc_queue (blob_id) VALUES ($1) ON CONFLICT (blob_id) DO NOTHING",
                )
                .bind(blob.to_uuid())
                .execute(&mut **tx)
                .await?;
            }
        }

        Ok(true)
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
