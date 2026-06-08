//! Org-chart / manager-hierarchy repository (reporting lines).
//!
//! Backs `migrations/0053_org_chart.sql`. Each participant may have at most one
//! manager (`org_reports.participant_id` is the PRIMARY KEY), forming a
//! Lark/Teams-style org chart. This repo owns the reporting-line CRUD plus three
//! read shapes: a participant's direct manager, a manager's direct reports, and
//! the full reporting chain walked upward to the top.
//!
//! Cycle safety lives here, not in the schema: [`OrgChartRepo::reporting_chain`]
//! caps its walk at a bounded depth and stops the moment it revisits a
//! participant, so a malformed `a → b → a` loop terminates instead of spinning.
//! Rejecting self-as-own-manager is the SERVER layer's job (a `400`), keeping the
//! validation message uniform with the rest of the API. Purely additive: a NEW
//! [`OrgChartRepo`]; no existing repo is touched, and no new id type is minted
//! (every column is a [`ParticipantId`]).

use std::collections::HashSet;

use aero_common::ParticipantId;
use sqlx::PgPool;

/// Hard ceiling on how deep [`OrgChartRepo::reporting_chain`] will ever walk,
/// regardless of the caller-supplied `max_depth`. A backstop against a
/// pathological or malicious request; cycle detection already guarantees
/// termination, this just bounds the work.
pub const MAX_CHAIN_DEPTH: usize = 100;

/// Default reporting-chain depth the server requests — deep enough for any real
/// org, shallow enough to stay cheap.
pub const DEFAULT_CHAIN_DEPTH: usize = 20;

/// Repository over the `org_reports` table (manager / reporting lines).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// the feature module builds one inline via [`OrgChartRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct OrgChartRepo {
    pool: PgPool,
}

impl OrgChartRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Set (or re-point) `participant`'s manager to `manager`, recording `set_by`
    /// as the actor who made the change. Upsert: one row per participant, so a
    /// second call replaces the existing line and refreshes `updated_at`.
    ///
    /// The caller is responsible for rejecting `participant == manager`
    /// (self-management) and for the authorization gate — this method performs
    /// neither check.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert.
    pub async fn set_manager(
        &self,
        participant: ParticipantId,
        manager: ParticipantId,
        set_by: ParticipantId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO org_reports (participant_id, manager_id, set_by, updated_at)
               VALUES ($1, $2, $3, now())
               ON CONFLICT (participant_id)
               DO UPDATE SET manager_id = EXCLUDED.manager_id,
                             set_by = EXCLUDED.set_by,
                             updated_at = now()",
        )
        .bind(participant.to_uuid())
        .bind(manager.to_uuid())
        .bind(set_by.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Clear `participant`'s manager (remove their reporting line). Returns `true`
    /// iff a row was removed — a second clear (or one for a participant with no
    /// manager) is a no-op returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn clear_manager(&self, participant: ParticipantId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM org_reports WHERE participant_id = $1")
            .bind(participant.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Fetch `participant`'s manager, or `None` if they have no reporting line.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn manager_of(
        &self,
        participant: ParticipantId,
    ) -> Result<Option<ParticipantId>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid,)>(
            "SELECT manager_id FROM org_reports WHERE participant_id = $1",
        )
        .bind(participant.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(m,)| ParticipantId::from_uuid(m)))
    }

    /// List the participants who report directly to `manager` (their direct
    /// reports), ordered by participant id for a stable response.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn direct_reports(
        &self,
        manager: ParticipantId,
    ) -> Result<Vec<ParticipantId>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            "SELECT participant_id FROM org_reports WHERE manager_id = $1 ORDER BY participant_id",
        )
        .bind(manager.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(p,)| ParticipantId::from_uuid(p))
            .collect())
    }

    /// Walk `participant`'s reporting line upward to the top, returning the chain
    /// of managers (nearest first), **excluding** `participant` itself. For
    /// `a → b → c`, `reporting_chain(a, _)` is `[b, c]`.
    ///
    /// The walk is bounded by `max_depth` (further clamped to [`MAX_CHAIN_DEPTH`])
    /// and stops the instant it would revisit an already-seen participant, so a
    /// cycle (`a → b → a`) terminates with a partial chain rather than looping
    /// forever.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the per-step lookups.
    pub async fn reporting_chain(
        &self,
        participant: ParticipantId,
        max_depth: usize,
    ) -> Result<Vec<ParticipantId>, sqlx::Error> {
        let cap = max_depth.min(MAX_CHAIN_DEPTH);
        let mut chain = Vec::new();
        let mut seen: HashSet<ParticipantId> = HashSet::new();
        seen.insert(participant);
        let mut current = participant;
        for _ in 0..cap {
            match self.manager_of(current).await? {
                Some(manager) => {
                    // A repeat means we've closed a cycle — stop before re-adding.
                    if !seen.insert(manager) {
                        break;
                    }
                    chain.push(manager);
                    current = manager;
                }
                None => break,
            }
        }
        Ok(chain)
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored org_chart
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

    /// Create a throwaway participant so the test is self-contained.
    async fn participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("org-chart-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn set_manager_chain_reports_and_cycle_guard() {
        let p = pool();
        let repo = OrgChartRepo::new(p.clone());
        let a = participant(&p).await;
        let b = participant(&p).await;
        let c = participant(&p).await;

        // Build a → b → c (a reports to b, b reports to c).
        repo.set_manager(a, b, a).await.unwrap();
        repo.set_manager(b, c, b).await.unwrap();

        // manager_of resolves each line; c has none.
        assert_eq!(repo.manager_of(a).await.unwrap(), Some(b));
        assert_eq!(repo.manager_of(b).await.unwrap(), Some(c));
        assert_eq!(repo.manager_of(c).await.unwrap(), None);

        // direct_reports(b) = [a].
        assert_eq!(repo.direct_reports(b).await.unwrap(), vec![a]);
        // direct_reports(c) = [b].
        assert_eq!(repo.direct_reports(c).await.unwrap(), vec![b]);

        // chain(a) = [b, c]; chain(c) = [].
        assert_eq!(repo.reporting_chain(a, 20).await.unwrap(), vec![b, c]);
        assert!(repo.reporting_chain(c, 20).await.unwrap().is_empty());

        // Re-point a → c (upsert replaces the line), then chain(a) = [c].
        repo.set_manager(a, c, a).await.unwrap();
        assert_eq!(repo.manager_of(a).await.unwrap(), Some(c));
        assert_eq!(repo.reporting_chain(a, 20).await.unwrap(), vec![c]);

        // Cycle guard: c → a closes a loop a → c → a. The walk must terminate.
        repo.set_manager(c, a, c).await.unwrap();
        let cyclic = repo.reporting_chain(a, 20).await.unwrap();
        assert!(
            cyclic.len() <= 2,
            "cycle walk terminates with a bounded chain, got {cyclic:?}"
        );

        // clear_manager removes the line once; a second clear is a no-op.
        assert!(repo.clear_manager(a).await.unwrap(), "first clear removes");
        assert!(!repo.clear_manager(a).await.unwrap(), "second clear no-op");
        assert_eq!(repo.manager_of(a).await.unwrap(), None);

        // Cleanup so reruns stay self-contained.
        for id in [a, b, c] {
            sqlx::query("DELETE FROM org_reports WHERE participant_id = $1 OR manager_id = $1")
                .bind(id.to_uuid())
                .execute(&p)
                .await
                .ok();
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(id.to_uuid())
                .execute(&p)
                .await
                .ok();
        }
    }
}
