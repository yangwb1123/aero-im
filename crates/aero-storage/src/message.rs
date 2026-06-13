//! Message repository.
//!
//! P2 additions: searchable_text column populated on insert/edit, edit + soft-delete,
//! full-text + trigram + vector search, embedding update for the AI worker.

use aero_common::{Block, Message, MessageId, ParticipantId, RoomId, WorkspaceId};
use pgvector::Vector;
use sqlx::{PgPool, Postgres, Transaction};

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
    pub expires_at: Option<time::OffsetDateTime>,
}

/// Search result row carrying the message + a relevance score.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub message: Message,
    pub score: f32,
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
        })
    }

    /// Fetch a single message by id (including soft-deleted, caller must filter).
    pub async fn get(&self, id: MessageId) -> Result<Option<Message>, sqlx::Error> {
        let row = sqlx::query_as::<_, MessageRow>(
            r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
               FROM messages WHERE id = $1"#,
        )
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Message::from))
    }

    /// Update message blocks. Caller has already checked authorization.
    /// Returns the updated message (or `None` if the row was deleted/missing).
    pub async fn edit(
        &self,
        id: MessageId,
        blocks: Vec<Block>,
    ) -> Result<Option<Message>, sqlx::Error> {
        let blocks_json = serde_json::to_value(&blocks)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let searchable = searchable_of(&blocks);
        let edited_at = time::OffsetDateTime::now_utc();

        let row = sqlx::query_as::<_, MessageRow>(
            r#"UPDATE messages
                  SET blocks = $1, searchable_text = $2, edited_at = $3, embedding = NULL
               WHERE id = $4 AND deleted_at IS NULL
            RETURNING id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at"#,
        )
        .bind(&blocks_json)
        .bind(&searchable)
        .bind(edited_at)
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Message::from))
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
            RETURNING id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at"#,
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
                r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
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
                r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
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

    /// Fetch up to `limit` messages in a room STRICTLY AFTER the `after` cursor,
    /// in ascending `(created_at, id)` order.
    ///
    /// This is the forward complement to [`Self::list_recent`] (which pages
    /// backward via `before`). A reconnecting client passes its last-seen
    /// message id as `after` to catch up, in chronological order, on whatever it
    /// missed while disconnected (ROADMAP 方向五: 断线重连补偿).
    ///
    /// Because [`MessageId`] is a time-sortable ULID stored as a UUID, ordering
    /// by `id` is equivalent to ordering by `(created_at, id)` and the keyset
    /// predicate `id > $cursor` is a stable, index-friendly forward cursor —
    /// the exact mirror of `list_recent`'s `id < $cursor` / `ORDER BY id DESC`.
    /// Messages in `room` whose content CHANGED after `since` — edits and deletes
    /// (tombstones included, so `deleted_at IS NULL` is deliberately NOT filtered).
    /// [`Self::list_since`] only returns NEW messages (`id > cursor`), so a client
    /// offline during an edit/delete to a message it already holds never learns of
    /// it; this is the change-replay half (ROADMAP 方向一). Ordered
    /// oldest-change-first. `GREATEST(edited_at, deleted_at)` is the latest
    /// mutation instant (it ignores NULLs), so the predicate is equivalent to
    /// `edited_at > since OR deleted_at > since` and is index-backed by migration
    /// 0125. The caller applies each row: a populated `deleted_at` means remove,
    /// otherwise replace.
    pub async fn changes_since(
        &self,
        room: RoomId,
        since: time::OffsetDateTime,
        limit: i64,
    ) -> Result<Vec<Message>, sqlx::Error> {
        let limit = clamp_page_limit(limit);
        let rows = sqlx::query_as::<_, MessageRow>(
            r"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
               FROM messages
               WHERE room_id = $1 AND GREATEST(edited_at, deleted_at) > $2
               ORDER BY GREATEST(edited_at, deleted_at) ASC
               LIMIT $3",
        )
        .bind(room.to_uuid())
        .bind(since)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Message::from).collect())
    }

    pub async fn list_since(
        &self,
        room: RoomId,
        after: MessageId,
        limit: i64,
    ) -> Result<Vec<Message>, sqlx::Error> {
        let limit = clamp_page_limit(limit);
        let rows = sqlx::query_as::<_, MessageRow>(
            r"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
               FROM messages
               WHERE room_id = $1 AND id > $2 AND deleted_at IS NULL
               ORDER BY id ASC
               LIMIT $3",
        )
        .bind(room.to_uuid())
        .bind(after.to_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Message::from).collect())
    }

    /// Fetch a window of messages CENTERED on `target` for the "jump to message"
    /// permalink view (Slack-style): up to `half_window` live messages before the
    /// target (id < target), the target itself, and up to `half_window` live
    /// messages after it (id > target), all in the same room, merged and returned
    /// in ascending `id` (chronological) order.
    ///
    /// `half_window` is clamped to `[1, 100]` (see [`clamp_half_window`]). The
    /// "before" half is read `ORDER BY id DESC LIMIT half_window` (the nearest
    /// preceding messages) and the "after" half `ORDER BY id ASC LIMIT
    /// half_window`; both are then folded together with the target and sorted
    /// ascending so the caller sees a single chronological slice. Soft-deleted
    /// rows (`deleted_at IS NOT NULL`) are excluded, including the target — if the
    /// target is deleted (or in another room) it simply won't appear in the
    /// result, and the surrounding window is returned without it.
    ///
    /// Because [`MessageId`] is a time-sortable ULID stored as a UUID, ordering by
    /// `id` is equivalent to chronological order — the same cursor contract as
    /// [`Self::list_since`] / [`Self::list_recent`].
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the underlying queries.
    pub async fn messages_around(
        &self,
        room: RoomId,
        target: MessageId,
        half_window: i64,
    ) -> Result<Vec<Message>, sqlx::Error> {
        let half_window = clamp_half_window(half_window);

        let before = sqlx::query_as::<_, MessageRow>(
            r"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
               FROM messages
               WHERE room_id = $1 AND id < $2 AND deleted_at IS NULL
               ORDER BY id DESC
               LIMIT $3",
        )
        .bind(room.to_uuid())
        .bind(target.to_uuid())
        .bind(half_window)
        .fetch_all(&self.pool)
        .await?;

        let target_row = sqlx::query_as::<_, MessageRow>(
            r"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
               FROM messages
               WHERE room_id = $1 AND id = $2 AND deleted_at IS NULL",
        )
        .bind(room.to_uuid())
        .bind(target.to_uuid())
        .fetch_optional(&self.pool)
        .await?;

        let after = sqlx::query_as::<_, MessageRow>(
            r"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
               FROM messages
               WHERE room_id = $1 AND id > $2 AND deleted_at IS NULL
               ORDER BY id ASC
               LIMIT $3",
        )
        .bind(room.to_uuid())
        .bind(target.to_uuid())
        .bind(half_window)
        .fetch_all(&self.pool)
        .await?;

        let mut out: Vec<Message> = before
            .into_iter()
            .chain(target_row)
            .chain(after)
            .map(Message::from)
            .collect();
        out.sort_by_key(|m| m.id);
        Ok(out)
    }

    /// Find messages in a room missing an embedding. Used by the embedding worker
    /// at startup to catch up on backlog before the live queue takes over.
    pub async fn list_without_embedding(&self, limit: i64) -> Result<Vec<Message>, sqlx::Error> {
        let limit = limit.clamp(1, 1000);
        let rows = sqlx::query_as::<_, MessageRow>(
            r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
               FROM messages
               WHERE embedding IS NULL
                 AND deleted_at IS NULL
                 AND searchable_text <> ''
               ORDER BY id ASC
               LIMIT $1"#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Message::from).collect())
    }

    /// Full-text search inside a room. Falls back to trigram similarity for short
    /// queries that don't yield FTS hits.
    pub async fn search_fts(
        &self,
        room: RoomId,
        query: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        let limit = limit.clamp(1, 100);
        let rows = sqlx::query_as::<_, ScoredMessageRow>(
            r#"SELECT
                 m.id, m.room_id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                 m.created_at, m.edited_at, m.deleted_at, m.expires_at,
                 GREATEST(
                   ts_rank(m.search_tsv, websearch_to_tsquery('english', f_unaccent($2))),
                   similarity(m.searchable_text, $2)
                 ) AS score
               FROM messages m
               WHERE m.room_id = $1
                 AND m.deleted_at IS NULL
                 AND (
                   m.search_tsv @@ websearch_to_tsquery('english', f_unaccent($2))
                   OR m.searchable_text % $2
                 )
               ORDER BY score DESC, m.id DESC
               LIMIT $3"#,
        )
        .bind(room.to_uuid())
        .bind(query)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(SearchHit::from).collect())
    }

    /// Semantic (cosine-distance) search in a room. Caller supplies a normalized
    /// query embedding of the same dimension as `messages.embedding`.
    pub async fn search_vector(
        &self,
        room: RoomId,
        embedding: Vec<f32>,
        limit: i64,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        let limit = limit.clamp(1, 100);
        let v = Vector::from(embedding);
        let mut tx = self.pool.begin().await?;
        // Widen the HNSW candidate walk for THIS query (transaction-local) so the
        // room filter post-selects from a broader candidate set rather than
        // pgvector's default ef_search=40 — sparse-tenant recall otherwise
        // collapses (ROADMAP 方向二).
        sqlx::query("SELECT set_config('hnsw.ef_search', $1, true)")
            .bind(hnsw_ef_search(limit).to_string())
            .execute(&mut *tx)
            .await?;
        let rows = sqlx::query_as::<_, ScoredMessageRow>(
            r#"SELECT
                 m.id, m.room_id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                 m.created_at, m.edited_at, m.deleted_at, m.expires_at,
                 (1 - (m.embedding <=> $2))::real AS score
               FROM messages m
               WHERE m.room_id = $1
                 AND m.deleted_at IS NULL
                 AND m.embedding IS NOT NULL
               ORDER BY m.embedding <=> $2
               LIMIT $3"#,
        )
        .bind(room.to_uuid())
        .bind(v)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(SearchHit::from).collect())
    }

    /// FTS-ranked rerank candidates inside a room — the lexical half of the RAG
    /// Reciprocal-Rank-Fusion rerank (方向三). Mirrors [`Self::search_vector`]'s
    /// boundary EXACTLY (`room_id` filter + `deleted_at IS NULL`) but ranks by
    /// `ts_rank` over `websearch_to_tsquery`, retrying with `plainto_tsquery`
    /// should the database reject the websearch parse. `score` carries the raw
    /// `ts_rank` value; only the ORDER matters to the fuser. No trigram fallback —
    /// fuzzy hits would dilute the lexical signal the fusion needs.
    pub async fn fts_candidates(
        &self,
        room: RoomId,
        query: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        let limit = limit.clamp(1, 100);
        match self.fts_candidates_with(room, query, limit, TsQueryFn::Websearch).await {
            Err(sqlx::Error::Database(_)) => {
                self.fts_candidates_with(room, query, limit, TsQueryFn::Plain).await
            }
            other => other,
        }
    }

    async fn fts_candidates_with(
        &self,
        room: RoomId,
        query: &str,
        limit: i64,
        parser: TsQueryFn,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        // `parser` expands to one of two compile-time constant function names,
        // so the format! is not an injection surface.
        let sql = format!(
            r"SELECT
                 m.id, m.room_id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                 m.created_at, m.edited_at, m.deleted_at, m.expires_at,
                 ts_rank(m.search_tsv, {f}('english', f_unaccent($2))) AS score
               FROM messages m
               WHERE m.room_id = $1
                 AND m.deleted_at IS NULL
                 AND m.search_tsv @@ {f}('english', f_unaccent($2))
               ORDER BY score DESC, m.id DESC
               LIMIT $3",
            f = parser.sql_name()
        );
        let rows = sqlx::query_as::<_, ScoredMessageRow>(&sql)
            .bind(room.to_uuid())
            .bind(query)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(SearchHit::from).collect())
    }

    // ------------------------------------------------------------- THREADS

    /// List the (non-deleted) replies hanging off a root message, oldest first,
    /// paginated by a keyset cursor. A "thread" is the flat set of messages whose
    /// `reply_to` points at `root` (matching Slack's flat-thread model). `after`
    /// is an exclusive lower bound — pass the newest id seen to page forward.
    pub async fn thread_replies(
        &self,
        root: MessageId,
        after: Option<MessageId>,
        limit: i64,
    ) -> Result<Vec<Message>, sqlx::Error> {
        let limit = clamp_page_limit(limit);
        let rows = sqlx::query_as::<_, MessageRow>(
            r"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
               FROM messages
               WHERE reply_to = $1
                 AND deleted_at IS NULL
                 AND ($2::uuid IS NULL OR id > $2)
               ORDER BY id ASC
               LIMIT $3",
        )
        .bind(root.to_uuid())
        .bind(after.map(|m| m.to_uuid()))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Message::from).collect())
    }

    /// Summarize a thread: reply count, distinct repliers (capped at 8 for avatar
    /// rendering), and the newest reply id/time. Returns a zero-count summary when
    /// the root has no replies. Drives the "N replies" affordance in history.
    pub async fn thread_summary(
        &self,
        root: MessageId,
    ) -> Result<aero_common::ThreadSummary, sqlx::Error> {
        // Single round-trip (previously three: count + newest reply + distinct
        // repliers, each re-scanning the same reply set). A `MATERIALIZED` CTE
        // scans the replies ONCE; the scalar subqueries then read from it.
        // Postgres has no `max(uuid)` aggregate, so the newest reply is the
        // ordered single-row read (ids are time-sortable ULIDs stored as UUID, so
        // `ORDER BY id DESC LIMIT 1` is the most recent). `repliers` is capped at 8
        // distinct senders for avatar rendering; `array_agg` over the empty set
        // yields `NULL` → decoded as `None` → an empty replier list.
        #[allow(clippy::type_complexity)]
        let (count, last_id, last_at, repliers): (
            i64,
            Option<uuid::Uuid>,
            Option<time::OffsetDateTime>,
            Option<Vec<uuid::Uuid>>,
        ) = sqlx::query_as(
            r"WITH replies AS MATERIALIZED (
                  SELECT id, sender_id, created_at
                    FROM messages
                   WHERE reply_to = $1 AND deleted_at IS NULL
              )
              SELECT
                  (SELECT COUNT(*) FROM replies),
                  (SELECT id FROM replies ORDER BY id DESC LIMIT 1),
                  (SELECT created_at FROM replies ORDER BY id DESC LIMIT 1),
                  (SELECT array_agg(sender_id)
                     FROM (SELECT DISTINCT sender_id FROM replies LIMIT 8) d)",
        )
        .bind(root.to_uuid())
        .fetch_one(&self.pool)
        .await?;

        Ok(aero_common::ThreadSummary {
            root_id: root,
            reply_count: u32::try_from(count).unwrap_or(u32::MAX),
            repliers: repliers
                .unwrap_or_default()
                .into_iter()
                .map(ParticipantId::from_uuid)
                .collect(),
            last_reply_id: last_id.map(MessageId::from_uuid),
            last_reply_at: last_at,
        })
    }

    /// Distinct participants who have posted at least one (non-deleted) reply in a
    /// thread. The thread is identified by `root` — every message whose `reply_to =
    /// root` and `deleted_at IS NULL`. Returns participant ids in arbitrary order;
    /// the caller join-fetches display names for the HTTP response. Empty when the
    /// thread has no replies (yet). Used by
    /// `GET /api/messages/:id/thread-participants`.
    pub async fn thread_participants(
        &self,
        root: MessageId,
    ) -> Result<Vec<ParticipantId>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT DISTINCT sender_id
               FROM messages
               WHERE reply_to = $1 AND deleted_at IS NULL",
        )
        .bind(root.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(p,)| ParticipantId::from_uuid(p)).collect())
    }

    // ------------------------------------------------------------- UNREAD

    /// Per-room count of messages the participant has not yet read — messages in
    /// rooms they belong to, created after their read receipt (or all, if none),
    /// excluding their own. Only rooms with at least one unread appear. Backs the
    /// sidebar unread badge; pairs with
    /// [`NotificationRepo::unread_counts_by_room`](crate::NotificationRepo::unread_counts_by_room)
    /// for the mention badge.
    pub async fn unread_counts_by_room(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<(RoomId, u32)>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid, i64)>(
            r"SELECT m.room_id, COUNT(*)
               FROM messages m
               JOIN room_members rm
                 ON rm.room_id = m.room_id AND rm.participant_id = $1
               LEFT JOIN read_receipts rr
                 ON rr.room_id = m.room_id AND rr.participant_id = $1
               WHERE m.deleted_at IS NULL
                 AND m.sender_id <> $1
                 AND (rr.last_read_message_id IS NULL OR m.id > rr.last_read_message_id)
               GROUP BY m.room_id",
        )
        .bind(participant.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(r, c)| (RoomId::from_uuid(r), u32::try_from(c).unwrap_or(u32::MAX)))
            .collect())
    }

    // ------------------------------------------------ CROSS-ROOM (GLOBAL) SEARCH

    /// Full-text + trigram search across **every room the caller belongs to**
    /// (Slack-style global search). The `JOIN room_members` predicate is the
    /// security guard: only rooms in which `participant` is a member are scanned,
    /// so a hit can never leak a room the caller isn't in. Scoring mirrors
    /// [`Self::search_fts`] (`GREATEST(ts_rank, similarity)`), and each returned
    /// [`Message`] carries its `room_id` so the client can label which room the
    /// hit came from.
    pub async fn search_all_rooms(
        &self,
        participant: ParticipantId,
        query: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        let limit = limit.clamp(1, 100);
        let rows = sqlx::query_as::<_, ScoredMessageRow>(
            r#"SELECT
                 m.id, m.room_id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                 m.created_at, m.edited_at, m.deleted_at, m.expires_at,
                 GREATEST(
                   ts_rank(m.search_tsv, websearch_to_tsquery('english', f_unaccent($2))),
                   similarity(m.searchable_text, $2)
                 ) AS score
               FROM messages m
               JOIN room_members rm
                 ON rm.room_id = m.room_id AND rm.participant_id = $1
               WHERE m.deleted_at IS NULL
                 AND (
                   m.search_tsv @@ websearch_to_tsquery('english', f_unaccent($2))
                   OR m.searchable_text % $2
                 )
               ORDER BY score DESC, m.id DESC
               LIMIT $3"#,
        )
        .bind(participant.to_uuid())
        .bind(query)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(SearchHit::from).collect())
    }

    /// Tenant-scoped variant of [`Self::search_all_rooms`]: same membership guard,
    /// additionally restricted to rooms in `workspace`. Lets a multi-tenant client
    /// search within one workspace without surfacing the caller's rooms in others.
    pub async fn search_all_rooms_in_workspace(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        query: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        let limit = limit.clamp(1, 100);
        let rows = sqlx::query_as::<_, ScoredMessageRow>(
            r#"SELECT
                 m.id, m.room_id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                 m.created_at, m.edited_at, m.deleted_at, m.expires_at,
                 GREATEST(
                   ts_rank(m.search_tsv, websearch_to_tsquery('english', f_unaccent($2))),
                   similarity(m.searchable_text, $2)
                 ) AS score
               FROM messages m
               JOIN room_members rm
                 ON rm.room_id = m.room_id AND rm.participant_id = $1
               WHERE m.deleted_at IS NULL
                 AND m.room_id IN (SELECT id FROM rooms WHERE workspace_id = $4)
                 AND (
                   m.search_tsv @@ websearch_to_tsquery('english', f_unaccent($2))
                   OR m.searchable_text % $2
                 )
               ORDER BY score DESC, m.id DESC
               LIMIT $3"#,
        )
        .bind(participant.to_uuid())
        .bind(query)
        .bind(limit)
        .bind(workspace.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(SearchHit::from).collect())
    }

    /// Semantic (cosine-distance) variant of [`Self::search_all_rooms_in_workspace`]:
    /// vector search across EVERY room the caller belongs to within `workspace`.
    ///
    /// The retrieval half of the workspace-wide RAG ask. Identical membership
    /// boundary to [`Self::search_all_rooms_in_workspace`] — `JOIN room_members`
    /// scopes to the caller's rooms and the `rooms.workspace_id` filter scopes to
    /// the one tenant — but ranked by embedding distance (`<=>`) instead of FTS.
    /// Caller supplies a normalized query embedding of the same dimension as
    /// `messages.embedding`. There is no post-filter: a message in a room the
    /// caller is not a member of can never be returned.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn search_vector_workspace(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        embedding: Vec<f32>,
        limit: i64,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        let limit = limit.clamp(1, 100);
        let v = Vector::from(embedding);
        let mut tx = self.pool.begin().await?;
        // Transaction-local ef_search widening — mirrors [`Self::search_vector`];
        // the membership/workspace filter is a post-filter over the HNSW walk, so
        // a too-narrow default starves recall here too (ROADMAP 方向二).
        sqlx::query("SELECT set_config('hnsw.ef_search', $1, true)")
            .bind(hnsw_ef_search(limit).to_string())
            .execute(&mut *tx)
            .await?;
        let rows = sqlx::query_as::<_, ScoredMessageRow>(
            r"SELECT
                 m.id, m.room_id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                 m.created_at, m.edited_at, m.deleted_at, m.expires_at,
                 (1 - (m.embedding <=> $2))::real AS score
               FROM messages m
               JOIN room_members rm
                 ON rm.room_id = m.room_id AND rm.participant_id = $1
               WHERE m.deleted_at IS NULL
                 AND m.embedding IS NOT NULL
                 AND m.room_id IN (SELECT id FROM rooms WHERE workspace_id = $4)
               ORDER BY m.embedding <=> $2
               LIMIT $3",
        )
        .bind(participant.to_uuid())
        .bind(v)
        .bind(limit)
        .bind(workspace.to_uuid())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(SearchHit::from).collect())
    }

    /// Recent (non-deleted) messages across EVERY room the caller belongs to within
    /// `workspace`, newest first. Same membership/tenant boundary as
    /// [`Self::search_vector_workspace`] (`JOIN room_members` + `rooms.workspace_id`),
    /// so a message in a room the caller is not a member of can never be returned.
    /// Backs the scheduled workspace digest's "what happened lately across my
    /// channels" summary. `limit` is clamped to `[1, 200]`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn recent_workspace(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        limit: i64,
    ) -> Result<Vec<Message>, sqlx::Error> {
        let limit = limit.clamp(1, 200);
        let rows = sqlx::query_as::<_, MessageRow>(
            r"SELECT m.id, m.room_id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                     m.created_at, m.edited_at, m.deleted_at, m.expires_at
               FROM messages m
               JOIN room_members rm
                 ON rm.room_id = m.room_id AND rm.participant_id = $1
               WHERE m.deleted_at IS NULL
                 AND m.room_id IN (SELECT id FROM rooms WHERE workspace_id = $3)
               ORDER BY m.id DESC
               LIMIT $2",
        )
        .bind(participant.to_uuid())
        .bind(limit)
        .bind(workspace.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Message::from).collect())
    }

    /// Workspace-scoped twin of [`Self::fts_candidates`] — the lexical half of
    /// the workspace-wide RAG rerank. Mirrors [`Self::search_vector_workspace`]'s
    /// membership boundary EXACTLY: `JOIN room_members` scopes hits to rooms the
    /// caller belongs to and the `rooms.workspace_id` filter scopes to one
    /// tenant, so a candidate can never leak a room the caller isn't in. Ranked
    /// by `ts_rank` over `websearch_to_tsquery`, retrying with `plainto_tsquery`
    /// should the database reject the websearch parse.
    pub async fn fts_candidates_workspace(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        query: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        let limit = limit.clamp(1, 100);
        match self
            .fts_candidates_workspace_with(participant, workspace, query, limit, TsQueryFn::Websearch)
            .await
        {
            Err(sqlx::Error::Database(_)) => {
                self.fts_candidates_workspace_with(
                    participant,
                    workspace,
                    query,
                    limit,
                    TsQueryFn::Plain,
                )
                .await
            }
            other => other,
        }
    }

    async fn fts_candidates_workspace_with(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        query: &str,
        limit: i64,
        parser: TsQueryFn,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        // `parser` expands to one of two compile-time constant function names,
        // so the format! is not an injection surface.
        let sql = format!(
            r"SELECT
                 m.id, m.room_id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                 m.created_at, m.edited_at, m.deleted_at, m.expires_at,
                 ts_rank(m.search_tsv, {f}('english', f_unaccent($2))) AS score
               FROM messages m
               JOIN room_members rm
                 ON rm.room_id = m.room_id AND rm.participant_id = $1
               WHERE m.deleted_at IS NULL
                 AND m.room_id IN (SELECT id FROM rooms WHERE workspace_id = $4)
                 AND m.search_tsv @@ {f}('english', f_unaccent($2))
               ORDER BY score DESC, m.id DESC
               LIMIT $3",
            f = parser.sql_name()
        );
        let rows = sqlx::query_as::<_, ScoredMessageRow>(&sql)
            .bind(participant.to_uuid())
            .bind(query)
            .bind(limit)
            .bind(workspace.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(SearchHit::from).collect())
    }

    /// The most recent messages sent by `sender`, capped at [`EXPORT_SENDER_CAP`]
    /// rows, newest first. Used exclusively for personal GDPR export
    /// (`GET /api/me/export`); not suitable for paginated listing.
    pub async fn by_sender(
        &self,
        sender: ParticipantId,
    ) -> Result<Vec<Message>, sqlx::Error> {
        let rows = sqlx::query_as::<_, MessageRow>(
            r"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
               FROM messages
               WHERE sender_id = $1 AND deleted_at IS NULL
               ORDER BY created_at DESC, id DESC
               LIMIT $2",
        )
        .bind(sender.to_uuid())
        .bind(EXPORT_SENDER_CAP)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Message::from).collect())
    }

    /// Keyset page over ALL messages sent by `sender`, oldest-first, `id` strictly
    /// after `after` (or from the start when `None`). The async full-export worker
    /// (方向四) loops this until a short page to assemble a complete, uncapped
    /// archive without holding a giant result set in memory. Includes deleted
    /// rows' tombstones? No — only live messages (`deleted_at IS NULL`), matching
    /// the synchronous export.
    pub async fn by_sender_paged(
        &self,
        sender: ParticipantId,
        after: Option<MessageId>,
        limit: i64,
    ) -> Result<Vec<Message>, sqlx::Error> {
        let after_uuid = after.map(|m| m.to_uuid()).unwrap_or(uuid::Uuid::nil());
        let rows = sqlx::query_as::<_, MessageRow>(
            r"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
               FROM messages
               WHERE sender_id = $1 AND deleted_at IS NULL AND id > $2
               ORDER BY id ASC
               LIMIT $3",
        )
        .bind(sender.to_uuid())
        .bind(after_uuid)
        .bind(limit.clamp(1, 1000))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Message::from).collect())
    }

    /// Hard-delete all messages whose `expires_at` is not null and has already
    /// passed. Returns the number of rows deleted. Called by the retention-sweep
    /// background task; errors are logged by the caller, never panicked.
    /// Hard-delete messages whose `expires_at` has passed, returning the
    /// `(message_id, room_id)` of each removed row so the caller can fan out a
    /// [`aero_common::RoomEvent::Deleted`] per message (ROADMAP 方向一):
    /// burn-after-reading messages must actually disappear from live and
    /// reconnecting clients, not linger on screen until a manual reload.
    pub async fn sweep_ephemeral(&self) -> Result<Vec<(MessageId, RoomId)>, sqlx::Error> {
        let rows: Vec<(uuid::Uuid, uuid::Uuid)> = sqlx::query_as(
            "DELETE FROM messages WHERE expires_at IS NOT NULL AND expires_at < NOW() \
             RETURNING id, room_id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, room)| (MessageId::from_uuid(id), RoomId::from_uuid(room)))
            .collect())
    }
}

/// pgvector HNSW `ef_search` to use for a vector query — the size of the
/// candidate list the index walk maintains. pgvector's default (40) is too
/// small once a room/workspace filter discards most of a busy multi-tenant
/// index's globally-nearest rows, collapsing recall for small/sparse tenants.
/// We widen it to comfortably exceed the requested `limit`, bounded so a large
/// limit can't explode latency. Overridable via `AERO_HNSW_EF_SEARCH` (a value
/// outside `[1, 1000]` is clamped). ROADMAP 方向二.
fn hnsw_ef_search(limit: i64) -> i64 {
    if let Ok(v) = std::env::var("AERO_HNSW_EF_SEARCH") {
        if let Ok(n) = v.parse::<i64>() {
            return n.clamp(1, 1000);
        }
    }
    // Default: 4× the requested rows, floored at 100, capped at 400.
    (limit.saturating_mul(4)).clamp(100, 400)
}

fn searchable_of(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter_map(Block::searchable_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract the blob ids a message's blocks reference (`File` + `Voice` blocks).
/// Used by [`MessageRepo::soft_delete`] to enqueue attachments for GC. A block
/// shape that doesn't deserialize is skipped rather than failing the delete.
fn attached_blob_ids(blocks_json: &serde_json::Value) -> Vec<aero_common::BlobId> {
    let blocks: Vec<Block> = serde_json::from_value(blocks_json.clone()).unwrap_or_default();
    blocks
        .iter()
        .filter_map(|b| match b {
            Block::File { blob_id, .. } | Block::Voice { blob_id, .. } => Some(*blob_id),
            _ => None,
        })
        .collect()
}

/// Maximum rows returned by [`MessageRepo::by_sender`] (personal GDPR export).
const EXPORT_SENDER_CAP: i64 = 500;

/// Which Postgres tsquery parser the FTS rerank-candidate queries use:
/// `Websearch` first (forgiving web-style syntax), `Plain` as the retry when
/// the database rejects the websearch parse.
#[derive(Clone, Copy)]
enum TsQueryFn {
    Websearch,
    Plain,
}

impl TsQueryFn {
    fn sql_name(self) -> &'static str {
        match self {
            Self::Websearch => "websearch_to_tsquery",
            Self::Plain => "plainto_tsquery",
        }
    }
}

/// Clamp a caller-supplied page size into the safe `[1, 200]` window used by
/// the keyset pagination queries ([`MessageRepo::list_since`]), matching the
/// bound `list_recent` applies inline. Factored out so the cursor/ordering
/// contract is unit-testable without a live database.
#[inline]
fn clamp_page_limit(limit: i64) -> i64 {
    limit.clamp(1, 200)
}

/// Clamp the caller-supplied half-window for [`MessageRepo::messages_around`]
/// (the permalink "jump to message" context view) into `[1, 100]`. Each side of
/// the target is fetched with this limit, so a caller can never pull an
/// unbounded slice; zero/negative values clamp up to 1. Factored out so the
/// window contract is unit-testable without a live database.
#[inline]
fn clamp_half_window(half_window: i64) -> i64 {
    half_window.clamp(1, 100)
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
    expires_at: Option<time::OffsetDateTime>,
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
            expires_at: r.expires_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct ScoredMessageRow {
    #[sqlx(flatten)]
    msg: MessageRow,
    score: f32,
}

impl From<ScoredMessageRow> for SearchHit {
    fn from(r: ScoredMessageRow) -> Self {
        Self { message: Message::from(r.msg), score: r.score }
    }
}

#[cfg(test)]
mod tests {
    use super::{attached_blob_ids, clamp_half_window, clamp_page_limit, hnsw_ef_search};
    use aero_common::{BlobId, Block, FileKind, MessageId};
    use ulid::Ulid;

    #[test]
    fn hnsw_ef_search_widens_and_bounds_the_candidate_walk() {
        // Default policy (no env override): 4× the limit, floored at 100,
        // capped at 400 — always ≥ pgvector's default 40 so recall widens.
        assert_eq!(hnsw_ef_search(5), 100, "small limit floored to 100");
        assert_eq!(hnsw_ef_search(40), 160, "4× in the mid range");
        assert_eq!(hnsw_ef_search(100), 400, "large limit capped at 400");
        assert!(hnsw_ef_search(1) >= 40, "never below pgvector's default");
    }

    #[test]
    fn attached_blob_ids_extracts_file_and_voice_blobs_only() {
        let file_blob = BlobId::new();
        let voice_blob = BlobId::new();
        let blocks = vec![
            Block::text("hello"),
            Block::File {
                blob_id: file_blob,
                kind: FileKind::Document,
                name: "doc.pdf".into(),
                size: 10,
            },
            Block::Voice { blob_id: voice_blob, duration_ms: 1000, transcript: None },
        ];
        let json = serde_json::to_value(&blocks).unwrap();
        let got = attached_blob_ids(&json);
        assert_eq!(got, vec![file_blob, voice_blob]);
    }

    #[test]
    fn attached_blob_ids_empty_for_text_only_and_garbage() {
        let text_only = serde_json::to_value(vec![Block::text("nothing here")]).unwrap();
        assert!(attached_blob_ids(&text_only).is_empty());
        // A malformed blocks value yields no ids rather than panicking.
        assert!(attached_blob_ids(&serde_json::json!({"not": "an array"})).is_empty());
    }

    #[test]
    fn page_limit_is_clamped_into_window() {
        // Below the floor clamps up to 1; zero and negatives are never honored.
        assert_eq!(clamp_page_limit(0), 1);
        assert_eq!(clamp_page_limit(-50), 1);
        assert_eq!(clamp_page_limit(1), 1);
        // In-window values pass through untouched.
        assert_eq!(clamp_page_limit(50), 50);
        assert_eq!(clamp_page_limit(200), 200);
        // Above the ceiling clamps down to 200, matching `list_recent`.
        assert_eq!(clamp_page_limit(201), 200);
        assert_eq!(clamp_page_limit(i64::MAX), 200);
    }

    #[test]
    fn half_window_is_clamped_into_window() {
        // Below the floor clamps up to 1; zero and negatives are never honored.
        assert_eq!(clamp_half_window(0), 1);
        assert_eq!(clamp_half_window(-7), 1);
        assert_eq!(clamp_half_window(1), 1);
        // In-window values pass through untouched.
        assert_eq!(clamp_half_window(2), 2);
        assert_eq!(clamp_half_window(100), 100);
        // Above the ceiling clamps down to 100.
        assert_eq!(clamp_half_window(101), 100);
        assert_eq!(clamp_half_window(i64::MAX), 100);
    }

    /// The `list_since` keyset cursor relies on `MessageId` (a time-sortable
    /// ULID) ordering by `id` being equivalent to chronological order, so that
    /// `WHERE id > $after ORDER BY id ASC` returns exactly the messages created
    /// strictly after the cursor, oldest-first. Verify that ordering contract
    /// without a database: ULIDs minted with increasing timestamps sort by id.
    #[test]
    fn message_id_ordering_is_chronological() {
        // Three ids with strictly increasing timestamp components.
        let a = MessageId::from_ulid(Ulid::from_parts(1_000, 0));
        let b = MessageId::from_ulid(Ulid::from_parts(2_000, 0));
        let c = MessageId::from_ulid(Ulid::from_parts(3_000, 0));

        // Ascending by id == ascending by creation time.
        assert!(a < b && b < c);

        // "Strictly after cursor `a`" excludes `a` and includes later ids, the
        // forward complement of list_recent's `id < before`.
        let after = a;
        let mut got: Vec<MessageId> = [a, b, c].into_iter().filter(|m| *m > after).collect();
        got.sort();
        assert_eq!(got, vec![b, c]);
    }
}

#[cfg(test)]
mod db_tests {
    use super::{MessageRepo, NewMessage};
    use aero_common::{Block, ParticipantId, RoomId, WorkspaceId};
    use sqlx::PgPool;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    // Insert a throwaway participant so messages/membership FKs are satisfiable.
    async fn participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("search-actor-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    // Insert a throwaway room (in the all-zero default workspace) created by
    // `creator`. The all-zero `WorkspaceId` is the post-migration-0006 default
    // tenant, which is guaranteed to exist.
    async fn room(p: &PgPool, creator: ParticipantId) -> RoomId {
        let id = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id) VALUES ($1,'group',$2,$3,$4)",
        )
        .bind(id.to_uuid())
        .bind(format!("search-room-{id}"))
        .bind(creator.to_uuid())
        .bind(WorkspaceId(ulid::Ulid(0)).to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        id
    }

    async fn join(p: &PgPool, room: RoomId, who: ParticipantId) {
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role) VALUES ($1,$2,'member')",
        )
        .bind(room.to_uuid())
        .bind(who.to_uuid())
        .execute(p)
        .await
        .expect("insert membership");
    }

    /// A hit may surface only from a room the caller is a member of: the
    /// `JOIN room_members` guard must scope global search to the caller's rooms.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn search_all_rooms_is_membership_scoped() {
        let p = pool();
        let repo = MessageRepo::new(p.clone());

        let me = participant(&p).await;
        let other = participant(&p).await;

        // A room I belong to, and one I do NOT (created/owned by someone else).
        let mine = room(&p, me).await;
        let theirs = room(&p, other).await;
        join(&p, mine, me).await;
        join(&p, theirs, other).await; // membership for `other`, not for `me`.

        // A distinctive token present in BOTH rooms' messages.
        let needle = format!("xqzzytoken{}", ParticipantId::new());
        let in_mine = repo
            .insert(NewMessage {
                room_id: mine,
                sender_id: me,
                blocks: vec![Block::text(format!("hello {needle} world"))],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("insert mine");
        repo.insert(NewMessage {
            room_id: theirs,
            sender_id: other,
            blocks: vec![Block::text(format!("secret {needle} stuff"))],
            reply_to: None,
            metadata: serde_json::json!({}),
            expires_at: None,
        })
        .await
        .expect("insert theirs");

        let hits = repo.search_all_rooms(me, &needle, 50).await.expect("search");

        assert!(
            hits.iter().all(|h| h.message.room_id == mine),
            "every hit comes from a room the caller belongs to"
        );
        assert!(
            hits.iter().any(|h| h.message.id == in_mine.id),
            "the matching message in the caller's room is found"
        );
        assert!(
            !hits.iter().any(|h| h.message.room_id == theirs),
            "a message in a room the caller is NOT in never leaks"
        );
    }

    /// FTS stems with the `english` config (migration 0128): a message whose text
    /// contains only the inflected form `deploying` is found by a search for the
    /// root `deploy`. Under the old `simple` config the tokens are distinct and
    /// this returns zero hits — so this test is the regression guard for the
    /// stemmer switch (query side and stored tsvector must agree on `english`).
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn search_stems_inflected_forms_with_english_config() {
        let p = pool();
        let repo = MessageRepo::new(p.clone());

        // Fresh caller + room → `search_all_rooms` sees only this one message.
        let me = participant(&p).await;
        let r = room(&p, me).await;
        join(&p, r, me).await;

        let inflected = repo
            .insert(NewMessage {
                room_id: r,
                sender_id: me,
                blocks: vec![Block::text("deploying the new release tonight")],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("insert inflected");

        // Search the ROOT form, which never appears literally in the message.
        let hits = repo.search_all_rooms(me, "deploy", 50).await.expect("search");
        assert!(
            hits.iter().any(|h| h.message.id == inflected.id),
            "the english stemmer matches 'deploy' against 'deploying'"
        );
    }

    /// FTS is accent-insensitive (migration 0131 `f_unaccent`): a message written
    /// with diacritics is found by an unaccented query. Under the bare-`english`
    /// config the tokens differ and this returns nothing — so this guards the
    /// unaccent wrapper staying in lock-step on both the stored tsvector and the
    /// query side.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn search_is_accent_insensitive() {
        let p = pool();
        let repo = MessageRepo::new(p.clone());

        let me = participant(&p).await;
        let r = room(&p, me).await;
        join(&p, r, me).await;

        let accented = repo
            .insert(NewMessage {
                room_id: r,
                sender_id: me,
                blocks: vec![Block::text("réunion au café about the résumé")],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("insert accented");

        // Plain ASCII query matches the diacritic'd message…
        let hits = repo.search_all_rooms(me, "cafe resume", 50).await.expect("search");
        assert!(
            hits.iter().any(|h| h.message.id == accented.id),
            "unaccented 'cafe resume' matches accented 'café … résumé'"
        );
        // …and the reverse (accented query → same message) also holds.
        let hits_rev = repo.search_all_rooms(me, "café", 50).await.expect("search rev");
        assert!(hits_rev.iter().any(|h| h.message.id == accented.id), "accented query also matches");
    }

    /// `thread_summary` returns reply count, distinct repliers, and the newest
    /// reply id/time from a SINGLE consolidated query. Verifies the
    /// `MATERIALIZED`-CTE rewrite preserves the original three-query semantics,
    /// including the empty-thread (`array_agg` → NULL → no repliers) case.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn thread_summary_counts_repliers_and_newest_reply() {
        let p = pool();
        let repo = MessageRepo::new(p.clone());
        let me = participant(&p).await;
        let other = participant(&p).await;
        let r = room(&p, me).await;

        let root = repo
            .insert(NewMessage {
                room_id: r,
                sender_id: me,
                blocks: vec![Block::text("root")],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("root");

        // Empty thread: no replies yet → zero count, no last reply, no repliers.
        let empty = repo.thread_summary(root.id).await.expect("empty summary");
        assert_eq!(empty.reply_count, 0);
        assert!(empty.repliers.is_empty());
        assert!(empty.last_reply_id.is_none());
        assert!(empty.last_reply_at.is_none());

        // Two replies from two distinct senders; the second is the newest (ULID
        // ids are monotonic, so the later insert has the larger id).
        repo.insert(NewMessage {
            room_id: r,
            sender_id: me,
            blocks: vec![Block::text("reply 1")],
            reply_to: Some(root.id),
            metadata: serde_json::json!({}),
            expires_at: None,
        })
        .await
        .expect("reply1");
        let r2 = repo
            .insert(NewMessage {
                room_id: r,
                sender_id: other,
                blocks: vec![Block::text("reply 2")],
                reply_to: Some(root.id),
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("reply2");

        let s = repo.thread_summary(root.id).await.expect("summary");
        assert_eq!(s.reply_count, 2, "two replies counted");
        assert_eq!(s.repliers.len(), 2, "two distinct repliers");
        assert!(s.repliers.contains(&me) && s.repliers.contains(&other));
        assert_eq!(s.last_reply_id, Some(r2.id), "newest reply id is the last insert");
        assert!(s.last_reply_at.is_some());

        // Cleanup: remove messages then the room + participants.
        sqlx::query("DELETE FROM messages WHERE room_id = $1").bind(r.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1").bind(r.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
            .bind(vec![me.to_uuid(), other.to_uuid()])
            .execute(&p)
            .await
            .ok();
    }

    /// The workspace-wide RAG retrieval (`search_vector_workspace`) must honor the
    /// same `JOIN room_members` boundary as the FTS variant: with two rooms in the
    /// SAME workspace but only one joined by the caller, a vector hit may surface
    /// only from the joined room — even when the message in the other room carries
    /// an identical embedding (so distance alone would rank them equally).
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn search_vector_workspace_is_membership_scoped() {
        let p = pool();
        let repo = MessageRepo::new(p.clone());
        // Both rooms live in the all-zero default workspace, so membership — not
        // the workspace filter — is the discriminator under test.
        let ws = WorkspaceId(ulid::Ulid(0));

        let me = participant(&p).await;
        let other = participant(&p).await;

        let mine = room(&p, me).await;
        let theirs = room(&p, other).await;
        join(&p, mine, me).await;
        join(&p, theirs, other).await; // membership for `other`, not for `me`.

        // Insert a message in each room, then give BOTH an identical embedding so
        // cosine distance to the query is the same — the only thing that can
        // exclude `theirs` is the membership JOIN.
        let in_mine = repo
            .insert(NewMessage {
                room_id: mine,
                sender_id: me,
                blocks: vec![Block::text("workspace rag in my room")],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("insert mine");
        let in_theirs = repo
            .insert(NewMessage {
                room_id: theirs,
                sender_id: other,
                blocks: vec![Block::text("workspace rag in their room")],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("insert theirs");

        // `messages.embedding` is `vector(1024)`; a uniform unit-ish vector is fine
        // for the distance tie — identical on both rows.
        let embedding = vec![0.1_f32; 1024];
        repo.update_embedding(in_mine.id, embedding.clone())
            .await
            .expect("embed mine");
        repo.update_embedding(in_theirs.id, embedding.clone())
            .await
            .expect("embed theirs");

        let hits = repo
            .search_vector_workspace(me, ws, embedding, 50)
            .await
            .expect("vector search");

        assert!(
            hits.iter().all(|h| h.message.room_id == mine),
            "every vector hit comes from a room the caller belongs to"
        );
        assert!(
            hits.iter().any(|h| h.message.id == in_mine.id),
            "the embedded message in the caller's room is found"
        );
        assert!(
            !hits.iter().any(|h| h.message.id == in_theirs.id),
            "an embedded message in a room the caller is NOT in never leaks"
        );
    }

    /// The FTS rerank-candidate retrieval (`fts_candidates_workspace`) must honor
    /// the same `JOIN room_members` boundary as the vector variant: with two rooms
    /// in the SAME workspace but only one joined by the caller, an FTS candidate
    /// may surface only from the joined room — even when the message in the other
    /// room contains the identical search token (so ts_rank alone would rank them
    /// equally).
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn fts_candidates_workspace_is_membership_scoped() {
        let p = pool();
        let repo = MessageRepo::new(p.clone());
        // Both rooms live in the all-zero default workspace, so membership — not
        // the workspace filter — is the discriminator under test.
        let ws = WorkspaceId(ulid::Ulid(0));

        let me = participant(&p).await;
        let other = participant(&p).await;

        let mine = room(&p, me).await;
        let theirs = room(&p, other).await;
        join(&p, mine, me).await;
        join(&p, theirs, other).await; // membership for `other`, not for `me`.

        // A distinctive token present in BOTH rooms' messages.
        let needle = format!("rrfneedle{}", ParticipantId::new());
        let in_mine = repo
            .insert(NewMessage {
                room_id: mine,
                sender_id: me,
                blocks: vec![Block::text(format!("rerank {needle} in my room"))],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("insert mine");
        let in_theirs = repo
            .insert(NewMessage {
                room_id: theirs,
                sender_id: other,
                blocks: vec![Block::text(format!("rerank {needle} in their room"))],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("insert theirs");

        let hits = repo
            .fts_candidates_workspace(me, ws, &needle, 40)
            .await
            .expect("fts candidates");

        assert!(
            hits.iter().all(|h| h.message.room_id == mine),
            "every FTS candidate comes from a room the caller belongs to"
        );
        assert!(
            hits.iter().any(|h| h.message.id == in_mine.id),
            "the matching message in the caller's room is found"
        );
        assert!(
            !hits.iter().any(|h| h.message.id == in_theirs.id),
            "a matching message in a room the caller is NOT in never leaks"
        );
    }

    /// `messages_around` returns a window straddling the target: with five
    /// messages inserted in order, `half_window = 2` around the MIDDLE message
    /// yields all five (two before, the target, two after) in ascending id order,
    /// the target sitting dead center.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn messages_around_straddles_target() {
        let p = pool();
        let repo = MessageRepo::new(p.clone());

        let me = participant(&p).await;
        let r = room(&p, me).await;
        join(&p, r, me).await;

        // Insert five messages in chronological order; ids are time-sortable
        // ULIDs minted by `insert`, so insertion order == id order.
        let mut ids = Vec::new();
        for i in 0..5 {
            let m = repo
                .insert(NewMessage {
                    room_id: r,
                    sender_id: me,
                    blocks: vec![Block::text(format!("around msg {i}"))],
                    reply_to: None,
                    metadata: serde_json::json!({}),
                    expires_at: None,
                })
                .await
                .expect("insert");
            ids.push(m.id);
        }

        let target = ids[2]; // the middle message
        let window = repo
            .messages_around(r, target, 2)
            .await
            .expect("messages_around");

        let got: Vec<_> = window.iter().map(|m| m.id).collect();
        // Two before + target + two after, all sorted ascending == every id.
        assert_eq!(got, ids, "window straddles the target in chronological order");

        // The target sits at the center of the returned slice.
        let pos = window
            .iter()
            .position(|m| m.id == target)
            .expect("target present in window");
        assert_eq!(pos, 2, "target is dead center with two on each side");
    }

    /// `sweep_ephemeral` hard-deletes only rows whose `expires_at` is in the
    /// past and leaves future-expiry rows untouched.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn test_sweep_ephemeral() {
        let p = pool();
        let repo = MessageRepo::new(p.clone());

        let me = participant(&p).await;
        let r = room(&p, me).await;
        join(&p, r, me).await;

        // Insert an already-expired message (expires_at 1 second in the past).
        let past = time::OffsetDateTime::now_utc() - time::Duration::seconds(1);
        let expired = repo
            .insert(NewMessage {
                room_id: r,
                sender_id: me,
                blocks: vec![Block::text("ephemeral past")],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: Some(past),
            })
            .await
            .expect("insert expired");

        // Insert a message with expires_at 1 hour in the future.
        let future = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
        let alive = repo
            .insert(NewMessage {
                room_id: r,
                sender_id: me,
                blocks: vec![Block::text("ephemeral future")],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: Some(future),
            })
            .await
            .expect("insert future");

        // sweep_ephemeral must delete exactly 1 row (the expired one) and return
        // its (message_id, room_id) so the caller can announce the deletion.
        let deleted = repo.sweep_ephemeral().await.expect("sweep");
        assert_eq!(deleted.len(), 1, "exactly one row hard-deleted");
        assert_eq!(deleted[0].0, expired.id, "returns the expired message id");

        // The expired message is gone.
        let gone = repo.get(expired.id).await.expect("get expired");
        assert!(gone.is_none(), "expired message is hard-deleted");

        // The future message is still present.
        let present = repo.get(alive.id).await.expect("get alive");
        assert!(present.is_some(), "future-expiry message still exists");

        // Cleanup.
        sqlx::query("DELETE FROM messages WHERE room_id = $1")
            .bind(r.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(r.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(me.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
