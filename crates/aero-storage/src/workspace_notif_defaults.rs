//! Workspace-level default notification level repository.
//!
//! Backs `migrations/0119_workspace_notif_defaults.sql`. A workspace admin can
//! configure a default per-channel notification level (`"all"` / `"mentions"` /
//! `"none"`) that is applied to new members when they join a channel, unless
//! they already have an explicit `channel_notification_prefs` row.
//!
//! Purely additive: a NEW [`WorkspaceNotifDefaultsRepo`]; no existing repo touched.

use aero_common::{Error, ParticipantId, WorkspaceId};
use sqlx::PgPool;

#[derive(Clone)]
pub struct WorkspaceNotifDefaultsRepo {
    pub pg: PgPool,
}

impl WorkspaceNotifDefaultsRepo {
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Fetch the configured default notification level for `workspace`, or
    /// `Ok(None)` when no default has been set.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(&self, workspace: WorkspaceId) -> Result<Option<String>, Error> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT default_level FROM workspace_notification_defaults \
             WHERE workspace_id = $1",
        )
        .bind(workspace.to_uuid())
        .fetch_optional(&self.pg)
        .await
        .map_err(Error::from)?;
        Ok(row.map(|(level,)| level))
    }

    /// Upsert the workspace default notification level. `level` must be one of
    /// `"all"` / `"mentions"` / `"none"` — the DB CHECK rejects anything else.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert, including a CHECK
    /// violation when `level` is invalid.
    pub async fn set(&self, workspace: WorkspaceId, level: &str) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO workspace_notification_defaults \
                 (workspace_id, default_level, updated_at) \
             VALUES ($1, $2, NOW()) \
             ON CONFLICT (workspace_id) DO UPDATE \
                 SET default_level = EXCLUDED.default_level, \
                     updated_at = NOW()",
        )
        .bind(workspace.to_uuid())
        .bind(level)
        .execute(&self.pg)
        .await
        .map_err(Error::from)?;
        Ok(())
    }

    /// Upsert the default only while `actor` remains an effective Owner/Admin
    /// under the same workspace lock and transaction as the write.
    pub async fn set_authorized(
        &self,
        workspace: WorkspaceId,
        level: &str,
        actor: ParticipantId,
    ) -> Result<(), Error> {
        let mut tx = self.pg.begin().await?;
        crate::workspace::authz::assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        sqlx::query(
            "INSERT INTO workspace_notification_defaults \
                 (workspace_id, default_level, updated_at) \
             VALUES ($1, $2, NOW()) \
             ON CONFLICT (workspace_id) DO UPDATE \
                 SET default_level = EXCLUDED.default_level, \
                     updated_at = NOW()",
        )
        .bind(workspace.to_uuid())
        .bind(level)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
}
