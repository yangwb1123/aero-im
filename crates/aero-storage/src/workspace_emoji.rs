//! Per-workspace custom emoji repository (migration 0115).
//!
//! Each workspace can define named custom emoji (`:shipit:`) backed by an
//! already-uploaded image blob. Unlike the existing [`crate::EmojiRepo`] (which
//! is also workspace-scoped), this table uses UUID primary keys and records the
//! `created_by` administrator for attribution. The two tables serve the same
//! purpose at different levels of history (the emoji repo is the primary
//! workspace emoji store; this is an additive, independently-operable
//! alternative seeded in migration 0115).

use aero_common::{Error, ParticipantId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// A custom emoji row returned by the workspace emoji repo.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct WorkspaceEmojiRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub blob_id: Uuid,
    pub created_by: Option<Uuid>,
}

/// Repository over the `workspace_emoji` table (migration 0115).
#[derive(Clone)]
pub struct WorkspaceEmojiRepo {
    pub pg: PgPool,
}

impl WorkspaceEmojiRepo {
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Create a custom emoji for `workspace` while `actor` remains an effective
    /// Owner/Admin. The backing blob is locked and must be a finalized,
    /// non-GC-queued image in the same immutable workspace scope.
    ///
    /// # Errors
    /// Returns [`Error::Invalid`] for an invalid normalized name,
    /// [`Error::Forbidden`] unless `actor` is a current effective Owner/Admin,
    /// [`Error::NotFound`] for an unavailable or cross-tenant blob, and
    /// [`Error::Conflict`] when the normalized name already exists.
    pub async fn create_authorized(
        &self,
        workspace: WorkspaceId,
        name: &str,
        blob_id: Uuid,
        actor: ParticipantId,
    ) -> Result<WorkspaceEmojiRow, Error> {
        let name = name.trim().to_lowercase();
        if !crate::emoji::is_valid_emoji_name(&name) {
            return Err(Error::Invalid(
                "emoji name must be 1-64 chars of lowercase [a-z0-9_-]".into(),
            ));
        }

        let mut tx = self.pg.begin().await?;
        crate::workspace::authz::assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        crate::emoji::lock_usable_emoji_blob(&mut tx, workspace, blob_id).await?;
        let row: Option<WorkspaceEmojiRow> = sqlx::query_as(
            "INSERT INTO workspace_emoji (workspace_id, name, blob_id, created_by) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (workspace_id, name) DO NOTHING \
             RETURNING id, workspace_id, name, blob_id, created_by",
        )
        .bind(workspace.to_uuid())
        .bind(&name)
        .bind(blob_id)
        .bind(actor.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let row = row.ok_or_else(|| {
            Error::Conflict(format!("emoji ':{name}:' already exists in this workspace"))
        })?;
        tx.commit().await?;
        Ok(row)
    }

    /// List all custom emoji for a workspace, ordered alphabetically by name.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_workspace(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<WorkspaceEmojiRow>, Error> {
        let rows: Vec<WorkspaceEmojiRow> = sqlx::query_as(
            "SELECT id, workspace_id, name, blob_id, created_by \
             FROM workspace_emoji WHERE workspace_id = $1 ORDER BY name",
        )
        .bind(workspace.to_uuid())
        .fetch_all(&self.pg)
        .await
        .map_err(Error::from)?;
        Ok(rows)
    }

    /// Delete a custom emoji by id, scoped to the workspace and guarded by a
    /// transactionally current effective Owner/Admin decision.
    ///
    /// # Errors
    /// Returns [`Error::Forbidden`] unless `actor` remains a current effective
    /// Owner/Admin, [`Error::NotFound`] when `id` is absent from the requested
    /// workspace, and propagates storage failures.
    pub async fn delete_authorized(
        &self,
        id: Uuid,
        workspace: WorkspaceId,
        actor: ParticipantId,
    ) -> Result<(), Error> {
        let mut tx = self.pg.begin().await?;
        crate::workspace::authz::assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        let locked = sqlx::query_scalar::<_, bool>(
            "SELECT true
               FROM workspace_emoji
              WHERE id = $1 AND workspace_id = $2
              FOR UPDATE",
        )
        .bind(id)
        .bind(workspace.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !locked {
            return Err(Error::NotFound("emoji".into()));
        }

        let r = sqlx::query("DELETE FROM workspace_emoji WHERE id = $1 AND workspace_id = $2")
            .bind(id)
            .bind(workspace.to_uuid())
            .execute(&mut *tx)
            .await
            .map_err(Error::from)?;
        if r.rows_affected() != 1 {
            return Err(Error::NotFound("emoji".into()));
        }
        tx.commit().await?;
        Ok(())
    }

    /// Look up a single emoji by id (workspace-agnostic).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(&self, id: Uuid) -> Result<Option<WorkspaceEmojiRow>, Error> {
        let row: Option<WorkspaceEmojiRow> = sqlx::query_as(
            "SELECT id, workspace_id, name, blob_id, created_by \
             FROM workspace_emoji WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pg)
        .await
        .map_err(Error::from)?;
        Ok(row)
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored workspace_emoji_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::WorkspaceId;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway participant + workspace + blob so tests are self-contained.
    async fn fixture(p: &PgPool) -> (WorkspaceId, Uuid, Uuid) {
        let actor = Uuid::new_v4();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(actor)
            .bind(format!("we-actor-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");
        let ws = WorkspaceId::new();
        let mut tx = p.begin().await.expect("begin workspace fixture");
        sqlx::query("INSERT INTO workspaces (id, name, slug, created_by, created_at) VALUES ($1,$2,$3,$4, now())")
            .bind(ws.to_uuid())
            .bind("WE Test WS")
            .bind(format!("we-{}", ws.to_uuid()))
            .bind(actor)
            .execute(&mut *tx)
            .await
            .expect("insert workspace");
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1, $2, 'owner')",
        )
        .bind(ws.to_uuid())
        .bind(actor)
        .execute(&mut *tx)
        .await
        .expect("insert workspace owner");
        tx.commit().await.expect("commit workspace fixture");
        let blob_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO blobs
                 (id, owner_id, workspace_id, kind, name, mime, size, storage_key,
                  finalized_at)
             VALUES ($1, $2, $3, 'image', 'emoji.png', 'image/png', 512, $4, now())",
        )
        .bind(blob_id)
        .bind(actor)
        .bind(ws.to_uuid())
        .bind(format!("test-key-{blob_id}"))
        .execute(p)
        .await
        .expect("insert blob");
        (ws, actor, blob_id)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn workspace_emoji_create_list_delete() {
        let p = pool();
        let repo = WorkspaceEmojiRepo::new(p.clone());
        let (ws, actor, blob_id) = fixture(&p).await;

        let created = repo
            .create_authorized(ws, "shipit", blob_id, ParticipantId::from_uuid(actor))
            .await
            .unwrap();
        assert_eq!(created.name, "shipit");
        assert_eq!(created.created_by, Some(actor));

        let list = repo.list_for_workspace(ws).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "shipit");

        let fetched = repo.get(created.id).await.unwrap().expect("emoji exists");
        assert_eq!(fetched.id, created.id);

        repo.delete_authorized(created.id, ws, ParticipantId::from_uuid(actor))
            .await
            .unwrap();
        assert!(matches!(
            repo.delete_authorized(created.id, ws, ParticipantId::from_uuid(actor))
                .await,
            Err(Error::NotFound(_))
        ));

        let list2 = repo.list_for_workspace(ws).await.unwrap();
        assert!(list2.is_empty());
    }
}
