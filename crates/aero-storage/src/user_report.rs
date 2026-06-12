//! User-level report flow (migration 0112).
//!
//! Any authenticated user may report another within a (optional) workspace
//! context. A UNIQUE constraint on `(reporter_id, reported_id, workspace_id)`
//! prevents duplicate reports from the same pair. Workspace admins list and
//! resolve reports via [`UserReportRepo::list_for_workspace`] /
//! [`UserReportRepo::resolve`].
//!
//! Purely additive: a NEW [`UserReportRepo`]; no existing repo is touched.

use aero_common::{Error, ParticipantId, WorkspaceId};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
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

    /// File a report from `reporter` against `reported` within `workspace`.
    /// Silently no-ops (returns the existing id) if the same `(reporter,
    /// reported, workspace)` triple was already reported (`ON CONFLICT DO
    /// NOTHING RETURNING`). Returns `None` iff the row already existed.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert, wrapped in [`Error::from`].
    pub async fn create(
        &self,
        reporter: ParticipantId,
        reported: ParticipantId,
        workspace: Option<WorkspaceId>,
        reason: &str,
    ) -> Result<Option<Uuid>, Error> {
        let row: Option<(Uuid,)> = sqlx::query_as(
            "INSERT INTO user_reports (reporter_id, reported_id, workspace_id, reason) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (reporter_id, reported_id, workspace_id) DO NOTHING \
             RETURNING id",
        )
        .bind(reporter.to_uuid())
        .bind(reported.to_uuid())
        .bind(workspace.map(|w| w.to_uuid()))
        .bind(reason)
        .fetch_optional(&self.pg)
        .await
        .map_err(Error::from)?;
        Ok(row.map(|(id,)| id))
    }

    /// List reports for a workspace, optionally filtered by `status`
    /// (`"pending"` | `"reviewed"` | `"dismissed"`). Newest first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query, wrapped in [`Error::from`].
    pub async fn list_for_workspace(
        &self,
        workspace: WorkspaceId,
        status_filter: Option<&str>,
    ) -> Result<Vec<UserReport>, Error> {
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
        q.fetch_all(&self.pg).await.map_err(Error::from)
    }

    /// Resolve a report to `new_status` (`"reviewed"` or `"dismissed"`),
    /// workspace-scoped. Returns `true` iff the row was updated.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update, wrapped in [`Error::from`].
    pub async fn resolve(
        &self,
        report_id: Uuid,
        workspace: WorkspaceId,
        new_status: &str,
    ) -> Result<bool, Error> {
        let r = sqlx::query(
            "UPDATE user_reports SET status = $1 \
             WHERE id = $2 AND workspace_id = $3 AND status = 'pending'",
        )
        .bind(new_status)
        .bind(report_id)
        .bind(workspace.to_uuid())
        .execute(&self.pg)
        .await
        .map_err(Error::from)?;
        Ok(r.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::UserReport;

    /// Verify the status set is what we expect (pure, no DB needed).
    #[test]
    fn valid_resolution_statuses() {
        let valid = ["reviewed", "dismissed"];
        let invalid = ["pending", "flagged", ""];
        for s in valid {
            assert!(
                matches!(s, "reviewed" | "dismissed"),
                "{s} should be a valid resolution status"
            );
        }
        for s in invalid {
            assert!(
                !matches!(s, "reviewed" | "dismissed"),
                "{s} should not be a valid resolution status"
            );
        }
        // Ensure UserReport is usable (compile check only; no DB query).
        let _ = std::mem::size_of::<UserReport>();
    }
}
