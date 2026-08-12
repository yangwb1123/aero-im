//! Transaction-owned room membership and channel administration.

use aero_common::{
    ParticipantId, Room, RoomId, RoomKind, WorkspaceId, WorkspaceRole, LOCAL_ACTION_ROOM_ARCHIVED,
    LOCAL_ACTION_ROOM_CREATE,
};

use super::RoomRepo;

mod service_identity;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomMemberRole {
    Owner,
    Admin,
    Member,
}

impl RoomMemberRole {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Member => "member",
        }
    }

    fn from_db_str(role: &str) -> Option<Self> {
        match role {
            "owner" => Some(Self::Owner),
            "admin" => Some(Self::Admin),
            "member" => Some(Self::Member),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RoomMembershipWriteError {
    #[error("workspace not found")]
    WorkspaceNotFound,
    #[error("room not found")]
    RoomNotFound,
    #[error("room is not a channel")]
    NotChannel,
    #[error("direct and group-DM membership is fixed")]
    FixedMembership,
    #[error("channel is private, archived, or otherwise not joinable")]
    NotJoinable,
    #[error("room member not found")]
    MemberNotFound,
    #[error("caller is not authorized to manage channel membership")]
    NotAuthorized,
    #[error("channel must retain at least one owner")]
    LastOwner,
    #[error("ownership recipient must have current effective workspace access")]
    TargetNotEligible,
    #[error("cannot transfer channel ownership to yourself")]
    TransferToSelf,
    #[error("invalid channel metadata: {0}")]
    InvalidInput(String),
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

pub const MAX_CHANNEL_TOPIC_CHARS: usize = 250;
pub const MAX_CHANNEL_DESCRIPTION_CHARS: usize = 4_000;

#[derive(Debug, Clone, Default)]
pub struct ChannelMetaPatch {
    pub topic: Option<Option<String>>,
    pub description: Option<Option<String>>,
    pub is_private: Option<bool>,
}

impl ChannelMetaPatch {
    fn validate(&self) -> Result<(), RoomMembershipWriteError> {
        if self
            .topic
            .as_ref()
            .and_then(Option::as_ref)
            .is_some_and(|value| value.chars().count() > MAX_CHANNEL_TOPIC_CHARS)
        {
            return Err(RoomMembershipWriteError::InvalidInput(format!(
                "topic is too long (max {MAX_CHANNEL_TOPIC_CHARS} chars)"
            )));
        }
        if self
            .description
            .as_ref()
            .and_then(Option::as_ref)
            .is_some_and(|value| value.chars().count() > MAX_CHANNEL_DESCRIPTION_CHARS)
        {
            return Err(RoomMembershipWriteError::InvalidInput(format!(
                "description is too long (max {MAX_CHANNEL_DESCRIPTION_CHARS} chars)"
            )));
        }
        Ok(())
    }
}

impl RoomRepo {
    /// Create a mutable room and its owner edge only while the creator still has
    /// effective Member-or-higher access to the workspace.
    ///
    /// The workspace row is the governance serialization point. Holding it
    /// through both inserts prevents a concurrent membership revocation,
    /// deactivation, account deletion, or mandatory-2FA change from authorizing
    /// a room after the creator has lost access.
    pub async fn create_in_workspace_authorized(
        &self,
        workspace: WorkspaceId,
        kind: RoomKind,
        name: Option<String>,
        creator: ParticipantId,
    ) -> Result<Room, RoomMembershipWriteError> {
        if kind == RoomKind::Direct {
            return Err(RoomMembershipWriteError::FixedMembership);
        }

        let mut tx = self.pool.begin().await?;
        lock_workspace(&mut tx, workspace).await?;
        let (role, is_guest) = locked_workspace_member(&mut tx, workspace, creator)
            .await?
            .ok_or(RoomMembershipWriteError::NotAuthorized)?;
        if is_guest
            || !role.at_least(WorkspaceRole::Member)
            || !effective_access(&mut tx, workspace, creator).await?
        {
            return Err(RoomMembershipWriteError::NotAuthorized);
        }

        let id = RoomId::new();
        let created_at = time::OffsetDateTime::now_utc();
        sqlx::query(
            r"INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
               VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id.to_uuid())
        .bind(super::room_kind_str(kind))
        .bind(&name)
        .bind(creator.to_uuid())
        .bind(created_at)
        .bind(workspace.to_uuid())
        .execute(&mut *tx)
        .await
        .map_err(map_storage_error)?;
        sqlx::query(
            r"INSERT INTO room_members (room_id, participant_id, role, joined_at)
               VALUES ($1, $2, 'owner', $3)",
        )
        .bind(id.to_uuid())
        .bind(creator.to_uuid())
        .bind(created_at)
        .execute(&mut *tx)
        .await
        .map_err(map_storage_error)?;
        // B5-1 S3: the room.create audit row rides the same transaction as
        // the room + owner edge. The 0245 AFTER INSERT trigger materializes
        // the 1:1 class 'room' outbox row in the same tx (commit ⇔ exactly 1
        // audit row + 1 outbox row). Detail carries only server-derived/
        // route-validated fields (room_id, kind) — the unvalidated `name`
        // argument is deliberately excluded (G-SEC2). `RoomKind::Direct` was
        // rejected above, so DM creation structurally never reaches this seam.
        crate::audit::AuditRepo::append_in_tx(
            &mut tx,
            workspace,
            Some(creator),
            LOCAL_ACTION_ROOM_CREATE,
            Some(&id.to_string()),
            serde_json::json!({
                "room_id": id,
                "kind": super::room_kind_str(kind),
            }),
        )
        .await
        .map_err(map_storage_error)?;
        tx.commit().await.map_err(map_storage_error)?;

        Ok(Room {
            id,
            kind,
            name,
            created_by: creator,
            created_at,
        })
    }

    /// Add a retained, non-guest workspace member to a mutable room while the
    /// caller's current room/workspace management authority is held to commit.
    ///
    /// A target may be deactivated or awaiting mandatory 2FA so administrators
    /// can pre-provision future access, but the participant must still exist and
    /// must not be deleted. Direct and marked group-DM membership is immutable.
    pub async fn add_member_authorized(
        &self,
        room: RoomId,
        caller: ParticipantId,
        participant: ParticipantId,
    ) -> Result<bool, RoomMembershipWriteError> {
        let mut tx = self.pool.begin().await?;
        let (workspace, kind, is_group_dm) = lock_room_aggregate(&mut tx, room).await?;
        if kind == "direct" || is_group_dm {
            return Err(RoomMembershipWriteError::FixedMembership);
        }
        authorize_room_manager(&mut tx, workspace, room, caller).await?;
        let (target_role, target_is_guest) =
            locked_workspace_member(&mut tx, workspace, participant)
                .await?
                .ok_or(RoomMembershipWriteError::TargetNotEligible)?;
        if target_role == WorkspaceRole::Guest
            || target_is_guest
            || !active_participant(&mut tx, participant).await?
        {
            return Err(RoomMembershipWriteError::TargetNotEligible);
        }
        let inserted = sqlx::query(
            r"INSERT INTO room_members (room_id, participant_id, role, joined_at)
               VALUES ($1, $2, 'member', NOW())
               ON CONFLICT (room_id, participant_id) DO NOTHING",
        )
        .bind(room.to_uuid())
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await
        .map_err(map_storage_error)?
        .rows_affected()
            == 1;
        tx.commit().await.map_err(map_storage_error)?;
        Ok(inserted)
    }

    /// Join an open channel after rechecking all joinability and actor access
    /// gates under the workspace + channel locks used by the insert.
    pub async fn join_public_channel_authorized(
        &self,
        room: RoomId,
        participant: ParticipantId,
    ) -> Result<(WorkspaceId, bool), RoomMembershipWriteError> {
        let mut tx = self.pool.begin().await?;
        let workspace = lock_channel_aggregate(&mut tx, room).await?;
        let (is_private, is_archived) = sqlx::query_as::<_, (bool, bool)>(
            "SELECT is_private, is_archived FROM rooms WHERE id = $1",
        )
        .bind(room.to_uuid())
        .fetch_one(&mut *tx)
        .await?;
        if is_private || is_archived {
            return Err(RoomMembershipWriteError::NotJoinable);
        }
        let (role, is_guest) = locked_workspace_member(&mut tx, workspace, participant)
            .await?
            .ok_or(RoomMembershipWriteError::NotAuthorized)?;
        if role == WorkspaceRole::Guest
            || is_guest
            || !effective_access(&mut tx, workspace, participant).await?
        {
            return Err(RoomMembershipWriteError::NotAuthorized);
        }
        let inserted = sqlx::query(
            r"INSERT INTO room_members (room_id, participant_id, role, joined_at)
               VALUES ($1, $2, 'member', NOW())
               ON CONFLICT (room_id, participant_id) DO NOTHING",
        )
        .bind(room.to_uuid())
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await
        .map_err(map_storage_error)?
        .rows_affected()
            == 1;
        tx.commit().await.map_err(map_storage_error)?;
        Ok((workspace, inserted))
    }

    /// Self-scoped channel leave. Revoked users may discard retained privilege,
    /// but a sole owner cannot orphan the channel.
    pub async fn leave_channel_authorized(
        &self,
        room: RoomId,
        participant: ParticipantId,
    ) -> Result<bool, RoomMembershipWriteError> {
        let mut tx = self.pool.begin().await?;
        crate::ownership::lock_membership_governance(&mut tx).await?;
        let workspace = lock_channel_aggregate(&mut tx, room).await?;
        let role = locked_room_roles(&mut tx, room, &[participant])
            .await?
            .into_iter()
            .find_map(|(id, role)| (id == participant).then_some(role));
        let Some(role) = role else {
            tx.commit().await?;
            return Ok(false);
        };
        if role == RoomMemberRole::Owner
            && !another_effective_channel_owner(&mut tx, workspace, room, participant).await?
        {
            return Err(RoomMembershipWriteError::LastOwner);
        }
        let deleted =
            sqlx::query("DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
                .bind(room.to_uuid())
                .bind(participant.to_uuid())
                .execute(&mut *tx)
                .await
                .map_err(map_storage_error)?;
        tx.commit().await?;
        Ok(deleted.rows_affected() == 1)
    }

    /// Change a member role with owner authority, effective caller access, target
    /// membership, and final-owner validation held through commit.
    pub async fn change_channel_member_role_authorized(
        &self,
        room: RoomId,
        caller: ParticipantId,
        participant: ParticipantId,
        role: RoomMemberRole,
    ) -> Result<RoomMemberRole, RoomMembershipWriteError> {
        let mut tx = self.pool.begin().await?;
        crate::ownership::lock_membership_governance(&mut tx).await?;
        let workspace = lock_channel_aggregate(&mut tx, room).await?;
        let roles = locked_room_roles(&mut tx, room, &[caller, participant]).await?;
        let caller_role = roles
            .iter()
            .find_map(|(id, role)| (*id == caller).then_some(*role))
            .ok_or(RoomMembershipWriteError::NotAuthorized)?;
        let previous = roles
            .iter()
            .find_map(|(id, role)| (*id == participant).then_some(*role))
            .ok_or(RoomMembershipWriteError::MemberNotFound)?;
        if caller_role != RoomMemberRole::Owner
            || !effective_access(&mut tx, workspace, caller).await?
        {
            return Err(RoomMembershipWriteError::NotAuthorized);
        }
        if role == RoomMemberRole::Owner
            && !effective_access(&mut tx, workspace, participant).await?
        {
            return Err(RoomMembershipWriteError::TargetNotEligible);
        }
        if previous == RoomMemberRole::Owner
            && role != RoomMemberRole::Owner
            && !another_effective_channel_owner(&mut tx, workspace, room, participant).await?
        {
            return Err(RoomMembershipWriteError::LastOwner);
        }
        let changed = sqlx::query(
            "UPDATE room_members SET role = $3 WHERE room_id = $1 AND participant_id = $2",
        )
        .bind(room.to_uuid())
        .bind(participant.to_uuid())
        .bind(role.as_str())
        .execute(&mut *tx)
        .await
        .map_err(map_storage_error)?;
        if changed.rows_affected() != 1 {
            return Err(RoomMembershipWriteError::MemberNotFound);
        }
        tx.commit().await?;
        Ok(previous)
    }

    /// Atomically promote the recipient and demote the current owner.
    pub async fn transfer_channel_ownership_authorized(
        &self,
        room: RoomId,
        caller: ParticipantId,
        participant: ParticipantId,
    ) -> Result<(), RoomMembershipWriteError> {
        if caller == participant {
            return Err(RoomMembershipWriteError::TransferToSelf);
        }
        let mut tx = self.pool.begin().await?;
        crate::ownership::lock_membership_governance(&mut tx).await?;
        let workspace = lock_channel_aggregate(&mut tx, room).await?;
        let roles = locked_room_roles(&mut tx, room, &[caller, participant]).await?;
        let caller_role = roles
            .iter()
            .find_map(|(id, role)| (*id == caller).then_some(*role))
            .ok_or(RoomMembershipWriteError::NotAuthorized)?;
        roles
            .iter()
            .find(|(id, _)| *id == participant)
            .ok_or(RoomMembershipWriteError::MemberNotFound)?;
        if caller_role != RoomMemberRole::Owner
            || !effective_access(&mut tx, workspace, caller).await?
        {
            return Err(RoomMembershipWriteError::NotAuthorized);
        }
        if !effective_access(&mut tx, workspace, participant).await? {
            return Err(RoomMembershipWriteError::TargetNotEligible);
        }
        let promoted = sqlx::query(
            "UPDATE room_members SET role = 'owner' WHERE room_id = $1 AND participant_id = $2",
        )
        .bind(room.to_uuid())
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await
        .map_err(map_storage_error)?;
        let demoted = sqlx::query(
            r"UPDATE room_members
                  SET role = 'member'
                WHERE room_id = $1 AND participant_id = $2 AND role = 'owner'",
        )
        .bind(room.to_uuid())
        .bind(caller.to_uuid())
        .execute(&mut *tx)
        .await
        .map_err(map_storage_error)?;
        if promoted.rows_affected() != 1 || demoted.rows_affected() != 1 {
            return Err(RoomMembershipWriteError::NotAuthorized);
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_channel_archived_authorized(
        &self,
        room: RoomId,
        caller: ParticipantId,
        archived: bool,
    ) -> Result<(), RoomMembershipWriteError> {
        let mut tx = self.pool.begin().await?;
        let workspace = lock_channel_aggregate(&mut tx, room).await?;
        authorize_channel_manager(&mut tx, workspace, room, caller).await?;
        update_one(
            &sqlx::query("UPDATE rooms SET is_archived = $2 WHERE id = $1 AND kind = 'channel'")
                .bind(room.to_uuid())
                .bind(archived)
                .execute(&mut *tx)
                .await?,
        )?;
        // B5-1 S4: the room.archived audit row rides the same transaction.
        // `update_one` guarantees exactly one row was updated, so every
        // successful archive/unarchive produces exactly one audit row (same
        // token, `detail.archived` carries the new flag); the 0245 trigger
        // materializes the 1:1 class 'room' outbox row in the same tx.
        crate::audit::AuditRepo::append_in_tx(
            &mut tx,
            workspace,
            Some(caller),
            LOCAL_ACTION_ROOM_ARCHIVED,
            Some(&room.to_string()),
            serde_json::json!({ "room_id": room, "archived": archived }),
        )
        .await
        .map_err(map_storage_error)?;
        tx.commit().await?;
        Ok(())
    }

    /// Apply all supplied metadata fields with one SQL UPDATE.
    pub async fn patch_channel_authorized(
        &self,
        room: RoomId,
        caller: ParticipantId,
        patch: ChannelMetaPatch,
    ) -> Result<(), RoomMembershipWriteError> {
        patch.validate()?;
        let mut tx = self.pool.begin().await?;
        let workspace = lock_channel_aggregate(&mut tx, room).await?;
        authorize_channel_manager(&mut tx, workspace, room, caller).await?;
        // `lock_channel_aggregate` already owns the room row lock. Capture the
        // canonical old topic under that lock so the history edge cannot be
        // confused by a concurrent metadata patch.
        let old_topic =
            sqlx::query_scalar::<_, Option<String>>("SELECT topic FROM rooms WHERE id = $1")
                .bind(room.to_uuid())
                .fetch_one(&mut *tx)
                .await?;
        let topic_present = patch.topic.is_some();
        let topic = patch.topic.flatten();
        let description_present = patch.description.is_some();
        let description = patch.description.flatten();
        let visibility_present = patch.is_private.is_some();
        let visibility = patch.is_private.unwrap_or(false);
        update_one(
            &sqlx::query(
                r"UPDATE rooms
                      SET topic = CASE WHEN $2 THEN $3 ELSE topic END,
                          description = CASE WHEN $4 THEN $5 ELSE description END,
                          is_private = CASE WHEN $6 THEN $7 ELSE is_private END
                    WHERE id = $1 AND kind = 'channel'",
            )
            .bind(room.to_uuid())
            .bind(topic_present)
            .bind(topic.as_deref())
            .bind(description_present)
            .bind(description.as_deref())
            .bind(visibility_present)
            .bind(visibility)
            .execute(&mut *tx)
            .await?,
        )?;
        // History is part of the mutation, not a best-effort follow-up. A
        // failure here rolls back the room UPDATE as well, and no-op/omitted
        // topic patches do not manufacture audit entries.
        if topic_present && old_topic != topic {
            sqlx::query(
                r"INSERT INTO channel_topic_history
                       (room_id, changed_by, old_topic, new_topic, changed_at)
                   VALUES ($1, $2, $3, $4, now())",
            )
            .bind(room.to_uuid())
            .bind(caller.to_uuid())
            .bind(old_topic.as_deref())
            .bind(topic.as_deref())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_channel_post_policy_authorized(
        &self,
        room: RoomId,
        caller: ParticipantId,
        policy: &str,
    ) -> Result<(), RoomMembershipWriteError> {
        if !matches!(policy, "everyone" | "admins") {
            return Err(RoomMembershipWriteError::InvalidInput(
                "post_policy must be 'everyone' or 'admins'".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        let workspace = lock_channel_aggregate(&mut tx, room).await?;
        authorize_channel_manager(&mut tx, workspace, room, caller).await?;
        update_one(
            &sqlx::query("UPDATE rooms SET post_policy = $2 WHERE id = $1 AND kind = 'channel'")
                .bind(room.to_uuid())
                .bind(policy)
                .execute(&mut *tx)
                .await?,
        )?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_channel_slowmode_authorized(
        &self,
        room: RoomId,
        caller: ParticipantId,
        seconds: i32,
    ) -> Result<(), RoomMembershipWriteError> {
        let mut tx = self.pool.begin().await?;
        let workspace = lock_channel_aggregate(&mut tx, room).await?;
        authorize_channel_manager(&mut tx, workspace, room, caller).await?;
        update_one(
            &sqlx::query(
                "UPDATE rooms SET slowmode_seconds = $2 WHERE id = $1 AND kind = 'channel'",
            )
            .bind(room.to_uuid())
            .bind(seconds)
            .execute(&mut *tx)
            .await?,
        )?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_channel_reaction_limit_authorized(
        &self,
        room: RoomId,
        caller: ParticipantId,
        limit: Option<i32>,
    ) -> Result<(), RoomMembershipWriteError> {
        let mut tx = self.pool.begin().await?;
        let workspace = lock_channel_aggregate(&mut tx, room).await?;
        authorize_channel_manager(&mut tx, workspace, room, caller).await?;
        update_one(
            &sqlx::query(
                r"UPDATE rooms
                      SET max_reactions_per_user = $2
                    WHERE id = $1 AND kind = 'channel'",
            )
            .bind(room.to_uuid())
            .bind(limit)
            .execute(&mut *tx)
            .await?,
        )?;
        tx.commit().await?;
        Ok(())
    }

    /// Set or clear a channel retention override after rechecking current room
    /// or workspace management authority under the same aggregate locks.
    pub async fn set_channel_retention_authorized(
        &self,
        room: RoomId,
        caller: ParticipantId,
        days: Option<i32>,
    ) -> Result<(), RoomMembershipWriteError> {
        let mut tx = self.pool.begin().await?;
        let workspace = lock_channel_aggregate(&mut tx, room).await?;
        authorize_channel_manager(&mut tx, workspace, room, caller).await?;
        update_one(
            &sqlx::query("UPDATE rooms SET retention_days = $2 WHERE id = $1 AND kind = 'channel'")
                .bind(room.to_uuid())
                .bind(days)
                .execute(&mut *tx)
                .await?,
        )?;
        tx.commit().await?;
        Ok(())
    }
}

fn update_one(result: &sqlx::postgres::PgQueryResult) -> Result<(), RoomMembershipWriteError> {
    if result.rows_affected() == 1 {
        Ok(())
    } else {
        Err(RoomMembershipWriteError::RoomNotFound)
    }
}

async fn effective_access(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Result<bool, sqlx::Error> {
    crate::workspace::members::effective_workspace_access_in_tx(tx, workspace, participant).await
}

async fn lock_workspace(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
) -> Result<(), RoomMembershipWriteError> {
    if sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace.to_uuid())
        .fetch_optional(&mut **tx)
        .await?
        .is_some()
    {
        Ok(())
    } else {
        Err(RoomMembershipWriteError::WorkspaceNotFound)
    }
}

/// Global governance lock order: workspace → room → membership rows.
async fn lock_room_aggregate(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    room: RoomId,
) -> Result<(WorkspaceId, String, bool), RoomMembershipWriteError> {
    let workspace =
        sqlx::query_scalar::<_, uuid::Uuid>("SELECT workspace_id FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(RoomMembershipWriteError::RoomNotFound)?;
    let workspace = WorkspaceId::from_uuid(workspace);
    lock_workspace(tx, workspace).await?;
    let locked = sqlx::query_as::<_, (uuid::Uuid, String, bool)>(
        "SELECT workspace_id, kind, is_group_dm FROM rooms WHERE id = $1 FOR UPDATE",
    )
    .bind(room.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(RoomMembershipWriteError::RoomNotFound)?;
    if locked.0 != workspace.to_uuid() {
        return Err(RoomMembershipWriteError::RoomNotFound);
    }
    Ok((workspace, locked.1, locked.2))
}

/// Global governance lock order: workspace → room → membership rows.
async fn lock_channel_aggregate(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    room: RoomId,
) -> Result<WorkspaceId, RoomMembershipWriteError> {
    let (workspace, kind, _) = lock_room_aggregate(tx, room).await?;
    if kind != "channel" {
        return Err(RoomMembershipWriteError::NotChannel);
    }
    Ok(workspace)
}

async fn locked_workspace_member(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Result<Option<(WorkspaceRole, bool)>, sqlx::Error> {
    let row = sqlx::query_as::<_, (String, bool)>(
        r"SELECT role, is_guest
            FROM workspace_members
           WHERE workspace_id = $1 AND participant_id = $2
           FOR UPDATE",
    )
    .bind(workspace.to_uuid())
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.and_then(|(role, is_guest)| {
        WorkspaceRole::from_db_str(&role).map(|role| (role, is_guest))
    }))
}

async fn active_participant(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    participant: ParticipantId,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT deleted_at IS NULL FROM participants WHERE id = $1 FOR SHARE",
    )
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(false))
}

async fn locked_room_roles(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    room: RoomId,
    participants: &[ParticipantId],
) -> Result<Vec<(ParticipantId, RoomMemberRole)>, sqlx::Error> {
    let mut ids: Vec<uuid::Uuid> = participants.iter().map(ParticipantId::to_uuid).collect();
    ids.sort_unstable();
    ids.dedup();
    let rows = sqlx::query_as::<_, (uuid::Uuid, String)>(
        r"SELECT participant_id, role
            FROM room_members
           WHERE room_id = $1 AND participant_id = ANY($2)
           ORDER BY participant_id
           FOR UPDATE",
    )
    .bind(room.to_uuid())
    .bind(&ids)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(participant, role)| {
            RoomMemberRole::from_db_str(&role)
                .map(|role| (ParticipantId::from_uuid(participant), role))
        })
        .collect())
}

async fn another_effective_channel_owner(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
    room: RoomId,
    participant: ParticipantId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r"SELECT EXISTS(
               SELECT 1
                 FROM room_members owner_membership
                 JOIN workspace_members workspace_membership
                   ON workspace_membership.workspace_id = $3
                  AND workspace_membership.participant_id =
                      owner_membership.participant_id
                 JOIN participants owner_participant
                   ON owner_participant.id = owner_membership.participant_id
                  AND owner_participant.deleted_at IS NULL
                 JOIN workspaces workspace
                   ON workspace.id = $3
                 LEFT JOIN workspace_deactivations deactivated
                   ON deactivated.workspace_id = $3
                  AND deactivated.participant_id =
                      owner_membership.participant_id
                 LEFT JOIN totp_secrets totp
                   ON totp.participant_id = owner_membership.participant_id
                WHERE owner_membership.room_id = $1
                  AND owner_membership.participant_id <> $2
                  AND owner_membership.role = 'owner'
                  AND deactivated.participant_id IS NULL
                  AND (
                      owner_participant.kind <> 'human'
                      OR NOT workspace.require_2fa
                      OR COALESCE(totp.activated, false)
                  )
           )",
    )
    .bind(room.to_uuid())
    .bind(participant.to_uuid())
    .bind(workspace.to_uuid())
    .fetch_one(&mut **tx)
    .await
}

fn map_storage_error(error: sqlx::Error) -> RoomMembershipWriteError {
    if crate::is_channel_effective_owner_violation(&error) {
        RoomMembershipWriteError::LastOwner
    } else {
        RoomMembershipWriteError::Storage(error)
    }
}

async fn authorize_channel_manager(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
    room: RoomId,
    caller: ParticipantId,
) -> Result<(), RoomMembershipWriteError> {
    authorize_room_manager(tx, workspace, room, caller).await
}

async fn authorize_room_manager(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
    room: RoomId,
    caller: ParticipantId,
) -> Result<(), RoomMembershipWriteError> {
    let room_role = locked_room_roles(tx, room, &[caller])
        .await?
        .into_iter()
        .find_map(|(participant, role)| (participant == caller).then_some(role))
        .ok_or(RoomMembershipWriteError::NotAuthorized)?;
    if !effective_access(tx, workspace, caller).await? {
        return Err(RoomMembershipWriteError::NotAuthorized);
    }
    let workspace_role = sqlx::query_scalar::<_, String>(
        r"SELECT role FROM workspace_members
           WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(workspace.to_uuid())
    .bind(caller.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(RoomMembershipWriteError::NotAuthorized)?;
    if matches!(room_role, RoomMemberRole::Owner | RoomMemberRole::Admin)
        || matches!(workspace_role.as_str(), "owner" | "admin")
    {
        Ok(())
    } else {
        Err(RoomMembershipWriteError::NotAuthorized)
    }
}

#[cfg(test)]
mod db_tests;

#[cfg(test)]
#[path = "governance/write_tests.rs"]
mod write_tests;
