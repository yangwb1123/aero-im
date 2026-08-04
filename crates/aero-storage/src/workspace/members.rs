//! Workspace membership — add/remove/list/role/guest operations.

use aero_common::{ParticipantId, WorkspaceId, WorkspaceMember, WorkspaceRole};

use super::{
    role_can_assign, role_can_invite, role_can_manage_member, role_can_remove, WorkspaceRepo,
};

/// Expected failures from transaction-owned workspace membership governance.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceMemberWriteError {
    #[error("workspace member not found")]
    MemberNotFound,
    #[error("caller is not authorized to manage workspace membership")]
    NotAuthorized,
    #[error("single-channel guests must be managed through the guest membership API")]
    InvalidGuestRole,
    #[error("an owner cannot leave the workspace")]
    OwnerCannotLeave,
    #[error("workspace must retain at least one owner")]
    LastOwner,
    #[error("member owns a channel that has no other effective owner")]
    ChannelOwnerProtected,
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

impl WorkspaceRepo {
    /// Add (or, on conflict, leave untouched) a member with the given role.
    /// Idempotent: re-adding an existing member is a no-op rather than an error.
    pub async fn add_member(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
        role: WorkspaceRole,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO workspace_members (workspace_id, participant_id, role, joined_at)
               VALUES ($1, $2, $3, NOW())
               ON CONFLICT (workspace_id, participant_id) DO NOTHING",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(role.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Set an existing member's role, returning whether the row existed.
    ///
    /// This low-level helper intentionally never inserts: a concurrent SCIM
    /// deprovision must not be undone by an upsert that resurrects membership.
    /// HTTP governance uses [`change_member_role_authorized`](Self::change_member_role_authorized).
    pub async fn update_member_role(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
        role: WorkspaceRole,
    ) -> Result<bool, sqlx::Error> {
        let changed = sqlx::query(
            r"UPDATE workspace_members
                  SET role = $3
                WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(role.as_str())
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0;
        Ok(changed)
    }

    /// Invite a member with caller authorization held under the same workspace
    /// lock as the insert.
    pub async fn add_member_authorized(
        &self,
        workspace: WorkspaceId,
        caller: ParticipantId,
        participant: ParticipantId,
        role: WorkspaceRole,
    ) -> Result<bool, WorkspaceMemberWriteError> {
        if role == WorkspaceRole::Guest {
            return Err(WorkspaceMemberWriteError::InvalidGuestRole);
        }
        let mut tx = self.pool.begin().await?;
        crate::ownership::lock_membership_governance(&mut tx).await?;
        lock_workspace(&mut tx, workspace).await?;
        let caller_role = locked_member_roles(&mut tx, workspace, &[caller])
            .await?
            .into_iter()
            .find_map(|(id, role)| (id == caller).then_some(role))
            .ok_or(WorkspaceMemberWriteError::NotAuthorized)?;
        if !effective_workspace_access_in_tx(&mut tx, workspace, caller).await? {
            return Err(WorkspaceMemberWriteError::NotAuthorized);
        }
        if !role_can_invite(caller_role) || !role_can_assign(caller_role, role) {
            return Err(WorkspaceMemberWriteError::NotAuthorized);
        }
        let inserted = sqlx::query(
            r"INSERT INTO workspace_members (workspace_id, participant_id, role, joined_at)
               VALUES ($1, $2, $3, NOW())
               ON CONFLICT (workspace_id, participant_id) DO NOTHING",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(role.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        tx.commit().await.map_err(map_member_storage_error)?;
        Ok(inserted)
    }

    /// Change an existing member's role with current caller/subject roles locked
    /// through commit. Demoting the final owner is rejected.
    pub async fn change_member_role_authorized(
        &self,
        workspace: WorkspaceId,
        caller: ParticipantId,
        participant: ParticipantId,
        role: WorkspaceRole,
    ) -> Result<WorkspaceRole, WorkspaceMemberWriteError> {
        if role == WorkspaceRole::Guest {
            return Err(WorkspaceMemberWriteError::InvalidGuestRole);
        }
        let mut tx = self.pool.begin().await?;
        crate::ownership::lock_membership_governance(&mut tx).await?;
        lock_workspace(&mut tx, workspace).await?;
        let roles = locked_member_roles(&mut tx, workspace, &[caller, participant]).await?;
        let caller_role = roles
            .iter()
            .find_map(|(id, role)| (*id == caller).then_some(*role))
            .ok_or(WorkspaceMemberWriteError::NotAuthorized)?;
        let previous = roles
            .iter()
            .find_map(|(id, role)| (*id == participant).then_some(*role))
            .ok_or(WorkspaceMemberWriteError::MemberNotFound)?;
        if !effective_workspace_access_in_tx(&mut tx, workspace, caller).await? {
            return Err(WorkspaceMemberWriteError::NotAuthorized);
        }
        if !role_can_assign(caller_role, role) || !role_can_manage_member(caller_role, previous) {
            return Err(WorkspaceMemberWriteError::NotAuthorized);
        }
        if previous == WorkspaceRole::Owner && role != WorkspaceRole::Owner {
            let another_owner = sqlx::query_scalar::<_, bool>(
                r"SELECT EXISTS(
                       SELECT 1
                         FROM workspace_members
                        WHERE workspace_id = $1
                          AND participant_id <> $2
                          AND role = 'owner'
                   )",
            )
            .bind(workspace.to_uuid())
            .bind(participant.to_uuid())
            .fetch_one(&mut *tx)
            .await?;
            if !another_owner {
                return Err(WorkspaceMemberWriteError::LastOwner);
            }
        }
        let changed = sqlx::query(
            r"UPDATE workspace_members
                  SET role = $3
                WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(role.as_str())
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(WorkspaceMemberWriteError::MemberNotFound);
        }
        tx.commit().await.map_err(map_member_storage_error)?;
        Ok(previous)
    }

    /// Remove an existing membership and all room edges with authorization,
    /// owner protection, and the writes in one transaction.
    pub async fn remove_member_authorized(
        &self,
        workspace: WorkspaceId,
        caller: ParticipantId,
        participant: ParticipantId,
    ) -> Result<WorkspaceRole, WorkspaceMemberWriteError> {
        let mut tx = self.pool.begin().await?;
        crate::ownership::lock_membership_governance(&mut tx).await?;
        lock_workspace(&mut tx, workspace).await?;
        let roles = locked_member_roles(&mut tx, workspace, &[caller, participant]).await?;
        let caller_role = roles
            .iter()
            .find_map(|(id, role)| (*id == caller).then_some(*role))
            .ok_or(WorkspaceMemberWriteError::NotAuthorized)?;
        let previous = roles
            .iter()
            .find_map(|(id, role)| (*id == participant).then_some(*role))
            .ok_or(WorkspaceMemberWriteError::MemberNotFound)?;
        if !effective_workspace_access_in_tx(&mut tx, workspace, caller).await? {
            return Err(WorkspaceMemberWriteError::NotAuthorized);
        }
        if caller == participant {
            if previous == WorkspaceRole::Owner {
                return Err(WorkspaceMemberWriteError::OwnerCannotLeave);
            }
        } else if !role_can_remove(caller_role) || !role_can_manage_member(caller_role, previous) {
            return Err(WorkspaceMemberWriteError::NotAuthorized);
        }
        sqlx::query(
            r"DELETE FROM workspace_members
               WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await
        .map_err(map_member_storage_error)?;
        sqlx::query(
            r"DELETE FROM room_members
               WHERE participant_id = $2
                 AND room_id IN (SELECT id FROM rooms WHERE workspace_id = $1)",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await
        .map_err(map_member_storage_error)?;
        tx.commit().await.map_err(map_member_storage_error)?;
        Ok(previous)
    }

    /// Remove a member from a workspace. No-op if they were not a member.
    pub async fn remove_member(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r"DELETE FROM workspace_members
               WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r"DELETE FROM room_members
               WHERE participant_id = $2
                 AND room_id IN (SELECT id FROM rooms WHERE workspace_id = $1)",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The participant's role in the workspace, or `None` if not a member.
    pub async fn member_role(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<Option<WorkspaceRole>, sqlx::Error> {
        let row = sqlx::query_as::<_, (String,)>(
            r"SELECT role FROM workspace_members
               WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(r,)| WorkspaceRole::from_db_str(&r)))
    }

    /// The participant's role only when every current workspace-access gate is
    /// satisfied.
    ///
    /// This is the workspace-scoped twin of the room access boundary: the
    /// participant must still be an active account and workspace member, must
    /// not be administratively deactivated in this workspace, and must have an
    /// activated TOTP enrollment when the workspace mandates 2FA. Callers that
    /// read tenant-wide data (without a room id to pass to `assert_room_access`)
    /// should use this method instead of bare [`Self::member_role`].
    pub async fn effective_member_role(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<Option<WorkspaceRole>, sqlx::Error> {
        let row = sqlx::query_as::<_, (String,)>(
            r"SELECT membership.role
                FROM workspace_members membership
                JOIN workspaces workspace
                  ON workspace.id = membership.workspace_id
                JOIN participants participant
                  ON participant.id = membership.participant_id
                 AND participant.deleted_at IS NULL
                LEFT JOIN workspace_deactivations deactivated
                  ON deactivated.workspace_id = membership.workspace_id
                 AND deactivated.participant_id = membership.participant_id
                LEFT JOIN totp_secrets totp
                  ON totp.participant_id = membership.participant_id
               WHERE membership.workspace_id = $1
                 AND membership.participant_id = $2
                 AND deactivated.participant_id IS NULL
                 AND (
                     participant.kind <> 'human'
                     OR NOT workspace.require_2fa
                     OR COALESCE(totp.activated, false)
                 )",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(role,)| WorkspaceRole::from_db_str(&role)))
    }

    /// Whether every requested participant currently has effective access to
    /// `workspace`, evaluated in one query.
    ///
    /// The input is de-duplicated before binding. Each participant must retain a
    /// workspace membership, have a non-deleted account, not be workspace-
    /// deactivated, and have activated TOTP when the workspace mandates 2FA.
    /// Empty input is vacuously true.
    pub async fn all_effective_members(
        &self,
        workspace: WorkspaceId,
        participants: &[ParticipantId],
    ) -> Result<bool, sqlx::Error> {
        let mut ids: Vec<uuid::Uuid> = participants.iter().map(ParticipantId::to_uuid).collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            return Ok(true);
        }
        let expected = i64::try_from(ids.len()).unwrap_or(i64::MAX);
        sqlx::query_scalar(
            r"SELECT count(*) = $3
                FROM unnest($2::uuid[]) requested(participant_id)
                JOIN workspace_members membership
                  ON membership.workspace_id = $1
                 AND membership.participant_id = requested.participant_id
                JOIN workspaces workspace
                  ON workspace.id = membership.workspace_id
                JOIN participants participant
                  ON participant.id = membership.participant_id
                 AND participant.deleted_at IS NULL
                LEFT JOIN workspace_deactivations deactivated
                  ON deactivated.workspace_id = membership.workspace_id
                 AND deactivated.participant_id = membership.participant_id
                LEFT JOIN totp_secrets totp
                  ON totp.participant_id = membership.participant_id
               WHERE deactivated.participant_id IS NULL
                 AND (
                     participant.kind <> 'human'
                     OR NOT workspace.require_2fa
                     OR COALESCE(totp.activated, false)
                 )",
        )
        .bind(workspace.to_uuid())
        .bind(ids)
        .bind(expected)
        .fetch_one(&self.pool)
        .await
    }

    /// All members of a workspace, newest joiners last.
    pub async fn list_members(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<WorkspaceMember>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, String, time::OffsetDateTime)>(
            r"SELECT workspace_id, participant_id, role, joined_at
               FROM workspace_members
               WHERE workspace_id = $1
               ORDER BY joined_at ASC, participant_id ASC",
        )
        .bind(workspace.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(ws, pid, role, joined_at)| WorkspaceMember {
                workspace_id: WorkspaceId::from_uuid(ws),
                participant_id: ParticipantId::from_uuid(pid),
                role: WorkspaceRole::from_db_str(&role).unwrap_or(WorkspaceRole::Guest),
                joined_at,
            })
            .collect())
    }

    /// All workspaces the participant can effectively access, most recently
    /// created first.
    ///
    /// Retained membership rows do not surface a tenant after account deletion,
    /// workspace deactivation, or while mandatory 2FA remains unsatisfied.
    pub async fn list_for_participant(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<aero_common::Workspace>, sqlx::Error> {
        let rows = sqlx::query_as::<
            _,
            (
                uuid::Uuid,
                String,
                String,
                Option<uuid::Uuid>,
                time::OffsetDateTime,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
            ),
        >(
            r"SELECT w.id, w.name, w.slug, w.created_by, w.created_at,
                     w.logo_url, w.color_scheme, w.custom_domain, w.description
               FROM workspaces w
               JOIN workspace_members m ON m.workspace_id = w.id
               JOIN participants p
                 ON p.id = m.participant_id
                AND p.deleted_at IS NULL
               LEFT JOIN workspace_deactivations deactivated
                 ON deactivated.workspace_id = m.workspace_id
                AND deactivated.participant_id = m.participant_id
               LEFT JOIN totp_secrets totp
                 ON totp.participant_id = m.participant_id
               WHERE m.participant_id = $1
                 AND deactivated.participant_id IS NULL
                 AND (
                     p.kind <> 'human'
                     OR NOT w.require_2fa
                     OR COALESCE(totp.activated, false)
                 )
               ORDER BY w.created_at DESC",
        )
        .bind(participant.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, name, slug, by, at, logo_url, color_scheme, custom_domain, description)| {
                    aero_common::Workspace {
                        id: WorkspaceId::from_uuid(id),
                        name,
                        slug,
                        created_by: by.map(ParticipantId::from_uuid),
                        created_at: at,
                        logo_url,
                        color_scheme,
                        custom_domain,
                        description,
                    }
                },
            )
            .collect())
    }

    /// Whether the participant is a member of the workspace.
    pub async fn is_member(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (i64,)>(
            r"SELECT COUNT(*) FROM workspace_members
               WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0 > 0)
    }
}

fn map_member_storage_error(error: sqlx::Error) -> WorkspaceMemberWriteError {
    if crate::is_workspace_owner_violation(&error) {
        WorkspaceMemberWriteError::LastOwner
    } else if crate::is_channel_effective_owner_violation(&error) {
        WorkspaceMemberWriteError::ChannelOwnerProtected
    } else {
        WorkspaceMemberWriteError::Storage(error)
    }
}

async fn lock_workspace(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
) -> Result<(), WorkspaceMemberWriteError> {
    let exists =
        sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .is_some();
    if exists {
        Ok(())
    } else {
        Err(WorkspaceMemberWriteError::MemberNotFound)
    }
}

async fn locked_member_roles(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
    participants: &[ParticipantId],
) -> Result<Vec<(ParticipantId, WorkspaceRole)>, sqlx::Error> {
    let mut ids: Vec<uuid::Uuid> = participants.iter().map(ParticipantId::to_uuid).collect();
    ids.sort_unstable();
    ids.dedup();
    let rows = sqlx::query_as::<_, (uuid::Uuid, String)>(
        r"SELECT participant_id, role
            FROM workspace_members
           WHERE workspace_id = $1
             AND participant_id = ANY($2)
           ORDER BY participant_id
           FOR UPDATE",
    )
    .bind(workspace.to_uuid())
    .bind(&ids)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, role)| {
            WorkspaceRole::from_db_str(&role).map(|role| (ParticipantId::from_uuid(id), role))
        })
        .collect())
}

/// Revalidate the caller's complete workspace access boundary inside an
/// already-open governance transaction.
///
/// Callers lock the workspace and any role rows first, then invoke the shared
/// database helper. The helper locks the active-account/TOTP rows it observes,
/// so deactivation, mandatory-2FA changes, account deletion, and membership
/// revocation cannot slip between authorization and commit.
pub(crate) async fn effective_workspace_access_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT aero_effective_workspace_access($1, $2)")
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .fetch_one(&mut **tx)
        .await
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use sqlx::PgPool;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_lazy(&url)
            .expect("valid DATABASE_URL")
    }

    async fn participant(pool: &PgPool) -> ParticipantId {
        let participant = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(participant.to_uuid())
            .bind(format!("workspace-governance-{participant}"))
            .execute(pool)
            .await
            .unwrap();
        participant
    }

    async fn lists_workspace(
        repo: &WorkspaceRepo,
        participant: ParticipantId,
        workspace: WorkspaceId,
    ) -> bool {
        repo.list_for_participant(participant)
            .await
            .unwrap()
            .iter()
            .any(|candidate| candidate.id == workspace)
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL with migrations applied"]
    async fn authorized_membership_writes_preserve_rbac_and_owner_invariants() {
        let pool = pool();
        let repo = WorkspaceRepo::new(pool.clone());
        let owner = participant(&pool).await;
        let admin = participant(&pool).await;
        let member = participant(&pool).await;
        let target = participant(&pool).await;
        let workspace = repo
            .create(
                format!("workspace-governance-{owner}"),
                format!("workspace-governance-{owner}"),
                owner,
            )
            .await
            .unwrap()
            .id;

        assert!(repo
            .add_member_authorized(workspace, owner, admin, WorkspaceRole::Admin)
            .await
            .unwrap());
        assert!(repo
            .add_member_authorized(workspace, admin, member, WorkspaceRole::Member)
            .await
            .unwrap());
        assert!(matches!(
            repo.add_member_authorized(workspace, admin, target, WorkspaceRole::Owner)
                .await
                .expect_err("admin cannot mint an owner"),
            WorkspaceMemberWriteError::NotAuthorized
        ));
        assert!(matches!(
            repo.change_member_role_authorized(workspace, admin, member, WorkspaceRole::Guest,)
                .await
                .expect_err("single-channel guests use the dedicated aggregate"),
            WorkspaceMemberWriteError::InvalidGuestRole
        ));
        assert!(matches!(
            repo.change_member_role_authorized(workspace, owner, owner, WorkspaceRole::Member,)
                .await
                .expect_err("the final owner cannot be demoted"),
            WorkspaceMemberWriteError::LastOwner
        ));

        repo.change_member_role_authorized(workspace, owner, admin, WorkspaceRole::Owner)
            .await
            .unwrap();
        repo.change_member_role_authorized(workspace, owner, owner, WorkspaceRole::Member)
            .await
            .unwrap();
        assert!(matches!(
            repo.add_member_authorized(workspace, owner, target, WorkspaceRole::Member)
                .await
                .expect_err("a demoted caller has no stale invite authority"),
            WorkspaceMemberWriteError::NotAuthorized
        ));
        assert_eq!(
            repo.remove_member_authorized(workspace, admin, owner)
                .await
                .unwrap(),
            WorkspaceRole::Member
        );
        assert!(matches!(
            repo.remove_member_authorized(workspace, admin, admin)
                .await
                .expect_err("an owner cannot self-remove"),
            WorkspaceMemberWriteError::OwnerCannotLeave
        ));
        assert!(
            !repo
                .update_member_role(workspace, ParticipantId::new(), WorkspaceRole::Owner)
                .await
                .unwrap(),
            "low-level role update never inserts a missing membership"
        );
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL with migrations applied"]
    async fn effective_member_role_enforces_account_deactivation_and_mandatory_2fa() {
        let pool = pool();
        let repo = WorkspaceRepo::new(pool.clone());
        let owner = participant(&pool).await;
        let member = participant(&pool).await;
        let outsider = participant(&pool).await;
        let workspace = repo
            .create(
                format!("effective-access-{owner}"),
                format!("effective-access-{owner}"),
                owner,
            )
            .await
            .unwrap()
            .id;
        repo.add_member(workspace, member, WorkspaceRole::Member)
            .await
            .unwrap();

        assert_eq!(
            repo.effective_member_role(workspace, member).await.unwrap(),
            Some(WorkspaceRole::Member)
        );
        assert!(
            lists_workspace(&repo, member, workspace).await,
            "an active member sees the workspace in their tenant switcher"
        );
        assert_eq!(
            repo.effective_member_role(workspace, outsider)
                .await
                .unwrap(),
            None,
            "bare account existence never grants tenant access"
        );

        sqlx::query(
            "INSERT INTO workspace_deactivations
                 (workspace_id, participant_id, deactivated_by)
             VALUES ($1, $2, $2)",
        )
        .bind(workspace.to_uuid())
        .bind(member.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            repo.effective_member_role(workspace, member).await.unwrap(),
            None,
            "workspace deactivation overrides a retained membership row"
        );
        assert!(
            !lists_workspace(&repo, member, workspace).await,
            "deactivated tenants are absent from list_for_participant"
        );
        sqlx::query(
            "DELETE FROM workspace_deactivations
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(member.to_uuid())
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO totp_secrets
                 (participant_id, secret, activated, activated_at)
             VALUES ($1, 'effective-access-owner', true, now())",
        )
        .bind(owner.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE workspaces SET require_2fa = true WHERE id = $1")
            .bind(workspace.to_uuid())
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            repo.effective_member_role(workspace, member).await.unwrap(),
            None,
            "mandatory 2FA denies an unenrolled member"
        );
        assert!(
            !lists_workspace(&repo, member, workspace).await,
            "mandatory 2FA also gates the tenant switcher"
        );
        sqlx::query(
            "INSERT INTO totp_secrets (participant_id, secret, activated)
             VALUES ($1, 'effective-access-test', false)",
        )
        .bind(member.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            repo.effective_member_role(workspace, member).await.unwrap(),
            None,
            "a pending 2FA enrollment is not sufficient"
        );
        assert!(!lists_workspace(&repo, member, workspace).await);
        sqlx::query(
            "UPDATE totp_secrets
                SET activated = true, activated_at = now()
              WHERE participant_id = $1",
        )
        .bind(member.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            repo.effective_member_role(workspace, member).await.unwrap(),
            Some(WorkspaceRole::Member),
            "activated 2FA restores effective access"
        );
        assert!(lists_workspace(&repo, member, workspace).await);

        sqlx::query("UPDATE participants SET deleted_at = now() WHERE id = $1")
            .bind(member.to_uuid())
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            repo.effective_member_role(workspace, member).await.unwrap(),
            None,
            "a tombstoned account cannot retain workspace access"
        );
        assert!(
            !lists_workspace(&repo, member, workspace).await,
            "deleted accounts receive no effective workspaces"
        );
    }
}

#[cfg(test)]
#[path = "members/effective_write_tests.rs"]
mod effective_write_tests;

#[cfg(test)]
#[path = "members/effective_batch_tests.rs"]
mod effective_batch_tests;
