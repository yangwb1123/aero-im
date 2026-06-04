//! Saved-search repository (per-user, workspace-scoped named queries).
//!
//! Backs `migrations/0032_saved_searches.sql`. A user saves a named search query
//! within a workspace, then lists, re-runs, or deletes it. The query string is
//! stored verbatim; *running* a saved search reuses the existing
//! membership-scoped cross-room search
//! ([`MessageRepo::search_all_rooms_in_workspace`](crate::MessageRepo)), so this
//! repo only owns the saved-query CRUD — it never executes a search itself.
//!
//! Every read/mutate method is owner-scoped (`participant_id` in the `WHERE`), so
//! a caller can only ever see or delete their own saved searches. Purely
//! additive: a NEW [`SavedSearchRepo`]; no existing repo is touched. The
//! [`SavedSearch`] model lives here (and is re-exported from the crate root)
//! rather than in `aero-common`, since it is a storage-layer projection.

use aero_common::{ParticipantId, SavedSearchId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;

/// One saved search — a per-user, workspace-scoped named query.
///
/// A storage-layer projection of a `saved_searches` row. `Serialize` so a handler
/// can hand the row straight back as JSON; `created_at` renders as RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct SavedSearch {
    /// The saved search's unique id.
    pub id: SavedSearchId,
    /// The owner the saved search belongs to (and is scoped to).
    pub participant_id: ParticipantId,
    /// The tenant the saved search is scoped to (its results stay in this one).
    pub workspace_id: WorkspaceId,
    /// Human-readable name the owner gave the saved search.
    pub name: String,
    /// The raw query string, run through the membership-scoped search on demand.
    pub query: String,
    /// When the saved search was created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// The columns a [`SavedSearch`] is built from, in select order. Shared by every
/// query so the row decoding stays in one place.
const COLUMNS: &str = "id, participant_id, workspace_id, name, query, created_at";

type Row = (
    uuid::Uuid,
    uuid::Uuid,
    uuid::Uuid,
    String,
    String,
    time::OffsetDateTime,
);

fn row_to_model(r: Row) -> SavedSearch {
    let (id, participant_id, workspace_id, name, query, created_at) = r;
    SavedSearch {
        id: SavedSearchId::from_uuid(id),
        participant_id: ParticipantId::from_uuid(participant_id),
        workspace_id: WorkspaceId::from_uuid(workspace_id),
        name,
        query,
        created_at,
    }
}

/// Repository over the `saved_searches` table (per-user named queries).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`SavedSearchRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct SavedSearchRepo {
    pool: PgPool,
}

impl SavedSearchRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persist a new saved search for `participant` in `workspace`, returning its
    /// generated id. The caller is responsible for workspace-membership and
    /// name/query validation.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn create(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        name: &str,
        query: &str,
    ) -> Result<SavedSearchId, sqlx::Error> {
        let id = SavedSearchId::new();
        sqlx::query(
            r"INSERT INTO saved_searches (id, participant_id, workspace_id, name, query)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .bind(name)
        .bind(query)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List `participant`'s saved searches in `workspace`, newest first.
    /// Owner-scoped — only the caller's own rows are returned.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
    ) -> Result<Vec<SavedSearch>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM saved_searches
              WHERE participant_id = $1 AND workspace_id = $2
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(participant.to_uuid())
            .bind(workspace.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Fetch one of `participant`'s saved searches by id, or `None` if no such row
    /// exists *for that owner*. Owner-scoped: a stranger's id resolves to `None`,
    /// so this can never surface another user's saved search.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(
        &self,
        id: SavedSearchId,
        participant: ParticipantId,
    ) -> Result<Option<SavedSearch>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS} FROM saved_searches WHERE id = $1 AND participant_id = $2"
        );
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .bind(participant.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// Delete one of `participant`'s saved searches. Returns `true` iff a row was
    /// removed — owner-scoped, so a caller can never delete another user's saved
    /// search, and a second delete (or a stranger's) is a no-op returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn delete(
        &self,
        id: SavedSearchId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result =
            sqlx::query("DELETE FROM saved_searches WHERE id = $1 AND participant_id = $2")
                .bind(id.to_uuid())
                .bind(participant.to_uuid())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored saved_search
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    /// The reserved all-zero default workspace, guaranteed to exist by migration
    /// 0006's backfill — reused so the saved-search rows are well-scoped.
    const DEFAULT_WS: &str = "00000000-0000-0000-0000-000000000000";

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway owner participant so the test is self-contained.
    async fn owner(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("saved-search-owner-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    fn default_ws() -> WorkspaceId {
        WorkspaceId::from_uuid(uuid::Uuid::parse_str(DEFAULT_WS).expect("valid uuid"))
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn saved_search_create_list_get_delete_owner_scoped() {
        let p = pool();
        let repo = SavedSearchRepo::new(p.clone());
        let ws = default_ws();
        let owner = owner(&p).await;
        let stranger = ParticipantId::new();

        // create → list shows it.
        let id = repo
            .create(owner, ws, "deploys", "deploy failed")
            .await
            .unwrap();
        let listed = repo.list_for(owner, ws).await.unwrap();
        assert!(
            listed.iter().any(|s| s.id == id),
            "list shows the saved search"
        );
        let found = listed.iter().find(|s| s.id == id).expect("present");
        assert_eq!(found.name, "deploys");
        assert_eq!(found.query, "deploy failed");

        // get (owner) works; get (stranger) is None.
        let got = repo.get(id, owner).await.unwrap().expect("owner can get");
        assert_eq!(got.id, id);
        assert_eq!(got.query, "deploy failed");
        assert!(
            repo.get(id, stranger).await.unwrap().is_none(),
            "stranger cannot get another user's saved search"
        );

        // A stranger's delete is a no-op; the owner's first delete succeeds, the
        // second is a no-op.
        assert!(
            !repo.delete(id, stranger).await.unwrap(),
            "stranger cannot delete another user's saved search"
        );
        assert!(repo.delete(id, owner).await.unwrap(), "owner deletes");
        assert!(
            !repo.delete(id, owner).await.unwrap(),
            "second delete is a no-op"
        );
        assert!(
            !repo.list_for(owner, ws).await.unwrap().iter().any(|s| s.id == id),
            "deleted saved search leaves the list"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM saved_searches WHERE participant_id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
