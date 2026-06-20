//! Room lifecycle operations — create, access-control, add-member.
//!
//! Extracted from `service.rs` as part of REFACTOR_PLAN.md Step 1g.

use aero_common::{
    Error, ParticipantId, Result, Room, RoomId, RoomKind, WorkspaceId, WorkspaceRole,
};
use aero_storage::WorkspaceRepo;
use tracing::{instrument, warn};

use crate::events::ImEvent;
use crate::service::events::{publish_event, EVENTS_SUBJECT};
use crate::service::orig::{can_access_room, can_create_channel, post_allowed};
use crate::ImService;

impl ImService {
    /// Create a new room.
    #[instrument(skip(self), fields(?creator, ?kind))]
    pub async fn create_room(
        &self,
        creator: ParticipantId,
        kind: RoomKind,
        name: Option<String>,
    ) -> Result<Room> {
        let room = self.rooms.create(kind, name, creator).await?;
        let event = ImEvent::RoomCreated(room.clone());
        if let Err(err) = publish_event(
            self.bus.as_ref(),
            &format!("{EVENTS_SUBJECT}.room.created"),
            &event,
        )
        .await
        {
            warn!(?err, room_id = %room.id, "publish RoomCreated failed");
        }
        Ok(room)
    }

    /// Reference to the wired workspace repo, or a clear internal error if the
    /// service was built without [`with_workspaces`](Self::with_workspaces).
    pub(crate) fn workspaces(&self) -> Result<&WorkspaceRepo> {
        self.workspaces.as_ref().ok_or_else(|| {
            Error::Internal(anyhow::anyhow!(
                "ImService used for a workspace-scoped operation without a WorkspaceRepo \
                 (call ImService::with_workspaces)"
            ))
        })
    }

    /// Create a channel inside `workspace` on behalf of `creator` — the tenant
    /// choke point. Verifies `creator` is a workspace member whose role permits
    /// channel creation ([`can_create_channel`]); otherwise returns
    /// [`Error::Forbidden`]. On success delegates to
    /// [`RoomRepo::create_in_workspace`](aero_storage::RoomRepo::create_in_workspace)
    /// so the room carries its `workspace_id`, then publishes `RoomCreated`.
    #[instrument(skip(self), fields(?creator, ?workspace, ?kind))]
    pub async fn create_room_in_workspace(
        &self,
        creator: ParticipantId,
        workspace: WorkspaceId,
        kind: RoomKind,
        name: Option<String>,
    ) -> Result<Room> {
        let role = self
            .workspaces()?
            .member_role(workspace, creator)
            .await?
            .ok_or_else(|| {
                Error::Forbidden(format!(
                    "{creator} is not a member of workspace {workspace}"
                ))
            })?;
        if !can_create_channel(role) {
            return Err(Error::Forbidden(format!(
                "role {role:?} may not create channels in workspace {workspace}"
            )));
        }

        let room = self
            .rooms
            .create_in_workspace(workspace, kind, name, creator)
            .await?;
        let event = ImEvent::RoomCreated(room.clone());
        if let Err(err) = publish_event(
            self.bus.as_ref(),
            &format!("{EVENTS_SUBJECT}.room.created"),
            &event,
        )
        .await
        {
            warn!(?err, room_id = %room.id, "publish RoomCreated failed");
        }
        Ok(room)
    }

    /// The single reusable tenant guard: assert `participant` may access `room`.
    ///
    /// Resolves the room's owning workspace via
    /// [`RoomRepo::room_workspace`](aero_storage::RoomRepo::room_workspace) and
    /// requires BOTH workspace membership AND room membership
    /// ([`can_access_room`]). Returns [`Error::NotFound`] if the room does not
    /// exist, otherwise [`Error::Forbidden`] when access is denied. Servers
    /// should call this before serving any of a room's data.
    #[instrument(skip(self), fields(?participant, ?room, aero.force_sample = true))]
    pub async fn assert_room_access(
        &self,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<()> {
        let workspace = self
            .rooms
            .room_workspace(room)
            .await?
            .ok_or_else(|| Error::NotFound(format!("room {room}")))?;

        // Deactivation gate (Wave 14): a member deactivated in this room's
        // workspace is locked out of its room data, even if still a row in
        // `room_members`. No-op when the store isn't wired.
        if let Some(deact) = self.deactivations.as_ref() {
            if deact.is_deactivated(workspace, participant).await? {
                return Err(Error::Forbidden(format!(
                    "{participant} is deactivated in workspace {workspace}"
                )));
            }
        }

        let is_workspace_member = self.workspaces()?.is_member(workspace, participant).await?;
        let is_room_member = self.rooms.is_member(room, participant).await?;

        if can_access_room(is_workspace_member, is_room_member) {
            // 2FA enforcement gate (Wave 24): if this room's workspace mandates
            // two-factor, a member who has not activated TOTP is locked out of its
            // room data until they enroll. The `/api/me/2fa/*` enroll routes are not
            // room-gated, so enrollment stays reachable. No-op when the store isn't
            // wired or the workspace doesn't require 2FA.
            if let Some(totp) = self.totp.as_ref() {
                if self.workspaces()?.require_2fa(workspace).await?
                    && !totp.is_activated(participant).await?
                {
                    return Err(Error::Forbidden(format!(
                        "2fa_required: workspace {workspace} mandates two-factor auth; enroll via /api/me/2fa"
                    )));
                }
            }
            Ok(())
        } else {
            Err(Error::Forbidden(format!(
                "{participant} may not access room {room}"
            )))
        }
    }

    /// Announcement-channel post guard (migration 0030). Reads the room's
    /// `post_policy` and decides via [`post_allowed`]:
    ///
    /// - `everyone` (the default, the overwhelmingly common case): returns
    ///   `Ok(())` after one cheap query — no membership re-check, no role lookup.
    /// - `admins`: allowed only when `sender` is the room's `created_by` OR a
    ///   workspace Admin/Owner of the room's workspace (the SAME admin
    ///   determination [`assert_room_access`](Self::assert_room_access) uses —
    ///   `member_role` + [`WorkspaceRole::can_administer`]). If no `WorkspaceRepo`
    ///   is wired, falls back to allowing the room creator only (never panics, so
    ///   non-tenant tests keep working).
    ///
    /// Returns [`Error::Forbidden`] when posting is denied. Membership is assumed
    /// already checked by the caller; this layers the policy on top.
    pub(crate) async fn assert_can_post(&self, sender: ParticipantId, room: RoomId) -> Result<()> {
        let policy = self.rooms.post_policy(room).await?;
        // Fast path: open channels (and any unknown policy) never do extra work.
        if post_allowed(&policy, false, false) {
            return Ok(());
        }

        // Restricted ('admins'): the creator may always post.
        let is_creator = self.rooms.created_by(room).await? == Some(sender);
        // A workspace Admin/Owner may also post.
        let is_admin = self.is_workspace_admin_of_room(sender, room).await?;

        if post_allowed(&policy, is_admin, is_creator) {
            Ok(())
        } else {
            Err(Error::Forbidden(format!(
                "room {room} is announcements-only; {sender} may not post"
            )))
        }
    }

    /// Whether `participant` is a workspace Admin/Owner of `room`'s workspace —
    /// the SAME admin determination [`assert_room_access`](Self::assert_room_access)
    /// relies on (`member_role` + [`WorkspaceRole::can_administer`]). Returns
    /// `false` (never an error) when no `WorkspaceRepo` is wired or the room has
    /// no resolvable workspace, so non-tenant callers degrade gracefully.
    pub(crate) async fn is_workspace_admin_of_room(
        &self,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<bool> {
        let Some(workspaces) = self.workspaces.as_ref() else {
            return Ok(false);
        };
        let Some(ws) = self.rooms.room_workspace(room).await? else {
            return Ok(false);
        };
        Ok(workspaces
            .member_role(ws, participant)
            .await?
            .is_some_and(WorkspaceRole::can_administer))
    }

    /// Add a member to a room. `actor` must themselves be a member.
    ///
    /// Guest enforcement (single-channel guests): a participant flagged as a guest
    /// in the room's workspace may NOT be added to (or self-join) an arbitrary
    /// public channel through this path — guests are confined to the specific
    /// channel(s) they were explicitly invited to, which the admin guest endpoint
    /// wires up directly. Adding a guest here is denied with [`Error::Forbidden`].
    /// This guard *fails open* when no [`WorkspaceRepo`] is wired
    /// ([`with_workspaces`](Self::with_workspaces) absent) so non-tenant tests and
    /// single-tenant deployments are unaffected.
    #[instrument(skip(self), fields(?actor, ?room, ?member))]
    pub async fn add_member(
        &self,
        actor: ParticipantId,
        room: RoomId,
        member: ParticipantId,
    ) -> Result<()> {
        if !self.rooms.is_member(room, actor).await? {
            return Err(Error::Forbidden(format!(
                "actor {actor} is not a member of room {room}"
            )));
        }
        // Deny adding a guest into an arbitrary channel. Resolve the room's
        // workspace and check the guest flag there; if tenancy is not wired
        // (`workspaces` is None) we skip the check entirely (fail open).
        if let Some(workspaces) = self.workspaces.as_ref() {
            if let Some(workspace) = self.rooms.room_workspace(room).await? {
                if workspaces.is_guest(workspace, member).await? {
                    return Err(Error::Forbidden(format!(
                        "guest {member} may not be added to channel {room}; \
                         guests are confined to their invited channel(s)"
                    )));
                }
            }
        }
        self.rooms.add_member(room, member).await?;
        let event = ImEvent::MemberAdded { room, participant: member };
        if let Err(err) = publish_event(
            self.bus.as_ref(),
            &format!("{EVENTS_SUBJECT}.room.member_added"),
            &event,
        )
        .await
        {
            warn!(?err, %room, %member, "publish MemberAdded failed");
        }
        Ok(())
    }

    /// List rooms the participant belongs to.
    #[instrument(skip(self), fields(?who))]
    pub async fn list_my_rooms(&self, who: ParticipantId) -> Result<Vec<Room>> {
        Ok(self.rooms.rooms_for(who).await?)
    }
}
