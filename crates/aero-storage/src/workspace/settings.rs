//! Workspace settings — name, branding, retention, 2FA, rate tier.

use aero_common::{Error, ParticipantId, Workspace, WorkspaceId};

use super::{authz::assert_effective_admin_in_tx, WorkspaceRepo};

type WorkspaceRow = (
    uuid::Uuid,
    String,
    String,
    Option<uuid::Uuid>,
    time::OffsetDateTime,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn workspace_from_row(
    (id, name, slug, created_by, created_at, logo_url, color_scheme, custom_domain, description): WorkspaceRow,
) -> Workspace {
    Workspace {
        id: WorkspaceId::from_uuid(id),
        name,
        slug,
        created_by: created_by.map(ParticipantId::from_uuid),
        created_at,
        logo_url,
        color_scheme,
        custom_domain,
        description,
    }
}

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

    /// Rename a workspace only while `actor` remains an effective Owner/Admin.
    ///
    /// The returned row is the exact committed transaction result, so the HTTP
    /// layer does not need a post-authorization reload that can race deletion.
    pub async fn update_name_authorized(
        &self,
        workspace: WorkspaceId,
        new_name: &str,
        actor: ParticipantId,
    ) -> Result<Workspace, Error> {
        let mut tx = self.pool.begin().await?;
        assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        let row = sqlx::query_as::<_, WorkspaceRow>(
            r"UPDATE workspaces
                  SET name = $2
                WHERE id = $1
            RETURNING id, name, slug, created_by, created_at,
                      logo_url, color_scheme, custom_domain, description",
        )
        .bind(workspace.to_uuid())
        .bind(new_name)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::NotFound(format!("workspace {workspace}")))?;
        tx.commit().await?;
        Ok(workspace_from_row(row))
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

    /// Patch branding under a transaction-owned effective administrator check
    /// and return the row produced by that same commit.
    pub async fn update_branding_authorized(
        &self,
        workspace: WorkspaceId,
        logo_url: Option<&str>,
        color_scheme: Option<&str>,
        custom_domain: Option<&str>,
        description: Option<&str>,
        actor: ParticipantId,
    ) -> Result<Workspace, Error> {
        let mut tx = self.pool.begin().await?;
        assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        let row = sqlx::query_as::<_, WorkspaceRow>(
            r"UPDATE workspaces
                  SET logo_url      = COALESCE($2, logo_url),
                      color_scheme  = COALESCE($3, color_scheme),
                      custom_domain = COALESCE($4, custom_domain),
                      description   = COALESCE($5, description)
                WHERE id = $1
            RETURNING id, name, slug, created_by, created_at,
                      logo_url, color_scheme, custom_domain, description",
        )
        .bind(workspace.to_uuid())
        .bind(logo_url)
        .bind(color_scheme)
        .bind(custom_domain)
        .bind(description)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::NotFound(format!("workspace {workspace}")))?;
        tx.commit().await?;
        Ok(workspace_from_row(row))
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

    /// Set retention only while `actor` remains an effective Owner/Admin in the
    /// same transaction as the policy write.
    pub async fn set_retention_authorized(
        &self,
        workspace: WorkspaceId,
        days: Option<i32>,
        actor: ParticipantId,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        sqlx::query("UPDATE workspaces SET retention_days = $2 WHERE id = $1")
            .bind(workspace.to_uuid())
            .bind(days)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The workspace's configured retention window in days, or `None`.
    pub async fn retention_days(&self, workspace: WorkspaceId) -> Result<Option<i32>, sqlx::Error> {
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

    /// Set the mandatory-2FA policy only while `actor` remains an effective
    /// Owner/Admin under the same workspace lock and transaction as the write.
    pub async fn set_require_2fa_authorized(
        &self,
        workspace: WorkspaceId,
        require: bool,
        actor: ParticipantId,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        if sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .fetch_optional(&mut *tx)
            .await?
            .is_none()
        {
            return Err(Error::NotFound(format!("workspace {workspace}")));
        }
        // Mandatory 2FA changes effective channel ownership. Lock every channel
        // before actor membership/participant/TOTP rows to retain the global
        // workspace -> room -> identity order.
        sqlx::query_scalar::<_, uuid::Uuid>(
            r"SELECT id
                FROM rooms
               WHERE workspace_id = $1
                 AND kind = 'channel'
               ORDER BY id
               FOR UPDATE",
        )
        .bind(workspace.to_uuid())
        .fetch_all(&mut *tx)
        .await?;
        assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        if require {
            let actor_ready = sqlx::query_scalar::<_, bool>(
                r"SELECT participant.kind <> 'human'
                          OR COALESCE(totp.activated, false)
                     FROM participants participant
                     LEFT JOIN totp_secrets totp
                       ON totp.participant_id = participant.id
                    WHERE participant.id = $1
                      AND participant.deleted_at IS NULL",
            )
            .bind(actor.to_uuid())
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or(false);
            if !actor_ready {
                return Err(Error::Forbidden(
                    "activate two-factor authentication before requiring it".into(),
                ));
            }
            let stranded = sqlx::query_scalar::<_, uuid::Uuid>(
                r"SELECT room.id
                    FROM rooms room
                   WHERE room.workspace_id = $1
                     AND room.kind = 'channel'
                     AND NOT EXISTS (
                         SELECT 1
                           FROM room_members owner_membership
                           JOIN participants participant
                             ON participant.id = owner_membership.participant_id
                            AND participant.deleted_at IS NULL
                           JOIN workspace_members workspace_membership
                             ON workspace_membership.workspace_id = room.workspace_id
                            AND workspace_membership.participant_id =
                                owner_membership.participant_id
                           LEFT JOIN workspace_deactivations deactivated
                             ON deactivated.workspace_id = room.workspace_id
                            AND deactivated.participant_id =
                                owner_membership.participant_id
                           LEFT JOIN totp_secrets totp
                             ON totp.participant_id =
                                owner_membership.participant_id
                          WHERE owner_membership.room_id = room.id
                            AND owner_membership.role = 'owner'
                            AND deactivated.participant_id IS NULL
                            AND (
                                participant.kind <> 'human'
                                OR COALESCE(totp.activated, false)
                            )
                     )
                   ORDER BY room.id
                   LIMIT 1",
            )
            .bind(workspace.to_uuid())
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(room) = stranded {
                return Err(Error::Conflict(format!(
                    "channel {} needs an enrolled owner before mandatory 2FA can be enabled",
                    aero_common::RoomId::from_uuid(room)
                )));
            }
        }
        sqlx::query("UPDATE workspaces SET require_2fa = $2 WHERE id = $1")
            .bind(workspace.to_uuid())
            .bind(require)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
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

    /// Set the backend code used for future blob reservations.
    ///
    /// Existing blobs are unaffected because their selected backend is
    /// snapshotted in `blobs.storage_region`.
    pub async fn set_region_code(
        &self,
        workspace: WorkspaceId,
        region_code: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(r"UPDATE workspaces SET region_code = $2 WHERE id = $1")
            .bind(workspace.to_uuid())
            .bind(region_code)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Update future blob placement with a transactionally current
    /// Owner/Admin decision.
    pub async fn set_region_code_authorized(
        &self,
        workspace: WorkspaceId,
        region_code: Option<&str>,
        actor: ParticipantId,
    ) -> Result<bool, Error> {
        let mut tx = self.pool.begin().await?;
        assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        let changed = sqlx::query("UPDATE workspaces SET region_code = $2 WHERE id = $1")
            .bind(workspace.to_uuid())
            .bind(region_code)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
        tx.commit().await?;
        Ok(changed)
    }

    /// The backend code used for future blobs, or `None` for the default.
    pub async fn region_code(&self, workspace: WorkspaceId) -> Result<Option<String>, sqlx::Error> {
        let row = sqlx::query_as::<_, (Option<String>,)>(
            r"SELECT region_code FROM workspaces WHERE id = $1",
        )
        .bind(workspace.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(region_code,)| region_code))
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

    /// Set the workspace rate tier only while `actor` remains an effective
    /// owner in the same transaction as the policy write.
    ///
    /// Billing/capacity tier changes are owner-only rather than ordinary
    /// Owner/Admin settings. The workspace and actor membership locks held by
    /// [`assert_effective_admin_in_tx`] make a concurrent demotion, access
    /// revocation, or workspace deletion linearize before or after this write.
    pub async fn set_rate_tier_authorized(
        &self,
        workspace: WorkspaceId,
        tier: &str,
        actor: ParticipantId,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        let role = assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        if !role.can_manage_workspace() {
            return Err(Error::Forbidden(
                "rate tier requires workspace owner".into(),
            ));
        }
        let changed = sqlx::query("UPDATE workspaces SET rate_tier = $2 WHERE id = $1")
            .bind(workspace.to_uuid())
            .bind(tier)
            .execute(&mut *tx)
            .await?;
        if changed.rows_affected() != 1 {
            return Err(Error::NotFound(format!("workspace {workspace}")));
        }
        tx.commit().await?;
        Ok(())
    }

    /// The workspace's stored rate-tier token, or `None` for an unknown workspace.
    pub async fn rate_tier(&self, workspace: WorkspaceId) -> Result<Option<String>, sqlx::Error> {
        let row = sqlx::query_as::<_, (String,)>(r"SELECT rate_tier FROM workspaces WHERE id = $1")
            .bind(workspace.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(t,)| t))
    }
}
