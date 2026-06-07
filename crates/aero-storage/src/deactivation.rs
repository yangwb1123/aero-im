//! Workspace user-deactivation repository (admin revokes a member's access).
//!
//! Backs `migrations/0045_workspace_deactivations.sql`. An admin/owner
//! deactivates a member within a workspace; a deactivated member is then denied
//! access to that workspace's rooms. The access-enforcement check itself lives in
//! the service layer (`ImService::assert_room_access`), which calls
//! [`DeactivationRepo::is_deactivated`] — this repo only owns the
//! deactivate/reactivate/query CRUD.
//!
//! Each deactivation is a single `(workspace_id, participant_id)` pair — the
//! composite primary key makes deactivating idempotent and needs no surrogate id.
//! Reactivating removes the row. Purely additive: a NEW [`DeactivationRepo`]; no
//! existing repo is touched. The [`DeactivatedMember`] model lives here (and is
//! re-exported from the crate root), since it is a storage-layer projection.

use aero_common::{ParticipantId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;

/// One deactivated member — a `(workspace, participant)` pair plus who/when.
///
/// A storage-layer projection of a `workspace_deactivations` row. `Serialize` so
/// a handler can hand the row straight back as JSON; `deactivated_at` renders as
/// RFC 3339. `deactivated_by` is `None` when the actor was not recorded.
#[derive(Debug, Clone, Serialize)]
pub struct DeactivatedMember {
    /// The tenant the deactivation is scoped to.
    pub workspace_id: WorkspaceId,
    /// The member that was deactivated within the workspace.
    pub participant_id: ParticipantId,
    /// When the member was deactivated (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub deactivated_at: time::OffsetDateTime,
    /// The admin/owner who performed the deactivation, if recorded.
    pub deactivated_by: Option<ParticipantId>,
}

/// The columns a [`DeactivatedMember`] is built from, in select order. Shared by
/// every query so the row decoding stays in one place.
const COLUMNS: &str = "workspace_id, participant_id, deactivated_at, deactivated_by";

type Row = (
    uuid::Uuid,
    uuid::Uuid,
    time::OffsetDateTime,
    Option<uuid::Uuid>,
);

fn row_to_model(r: Row) -> DeactivatedMember {
    let (workspace_id, participant_id, deactivated_at, deactivated_by) = r;
    DeactivatedMember {
        workspace_id: WorkspaceId::from_uuid(workspace_id),
        participant_id: ParticipantId::from_uuid(participant_id),
        deactivated_at,
        deactivated_by: deactivated_by.map(ParticipantId::from_uuid),
    }
}

/// Repository over the `workspace_deactivations` table (admin member-deactivation).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`DeactivationRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct DeactivationRepo {
    pool: PgPool,
}

impl DeactivationRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Deactivate `participant` in `workspace`, recording `by` as the actor.
    /// Idempotent: re-deactivating an already-deactivated member is a no-op
    /// (`ON CONFLICT DO NOTHING`), leaving the original actor/timestamp intact.
    /// The caller is responsible for the admin-privilege check.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn deactivate(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
        by: ParticipantId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO workspace_deactivations (workspace_id, participant_id, deactivated_by)
               VALUES ($1, $2, $3)
               ON CONFLICT (workspace_id, participant_id) DO NOTHING",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(by.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Reactivate `participant` in `workspace`, restoring their access. Returns
    /// `true` iff a row was removed — reactivating a member who was never
    /// deactivated (or a second reactivate) is a no-op returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn reactivate(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM workspace_deactivations WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Whether `participant` is currently deactivated in `workspace`. This is the
    /// integrator seam: the service layer calls it inside `assert_room_access` to
    /// forbid a deactivated member from that workspace's room data.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn is_deactivated(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (i32,)>(
            r"SELECT 1 FROM workspace_deactivations
               WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// List the deactivated members in `workspace`, newest first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list(&self, workspace: WorkspaceId) -> Result<Vec<DeactivatedMember>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM workspace_deactivations
              WHERE workspace_id = $1
              ORDER BY deactivated_at DESC, participant_id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(workspace.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored deactivation
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    /// The reserved all-zero default workspace, guaranteed to exist by migration
    /// 0006's backfill — reused so the deactivation rows are well-scoped.
    const DEFAULT_WS: &str = "00000000-0000-0000-0000-000000000000";

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway participant so the test is self-contained.
    async fn mk_participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("deactivation-{id}"))
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
    async fn deactivate_is_deactivated_list_reactivate() {
        let p = pool();
        let repo = DeactivationRepo::new(p.clone());
        let ws = default_ws();
        let member = mk_participant(&p).await;
        let admin = mk_participant(&p).await;

        // Not deactivated yet.
        assert!(
            !repo.is_deactivated(ws, member).await.unwrap(),
            "member is active before deactivate"
        );

        // deactivate → idempotent; is_deactivated true; list shows it with the actor.
        repo.deactivate(ws, member, admin).await.unwrap();
        repo.deactivate(ws, member, admin).await.unwrap(); // idempotent
        assert!(
            repo.is_deactivated(ws, member).await.unwrap(),
            "member is deactivated"
        );
        let listed = repo.list(ws).await.unwrap();
        let found = listed
            .iter()
            .find(|d| d.participant_id == member)
            .expect("list shows the deactivated member");
        assert_eq!(found.deactivated_by, Some(admin));

        // reactivate → true once; is_deactivated false; absent from the list.
        assert!(repo.reactivate(ws, member).await.unwrap(), "reactivate removes the row");
        assert!(
            !repo.reactivate(ws, member).await.unwrap(),
            "second reactivate is a no-op"
        );
        assert!(
            !repo.is_deactivated(ws, member).await.unwrap(),
            "member is active again after reactivate"
        );
        assert!(
            !repo.list(ws).await.unwrap().iter().any(|d| d.participant_id == member),
            "reactivated member leaves the list"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM workspace_deactivations WHERE participant_id = $1")
            .bind(member.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
