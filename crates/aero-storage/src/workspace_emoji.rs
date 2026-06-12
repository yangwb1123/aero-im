//! Per-workspace custom emoji repository (migration 0115).
//!
//! Each workspace can define named custom emoji (`:shipit:`) backed by an
//! already-uploaded image blob. Unlike the existing [`crate::EmojiRepo`] (which
//! is also workspace-scoped), this table uses UUID primary keys and records the
//! `created_by` participant so admin-delete gates can be enforced. The two tables
//! serve the same purpose at different levels of history (the emoji repo is the
//! primary workspace emoji store; this is an additive, independently-operable
//! alternative seeded in migration 0115).

use aero_common::{Error, WorkspaceId};
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

    /// Create a custom emoji for `workspace`. Name is normalised to lowercase
    /// with leading/trailing whitespace removed. Returns a `UniqueViolation`
    /// sqlx error on a duplicate `(workspace, name)` pair — callers map it to
    /// `409 Conflict`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert, including a
    /// unique-constraint violation when the name already exists in the workspace.
    pub async fn create(
        &self,
        workspace: WorkspaceId,
        name: &str,
        blob_id: Uuid,
        created_by: Uuid,
    ) -> Result<WorkspaceEmojiRow, Error> {
        let row: WorkspaceEmojiRow = sqlx::query_as(
            "INSERT INTO workspace_emoji (workspace_id, name, blob_id, created_by) \
             VALUES ($1, $2, $3, $4) \
             RETURNING id, workspace_id, name, blob_id, created_by",
        )
        .bind(workspace.to_uuid())
        .bind(name.trim().to_lowercase())
        .bind(blob_id)
        .bind(created_by)
        .fetch_one(&self.pg)
        .await
        .map_err(Error::from)?;
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

    /// Delete a custom emoji by id, scoped to the workspace (so a cross-tenant
    /// delete is a no-op). Returns `true` iff a row was removed.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn delete(&self, id: Uuid, workspace: WorkspaceId) -> Result<bool, Error> {
        let r = sqlx::query(
            "DELETE FROM workspace_emoji WHERE id = $1 AND workspace_id = $2",
        )
        .bind(id)
        .bind(workspace.to_uuid())
        .execute(&self.pg)
        .await
        .map_err(Error::from)?;
        Ok(r.rows_affected() > 0)
    }

    /// Look up a single emoji by id (workspace-agnostic). Used by the delete
    /// handler to resolve the creator before the auth guard runs.
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
        sqlx::query("INSERT INTO workspaces (id, name, slug, created_by, created_at) VALUES ($1,$2,$3,$4, now())")
            .bind(ws.to_uuid())
            .bind("WE Test WS")
            .bind(format!("we-{}", ws.to_uuid()))
            .bind(actor)
            .execute(p)
            .await
            .expect("insert workspace");
        let blob_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO blobs (id, owner_id, kind, name, mime, size, storage_key) \
             VALUES ($1, $2, 'image', 'emoji.png', 'image/png', 512, $3)",
        )
        .bind(blob_id)
        .bind(actor)
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

        let created = repo.create(ws, "shipit", blob_id, actor).await.unwrap();
        assert_eq!(created.name, "shipit");
        assert_eq!(created.created_by, Some(actor));

        let list = repo.list_for_workspace(ws).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "shipit");

        let fetched = repo.get(created.id).await.unwrap().expect("emoji exists");
        assert_eq!(fetched.id, created.id);

        assert!(repo.delete(created.id, ws).await.unwrap());
        assert!(!repo.delete(created.id, ws).await.unwrap(), "idempotent");

        let list2 = repo.list_for_workspace(ws).await.unwrap();
        assert!(list2.is_empty());
    }
}
