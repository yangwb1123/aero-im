//! Channel operations — join, leave, archive, metadata, post-policy.
//!
//! Extracted from `service.rs` as part of REFACTOR_PLAN.md Step 1h.

use aero_common::{
    Error, MembershipOp, Message, MessageId, ParticipantId, Result, Room, RoomEvent, RoomId,
    WorkspaceId,
};
use tracing::{instrument, warn};

use crate::service::orig::can_join_public_channel;
use crate::ImService;

impl ImService {
    /// Join a public channel. The room must be a non-archived PUBLIC channel in a
    /// workspace the actor belongs to ([`can_join_public_channel`]); then the
    /// actor is enrolled and a `Membership { Join }` event fans out to the room.
    /// Idempotent at the storage layer (re-join is a no-op upsert).
    #[instrument(skip(self), fields(?actor, ?room))]
    pub async fn join_channel(&self, actor: ParticipantId, room: RoomId) -> Result<()> {
        let workspace = self
            .rooms
            .room_workspace(room)
            .await?
            .ok_or_else(|| Error::NotFound(format!("room {room}")))?;
        if !self.workspaces()?.is_member(workspace, actor).await? {
            return Err(Error::Forbidden(format!(
                "{actor} is not a member of workspace {workspace}"
            )));
        }
        // Single-channel guests may NOT self-join open channels: they are confined
        // to the channel(s) an admin explicitly placed them in (the guest admin
        // endpoint), mirroring the guard in [`add_member`]. `join_channel` only
        // runs when a `WorkspaceRepo` is wired (the membership check above already
        // required it), so there is no fail-open branch to add here.
        if self.workspaces()?.is_guest(workspace, actor).await? {
            return Err(Error::Forbidden(format!(
                "guest {actor} may not self-join channel {room}; \
                 guests are confined to their invited channel(s)"
            )));
        }
        let is_private = self
            .rooms
            .is_private(room)
            .await?
            .ok_or_else(|| Error::NotFound(format!("room {room}")))?;
        let is_archived = self.rooms.is_archived(room).await?.unwrap_or(false);
        if !can_join_public_channel(is_private, is_archived) {
            return Err(Error::Forbidden(format!(
                "channel {room} is not openly joinable (private or archived)"
            )));
        }
        self.rooms.add_member(room, actor).await?;
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
                                if let Err(err) = prefs.set_level(actor, room, level).await {
                                    warn!(?err, %actor, %room, "apply workspace notif default failed");
                                }
                            }
                            Ok(Some(_)) => {} // user already has an explicit pref — leave it
                            Err(err) => warn!(?err, %actor, %room, "get_level for notif default check failed"),
                        }
                    }
                    Ok(_) => {} // no default set, or default is "all" (the system default — no-op)
                    Err(err) => warn!(?err, %workspace, "fetch workspace notif default failed"),
                }
            }
        }
        self.publish_room_event(
            room,
            &RoomEvent::Membership { room_id: room, participant: actor, op: MembershipOp::Join },
        )
        .await;
        Ok(())
    }

    /// Leave a channel the actor is a member of. Emits `Membership { Leave }`.
    /// Idempotent: leaving a room you are not in is a no-op success.
    #[instrument(skip(self), fields(?actor, ?room))]
    pub async fn leave_channel(&self, actor: ParticipantId, room: RoomId) -> Result<()> {
        // The room must exist (resolve its tenant) before we touch membership.
        self.rooms
            .room_workspace(room)
            .await?
            .ok_or_else(|| Error::NotFound(format!("room {room}")))?;
        self.rooms.remove_member(room, actor).await?;
        self.publish_room_event(
            room,
            &RoomEvent::Membership { room_id: room, participant: actor, op: MembershipOp::Leave },
        )
        .await;
        Ok(())
    }

    /// Archive (or un-archive) a channel. Requires the actor be a member of the
    /// room (authorization kept simple but real).
    #[instrument(skip(self), fields(?actor, ?room, archived))]
    pub async fn archive_channel(
        &self,
        actor: ParticipantId,
        room: RoomId,
        archived: bool,
    ) -> Result<()> {
        if !self.rooms.is_member(room, actor).await? {
            return Err(Error::Forbidden(format!(
                "{actor} is not a member of room {room}"
            )));
        }
        self.rooms.set_archived(room, archived).await?;
        Ok(())
    }

    /// Update a channel's metadata (topic, description, visibility). Each field is
    /// optional — only provided fields are written. Requires the actor be a member
    /// of the room. Returns the refreshed [`Room`] (base shape; channel metadata
    /// lives on the row but is not part of the wire `Room`).
    #[instrument(skip(self), fields(?actor, ?room))]
    pub async fn set_channel_meta(
        &self,
        actor: ParticipantId,
        room: RoomId,
        topic: Option<Option<String>>,
        description: Option<Option<String>>,
        is_private: Option<bool>,
    ) -> Result<()> {
        if !self.rooms.is_member(room, actor).await? {
            return Err(Error::Forbidden(format!(
                "{actor} is not a member of room {room}"
            )));
        }
        if let Some(topic) = topic {
            self.rooms.set_topic(room, topic.as_deref()).await?;
        }
        if let Some(description) = description {
            self.rooms.set_description(room, description.as_deref()).await?;
        }
        if let Some(is_private) = is_private {
            self.rooms.set_visibility(room, is_private).await?;
        }
        Ok(())
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
        let creator = self
            .rooms
            .created_by(room)
            .await?
            .ok_or_else(|| Error::NotFound(format!("room {room}")))?;
        let is_creator = creator == actor;
        let is_admin = self.is_workspace_admin_of_room(actor, room).await?;
        if !is_creator && !is_admin {
            return Err(Error::Forbidden(format!(
                "{actor} may not change the post policy of room {room}"
            )));
        }
        self.rooms.set_post_policy(room, policy).await?;
        Ok(())
    }

    /// Read a room's post policy (`everyone` or `admins`). The actor must be able
    /// to access the room ([`assert_room_access`](Self::assert_room_access)).
    #[instrument(skip(self), fields(?actor, ?room))]
    pub async fn room_post_policy(&self, actor: ParticipantId, room: RoomId) -> Result<String> {
        self.assert_room_access(actor, room).await?;
        Ok(self.rooms.post_policy(room).await?)
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
        if !self.workspaces()?.is_member(workspace, actor).await? {
            return Err(Error::Forbidden(format!(
                "{actor} is not a member of workspace {workspace}"
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
        if !self.rooms.is_member(room, who).await? {
            return Err(Error::Forbidden(format!("{who} is not a member of room {room}")));
        }
        Ok(self.messages.list_recent(room, before, limit).await?)
    }
}
