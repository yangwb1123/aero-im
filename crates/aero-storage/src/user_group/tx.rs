use aero_common::{ParticipantId, UserGroupId, WorkspaceId, WorkspaceRole};
use sqlx::{Postgres, Transaction};

use super::{row_to_model, Row, UserGroup, UserGroupWriteError, MAX_HANDLE_CHARS};

pub(super) fn canonical_members(members: &[ParticipantId]) -> Vec<ParticipantId> {
    let mut ids = members.to_vec();
    ids.sort_unstable_by_key(ParticipantId::to_uuid);
    ids.dedup();
    ids
}

pub(super) fn suffixed_handle(base: &str, attempt: u32) -> String {
    if attempt == 0 {
        return base.chars().take(MAX_HANDLE_CHARS).collect();
    }
    let suffix = format!("-{}", attempt + 1);
    let keep = MAX_HANDLE_CHARS.saturating_sub(suffix.chars().count());
    let stem: String = base.chars().take(keep).collect();
    format!("{}{suffix}", stem.trim_end_matches('-'))
}

pub(super) async fn scim_creator_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
) -> Result<ParticipantId, UserGroupWriteError> {
    let participant = sqlx::query_scalar::<_, uuid::Uuid>(
        r"SELECT wm.participant_id
            FROM workspace_members wm
           WHERE wm.workspace_id = $1
             AND wm.role IN ('owner', 'admin', 'member')
           ORDER BY CASE wm.role
                        WHEN 'owner' THEN 0
                        WHEN 'admin' THEN 1
                        ELSE 2
                    END,
                    wm.joined_at,
                    wm.participant_id
           LIMIT 1
           FOR KEY SHARE OF wm",
    )
    .bind(workspace.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(UserGroupWriteError::WorkspaceHasNoCreator)?;
    Ok(ParticipantId::from_uuid(participant))
}

pub(super) async fn lock_workspace_for_group(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
) -> Result<(), UserGroupWriteError> {
    let exists =
        sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .is_some();
    if exists {
        Ok(())
    } else {
        Err(UserGroupWriteError::NotFound)
    }
}

pub(super) async fn lock_group(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    id: UserGroupId,
) -> Result<UserGroup, UserGroupWriteError> {
    let row = sqlx::query_as::<_, Row>(
        r"SELECT id, workspace_id, handle, name, created_by, created_at
            FROM user_groups
           WHERE id = $1 AND workspace_id = $2
           FOR UPDATE",
    )
    .bind(id.to_uuid())
    .bind(workspace.to_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    row.map(row_to_model).ok_or(UserGroupWriteError::NotFound)
}

pub(super) async fn lock_workspace_role(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Result<WorkspaceRole, UserGroupWriteError> {
    let role = sqlx::query_scalar::<_, String>(
        r"SELECT role
            FROM workspace_members
           WHERE workspace_id = $1 AND participant_id = $2
           FOR UPDATE",
    )
    .bind(workspace.to_uuid())
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .and_then(|role| WorkspaceRole::from_db_str(&role))
    .ok_or(UserGroupWriteError::NotAuthorized)?;
    if !crate::workspace::members::effective_workspace_access_in_tx(tx, workspace, participant)
        .await?
    {
        return Err(UserGroupWriteError::NotAuthorized);
    }
    Ok(role)
}

pub(super) async fn lock_managed_group(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    group: UserGroupId,
    caller: ParticipantId,
) -> Result<UserGroup, UserGroupWriteError> {
    // Group-first matches the SCIM replace/PATCH lock order. The subsequent
    // `FOR UPDATE` membership lock still conflicts with both removal and role
    // downgrade and remains held through the mutation commit.
    let group = lock_group(tx, workspace, group).await?;
    let role = lock_workspace_role(tx, workspace, caller).await?;
    if role.can_administer() || group.created_by == caller {
        Ok(group)
    } else {
        Err(UserGroupWriteError::NotAuthorized)
    }
}

pub(super) async fn lock_workspace_members(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    members: &[ParticipantId],
) -> Result<(), UserGroupWriteError> {
    if members.is_empty() {
        return Ok(());
    }
    let ids: Vec<uuid::Uuid> = members.iter().map(ParticipantId::to_uuid).collect();
    let found: std::collections::HashSet<uuid::Uuid> = sqlx::query_scalar::<_, uuid::Uuid>(
        r"SELECT participant_id
            FROM workspace_members
           WHERE workspace_id = $1
             AND participant_id = ANY($2)
           ORDER BY participant_id
           FOR KEY SHARE",
    )
    .bind(workspace.to_uuid())
    .bind(&ids)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .collect();
    for participant in members {
        if !found.contains(&participant.to_uuid()) {
            return Err(UserGroupWriteError::MemberNotInWorkspace(*participant));
        }
    }
    Ok(())
}

pub(super) async fn insert_members(
    tx: &mut Transaction<'_, Postgres>,
    group: UserGroupId,
    members: &[ParticipantId],
) -> Result<(), sqlx::Error> {
    if members.is_empty() {
        return Ok(());
    }
    let ids: Vec<uuid::Uuid> = members.iter().map(ParticipantId::to_uuid).collect();
    sqlx::query(
        r"INSERT INTO user_group_members (group_id, participant_id)
           SELECT $1, member_id
             FROM UNNEST($2::uuid[]) AS member_id
           ON CONFLICT (group_id, participant_id) DO NOTHING",
    )
    .bind(group.to_uuid())
    .bind(&ids)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(super) async fn members_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    group: UserGroupId,
) -> Result<Vec<ParticipantId>, sqlx::Error> {
    let rows = sqlx::query_scalar::<_, uuid::Uuid>(
        r"SELECT participant_id
            FROM user_group_members
           WHERE group_id = $1
           ORDER BY added_at, participant_id",
    )
    .bind(group.to_uuid())
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows.into_iter().map(ParticipantId::from_uuid).collect())
}
