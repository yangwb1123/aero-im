//! Channel operations — join, leave, archive, metadata, post-policy.
//!
//! Extracted from `service.rs` as part of REFACTOR_PLAN.md Step 1h.

use aero_common::{
    Error, MembershipOp, Message, MessageId, ParticipantId, Result, Room, RoomEvent, RoomId,
    WorkspaceId,
};
use aero_storage::{ChannelMetaPatch, RoomMembershipWriteError};
use tracing::{instrument, warn};

use crate::ImService;

pub(super) fn map_channel_write_error(error: RoomMembershipWriteError) -> Error {
    match error {
        RoomMembershipWriteError::WorkspaceNotFound => Error::NotFound("workspace".into()),
        RoomMembershipWriteError::RoomNotFound => Error::NotFound("room".into()),
        RoomMembershipWriteError::NotChannel => Error::Invalid("room is not a channel".into()),
        RoomMembershipWriteError::FixedMembership => {
            Error::Conflict("direct and group-DM membership is fixed".into())
        }
        RoomMembershipWriteError::NotJoinable => {
            Error::Forbidden("channel is private or archived".into())
        }
        RoomMembershipWriteError::MemberNotFound => Error::NotFound("room member".into()),
        RoomMembershipWriteError::NotAuthorized => {
            Error::Forbidden("current channel management authority is required".into())
        }
        RoomMembershipWriteError::LastOwner => {
            Error::Conflict("cannot remove or demote the last channel owner".into())
        }
        RoomMembershipWriteError::TargetNotEligible => Error::Conflict(
            "target must be a retained, non-guest workspace member with an active account".into(),
        ),
        RoomMembershipWriteError::TransferToSelf => {
            Error::Invalid("cannot transfer channel ownership to yourself".into())
        }
        RoomMembershipWriteError::InvalidInput(message) => Error::Invalid(message),
        RoomMembershipWriteError::Storage(error) => Error::from(error),
    }
}

impl ImService {
    /// Join a public channel. The room must be a non-archived PUBLIC channel in a
    /// workspace the actor belongs to ([`can_join_public_channel`]); then the
    /// actor is enrolled and a `Membership { Join }` event fans out to the room.
    /// Idempotent at the storage layer (re-join is a no-op upsert).
    #[instrument(skip(self), fields(?actor, ?room))]
    pub async fn join_channel(&self, actor: ParticipantId, room: RoomId) -> Result<()> {
        let (workspace, inserted) = self
            .rooms
            .join_public_channel_authorized(room, actor)
            .await
            .map_err(map_channel_write_error)?;
        if !inserted {
            return Ok(());
        }
        // Apply workspace default notification level (ROADMAP12 migration 0119).
        // Best-effort: any error here is logged and ignored — a notification
        // pref failure must never block a successful join.
        if let Some(notif_defaults) = self.workspace_notif_defaults.as_ref() {
            if let Some(prefs) = self.prefs.as_ref() {
                match notif_defaults.get(workspace).await {
                    Ok(Some(ref level)) if level != "all" => {
                        // Only set if the user has no existing pref for this room.
                        match prefs.get_level(actor, room).await {
                            Ok(None) => {
                                if let Err(err) =
                                    prefs.set_level_authorized(actor, room, level).await
                                {
                                    warn!(?err, %actor, %room, "apply workspace notif default failed");
                                }
                            }
                            Ok(Some(_)) => {} // user already has an explicit pref — leave it
                            Err(err) => {
                                warn!(?err, %actor, %room, "get_level for notif default check failed");
                            }
                        }
                    }
                    Ok(_) => {} // no default set, or default is "all" (the system default — no-op)
                    Err(err) => warn!(?err, %workspace, "fetch workspace notif default failed"),
                }
            }
        }
        self.publish_room_event(
            room,
            &RoomEvent::Membership {
                room_id: room,
                participant: actor,
                op: MembershipOp::Join,
            },
        )
        .await;
        Ok(())
    }

    /// Leave a channel the actor is a member of. Emits `Membership { Leave }`.
    /// Idempotent: leaving a room you are not in is a no-op success.
    #[instrument(skip(self), fields(?actor, ?room))]
    pub async fn leave_channel(&self, actor: ParticipantId, room: RoomId) -> Result<()> {
        let removed = self
            .rooms
            .leave_channel_authorized(room, actor)
            .await
            .map_err(map_channel_write_error)?;
        if removed {
            self.publish_room_event(
                room,
                &RoomEvent::Membership {
                    room_id: room,
                    participant: actor,
                    op: MembershipOp::Leave,
                },
            )
            .await;
        }
        Ok(())
    }

    /// Archive (or un-archive) a channel. Requires current effective channel or
    /// workspace management authority, rechecked in the write transaction.
    #[instrument(skip(self), fields(?actor, ?room, archived))]
    pub async fn archive_channel(
        &self,
        actor: ParticipantId,
        room: RoomId,
        archived: bool,
    ) -> Result<()> {
        self.rooms
            .set_channel_archived_authorized(room, actor, archived)
            .await
            .map_err(map_channel_write_error)
    }

    /// Update a channel's metadata (topic, description, visibility). Each field is
    /// optional — only provided fields are written. All supplied fields are
    /// committed in one SQL update after a transaction-owned current-manager
    /// recheck.
    #[instrument(skip(self), fields(?actor, ?room))]
    pub async fn set_channel_meta(
        &self,
        actor: ParticipantId,
        room: RoomId,
        topic: Option<Option<String>>,
        description: Option<Option<String>>,
        is_private: Option<bool>,
    ) -> Result<()> {
        self.rooms
            .patch_channel_authorized(
                room,
                actor,
                ChannelMetaPatch {
                    topic,
                    description,
                    is_private,
                },
            )
            .await
            .map_err(map_channel_write_error)
    }

    /// Set a room's post policy (announcement channels, migration 0030). `policy`
    /// must be `everyone` or `admins` — any other value is rejected with
    /// [`Error::Invalid`]. The actor must be the room's creator OR a workspace
    /// Admin/Owner of the room's workspace (the SAME governance bar as the rest of
    /// channel administration); otherwise [`Error::Forbidden`]. Returns
    /// [`Error::NotFound`] for an unknown room.
    #[instrument(skip(self), fields(?actor, ?room, policy))]
    pub async fn set_room_post_policy(
        &self,
        actor: ParticipantId,
        room: RoomId,
        policy: &str,
    ) -> Result<()> {
        if policy != "everyone" && policy != "admins" {
            return Err(Error::Invalid(format!(
                "post_policy must be 'everyone' or 'admins', got {policy:?}"
            )));
        }
        self.rooms
            .set_channel_post_policy_authorized(room, actor, policy)
            .await
            .map_err(map_channel_write_error)
    }

    /// Read a room's post policy (`everyone` or `admins`). The actor must be able
    /// to access the room ([`assert_room_access`](Self::assert_room_access)).
    #[instrument(skip(self), fields(?actor, ?room))]
    pub async fn room_post_policy(&self, actor: ParticipantId, room: RoomId) -> Result<String> {
        self.assert_channel_access(actor, room).await?;
        Ok(self.rooms.post_policy(room).await?)
    }

    /// Set channel slowmode after transactionally rechecking effective manager
    /// authority.
    pub async fn set_channel_slowmode(
        &self,
        actor: ParticipantId,
        room: RoomId,
        seconds: i32,
    ) -> Result<()> {
        self.rooms
            .set_channel_slowmode_authorized(room, actor, seconds)
            .await
            .map_err(map_channel_write_error)
    }

    /// Set or clear a channel reaction cap after transactionally rechecking
    /// effective manager authority.
    pub async fn set_channel_reaction_limit(
        &self,
        actor: ParticipantId,
        room: RoomId,
        limit: Option<i32>,
    ) -> Result<()> {
        self.rooms
            .set_channel_reaction_limit_authorized(room, actor, limit)
            .await
            .map_err(map_channel_write_error)
    }

    /// Set or clear a channel retention override after transactionally
    /// rechecking the actor's current room/workspace management authority.
    pub async fn set_channel_retention(
        &self,
        actor: ParticipantId,
        room: RoomId,
        days: Option<i32>,
    ) -> Result<()> {
        self.rooms
            .set_channel_retention_authorized(room, actor, days)
            .await
            .map_err(map_channel_write_error)
    }

    /// List the public, joinable channels of a workspace. Requires the actor be a
    /// member of the workspace. An optional `q` name-filter performs a
    /// case-insensitive substring match (`ILIKE '%q%'`) on the channel name.
    #[instrument(skip(self), fields(?actor, ?workspace))]
    pub async fn list_workspace_channels(
        &self,
        actor: ParticipantId,
        workspace: WorkspaceId,
        q: Option<&str>,
    ) -> Result<Vec<Room>> {
        if self
            .workspaces()?
            .effective_member_role(workspace, actor)
            .await?
            .is_none()
        {
            return Err(Error::Forbidden(format!(
                "{actor} may not access workspace {workspace}"
            )));
        }
        Ok(self.rooms.list_public_channels(workspace, q).await?)
    }

    /// Snapshot the current member set of a room (for callee fan-out, etc).
    pub async fn room_members(&self, room: RoomId) -> Result<Vec<ParticipantId>> {
        Ok(self.rooms.members(room).await?)
    }

    /// Paginated history. `before` is exclusive.
    #[instrument(skip(self), fields(?who, ?room, ?before, limit))]
    pub async fn history(
        &self,
        who: ParticipantId,
        room: RoomId,
        before: Option<MessageId>,
        limit: i64,
    ) -> Result<Vec<Message>> {
        self.assert_room_access(who, room).await?;
        Ok(self.messages.list_recent(room, before, limit).await?)
    }
}

#[cfg(test)]
mod authorization_mapping_tests {
    use super::*;

    #[test]
    fn channel_governance_errors_keep_forbidden_and_not_found_statuses() {
        assert_eq!(
            map_channel_write_error(RoomMembershipWriteError::NotAuthorized).status_code(),
            403
        );
        assert_eq!(
            map_channel_write_error(RoomMembershipWriteError::RoomNotFound).status_code(),
            404
        );
        assert_eq!(
            map_channel_write_error(RoomMembershipWriteError::LastOwner).status_code(),
            409
        );
    }
}
