//! Message repository.
//!
//! P2 additions: searchable_text column populated on insert/edit, edit + soft-delete,
//! full-text + trigram + vector search, embedding update for the AI worker.

use aero_common::{Block, Message, MessageId, ParticipantId, RoomId, WorkspaceId};
use pgvector::Vector;
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
                 (id, room_id, sender_id, blocks, reply_to, metadata, searchable_text, created_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"#,
        )
        .bind(id.to_uuid())
        .bind(new.room_id.to_uuid())
        .bind(new.sender_id.to_uuid())
        .bind(&blocks_json)
        .bind(new.reply_to.map(|m| m.to_uuid()))
        .bind(&new.metadata)
        .bind(&searchable)
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

    /// Fetch a single message by id (including soft-deleted, caller must filter).
    pub async fn get(&self, id: MessageId) -> Result<Option<Message>, sqlx::Error> {
        let row = sqlx::query_as::<_, MessageRow>(
            r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at
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
            RETURNING id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at"#,
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

        // Read + lock the row; capture its blocks so we can extract blob ids.
        let row = sqlx::query_as::<_, (serde_json::Value,)>(
            "SELECT blocks FROM messages WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some((blocks_json,)) = row else {
            tx.commit().await?;
            return Ok(false);
        };
        let blob_ids = attached_blob_ids(&blocks_json);

        sqlx::query(
            r#"UPDATE messages
                  SET deleted_at = NOW(), blocks = '[]'::jsonb, searchable_text = '', embedding = NULL
               WHERE id = $1 AND deleted_at IS NULL"#,
        )
        .bind(id.to_uuid())
        .execute(&mut *tx)
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
            .fetch_one(&mut *tx)
            .await?
            .0;
            if !still_referenced {
                sqlx::query(
                    r"INSERT INTO blob_gc_queue (blob_id) VALUES ($1) ON CONFLICT (blob_id) DO NOTHING",
                )
                .bind(blob.to_uuid())
                .execute(&mut *tx)
                .await?;
            }
        }

        tx.commit().await?;
        Ok(true)
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
            RETURNING id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at"#,
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
    pub async fn list_since(
        &self,
        room: RoomId,
        after: MessageId,
        limit: i64,
    ) -> Result<Vec<Message>, sqlx::Error> {
        let limit = clamp_page_limit(limit);
        let rows = sqlx::query_as::<_, MessageRow>(
            r"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at
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

    /// Find messages in a room missing an embedding. Used by the embedding worker
    /// at startup to catch up on backlog before the live queue takes over.
    pub async fn list_without_embedding(&self, limit: i64) -> Result<Vec<Message>, sqlx::Error> {
        let limit = limit.clamp(1, 1000);
        let rows = sqlx::query_as::<_, MessageRow>(
            r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at
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
                 m.created_at, m.edited_at, m.deleted_at,
                 GREATEST(
                   ts_rank(m.search_tsv, websearch_to_tsquery('simple', $2)),
                   similarity(m.searchable_text, $2)
                 ) AS score
               FROM messages m
               WHERE m.room_id = $1
                 AND m.deleted_at IS NULL
                 AND (
                   m.search_tsv @@ websearch_to_tsquery('simple', $2)
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
        let rows = sqlx::query_as::<_, ScoredMessageRow>(
            r#"SELECT
                 m.id, m.room_id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                 m.created_at, m.edited_at, m.deleted_at,
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
            r"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at
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
        // Postgres has no `max(uuid)` aggregate, so the newest reply is taken via
        // an ordered single-row read (ids are time-sortable ULIDs stored as UUID,
        // so `ORDER BY id DESC LIMIT 1` is the most recent reply).
        let count = sqlx::query_as::<_, (i64,)>(
            r"SELECT COUNT(*) FROM messages WHERE reply_to = $1 AND deleted_at IS NULL",
        )
        .bind(root.to_uuid())
        .fetch_one(&self.pool)
        .await?;

        let last = sqlx::query_as::<_, (uuid::Uuid, time::OffsetDateTime)>(
            r"SELECT id, created_at
               FROM messages
               WHERE reply_to = $1 AND deleted_at IS NULL
               ORDER BY id DESC
               LIMIT 1",
        )
        .bind(root.to_uuid())
        .fetch_optional(&self.pool)
        .await?;

        let repliers = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT DISTINCT sender_id
               FROM messages
               WHERE reply_to = $1 AND deleted_at IS NULL
               LIMIT 8",
        )
        .bind(root.to_uuid())
        .fetch_all(&self.pool)
        .await?;

        Ok(aero_common::ThreadSummary {
            root_id: root,
            reply_count: u32::try_from(count.0).unwrap_or(u32::MAX),
            repliers: repliers
                .into_iter()
                .map(|(u,)| ParticipantId::from_uuid(u))
                .collect(),
            last_reply_id: last.map(|(id, _)| MessageId::from_uuid(id)),
            last_reply_at: last.map(|(_, at)| at),
        })
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
                 m.created_at, m.edited_at, m.deleted_at,
                 GREATEST(
                   ts_rank(m.search_tsv, websearch_to_tsquery('simple', $2)),
                   similarity(m.searchable_text, $2)
                 ) AS score
               FROM messages m
               JOIN room_members rm
                 ON rm.room_id = m.room_id AND rm.participant_id = $1
               WHERE m.deleted_at IS NULL
                 AND (
                   m.search_tsv @@ websearch_to_tsquery('simple', $2)
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
                 m.created_at, m.edited_at, m.deleted_at,
                 GREATEST(
                   ts_rank(m.search_tsv, websearch_to_tsquery('simple', $2)),
                   similarity(m.searchable_text, $2)
                 ) AS score
               FROM messages m
               JOIN room_members rm
                 ON rm.room_id = m.room_id AND rm.participant_id = $1
               WHERE m.deleted_at IS NULL
                 AND m.room_id IN (SELECT id FROM rooms WHERE workspace_id = $4)
                 AND (
                   m.search_tsv @@ websearch_to_tsquery('simple', $2)
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
        let rows = sqlx::query_as::<_, ScoredMessageRow>(
            r"SELECT
                 m.id, m.room_id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                 m.created_at, m.edited_at, m.deleted_at,
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
            r"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at
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
            r"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at
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

/// Clamp a caller-supplied page size into the safe `[1, 200]` window used by
/// the keyset pagination queries ([`MessageRepo::list_since`]), matching the
/// bound `list_recent` applies inline. Factored out so the cursor/ordering
/// contract is unit-testable without a live database.
#[inline]
fn clamp_page_limit(limit: i64) -> i64 {
    limit.clamp(1, 200)
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
    use super::{attached_blob_ids, clamp_page_limit};
    use aero_common::{BlobId, Block, FileKind, MessageId};
    use ulid::Ulid;

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
            })
            .await
            .expect("insert mine");
        repo.insert(NewMessage {
            room_id: theirs,
            sender_id: other,
            blocks: vec![Block::text(format!("secret {needle} stuff"))],
            reply_to: None,
            metadata: serde_json::json!({}),
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
}
