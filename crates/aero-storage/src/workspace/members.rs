//! Workspace membership — add/remove/list/role/guest operations.

use aero_common::{ParticipantId, WorkspaceId, WorkspaceMember, WorkspaceRole};

use super::WorkspaceRepo;

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

    /// Set (insert-or-update) a member's role, idempotently.
    pub async fn update_member_role(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
        role: WorkspaceRole,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO workspace_members (workspace_id, participant_id, role, joined_at)
               VALUES ($1, $2, $3, NOW())
               ON CONFLICT (workspace_id, participant_id)
               DO UPDATE SET role = EXCLUDED.role",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(role.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
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

    /// All workspaces a participant belongs to, most recently created first.
    pub async fn list_for_participant(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<aero_common::Workspace>, sqlx::Error> {
        let rows = sqlx::query_as::<
            _,
            (uuid::Uuid, String, String, Option<uuid::Uuid>, time::OffsetDateTime,
             Option<String>, Option<String>, Option<String>, Option<String>),
        >(
            r"SELECT w.id, w.name, w.slug, w.created_by, w.created_at,
                     w.logo_url, w.color_scheme, w.custom_domain, w.description
               FROM workspaces w
               JOIN workspace_members m ON m.workspace_id = w.id
               WHERE m.participant_id = $1
               ORDER BY w.created_at DESC",
        )
        .bind(participant.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, name, slug, by, at, logo_url, color_scheme, custom_domain, description)| aero_common::Workspace {
                id: WorkspaceId::from_uuid(id),
                name,
                slug,
                created_by: by.map(ParticipantId::from_uuid),
                created_at: at,
                logo_url,
                color_scheme,
                custom_domain,
                description,
            })
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

    // ---------------------------------------------------------------- single-channel guests

    /// Enroll (or promote) `participant` as a **guest** member of `workspace`.
    pub async fn add_guest_member(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO workspace_members (workspace_id, participant_id, role, is_guest, joined_at)
               VALUES ($1, $2, $3, true, NOW())
               ON CONFLICT (workspace_id, participant_id)
               DO UPDATE SET is_guest = true",
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
