//! Full-text, trigram, and vector search across rooms and workspaces.
//!
//! Extracted from `message/orig.rs` as part of REFACTOR_PLAN.md Step 2.

use aero_common::{ParticipantId, RoomId, WorkspaceId};
use pgvector::Vector;

use super::{MessageRepo, SearchHit};
use crate::message::orig::ScoredMessageRow;

impl MessageRepo {
    /// Full-text + trigram search in a room.
    pub async fn search_fts(
        &self,
        room: RoomId,
        query: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        let limit = limit.clamp(1, 100);
        let rows = sqlx::query_as::<_, ScoredMessageRow>(
            r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata,
                      created_at, edited_at, deleted_at, expires_at, version,
                      GREATEST(fts_score, trigram_score) AS score
               FROM (
                 -- FTS branch: uses GIN idx_message_search_tsv
                 SELECT m.id, m.room_id, m.sender_id, m.blocks, m.reply_to,
                        m.metadata, m.created_at, m.edited_at, m.deleted_at,
                        m.expires_at, m.version,
                        ts_rank(m.search_tsv,
                          websearch_to_tsquery('english', f_unaccent($2))) AS fts_score,
                        0::real AS trigram_score
                 FROM messages m
                 WHERE m.room_id = $1
                   AND m.deleted_at IS NULL
                   AND (m.expires_at IS NULL OR m.expires_at > now())
                   AND m.search_tsv @@ websearch_to_tsquery('english', f_unaccent($2))
                 UNION ALL
                 -- Trigram branch: uses gin_trgm idx_message_searchable_text
                 SELECT m.id, m.room_id, m.sender_id, m.blocks, m.reply_to,
                        m.metadata, m.created_at, m.edited_at, m.deleted_at,
                        m.expires_at, m.version,
                        0::real AS fts_score,
                        similarity(m.searchable_text, $2) AS trigram_score
                 FROM messages m
                 WHERE m.room_id = $1
                   AND m.deleted_at IS NULL
                   AND (m.expires_at IS NULL OR m.expires_at > now())
                   AND m.searchable_text % $2
               ) sub
               ORDER BY score DESC, id DESC
               LIMIT $3"#,
        )
        .bind(room.to_uuid())
        .bind(query)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(SearchHit::from).collect())
    }

    /// Semantic (cosine-distance) search in a room.
    pub async fn search_vector(
        &self,
        room: RoomId,
        embedding: Vec<f32>,
        limit: i64,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        let limit = limit.clamp(1, 100);
        let v = Vector::from(embedding);
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT set_config('hnsw.ef_search', $1, true)")
            .bind(super::orig::hnsw_ef_search(limit).to_string())
            .execute(&mut *tx)
            .await?;
        let rows = sqlx::query_as::<_, ScoredMessageRow>(
            r#"SELECT
                 m.id, m.room_id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                 m.created_at, m.edited_at, m.deleted_at, m.expires_at, m.version,
                 (1 - (m.embedding <=> $2))::real AS score
               FROM messages m
               WHERE m.room_id = $1
                 AND m.deleted_at IS NULL AND (m.expires_at IS NULL OR m.expires_at > now())
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

    /// Token-level candidate pool (RRF fusion): full-text + trigram.
    pub async fn fts_candidates(
        &self,
        room: RoomId,
        query: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        self.fts_candidates_with(room, query, limit, None).await
    }

    /// Token-level candidate pool with optional sender filter.
    async fn fts_candidates_with(
        &self,
        room: RoomId,
        query: &str,
        limit: i64,
        from: Option<ParticipantId>,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        let limit = limit.clamp(1, 1000);
        let rows = sqlx::query_as::<_, ScoredMessageRow>(
            r#"SELECT
                 m.id, m.room_id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                 m.created_at, m.edited_at, m.deleted_at, m.expires_at, m.version,
                 GREATEST(
                   ts_rank(m.search_tsv, websearch_to_tsquery('english', f_unaccent($3))),
                   similarity(m.searchable_text, $3)
                 ) AS score
               FROM messages m
               WHERE m.room_id = $1
                 AND m.deleted_at IS NULL AND (m.expires_at IS NULL OR m.expires_at > now())
                 AND (
                   m.search_tsv @@ websearch_to_tsquery('english', f_unaccent($3))
                   OR m.searchable_text % $3
                 )
                 AND ($4::uuid IS NULL OR m.sender_id = $4::uuid)
               ORDER BY score DESC, m.id DESC
               LIMIT $2"#,
        )
        .bind(room.to_uuid())
        .bind(limit)
        .bind(query)
        .bind(from.map(|p| p.to_uuid()))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(SearchHit::from).collect())
    }

    /// Cross-room full-text + trigram search (workspace-wide, members only).
    pub async fn search_all_rooms(
        &self,
        pid: ParticipantId,
        query: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        let limit = limit.clamp(1, 100);
        let rows = sqlx::query_as::<_, ScoredMessageRow>(
            r#"SELECT
                 m.id, m.room_id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                 m.created_at, m.edited_at, m.deleted_at, m.expires_at, m.version,
                 GREATEST(
                   ts_rank(m.search_tsv, websearch_to_tsquery('english', f_unaccent($2))),
                   similarity(m.searchable_text, $2)
                 ) AS score
               FROM messages m
               JOIN room_members rm ON rm.room_id = m.room_id
               WHERE rm.participant_id = $1
                 AND m.deleted_at IS NULL AND (m.expires_at IS NULL OR m.expires_at > now())
                 AND (
                   m.search_tsv @@ websearch_to_tsquery('english', f_unaccent($2))
                   OR m.searchable_text % $2
                 )
               ORDER BY score DESC, m.id DESC
               LIMIT $3"#,
        )
        .bind(pid.to_uuid())
        .bind(query)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(SearchHit::from).collect())
    }

    /// Cross-room search scoped to a workspace.
    pub async fn search_all_rooms_in_workspace(
        &self,
        pid: ParticipantId,
        ws: WorkspaceId,
        query: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        let limit = limit.clamp(1, 100);
        let rows = sqlx::query_as::<_, ScoredMessageRow>(
            r#"SELECT
                 m.id, m.room_id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                 m.created_at, m.edited_at, m.deleted_at, m.expires_at, m.version,
                 GREATEST(
                   ts_rank(m.search_tsv, websearch_to_tsquery('english', f_unaccent($2))),
                   similarity(m.searchable_text, $2)
                 ) AS score
               FROM messages m
               JOIN rooms r ON r.id = m.room_id
               JOIN room_members rm ON rm.room_id = m.room_id
               WHERE rm.participant_id = $1
                 AND r.workspace_id = $4
                 AND m.deleted_at IS NULL AND (m.expires_at IS NULL OR m.expires_at > now())
                 AND (
                   m.search_tsv @@ websearch_to_tsquery('english', f_unaccent($2))
                   OR m.searchable_text % $2
                 )
               ORDER BY score DESC, m.id DESC
               LIMIT $3"#,
        )
        .bind(pid.to_uuid())
        .bind(query)
        .bind(limit)
        .bind(ws.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(SearchHit::from).collect())
    }

    /// Workspace-scoped vector search.
    pub async fn search_vector_workspace(
        &self,
        pid: ParticipantId,
        ws: WorkspaceId,
        embedding: Vec<f32>,
        limit: i64,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        let limit = limit.clamp(1, 100);
        let v = Vector::from(embedding);
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT set_config('hnsw.ef_search', $1, true)")
            .bind(super::orig::hnsw_ef_search(limit).to_string())
            .execute(&mut *tx)
            .await?;
        let rows = sqlx::query_as::<_, ScoredMessageRow>(
            r#"SELECT
                 m.id, m.room_id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                 m.created_at, m.edited_at, m.deleted_at, m.expires_at, m.version,
                 (1 - (m.embedding <=> $2))::real AS score
               FROM messages m
               JOIN rooms r ON r.id = m.room_id
               JOIN room_members rm ON rm.room_id = m.room_id
               WHERE rm.participant_id = $1
                 AND r.workspace_id = $4
                 AND m.deleted_at IS NULL AND (m.expires_at IS NULL OR m.expires_at > now())
                 AND m.embedding IS NOT NULL
               ORDER BY m.embedding <=> $2
               LIMIT $3"#,
        )
        .bind(pid.to_uuid())
        .bind(v)
        .bind(limit)
        .bind(ws.to_uuid())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(SearchHit::from).collect())
    }

    /// Workspace-scoped candidate pool.
    pub async fn fts_candidates_workspace(
        &self,
        pid: ParticipantId,
        ws: WorkspaceId,
        query: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        self.fts_candidates_workspace_with(pid, ws, query, limit, None).await
    }

    /// Workspace-scoped candidate pool with optional sender filter.
    async fn fts_candidates_workspace_with(
        &self,
        pid: ParticipantId,
        ws: WorkspaceId,
        query: &str,
        limit: i64,
        from: Option<ParticipantId>,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        let limit = limit.clamp(1, 1000);
        let rows = sqlx::query_as::<_, ScoredMessageRow>(
            r#"SELECT
                 m.id, m.room_id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                 m.created_at, m.edited_at, m.deleted_at, m.expires_at, m.version,
                 GREATEST(
                   ts_rank(m.search_tsv, websearch_to_tsquery('english', f_unaccent($3))),
                   similarity(m.searchable_text, $3)
                 ) AS score
               FROM messages m
               JOIN rooms r ON r.id = m.room_id
               JOIN room_members rm ON rm.room_id = m.room_id
               WHERE rm.participant_id = $1
                 AND r.workspace_id = $5
                 AND m.deleted_at IS NULL AND (m.expires_at IS NULL OR m.expires_at > now())
                 AND (
                   m.search_tsv @@ websearch_to_tsquery('english', f_unaccent($3))
                   OR m.searchable_text % $3
                 )
                 AND ($4::uuid IS NULL OR m.sender_id = $4::uuid)
               ORDER BY score DESC, m.id DESC
               LIMIT $2"#,
        )
        .bind(pid.to_uuid())
        .bind(limit)
        .bind(query)
        .bind(from.map(|p| p.to_uuid()))
        .bind(ws.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(SearchHit::from).collect())
    }
}
