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

    /// Cross-room full-text + trigram search over rooms the participant may
    /// currently access.
    ///
    /// Authorization is fail-closed across room membership, workspace
    /// membership, account/deactivation state, and mandatory 2FA.
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
               JOIN rooms r ON r.id = m.room_id
               JOIN workspaces w ON w.id = r.workspace_id
               JOIN room_members rm
                 ON rm.room_id = m.room_id AND rm.participant_id = $1
               JOIN workspace_members wm
                 ON wm.workspace_id = r.workspace_id AND wm.participant_id = $1
               JOIN participants viewer
                 ON viewer.id = $1 AND viewer.deleted_at IS NULL
               LEFT JOIN workspace_deactivations deactivated
                 ON deactivated.workspace_id = r.workspace_id
                AND deactivated.participant_id = $1
               LEFT JOIN totp_secrets totp ON totp.participant_id = $1
               WHERE deactivated.participant_id IS NULL
                 AND (
                     viewer.kind <> 'human'
                     OR NOT w.require_2fa
                     OR COALESCE(totp.activated, false)
                 )
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

    /// Cross-room search scoped to one workspace with the same effective-access
    /// boundary as [`Self::search_all_rooms`].
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
               JOIN workspaces w ON w.id = r.workspace_id
               JOIN room_members rm
                 ON rm.room_id = m.room_id AND rm.participant_id = $1
               JOIN workspace_members wm
                 ON wm.workspace_id = r.workspace_id AND wm.participant_id = $1
               JOIN participants viewer
                 ON viewer.id = $1 AND viewer.deleted_at IS NULL
               LEFT JOIN workspace_deactivations deactivated
                 ON deactivated.workspace_id = r.workspace_id
                AND deactivated.participant_id = $1
               LEFT JOIN totp_secrets totp ON totp.participant_id = $1
               WHERE deactivated.participant_id IS NULL
                 AND (
                     viewer.kind <> 'human'
                     OR NOT w.require_2fa
                     OR COALESCE(totp.activated, false)
                 )
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

    /// Workspace-scoped vector search over effective current room access.
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
               JOIN workspaces w ON w.id = r.workspace_id
               JOIN room_members rm
                 ON rm.room_id = m.room_id AND rm.participant_id = $1
               JOIN workspace_members wm
                 ON wm.workspace_id = r.workspace_id AND wm.participant_id = $1
               JOIN participants viewer
                 ON viewer.id = $1 AND viewer.deleted_at IS NULL
               LEFT JOIN workspace_deactivations deactivated
                 ON deactivated.workspace_id = r.workspace_id
                AND deactivated.participant_id = $1
               LEFT JOIN totp_secrets totp ON totp.participant_id = $1
               WHERE deactivated.participant_id IS NULL
                 AND (
                     viewer.kind <> 'human'
                     OR NOT w.require_2fa
                     OR COALESCE(totp.activated, false)
                 )
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

    /// Workspace-scoped candidate pool over effective current room access.
    pub async fn fts_candidates_workspace(
        &self,
        pid: ParticipantId,
        ws: WorkspaceId,
        query: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        self.fts_candidates_workspace_with(pid, ws, query, limit, None)
            .await
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
               JOIN workspaces w ON w.id = r.workspace_id
               JOIN room_members rm
                 ON rm.room_id = m.room_id AND rm.participant_id = $1
               JOIN workspace_members wm
                 ON wm.workspace_id = r.workspace_id AND wm.participant_id = $1
               JOIN participants viewer
                 ON viewer.id = $1 AND viewer.deleted_at IS NULL
               LEFT JOIN workspace_deactivations deactivated
                 ON deactivated.workspace_id = r.workspace_id
                AND deactivated.participant_id = $1
               LEFT JOIN totp_secrets totp ON totp.participant_id = $1
               WHERE deactivated.participant_id IS NULL
                 AND (
                     viewer.kind <> 'human'
                     OR NOT w.require_2fa
                     OR COALESCE(totp.activated, false)
                 )
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

#[cfg(test)]
mod policy_tests {
    use super::*;
    use crate::{
        parse_search_query, AdvancedSearchRepo, NewMessage, RoomRepo, WorkspaceFileRepo,
        WorkspaceRepo,
    };
    use aero_common::{BlobId, Block, FileKind, RoomKind, WorkspaceRole};

    fn pool() -> sqlx::PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(3)
            .connect_lazy(&url)
            .expect("valid DATABASE_URL")
    }

    async fn participant(pool: &sqlx::PgPool, label: &str) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, $2, $3)")
            .bind(id.to_uuid())
            .bind("human")
            .bind(format!("cross-room-policy-{label}-{id}"))
            .execute(pool)
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL with migrations applied"]
    async fn cross_room_content_queries_enforce_effective_workspace_access() {
        let pool = pool();
        let workspaces = WorkspaceRepo::new(pool.clone());
        let rooms = RoomRepo::new(pool.clone());
        let messages = MessageRepo::new(pool.clone());
        let advanced = AdvancedSearchRepo::new(pool.clone());
        let files = WorkspaceFileRepo::new(pool.clone());
        let owner = participant(&pool, "owner").await;
        let viewer = participant(&pool, "viewer").await;
        let workspace = workspaces
            .create(
                format!("cross-room-policy-{owner}"),
                format!("cross-room-policy-{owner}"),
                owner,
            )
            .await
            .unwrap()
            .id;
        workspaces
            .add_member(workspace, viewer, WorkspaceRole::Member)
            .await
            .unwrap();
        let room = rooms
            .create_in_workspace(
                workspace,
                RoomKind::Group,
                Some(format!("cross-room-policy-{owner}")),
                owner,
            )
            .await
            .unwrap()
            .id;
        rooms.add_member(room, viewer).await.unwrap();

        let term = format!("policy{}", uuid::Uuid::new_v4().simple());
        messages
            .insert(NewMessage {
                room_id: room,
                sender_id: owner,
                blocks: vec![
                    Block::text(format!("{term} effective access")),
                    Block::File {
                        blob_id: BlobId::new(),
                        kind: FileKind::Document,
                        name: format!("{term}.pdf"),
                        size: 1,
                    },
                ],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .unwrap();

        assert_eq!(
            messages
                .search_all_rooms_in_workspace(viewer, workspace, &term, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            messages
                .fts_candidates_workspace(viewer, workspace, &term, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            messages
                .recent_workspace(viewer, workspace, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            advanced
                .search(viewer, workspace, &parse_search_query(&term), 10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            files
                .list_for_workspace(workspace, viewer, None, Some(&term), 10, 0)
                .await
                .unwrap()
                .len(),
            1
        );
        // Exercise the vector SQL even though this fixture has no embedding.
        assert!(messages
            .search_vector_workspace(viewer, workspace, vec![0.0; 1024], 10)
            .await
            .unwrap()
            .is_empty());

        sqlx::query(
            r"INSERT INTO workspace_deactivations
                  (workspace_id, participant_id, deactivated_by)
               VALUES ($1, $2, $3)",
        )
        .bind(workspace.to_uuid())
        .bind(viewer.to_uuid())
        .bind(owner.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
        assert!(messages
            .search_all_rooms(viewer, &term, 10)
            .await
            .unwrap()
            .is_empty());
        assert!(messages
            .recent_workspace(viewer, workspace, 10)
            .await
            .unwrap()
            .is_empty());
        assert!(advanced
            .search(viewer, workspace, &parse_search_query(&term), 10)
            .await
            .unwrap()
            .is_empty());
        assert!(files
            .list_for_workspace(workspace, viewer, None, Some(&term), 10, 0)
            .await
            .unwrap()
            .is_empty());

        sqlx::query(
            "DELETE FROM workspace_deactivations WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(viewer.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r"INSERT INTO totp_secrets
                  (participant_id, secret, activated, activated_at)
               VALUES ($1, 'cross-room-owner-secret', true, now())",
        )
        .bind(owner.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE workspaces SET require_2fa = true WHERE id = $1")
            .bind(workspace.to_uuid())
            .execute(&pool)
            .await
            .unwrap();
        assert!(messages
            .search_all_rooms(viewer, &term, 10)
            .await
            .unwrap()
            .is_empty());
        assert!(advanced
            .search(viewer, workspace, &parse_search_query(&term), 10)
            .await
            .unwrap()
            .is_empty());
        assert!(files
            .list_for_workspace(workspace, viewer, None, Some(&term), 10, 0)
            .await
            .unwrap()
            .is_empty());

        sqlx::query(
            r"INSERT INTO totp_secrets
                  (participant_id, secret, activated, activated_at)
               VALUES ($1, 'cross-room-policy-secret', true, now())",
        )
        .bind(viewer.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            messages
                .search_all_rooms(viewer, &term, 10)
                .await
                .unwrap()
                .len(),
            1
        );

        sqlx::query(
            "DELETE FROM workspace_members WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(viewer.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
        assert!(messages
            .search_all_rooms(viewer, &term, 10)
            .await
            .unwrap()
            .is_empty());
        assert!(advanced
            .search(viewer, workspace, &parse_search_query(&term), 10)
            .await
            .unwrap()
            .is_empty());
        assert!(files
            .list_for_workspace(workspace, viewer, None, Some(&term), 10, 0)
            .await
            .unwrap()
            .is_empty());

        workspaces
            .add_member(workspace, viewer, WorkspaceRole::Member)
            .await
            .unwrap();
        sqlx::query("UPDATE participants SET deleted_at = now() WHERE id = $1")
            .bind(viewer.to_uuid())
            .execute(&pool)
            .await
            .unwrap();
        assert!(messages
            .search_all_rooms(viewer, &term, 10)
            .await
            .unwrap()
            .is_empty());
        assert!(advanced
            .search(viewer, workspace, &parse_search_query(&term), 10)
            .await
            .unwrap()
            .is_empty());
        assert!(files
            .list_for_workspace(workspace, viewer, None, Some(&term), 10, 0)
            .await
            .unwrap()
            .is_empty());
    }
}
