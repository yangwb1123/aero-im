//! Transaction-owned single-channel guest membership aggregate.

use aero_common::{ParticipantId, RoomId, WorkspaceId, WorkspaceMember, WorkspaceRole};

use super::WorkspaceRepo;

/// Expected failures from the transaction-owned single-channel guest aggregate.
#[derive(Debug, thiserror::Error)]
pub enum GuestMembershipWriteError {
    #[error("workspace not found")]
    WorkspaceNotFound,
    #[error("participant not found")]
    ParticipantNotFound,
    #[error("room not found")]
    RoomNotFound,
    #[error("room does not belong to the workspace")]
    RoomOutsideWorkspace,
    #[error("guest access can only target a channel")]
    NotChannel,
    #[error("caller is not authorized to manage guests")]
    NotAuthorized,
    #[error("an existing non-guest workspace member cannot be converted to a guest")]
    ExistingMember,
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

impl WorkspaceRepo {
    /// Enroll `participant` as the workspace's single-channel guest, with the
    /// current caller role, target membership, tenant room, guest flag, and room
    /// edges governed by one transaction.
    ///
    /// The lock order is workspace → canonical membership rows → workspace room
    /// rows. Every governed workspace membership mutation takes the workspace
    /// lock first, so a concurrent caller demotion cannot leave stale authority
    /// in this operation. An existing member, administrator, or owner cannot be
    /// converted through this endpoint; only an existing guest may be re-scoped.
    ///
    /// The returned room ids are the membership-cache entries affected by the
    /// move. A guest has exactly one room in the workspace after commit.
    pub async fn add_guest_authorized(
        &self,
        workspace: WorkspaceId,
        caller: ParticipantId,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<Vec<RoomId>, GuestMembershipWriteError> {
        let mut tx = self.pool.begin().await?;
        crate::ownership::lock_membership_governance(&mut tx).await?;
        lock_guest_workspace(&mut tx, workspace).await?;

        let memberships =
            locked_guest_memberships(&mut tx, workspace, &[caller, participant]).await?;
        let caller_role = memberships
            .iter()
            .find_map(|state| (state.participant == caller).then_some(state.role))
            .ok_or(GuestMembershipWriteError::NotAuthorized)?;
        if !super::members::effective_workspace_access_in_tx(&mut tx, workspace, caller).await? {
            return Err(GuestMembershipWriteError::NotAuthorized);
        }
        if !caller_role.can_administer() {
            return Err(GuestMembershipWriteError::NotAuthorized);
        }
        let target_state = memberships
            .iter()
            .find(|state| state.participant == participant);
        if target_state.is_some_and(|state| {
            !state.is_guest || matches!(state.role, WorkspaceRole::Owner | WorkspaceRole::Admin)
        }) {
            return Err(GuestMembershipWriteError::ExistingMember);
        }

        let participant_exists = sqlx::query_scalar::<_, bool>(
            "SELECT true FROM participants WHERE id = $1 FOR KEY SHARE",
        )
        .bind(participant.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !participant_exists {
            return Err(GuestMembershipWriteError::ParticipantNotFound);
        }

        // Resolve tenancy without taking a cross-workspace room lock. The
        // canonical, sorted room lock below revalidates existence before writes.
        let room_workspace =
            sqlx::query_scalar::<_, uuid::Uuid>("SELECT workspace_id FROM rooms WHERE id = $1")
                .bind(room.to_uuid())
                .fetch_optional(&mut *tx)
                .await?;
        let Some(room_workspace) = room_workspace else {
            return Err(GuestMembershipWriteError::RoomNotFound);
        };
        if room_workspace != workspace.to_uuid() {
            return Err(GuestMembershipWriteError::RoomOutsideWorkspace);
        }

        let locked_rooms = lock_workspace_rooms(&mut tx, workspace).await?;
        let Some((_, room_kind)) = locked_rooms.iter().find(|(id, _)| *id == room) else {
            return Err(GuestMembershipWriteError::RoomNotFound);
        };
        if room_kind != "channel" {
            return Err(GuestMembershipWriteError::NotChannel);
        }

        let mut affected_rooms = sqlx::query_scalar::<_, uuid::Uuid>(
            r"SELECT rm.room_id
                FROM room_members rm
                JOIN rooms r ON r.id = rm.room_id
               WHERE r.workspace_id = $1
                 AND rm.participant_id = $2
               ORDER BY rm.room_id",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(RoomId::from_uuid)
        .collect::<Vec<_>>();

        if target_state.is_none() {
            sqlx::query(
                r"INSERT INTO workspace_members
                       (workspace_id, participant_id, role, is_guest, joined_at)
                   VALUES ($1, $2, 'member', true, NOW())",
            )
            .bind(workspace.to_uuid())
            .bind(participant.to_uuid())
            .execute(&mut *tx)
            .await?;
        }

        // A guest is scoped to exactly the requested room. Removing old edges
        // and inserting the canonical edge share the membership transaction.
        sqlx::query(
            r"DELETE FROM room_members
               WHERE participant_id = $2
                 AND room_id IN (
                     SELECT id FROM rooms
                      WHERE workspace_id = $1
                        AND id <> $3
                 )",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(room.to_uuid())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r"INSERT INTO room_members (room_id, participant_id, role, joined_at)
               VALUES ($1, $2, 'member', NOW())
               ON CONFLICT (room_id, participant_id) DO NOTHING",
        )
        .bind(room.to_uuid())
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await?;

        affected_rooms.push(room);
        affected_rooms.sort_unstable();
        affected_rooms.dedup();
        tx.commit().await?;
        Ok(affected_rooms)
    }

    /// Revoke a guest membership and every room edge in that workspace in one
    /// transaction. Missing or ordinary non-guest memberships remain idempotent
    /// no-ops; privileged targets are explicit conflicts.
    ///
    /// The returned room ids are the membership-cache entries removed at commit.
    pub async fn remove_guest_authorized(
        &self,
        workspace: WorkspaceId,
        caller: ParticipantId,
        participant: ParticipantId,
    ) -> Result<Vec<RoomId>, GuestMembershipWriteError> {
        let mut tx = self.pool.begin().await?;
        crate::ownership::lock_membership_governance(&mut tx).await?;
        lock_guest_workspace(&mut tx, workspace).await?;

        let memberships =
            locked_guest_memberships(&mut tx, workspace, &[caller, participant]).await?;
        let caller_role = memberships
            .iter()
            .find_map(|state| (state.participant == caller).then_some(state.role))
            .ok_or(GuestMembershipWriteError::NotAuthorized)?;
        if !super::members::effective_workspace_access_in_tx(&mut tx, workspace, caller).await? {
            return Err(GuestMembershipWriteError::NotAuthorized);
        }
        if !caller_role.can_administer() {
            return Err(GuestMembershipWriteError::NotAuthorized);
        }
        let Some(target_state) = memberships
            .iter()
            .find(|state| state.participant == participant)
        else {
            return Ok(Vec::new());
        };
        if matches!(
            target_state.role,
            WorkspaceRole::Owner | WorkspaceRole::Admin
        ) {
            return Err(GuestMembershipWriteError::ExistingMember);
        }
        if !target_state.is_guest {
            return Ok(Vec::new());
        }

        lock_workspace_rooms(&mut tx, workspace).await?;
        let affected_rooms = sqlx::query_scalar::<_, uuid::Uuid>(
            r"SELECT rm.room_id
                FROM room_members rm
                JOIN rooms r ON r.id = rm.room_id
               WHERE r.workspace_id = $1
                 AND rm.participant_id = $2
               ORDER BY rm.room_id",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(RoomId::from_uuid)
        .collect::<Vec<_>>();

        sqlx::query(
            r"DELETE FROM workspace_members
               WHERE workspace_id = $1
                 AND participant_id = $2
                 AND is_guest = true
                 AND role NOT IN ('owner', 'admin')",
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
        Ok(affected_rooms)
    }

    /// Test-only low-level fixture helper that enrolls a new guest.
    ///
    /// Existing workspace members are deliberately never converted.
    #[cfg(test)]
    pub(crate) async fn add_guest_member(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO workspace_members (workspace_id, participant_id, role, is_guest, joined_at)
               VALUES ($1, $2, $3, true, NOW())
               ON CONFLICT (workspace_id, participant_id) DO NOTHING",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(WorkspaceRole::Member.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Whether `participant` is a **guest** member of `workspace`.
    pub async fn is_guest(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (bool,)>(
            r"SELECT COALESCE(BOOL_OR(is_guest), false)
               FROM workspace_members
               WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// List the workspace's guest members, oldest joiners first.
    pub async fn list_guests(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<WorkspaceMember>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, String, time::OffsetDateTime)>(
            r"SELECT workspace_id, participant_id, role, joined_at
               FROM workspace_members
               WHERE workspace_id = $1 AND is_guest = true
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
}

#[derive(Clone, Copy)]
struct LockedGuestMembership {
    participant: ParticipantId,
    role: WorkspaceRole,
    is_guest: bool,
}

async fn lock_guest_workspace(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
) -> Result<(), GuestMembershipWriteError> {
    let exists =
        sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .is_some();
    if exists {
        Ok(())
    } else {
        Err(GuestMembershipWriteError::WorkspaceNotFound)
    }
}

async fn locked_guest_memberships(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
    participants: &[ParticipantId],
) -> Result<Vec<LockedGuestMembership>, sqlx::Error> {
    let mut ids: Vec<uuid::Uuid> = participants.iter().map(ParticipantId::to_uuid).collect();
    ids.sort_unstable();
    ids.dedup();
    let rows = sqlx::query_as::<_, (uuid::Uuid, String, bool)>(
        r"SELECT participant_id, role, is_guest
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
        .filter_map(|(id, role, is_guest)| {
            WorkspaceRole::from_db_str(&role).map(|role| LockedGuestMembership {
                participant: ParticipantId::from_uuid(id),
                role,
                is_guest,
            })
        })
        .collect())
}

async fn lock_workspace_rooms(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
) -> Result<Vec<(RoomId, String)>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (uuid::Uuid, String)>(
        r"SELECT id, kind
            FROM rooms
           WHERE workspace_id = $1
           ORDER BY id
           FOR UPDATE",
    )
    .bind(workspace.to_uuid())
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, kind)| (RoomId::from_uuid(id), kind))
        .collect())
}

#[cfg(test)]
#[path = "guests/tests.rs"]
mod tests;
