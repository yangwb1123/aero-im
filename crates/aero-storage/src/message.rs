//! Message repository.
//!
//! P2 additions: searchable_text column populated on insert/edit, edit + soft-delete,
//! full-text + trigram + vector search, embedding update for the AI worker.

use aero_common::{Block, Message, MessageId, ParticipantId, RoomId};
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
    pub async fn soft_delete(&self, id: MessageId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r#"UPDATE messages
                  SET deleted_at = NOW(), blocks = '[]'::jsonb, searchable_text = '', embedding = NULL
               WHERE id = $1 AND deleted_at IS NULL"#,
        )
        .bind(id.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
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
}

fn searchable_of(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter_map(Block::searchable_text)
        .collect::<Vec<_>>()
        .join("\n")
}

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
    use super::clamp_page_limit;
    use aero_common::MessageId;
    use ulid::Ulid;

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
