//! Workspace settings — name, branding, retention, 2FA, rate tier.

use aero_common::WorkspaceId;

use super::WorkspaceRepo;

impl WorkspaceRepo {
    /// Rename a workspace.
    pub async fn update_name(
        &self,
        workspace: WorkspaceId,
        new_name: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("UPDATE workspaces SET name = $2 WHERE id = $1")
            .bind(workspace.to_uuid())
            .bind(new_name)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Update workspace branding fields. Only `Some(…)` values are written.
    pub async fn update_branding(
        &self,
        workspace: WorkspaceId,
        logo_url: Option<&str>,
        color_scheme: Option<&str>,
        custom_domain: Option<&str>,
        description: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE workspaces
                  SET logo_url      = COALESCE($2, logo_url),
                      color_scheme  = COALESCE($3, color_scheme),
                      custom_domain = COALESCE($4, custom_domain),
                      description   = COALESCE($5, description)
                WHERE id = $1",
        )
        .bind(workspace.to_uuid())
        .bind(logo_url)
        .bind(color_scheme)
        .bind(custom_domain)
        .bind(description)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Set (or clear) the message-retention window in days.
    pub async fn set_retention(
        &self,
        workspace: WorkspaceId,
        days: Option<i32>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(r"UPDATE workspaces SET retention_days = $2 WHERE id = $1")
            .bind(workspace.to_uuid())
            .bind(days)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The workspace's configured retention window in days, or `None`.
    pub async fn retention_days(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Option<i32>, sqlx::Error> {
        let row = sqlx::query_as::<_, (Option<i32>,)>(
            r"SELECT retention_days FROM workspaces WHERE id = $1",
        )
        .bind(workspace.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(d,)| d))
    }

    /// Set whether this workspace mandates 2FA.
    pub async fn set_require_2fa(
        &self,
        workspace: WorkspaceId,
        require: bool,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(r"UPDATE workspaces SET require_2fa = $2 WHERE id = $1")
            .bind(workspace.to_uuid())
            .bind(require)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Whether this workspace mandates 2FA. `false` for an unknown workspace.
    pub async fn require_2fa(&self, workspace: WorkspaceId) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (bool,)>(r"SELECT require_2fa FROM workspaces WHERE id = $1")
            .bind(workspace.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some_and(|(r,)| r))
    }

    /// Set the workspace's rate-limit tier token.
    pub async fn set_rate_tier(
        &self,
        workspace: WorkspaceId,
        tier: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(r"UPDATE workspaces SET rate_tier = $2 WHERE id = $1")
            .bind(workspace.to_uuid())
            .bind(tier)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// The workspace's stored rate-tier token, or `None` for an unknown workspace.
    pub async fn rate_tier(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Option<String>, sqlx::Error> {
        let row = sqlx::query_as::<_, (String,)>(
            r"SELECT rate_tier FROM workspaces WHERE id = $1",
        )
        .bind(workspace.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(t,)| t))
    }
}
