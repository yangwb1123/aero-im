//! User-group / @-usergroup repository (workspace-scoped named member groups).
//!
//! Backs `migrations/0033_user_groups.sql`. A workspace member creates a named
//! group (`@designers`) of participants, then lists, resolves, or deletes it and
//! manages its membership. The handle is unique within a workspace, so an
//! `@handle` mention resolves to at most one group via [`UserGroupRepo::resolve`].
//! The mention fan-out into notifications is wired separately and reuses
//! [`UserGroupRepo::resolve`]/[`UserGroupRepo::members`] as its seam — this repo
//! owns only the group CRUD + membership, never the notification side.
//!
//! Purely additive: a NEW [`UserGroupRepo`]; no existing repo is touched. The
//! [`UserGroup`] model lives here (and is re-exported from the crate root) rather
//! than in `aero-common`, since it is a storage-layer projection — mirroring
//! [`SavedSearch`](crate::SavedSearch) and [`ChannelSection`](crate::ChannelSection).

use aero_common::{ParticipantId, UserGroupId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;

/// One user group — a workspace-scoped, named group of member participants.
///
/// A storage-layer projection of a `user_groups` row. `Serialize` so a handler
/// can hand the row straight back as JSON; `created_at` renders as RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct UserGroup {
    /// The group's unique id.
    pub id: UserGroupId,
    /// The tenant the group is scoped to (handles are unique within it).
    pub workspace_id: WorkspaceId,
    /// The URL/mention-safe handle (`designers`), normalized lowercase + trimmed.
    pub handle: String,
    /// Human-readable display name the creator gave the group.
    pub name: String,
    /// The participant who created the group (a delete/manage authority alongside
    /// workspace admins).
    pub created_by: ParticipantId,
    /// When the group was created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// Normalize a raw handle into its canonical, comparable form: trimmed of
/// surrounding whitespace and lowercased. Pure (no I/O) so it can be reused on
/// both the write path (storing `create`'s handle) and the read path
/// ([`UserGroupRepo::resolve`]) and unit-tested offline. Callers are responsible
/// for character/length validation; this only canonicalizes case + whitespace.
#[must_use]
pub fn normalize_handle(handle: &str) -> String {
    handle.trim().to_lowercase()
}

/// The columns a [`UserGroup`] is built from, in select order. Shared by every
/// query so the row decoding stays in one place.
const COLUMNS: &str = "id, workspace_id, handle, name, created_by, created_at";

type Row = (
    uuid::Uuid,
    uuid::Uuid,
    String,
    String,
    uuid::Uuid,
    time::OffsetDateTime,
);

fn row_to_model(r: Row) -> UserGroup {
    let (id, workspace_id, handle, name, created_by, created_at) = r;
    UserGroup {
        id: UserGroupId::from_uuid(id),
        workspace_id: WorkspaceId::from_uuid(workspace_id),
        handle,
        name,
        created_by: ParticipantId::from_uuid(created_by),
        created_at,
    }
}

/// Repository over the `user_groups` + `user_group_members` tables.
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`UserGroupRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct UserGroupRepo {
    pool: PgPool,
}

impl UserGroupRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persist a new group in `workspace`, returning its generated id. The handle
    /// is stored [`normalize_handle`]-normalized so resolution stays case-stable.
    /// The caller is responsible for workspace-membership and handle/name
    /// validation; a duplicate `(workspace, handle)` surfaces as a `sqlx` unique
    /// violation the handler maps to a `409`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert (including the unique
    /// violation on `(workspace_id, handle)`).
    pub async fn create(
        &self,
        workspace: WorkspaceId,
        handle: &str,
        name: &str,
        created_by: ParticipantId,
    ) -> Result<UserGroupId, sqlx::Error> {
        let id = UserGroupId::new();
        sqlx::query(
            r"INSERT INTO user_groups (id, workspace_id, handle, name, created_by)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id.to_uuid())
        .bind(workspace.to_uuid())
        .bind(normalize_handle(handle))
        .bind(name)
        .bind(created_by.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Fetch one group by id, or `None` if no such row exists.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(&self, id: UserGroupId) -> Result<Option<UserGroup>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM user_groups WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// List every group in `workspace`, by handle ascending.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_workspace(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<UserGroup>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM user_groups
              WHERE workspace_id = $1
              ORDER BY handle ASC, id ASC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(workspace.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Resolve a group by its (normalized) handle within `workspace`, or `None`.
    /// The lookup [`normalize_handle`]-normalizes the input so `@Designers` and
    /// `@designers` resolve identically — the seam an `@handle` mention uses.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn resolve(
        &self,
        workspace: WorkspaceId,
        handle: &str,
    ) -> Result<Option<UserGroup>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS} FROM user_groups WHERE workspace_id = $1 AND handle = $2"
        );
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(workspace.to_uuid())
            .bind(normalize_handle(handle))
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// Delete a group (its memberships cascade away via the FK). Scoped to
    /// `workspace` so a group id from another tenant is a no-op. Returns `true`
    /// iff a row was removed.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn delete(
        &self,
        id: UserGroupId,
        workspace: WorkspaceId,
    ) -> Result<bool, sqlx::Error> {
        let result =
            sqlx::query("DELETE FROM user_groups WHERE id = $1 AND workspace_id = $2")
                .bind(id.to_uuid())
                .bind(workspace.to_uuid())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Add `participant` to `group`, idempotently (a duplicate is a no-op via
    /// `ON CONFLICT DO NOTHING`). The caller is responsible for authorization and
    /// for verifying the group exists.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn add_member(
        &self,
        group: UserGroupId,
        participant: ParticipantId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO user_group_members (group_id, participant_id)
               VALUES ($1, $2)
               ON CONFLICT (group_id, participant_id) DO NOTHING",
        )
        .bind(group.to_uuid())
        .bind(participant.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Remove `participant` from `group`. Returns `true` iff a membership row was
    /// actually removed (a second removal, or a non-member, returns `false`).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn remove_member(
        &self,
        group: UserGroupId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM user_group_members WHERE group_id = $1 AND participant_id = $2",
        )
        .bind(group.to_uuid())
        .bind(participant.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// List the participant ids that belong to `group`, oldest membership first.
    /// The seam an `@handle` mention fan-out reads to find recipients.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn members(&self, group: UserGroupId) -> Result<Vec<ParticipantId>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT participant_id
               FROM user_group_members
              WHERE group_id = $1
              ORDER BY added_at ASC, participant_id ASC",
        )
        .bind(group.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(p,)| ParticipantId::from_uuid(p)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_handle_lowercases_and_trims() {
        assert_eq!(normalize_handle("  Designers  "), "designers");
        assert_eq!(normalize_handle("ON-CALL"), "on-call");
        assert_eq!(normalize_handle("team_42"), "team_42");
        // Idempotent: normalizing an already-normalized handle is a fixed point.
        assert_eq!(normalize_handle("designers"), "designers");
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored user_group
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway participant so the test is self-contained. Groups store
    /// opaque uuids for `workspace_id` / member `participant_id` (no FK to those),
    /// so fresh ids suffice without inserting workspaces — but `created_by` and
    /// members are inserted as real participants for realism.
    async fn actor(p: &PgPool) -> ParticipantId {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(actor.to_uuid())
            .bind(format!("user-group-actor-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");
        actor
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn user_group_create_get_list_resolve() {
        let p = pool();
        let repo = UserGroupRepo::new(p.clone());
        let ws = WorkspaceId::new();
        let creator = actor(&p).await;

        // create → get returns the canonical (normalized) row.
        let id = repo
            .create(ws, "  Designers ", "Design Team", creator)
            .await
            .unwrap();
        let got = repo.get(id).await.unwrap().expect("group present");
        assert_eq!(got.id, id);
        assert_eq!(got.handle, "designers", "handle stored normalized");
        assert_eq!(got.name, "Design Team");
        assert_eq!(got.created_by, creator);
        assert_eq!(got.workspace_id, ws);

        // list_for_workspace shows it.
        let listed = repo.list_for_workspace(ws).await.unwrap();
        assert!(listed.iter().any(|g| g.id == id), "list shows the group");

        // resolve normalizes the input handle and finds the same row; a different
        // workspace does not see it.
        let resolved = repo
            .resolve(ws, "DESIGNERS")
            .await
            .unwrap()
            .expect("resolve by handle");
        assert_eq!(resolved.id, id);
        assert!(
            repo.resolve(WorkspaceId::new(), "designers")
                .await
                .unwrap()
                .is_none(),
            "another workspace does not resolve this handle"
        );

        // Cleanup so reruns stay self-contained.
        repo.delete(id, ws).await.ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn user_group_membership_add_remove_and_cascade() {
        let p = pool();
        let repo = UserGroupRepo::new(p.clone());
        let ws = WorkspaceId::new();
        let creator = actor(&p).await;
        let alice = actor(&p).await;
        let bob = actor(&p).await;

        let id = repo.create(ws, "oncall", "On-Call", creator).await.unwrap();

        // add (idempotent) → members lists both, oldest first.
        repo.add_member(id, alice).await.unwrap();
        repo.add_member(id, bob).await.unwrap();
        repo.add_member(id, alice).await.unwrap(); // no-op
        let members = repo.members(id).await.unwrap();
        assert_eq!(members, vec![alice, bob], "members listed in insertion order");

        // remove one → only the other remains; a second removal is a no-op.
        assert!(repo.remove_member(id, alice).await.unwrap(), "removed alice");
        assert!(
            !repo.remove_member(id, alice).await.unwrap(),
            "second remove is a no-op"
        );
        assert_eq!(repo.members(id).await.unwrap(), vec![bob], "only bob remains");

        // delete cascades: the group and its remaining memberships are gone.
        assert!(repo.delete(id, ws).await.unwrap(), "delete removes the group");
        assert!(repo.get(id).await.unwrap().is_none(), "group gone after delete");
        assert!(
            repo.members(id).await.unwrap().is_empty(),
            "memberships cascaded away with the group"
        );
    }
}
