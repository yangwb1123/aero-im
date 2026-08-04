//! Org-chart / manager-hierarchy repository (reporting lines).
//!
//! Backs `migrations/0053_org_chart.sql` plus the workspace-containment
//! migration. Each participant may have at most one manager *per workspace*,
//! forming a tenant-local Lark/Teams-style org chart.
//!
//! Cycle safety lives here, not in the schema: [`OrgChartRepo::reporting_chain`]
//! caps its walk at a bounded depth and stops the moment it revisits a
//! participant, so a malformed `a → b → a` loop terminates instead of spinning.
//! Self-as-own-manager and cycle rejection are repeated in this repository so a
//! caller cannot bypass the HTTP validation. No new id type is minted (every
//! identity column is a [`ParticipantId`]).

use std::collections::HashSet;

use aero_common::{ParticipantId, WorkspaceId, WorkspaceRole};
use sqlx::{PgPool, Postgres, Transaction};

/// Hard ceiling on how deep [`OrgChartRepo::reporting_chain`] will ever walk,
/// regardless of the caller-supplied `max_depth`. A backstop against a
/// pathological or malicious request; cycle detection already guarantees
/// termination, this just bounds the work.
pub const MAX_CHAIN_DEPTH: usize = 100;

/// Default reporting-chain depth the server requests — deep enough for any real
/// org, shallow enough to stay cheap.
pub const DEFAULT_CHAIN_DEPTH: usize = 20;

#[derive(Debug, thiserror::Error)]
pub enum OrgChartWriteError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("actor is not an effective workspace member")]
    ActorNotMember,
    #[error("only a workspace administrator may manage another member")]
    Forbidden,
    #[error("target is not an effective workspace member")]
    TargetNotMember,
    #[error("manager is not an effective workspace member")]
    ManagerNotMember,
    #[error("a participant cannot manage themselves")]
    SelfManager,
    #[error("reporting line would create a cycle")]
    Cycle,
}

async fn effective_role(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Result<Option<WorkspaceRole>, sqlx::Error> {
    let role = sqlx::query_scalar::<_, String>(
        r"SELECT membership.role
            FROM workspace_members membership
            JOIN workspaces workspace
              ON workspace.id = membership.workspace_id
            JOIN participants participant
              ON participant.id = membership.participant_id
             AND participant.deleted_at IS NULL
           WHERE membership.workspace_id = $1
             AND membership.participant_id = $2
             AND NOT EXISTS (
                 SELECT 1
                   FROM workspace_deactivations deactivated
                  WHERE deactivated.workspace_id = membership.workspace_id
                    AND deactivated.participant_id = membership.participant_id
             )
             AND (
                 participant.kind <> 'human'
                 OR NOT workspace.require_2fa
                 OR EXISTS (
                     SELECT 1
                       FROM totp_secrets totp
                      WHERE totp.participant_id = membership.participant_id
                        AND totp.activated
                 )
             )
           FOR SHARE OF membership, workspace, participant",
    )
    .bind(workspace.to_uuid())
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    Ok(role.and_then(|role| WorkspaceRole::from_db_str(&role)))
}

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

    /// Set (or re-point) `participant`'s manager within `workspace`, recording
    /// `set_by` as the actor. Actor authority, all three effective memberships,
    /// self-management, and cycles are checked in the upsert transaction.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert.
    pub async fn set_manager(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
        manager: ParticipantId,
        set_by: ParticipantId,
    ) -> Result<(), OrgChartWriteError> {
        if participant == manager {
            return Err(OrgChartWriteError::SelfManager);
        }
        let mut tx = self.pool.begin().await?;
        // A cycle spans multiple rows, so row locks alone cannot prevent two
        // concurrent, individually-valid writes from creating `a → b → a`.
        // Serialize graph mutations per workspace before checking the chain.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text, 0))")
            .bind(workspace.to_uuid())
            .execute(&mut *tx)
            .await?;
        let actor_role = effective_role(&mut tx, workspace, set_by)
            .await?
            .ok_or(OrgChartWriteError::ActorNotMember)?;
        if set_by != participant && !actor_role.can_administer() {
            return Err(OrgChartWriteError::Forbidden);
        }
        effective_role(&mut tx, workspace, participant)
            .await?
            .ok_or(OrgChartWriteError::TargetNotMember)?;
        effective_role(&mut tx, workspace, manager)
            .await?
            .ok_or(OrgChartWriteError::ManagerNotMember)?;

        let creates_cycle = sqlx::query_scalar::<_, bool>(
            r"WITH RECURSIVE manager_chain(participant_id) AS (
                   SELECT $2::uuid
                   UNION
                   SELECT reports.manager_id
                     FROM org_reports reports
                     JOIN manager_chain chain
                       ON reports.participant_id = chain.participant_id
                    WHERE reports.workspace_id = $1
               )
               SELECT EXISTS (
                   SELECT 1 FROM manager_chain WHERE participant_id = $3
               )",
        )
        .bind(workspace.to_uuid())
        .bind(manager.to_uuid())
        .bind(participant.to_uuid())
        .fetch_one(&mut *tx)
        .await?;
        if creates_cycle {
            return Err(OrgChartWriteError::Cycle);
        }

        sqlx::query(
            r"INSERT INTO org_reports
                  (workspace_id, participant_id, manager_id, set_by, updated_at)
               VALUES ($1, $2, $3, $4, now())
               ON CONFLICT (workspace_id, participant_id)
               DO UPDATE SET manager_id = EXCLUDED.manager_id,
                             set_by = EXCLUDED.set_by,
                             updated_at = now()",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(manager.to_uuid())
        .bind(set_by.to_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Clear `participant`'s manager while the actor remains authorized in the
    /// same workspace transaction.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn clear_manager(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
        actor: ParticipantId,
    ) -> Result<bool, OrgChartWriteError> {
        let mut tx = self.pool.begin().await?;
        let actor_role = effective_role(&mut tx, workspace, actor)
            .await?
            .ok_or(OrgChartWriteError::ActorNotMember)?;
        if actor != participant && !actor_role.can_administer() {
            return Err(OrgChartWriteError::Forbidden);
        }
        effective_role(&mut tx, workspace, participant)
            .await?
            .ok_or(OrgChartWriteError::TargetNotMember)?;
        let result = sqlx::query(
            "DELETE FROM org_reports
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }

    /// Fetch `participant`'s manager, or `None` if they have no reporting line.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn manager_of(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<Option<ParticipantId>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT reports.manager_id
                FROM org_reports reports
                JOIN workspace_members membership
                  ON membership.workspace_id = reports.workspace_id
                 AND membership.participant_id = reports.manager_id
                JOIN workspaces workspace
                  ON workspace.id = membership.workspace_id
                JOIN participants participant
                  ON participant.id = reports.manager_id
                 AND participant.deleted_at IS NULL
               WHERE reports.workspace_id = $1
                 AND reports.participant_id = $2
                 AND NOT EXISTS (
                     SELECT 1
                       FROM workspace_deactivations deactivated
                      WHERE deactivated.workspace_id = reports.workspace_id
                        AND deactivated.participant_id = reports.manager_id
                 )
                 AND (
                     participant.kind <> 'human'
                     OR NOT workspace.require_2fa
                     OR EXISTS (
                         SELECT 1
                           FROM totp_secrets totp
                          WHERE totp.participant_id = reports.manager_id
                            AND totp.activated
                     )
                 )",
        )
        .bind(workspace.to_uuid())
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
        workspace: WorkspaceId,
        manager: ParticipantId,
    ) -> Result<Vec<ParticipantId>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT reports.participant_id
                FROM org_reports reports
                JOIN workspace_members membership
                  ON membership.workspace_id = reports.workspace_id
                 AND membership.participant_id = reports.participant_id
                JOIN workspaces workspace
                  ON workspace.id = membership.workspace_id
                JOIN participants participant
                  ON participant.id = reports.participant_id
                 AND participant.deleted_at IS NULL
               WHERE reports.workspace_id = $1
                 AND reports.manager_id = $2
                 AND NOT EXISTS (
                     SELECT 1
                       FROM workspace_deactivations deactivated
                      WHERE deactivated.workspace_id = reports.workspace_id
                        AND deactivated.participant_id = reports.participant_id
                 )
                 AND (
                     participant.kind <> 'human'
                     OR NOT workspace.require_2fa
                     OR EXISTS (
                         SELECT 1
                           FROM totp_secrets totp
                          WHERE totp.participant_id = reports.participant_id
                            AND totp.activated
                     )
                 )
               ORDER BY reports.participant_id",
        )
        .bind(workspace.to_uuid())
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
        workspace: WorkspaceId,
        participant: ParticipantId,
        max_depth: usize,
    ) -> Result<Vec<ParticipantId>, sqlx::Error> {
        let cap = max_depth.min(MAX_CHAIN_DEPTH);
        let mut chain = Vec::new();
        let mut seen: HashSet<ParticipantId> = HashSet::new();
        seen.insert(participant);
        let mut current = participant;
        for _ in 0..cap {
            match self.manager_of(workspace, current).await? {
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

    async fn workspace_with_members(
        p: &PgPool,
        owner: ParticipantId,
        members: &[ParticipantId],
    ) -> WorkspaceId {
        let workspace = WorkspaceId::new();
        let mut tx = p.begin().await.expect("begin workspace fixture");
        sqlx::query(
            "INSERT INTO workspaces (id, name, slug, created_by)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(workspace.to_uuid())
        .bind(format!("org-chart-{workspace}"))
        .bind(format!("org-chart-{workspace}"))
        .bind(owner.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert org-chart workspace");
        for member in members {
            sqlx::query(
                "INSERT INTO workspace_members
                     (workspace_id, participant_id, role, joined_at)
                 VALUES ($1, $2, $3, now())",
            )
            .bind(workspace.to_uuid())
            .bind(member.to_uuid())
            .bind(if *member == owner { "owner" } else { "member" })
            .execute(&mut *tx)
            .await
            .expect("insert org-chart member");
        }
        tx.commit().await.expect("commit workspace fixture");
        workspace
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn set_manager_chain_reports_and_cycle_guard() {
        let p = pool();
        let repo = OrgChartRepo::new(p.clone());
        let a = participant(&p).await;
        let b = participant(&p).await;
        let c = participant(&p).await;
        let outsider = participant(&p).await;
        let workspace = workspace_with_members(&p, a, &[a, b, c]).await;
        let other_workspace = workspace_with_members(&p, a, &[a, b, c]).await;

        // Build a → b → c (a reports to b, b reports to c).
        repo.set_manager(workspace, a, b, a).await.unwrap();
        repo.set_manager(workspace, b, c, b).await.unwrap();

        // manager_of resolves each line; c has none.
        assert_eq!(repo.manager_of(workspace, a).await.unwrap(), Some(b));
        assert_eq!(repo.manager_of(workspace, b).await.unwrap(), Some(c));
        assert_eq!(repo.manager_of(workspace, c).await.unwrap(), None);

        // direct_reports(b) = [a].
        assert_eq!(repo.direct_reports(workspace, b).await.unwrap(), vec![a]);
        // direct_reports(c) = [b].
        assert_eq!(repo.direct_reports(workspace, c).await.unwrap(), vec![b]);

        // chain(a) = [b, c]; chain(c) = [].
        assert_eq!(
            repo.reporting_chain(workspace, a, 20).await.unwrap(),
            vec![b, c]
        );
        assert!(repo
            .reporting_chain(workspace, c, 20)
            .await
            .unwrap()
            .is_empty());

        // Re-point a → c (upsert replaces the line), then chain(a) = [c].
        repo.set_manager(workspace, a, c, a).await.unwrap();
        assert_eq!(repo.manager_of(workspace, a).await.unwrap(), Some(c));
        assert_eq!(
            repo.reporting_chain(workspace, a, 20).await.unwrap(),
            vec![c]
        );

        // A cycle and a cross-workspace manager are rejected transactionally.
        assert!(matches!(
            repo.set_manager(workspace, c, a, c).await,
            Err(OrgChartWriteError::Cycle)
        ));
        assert!(matches!(
            repo.set_manager(workspace, a, outsider, a).await,
            Err(OrgChartWriteError::ManagerNotMember)
        ));

        repo.set_manager(other_workspace, a, b, a).await.unwrap();
        assert_eq!(repo.manager_of(other_workspace, a).await.unwrap(), Some(b));
        assert_eq!(
            repo.manager_of(workspace, a).await.unwrap(),
            Some(c),
            "another workspace cannot overwrite this tenant's reporting line"
        );

        // clear_manager removes the line once; a second clear is a no-op.
        assert!(
            repo.clear_manager(workspace, a, a).await.unwrap(),
            "first clear removes"
        );
        assert!(
            !repo.clear_manager(workspace, a, a).await.unwrap(),
            "second clear no-op"
        );
        assert_eq!(repo.manager_of(workspace, a).await.unwrap(), None);

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM org_reports WHERE workspace_id = ANY($1)")
            .bind(vec![workspace.to_uuid(), other_workspace.to_uuid()])
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM workspaces WHERE id = ANY($1)")
            .bind(vec![workspace.to_uuid(), other_workspace.to_uuid()])
            .execute(&p)
            .await
            .ok();
        for id in [a, b, c, outsider] {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(id.to_uuid())
                .execute(&p)
                .await
                .ok();
        }
    }
}
