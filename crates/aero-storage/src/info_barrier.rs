//! Information-barrier / ethical-wall repository (Microsoft Purview-style).
//!
//! Backs `migrations/0059_info_barriers.sql`. An admin defines barred PAIRS of
//! user-groups (migration 0033, [`UserGroupRepo`](crate::UserGroupRepo)). Members
//! across a barred pair may not DM each other or share a channel. The barrier is
//! symmetric: `(group_a, group_b)` bars traffic in either direction.
//!
//! The KEY method is [`BarrierRepo::barred`]: it answers "may these two
//! participants converse?" by joining `info_barriers` against each participant's
//! `user_group_members` rows in BOTH orderings, scoped to one workspace. DM /
//! group-DM creation checks it, and every user-authored message create/edit
//! transaction repeats an aggregate room-recipient check while holding the
//! workspace policy fence. A barrier created after a conversation therefore
//! blocks subsequent communication too.
//!
//! Purely additive: a NEW [`BarrierRepo`]; no existing repo is touched. The
//! [`InfoBarrier`] model lives here (and is re-exported from the crate root)
//! rather than in `aero-common`, since it is a storage-layer projection — mirroring
//! [`UserGroup`](crate::UserGroup) and [`SavedSearch`](crate::SavedSearch).

use aero_common::{BarrierId, Error, ParticipantId, UserGroupId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;

use crate::workspace::authz::assert_effective_admin_in_tx;

/// One information barrier — an admin-defined barred PAIR of user-groups in a
/// workspace.
///
/// A storage-layer projection of an `info_barriers` row. `Serialize` so a handler
/// can hand the row straight back as JSON; `created_at` renders as RFC 3339. The
/// pair is symmetric — `(group_a, group_b)` and `(group_b, group_a)` mean the same
/// thing — so callers should not rely on the column order.
#[derive(Debug, Clone, Serialize)]
pub struct InfoBarrier {
    /// The barrier's unique id.
    pub id: BarrierId,
    /// The tenant the barrier is scoped to (only this workspace's traffic is gated).
    pub workspace_id: WorkspaceId,
    /// One side of the barred pair.
    pub group_a: UserGroupId,
    /// The other side of the barred pair (symmetric with `group_a`).
    pub group_b: UserGroupId,
    /// The admin who created the barrier.
    pub created_by: ParticipantId,
    /// When the barrier was created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// The columns an [`InfoBarrier`] is built from, in select order. Shared by every
/// query so the row decoding stays in one place.
const COLUMNS: &str = "id, workspace_id, group_a, group_b, created_by, created_at";

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: uuid::Uuid,
    workspace_id: uuid::Uuid,
    group_a: uuid::Uuid,
    group_b: uuid::Uuid,
    created_by: uuid::Uuid,
    created_at: time::OffsetDateTime,
}

fn row_to_model(r: Row) -> InfoBarrier {
    InfoBarrier {
        id: BarrierId::from_uuid(r.id),
        workspace_id: WorkspaceId::from_uuid(r.workspace_id),
        group_a: UserGroupId::from_uuid(r.group_a),
        group_b: UserGroupId::from_uuid(r.group_b),
        created_by: ParticipantId::from_uuid(r.created_by),
        created_at: r.created_at,
    }
}

/// Repository over the `info_barriers` table (information barriers / ethical walls).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules (and the DM enforcement points) build one inline via
/// [`BarrierRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct BarrierRepo {
    pool: PgPool,
}

impl BarrierRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persist a new barred pair in `workspace`, returning its generated id. The
    /// caller is responsible for admin authorization and for validating that the
    /// two groups are distinct and belong to this workspace.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    #[cfg(test)]
    pub(crate) async fn create(
        &self,
        workspace: WorkspaceId,
        group_a: UserGroupId,
        group_b: UserGroupId,
        created_by: ParticipantId,
    ) -> Result<BarrierId, sqlx::Error> {
        let id = BarrierId::new();
        sqlx::query(
            r"INSERT INTO info_barriers (id, workspace_id, group_a, group_b, created_by)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id.to_uuid())
        .bind(workspace.to_uuid())
        .bind(group_a.to_uuid())
        .bind(group_b.to_uuid())
        .bind(created_by.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Create a barred pair with administrator authorization, distinctness, and
    /// group tenant-containment checked in the same transaction as the insert.
    pub async fn create_authorized(
        &self,
        workspace: WorkspaceId,
        group_a: UserGroupId,
        group_b: UserGroupId,
        actor: ParticipantId,
    ) -> Result<BarrierId, Error> {
        if group_a == group_b {
            return Err(Error::Invalid(
                "a group cannot be barred from itself".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        let groups = sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT id
               FROM user_groups
              WHERE workspace_id = $1 AND id = ANY($2)
              FOR SHARE",
        )
        .bind(workspace.to_uuid())
        .bind(vec![group_a.to_uuid(), group_b.to_uuid()])
        .fetch_all(&mut *tx)
        .await?;
        if groups.len() != 2 {
            return Err(Error::NotFound(
                "both groups must belong to the workspace".into(),
            ));
        }
        let id = BarrierId::new();
        sqlx::query(
            "INSERT INTO info_barriers (id, workspace_id, group_a, group_b, created_by)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id.to_uuid())
        .bind(workspace.to_uuid())
        .bind(group_a.to_uuid())
        .bind(group_b.to_uuid())
        .bind(actor.to_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Fetch one barrier by id, or `None` if no such row exists. Used by the
    /// workspace-less `DELETE` path to resolve the barrier's tenant before
    /// re-checking admin against it.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(&self, id: BarrierId) -> Result<Option<InfoBarrier>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM info_barriers WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// List every barrier in `workspace`, newest first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list(&self, workspace: WorkspaceId) -> Result<Vec<InfoBarrier>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM info_barriers
              WHERE workspace_id = $1
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(workspace.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Delete a barrier, scoped to `workspace` so an id from another tenant is a
    /// no-op. Returns `true` iff a row was removed (a second delete returns `false`).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    #[cfg(test)]
    pub(crate) async fn delete(
        &self,
        id: BarrierId,
        workspace: WorkspaceId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM info_barriers WHERE id = $1 AND workspace_id = $2")
            .bind(id.to_uuid())
            .bind(workspace.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Delete a barrier only if `actor` remains an effective administrator of
    /// the barrier's immutable workspace until commit.
    pub async fn delete_authorized(
        &self,
        id: BarrierId,
        actor: ParticipantId,
    ) -> Result<bool, Error> {
        let mut tx = self.pool.begin().await?;
        let workspace = sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT workspace_id FROM info_barriers WHERE id = $1",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .map(WorkspaceId::from_uuid)
        .ok_or_else(|| Error::NotFound(format!("barrier {id}")))?;
        assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        let removed = sqlx::query("DELETE FROM info_barriers WHERE id = $1 AND workspace_id = $2")
            .bind(id.to_uuid())
            .bind(workspace.to_uuid())
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
        tx.commit().await?;
        Ok(removed)
    }

    /// Are participants `x` and `y` barred from conversing in `workspace`?
    ///
    /// `true` iff `x` and `y` belong to two groups that form a barred pair: the
    /// query joins `info_barriers` against each participant's `user_group_members`
    /// rows in BOTH orderings (`x∈a ∧ y∈b` OR `x∈b ∧ y∈a`), so the symmetric pair
    /// is honored regardless of which participant is on which side. Workspace-scoped
    /// via the `info_barriers.workspace_id` filter.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn barred(
        &self,
        workspace: WorkspaceId,
        x: ParticipantId,
        y: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let barred: bool = sqlx::query_scalar(
            r"SELECT EXISTS (
                  SELECT 1
                    FROM info_barriers b
                    JOIN user_group_members ma ON ma.group_id = b.group_a
                    JOIN user_group_members mb ON mb.group_id = b.group_b
                   WHERE b.workspace_id = $1
                     AND (
                           (ma.participant_id = $2 AND mb.participant_id = $3)
                        OR (ma.participant_id = $3 AND mb.participant_id = $2)
                     )
              )",
        )
        .bind(workspace.to_uuid())
        .bind(x.to_uuid())
        .bind(y.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(barred)
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored info_barrier
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::UserGroupRepo;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway participant so the test is self-contained.
    async fn actor(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("barrier-actor-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn info_barrier_barred_then_freed_after_remove() {
        let p = pool();
        let groups = UserGroupRepo::new(p.clone());
        let barriers = BarrierRepo::new(p.clone());
        let ws = WorkspaceId::new();
        let creator = actor(&p).await;
        let alice = actor(&p).await; // group A
        let bob = actor(&p).await; // group B
        let carol = actor(&p).await; // ungrouped

        // Two groups in the workspace; alice in A, bob in B, carol in neither.
        let ga = groups
            .create(ws, "traders", "Traders", creator)
            .await
            .unwrap();
        let gb = groups
            .create(ws, "research", "Research", creator)
            .await
            .unwrap();
        groups.add_member(ga, alice).await.unwrap();
        groups.add_member(gb, bob).await.unwrap();

        // No barrier yet → not barred.
        assert!(
            !barriers.barred(ws, alice, bob).await.unwrap(),
            "no barrier means free to converse"
        );

        // Bar the pair → alice/bob barred in BOTH orderings; an ungrouped user is free.
        let bid = barriers.create(ws, ga, gb, creator).await.unwrap();
        assert!(barriers.barred(ws, alice, bob).await.unwrap(), "barred a→b");
        assert!(
            barriers.barred(ws, bob, alice).await.unwrap(),
            "barred b→a (symmetric)"
        );
        assert!(
            !barriers.barred(ws, alice, carol).await.unwrap(),
            "ungrouped participant is not barred"
        );

        // list shows it; a different workspace does not evaluate this barrier.
        assert!(
            barriers.list(ws).await.unwrap().iter().any(|b| b.id == bid),
            "list shows the barrier"
        );
        assert!(
            !barriers
                .barred(WorkspaceId::new(), alice, bob)
                .await
                .unwrap(),
            "another workspace's traffic is not gated by this barrier"
        );

        // remove the barrier → not barred again; a second delete is a no-op.
        assert!(barriers.delete(bid, ws).await.unwrap(), "barrier removed");
        assert!(
            !barriers.barred(ws, alice, bob).await.unwrap(),
            "freed after barrier removed"
        );
        assert!(
            !barriers.delete(bid, ws).await.unwrap(),
            "second delete is a no-op"
        );

        // Cleanup so reruns stay self-contained.
        groups.delete(ga, ws).await.ok();
        groups.delete(gb, ws).await.ok();
    }
}
