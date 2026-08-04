//! Room-scoped Bot/Agent installation.

use aero_common::{Participant, ParticipantId, ParticipantKind, RoomId};

use super::{
    authorize_room_manager, lock_room_aggregate, map_storage_error, RoomMembershipWriteError,
};
use crate::RoomRepo;

impl RoomRepo {
    /// Create a Bot/Agent and install it into the room's workspace and member
    /// roster in one transaction.
    ///
    /// The caller must remain an effective room/workspace manager through
    /// commit. Direct rooms and marked group DMs have fixed membership and are
    /// rejected before any participant row is written.
    pub async fn create_service_identity_authorized(
        &self,
        room: RoomId,
        caller: ParticipantId,
        display_name: &str,
        kind: ParticipantKind,
        avatar_url: Option<&str>,
    ) -> Result<Participant, RoomMembershipWriteError> {
        let kind_db = match kind {
            ParticipantKind::Bot => "bot",
            ParticipantKind::Agent => "agent",
            ParticipantKind::Human => {
                return Err(RoomMembershipWriteError::InvalidInput(
                    "service identity kind must be bot or agent".into(),
                ));
            }
        };

        let mut tx = self.pool.begin().await?;
        crate::ownership::lock_membership_governance(&mut tx).await?;
        let (workspace, room_kind, is_group_dm) = lock_room_aggregate(&mut tx, room).await?;
        if room_kind == "direct" || is_group_dm {
            return Err(RoomMembershipWriteError::FixedMembership);
        }
        authorize_room_manager(&mut tx, workspace, room, caller).await?;

        let participant = Participant {
            id: ParticipantId::new(),
            kind,
            display_name: display_name.to_owned(),
            avatar_url: avatar_url.map(str::to_owned),
            created_by: Some(caller),
            created_at: time::OffsetDateTime::now_utc(),
        };
        sqlx::query(
            r"INSERT INTO participants
                  (id, kind, display_name, avatar_url, created_by, created_at)
               VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(participant.id.to_uuid())
        .bind(kind_db)
        .bind(&participant.display_name)
        .bind(participant.avatar_url.as_deref())
        .bind(caller.to_uuid())
        .bind(participant.created_at)
        .execute(&mut *tx)
        .await
        .map_err(map_storage_error)?;
        sqlx::query(
            r"INSERT INTO workspace_members
                  (workspace_id, participant_id, role, joined_at)
               VALUES ($1, $2, 'member', $3)",
        )
        .bind(workspace.to_uuid())
        .bind(participant.id.to_uuid())
        .bind(participant.created_at)
        .execute(&mut *tx)
        .await
        .map_err(map_storage_error)?;
        sqlx::query(
            r"INSERT INTO room_members
                  (room_id, participant_id, role, joined_at)
               VALUES ($1, $2, 'member', $3)",
        )
        .bind(room.to_uuid())
        .bind(participant.id.to_uuid())
        .bind(participant.created_at)
        .execute(&mut *tx)
        .await
        .map_err(map_storage_error)?;
        tx.commit().await.map_err(map_storage_error)?;
        Ok(participant)
    }
}

#[cfg(test)]
#[path = "service_identity_tests.rs"]
mod db_tests;
