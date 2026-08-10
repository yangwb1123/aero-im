//! User-level report flow (migrations 0112 and 0203).
//!
//! Any authenticated user may report another within a (optional) workspace
//! context. Workspace-scoped reports bind both participants to current effective
//! tenant access in the same transaction as the insert. Workspace admins list
//! and resolve reports through transaction-owned authorization; a concurrent
//! demotion or deactivation therefore cannot race a moderation action.
//!
//! Purely additive: a NEW [`UserReportRepo`]; no existing repo is touched.

use aero_common::{Error, ParticipantId, WorkspaceId};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// One user-report row — a participant's complaint about another.
///
/// `Serialize` so a handler can return it directly as JSON.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct UserReport {
    pub id: Uuid,
    pub reporter_id: Uuid,
    pub reported_id: Uuid,
    pub workspace_id: Option<Uuid>,
    pub reason: String,
    /// `"pending"` | `"reviewed"` | `"dismissed"`.
    pub status: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// Repository over the `user_reports` table.
///
/// Cheap to clone — wraps a [`PgPool`]; feature modules build one inline via
/// [`UserReportRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct UserReportRepo {
    pg: PgPool,
}

impl UserReportRepo {
    /// Build a repo over the given pool.
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// File a report from `reporter` against `target`.
    ///
    /// For a workspace-scoped report, both participants must retain effective
    /// access to that workspace through commit. The workspace and membership
    /// boundary is locked before the insert, so a forged tenant id and a
    /// concurrent membership revocation both fail closed. A global report
    /// instead locks and verifies two active participant rows.
    ///
    /// Silently no-ops (returns `None`) when the pair was already reported in
    /// the same scope. Migration 0203 supplies the partial unique key required
    /// to make this rule apply to the `NULL` (global) scope as well.
    ///
    /// # Errors
    /// Returns [`Error::Invalid`] for self-reports or an overlong reason,
    /// [`Error::NotFound`] for a missing/deleted global participant or missing
    /// workspace, [`Error::Forbidden`] unless both participants have effective
    /// access to a requested workspace, and propagates storage errors.
    pub async fn create_authorized(
        &self,
        reporter: ParticipantId,
        target: ParticipantId,
        workspace: Option<WorkspaceId>,
        reason: &str,
    ) -> Result<Option<Uuid>, Error> {
        if reporter == target {
            return Err(Error::Invalid("cannot report yourself".into()));
        }
        if reason.len() > 2_000 {
            return Err(Error::Invalid("reason too long".into()));
        }

        let mut tx = self.pg.begin().await?;
        if let Some(workspace) = workspace {
            let exists = sqlx::query_scalar::<_, bool>(
                "SELECT true FROM workspaces WHERE id = $1 FOR UPDATE",
            )
            .bind(workspace.to_uuid())
            .fetch_optional(&mut *tx)
            .await?
            .is_some();
            if !exists {
                return Err(Error::NotFound(format!("workspace {workspace}")));
            }

            // Opposing A→B and B→A reports must acquire membership locks in the
            // same order. The shared helper also rechecks deletion,
            // deactivation, and mandatory-2FA state under locks.
            let mut participants = [reporter, target];
            participants.sort_by_key(aero_common::ParticipantId::to_uuid);
            for participant in participants {
                if !crate::workspace::members::effective_workspace_access_in_tx(
                    &mut tx,
                    workspace,
                    participant,
                )
                .await?
                {
                    return Err(Error::Forbidden(
                        "both report participants must have workspace access".into(),
                    ));
                }
            }
        } else {
            lock_active_participants(&mut tx, reporter, target).await?;
        }

        let row: Option<(Uuid,)> = sqlx::query_as(
            "INSERT INTO user_reports (reporter_id, reported_id, workspace_id, reason) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT DO NOTHING \
             RETURNING id",
        )
        .bind(reporter.to_uuid())
        .bind(target.to_uuid())
        .bind(workspace.map(|w| w.to_uuid()))
        .bind(reason)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Error::from)?;
        tx.commit().await?;
        Ok(row.map(|(id,)| id))
    }

    /// List reports for a workspace while `actor` remains an effective
    /// Owner/Admin, optionally filtered by `status`. Newest first.
    ///
    /// # Errors
    /// Returns [`Error::Invalid`] for an unknown status,
    /// [`Error::Forbidden`] unless `actor` is a current effective workspace
    /// admin, and propagates storage errors.
    pub async fn list_for_workspace_authorized(
        &self,
        workspace: WorkspaceId,
        status_filter: Option<&str>,
        actor: ParticipantId,
    ) -> Result<Vec<UserReport>, Error> {
        if status_filter.is_some_and(|status| !valid_report_status(status)) {
            return Err(Error::Invalid(
                "status must be 'pending', 'reviewed', or 'dismissed'".into(),
            ));
        }

        let mut tx = self.pg.begin().await?;
        crate::workspace::authz::assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        let base = "SELECT id, reporter_id, reported_id, workspace_id, reason, status, \
                    created_at FROM user_reports WHERE workspace_id = $1";
        let order = " ORDER BY created_at DESC";
        let filter = if status_filter.is_some() {
            " AND status = $2"
        } else {
            ""
        };
        let sql = format!("{base}{filter}{order}");
        let mut q = sqlx::query_as::<_, UserReport>(&sql).bind(workspace.to_uuid());
        if let Some(s) = status_filter {
            q = q.bind(s);
        }
        let reports = q.fetch_all(&mut *tx).await?;
        tx.commit().await?;
        Ok(reports)
    }

    /// Resolve a report to `new_status` (`"reviewed"` or `"dismissed"`),
    /// workspace-scoped, while `actor` remains an effective Owner/Admin.
    ///
    /// # Errors
    /// Returns [`Error::Invalid`] for an unsupported state,
    /// [`Error::Forbidden`] unless `actor` is a current effective workspace
    /// admin, [`Error::NotFound`] when the id belongs to another tenant or is no
    /// longer pending, and propagates storage errors.
    pub async fn resolve_authorized(
        &self,
        report_id: Uuid,
        workspace: WorkspaceId,
        new_status: &str,
        actor: ParticipantId,
    ) -> Result<(), Error> {
        if !valid_resolution_status(new_status) {
            return Err(Error::Invalid(
                "status must be 'reviewed' or 'dismissed'".into(),
            ));
        }

        let mut tx = self.pg.begin().await?;
        crate::workspace::authz::assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        let r = sqlx::query(
            "UPDATE user_reports SET status = $1 \
             WHERE id = $2 AND workspace_id = $3 AND status = 'pending'",
        )
        .bind(new_status)
        .bind(report_id)
        .bind(workspace.to_uuid())
        .execute(&mut *tx)
        .await
        .map_err(Error::from)?;
        if r.rows_affected() != 1 {
            return Err(Error::NotFound(format!(
                "pending report {report_id} in workspace {workspace}"
            )));
        }
        tx.commit().await?;
        Ok(())
    }
}

fn valid_report_status(status: &str) -> bool {
    matches!(status, "pending" | "reviewed" | "dismissed")
}

fn valid_resolution_status(status: &str) -> bool {
    matches!(status, "reviewed" | "dismissed")
}

async fn lock_active_participants(
    tx: &mut Transaction<'_, Postgres>,
    reporter: ParticipantId,
    target: ParticipantId,
) -> Result<(), Error> {
    let mut ids = [reporter.to_uuid(), target.to_uuid()];
    ids.sort_unstable();
    let active = sqlx::query_scalar::<_, Uuid>(
        "SELECT id
           FROM participants
          WHERE id = ANY($1)
            AND deleted_at IS NULL
          ORDER BY id
          FOR SHARE",
    )
    .bind(&ids[..])
    .fetch_all(&mut **tx)
    .await?;
    if active.len() == ids.len() {
        Ok(())
    } else {
        Err(Error::NotFound("active report participant".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::{valid_report_status, valid_resolution_status, UserReport};

    /// Verify the status set is what we expect (pure, no DB needed).
    #[test]
    fn valid_resolution_statuses() {
        let valid = ["reviewed", "dismissed"];
        let invalid = ["pending", "flagged", ""];
        for s in valid {
            assert!(valid_resolution_status(s));
            assert!(valid_report_status(s));
        }
        for s in invalid {
            assert!(!valid_resolution_status(s));
        }
        assert!(valid_report_status("pending"));
        assert!(!valid_report_status("flagged"));
        // Ensure UserReport is usable (compile check only; no DB query).
        let _ = std::mem::size_of::<UserReport>();
    }
}

#[cfg(test)]
mod db_tests {
    use aero_common::{Error, ParticipantId, WorkspaceRole};
    use sqlx::PgPool;

    use super::UserReportRepo;
    use crate::WorkspaceRepo;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_lazy(&url)
            .expect("valid DATABASE_URL")
    }

    async fn participant(pool: &PgPool, label: &str) -> ParticipantId {
        let participant = ParticipantId::new();
        sqlx::query(
            "INSERT INTO participants (id, kind, display_name)
             VALUES ($1, 'human', $2)",
        )
        .bind(participant.to_uuid())
        .bind(format!("user-report-{label}-{participant}"))
        .execute(pool)
        .await
        .unwrap();
        participant
    }

    fn constraint(error: &sqlx::Error) -> Option<&str> {
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint)
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL with migrations applied"]
    async fn authorized_reports_bind_scope_dedupe_and_current_admin_access() {
        let pool = pool();
        let workspaces = WorkspaceRepo::new(pool.clone());
        let reports = UserReportRepo::new(pool.clone());

        let owner = participant(&pool, "owner").await;
        let admin = participant(&pool, "admin").await;
        let reporter = participant(&pool, "reporter").await;
        let target = participant(&pool, "reported").await;
        let other_owner = participant(&pool, "other-owner").await;
        let outsider = participant(&pool, "outsider").await;
        let deleted = participant(&pool, "deleted").await;

        let workspace = workspaces
            .create(
                format!("Report workspace {owner}"),
                format!("report-{owner}"),
                owner,
            )
            .await
            .unwrap()
            .id;
        let other_workspace = workspaces
            .create(
                format!("Other report workspace {other_owner}"),
                format!("other-report-{other_owner}"),
                other_owner,
            )
            .await
            .unwrap()
            .id;
        for (member, role) in [
            (admin, WorkspaceRole::Admin),
            (reporter, WorkspaceRole::Member),
            (target, WorkspaceRole::Member),
        ] {
            workspaces
                .add_member(workspace, member, role)
                .await
                .unwrap();
        }
        workspaces
            .add_member(other_workspace, outsider, WorkspaceRole::Member)
            .await
            .unwrap();

        let scoped = reports
            .create_authorized(reporter, target, Some(workspace), "tenant report")
            .await
            .unwrap()
            .expect("first workspace report is inserted");
        assert!(
            reports
                .create_authorized(reporter, target, Some(workspace), "duplicate")
                .await
                .unwrap()
                .is_none(),
            "workspace report pair is idempotent"
        );

        let global = reports
            .create_authorized(reporter, outsider, None, "global report")
            .await
            .unwrap()
            .expect("first global report is inserted");
        assert!(
            reports
                .create_authorized(reporter, outsider, None, "duplicate global")
                .await
                .unwrap()
                .is_none(),
            "migration 0203 makes NULL-scope reports idempotent"
        );

        assert!(matches!(
            reports
                .create_authorized(reporter, outsider, Some(workspace), "forged scope")
                .await,
            Err(Error::Forbidden(_))
        ));
        let direct = sqlx::query(
            "INSERT INTO user_reports
                 (reporter_id, reported_id, workspace_id, reason)
             VALUES ($1, $2, $3, 'raw forged scope')",
        )
        .bind(reporter.to_uuid())
        .bind(outsider.to_uuid())
        .bind(workspace.to_uuid())
        .execute(&pool)
        .await
        .expect_err("database trigger rejects a bypassed cross-tenant report");
        assert_eq!(
            constraint(&direct),
            Some("user_reports_workspace_membership_chk")
        );

        sqlx::query("UPDATE participants SET deleted_at = NOW() WHERE id = $1")
            .bind(deleted.to_uuid())
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            reports
                .create_authorized(reporter, deleted, None, "deleted target")
                .await,
            Err(Error::NotFound(_))
        ));

        let visible = reports
            .list_for_workspace_authorized(workspace, Some("pending"), admin)
            .await
            .unwrap();
        assert!(visible.iter().any(|report| report.id == scoped));
        assert!(
            visible.iter().all(|report| report.id != global),
            "global reports never leak into a workspace moderation queue"
        );
        assert!(matches!(
            reports
                .resolve_authorized(scoped, other_workspace, "reviewed", other_owner)
                .await,
            Err(Error::NotFound(_))
        ));

        sqlx::query(
            "DELETE FROM workspace_members
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(admin.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
        assert!(matches!(
            reports
                .list_for_workspace_authorized(workspace, None, admin)
                .await,
            Err(Error::Forbidden(_))
        ));
        assert!(matches!(
            reports
                .resolve_authorized(scoped, workspace, "reviewed", admin)
                .await,
            Err(Error::Forbidden(_))
        ));

        reports
            .resolve_authorized(scoped, workspace, "reviewed", owner)
            .await
            .unwrap();
        assert!(matches!(
            reports
                .resolve_authorized(scoped, workspace, "dismissed", owner)
                .await,
            Err(Error::NotFound(_))
        ));
    }
}
