//! Room lifecycle operations — create, access-control, add-member.
//!
//! Extracted from `service.rs` as part of REFACTOR_PLAN.md Step 1g.

use aero_common::{
    Error, Participant, ParticipantId, ParticipantKind, Result, Room, RoomId, RoomKind,
    WorkspaceId, WorkspaceRole,
};
use aero_storage::WorkspaceRepo;
use tracing::{instrument, warn};

use crate::events::ImEvent;
use crate::service::channels::map_channel_write_error;
use crate::service::events::{publish_event, EVENTS_SUBJECT};
use crate::service::orig::{can_access_room, post_allowed};
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
        if kind == RoomKind::Direct {
            return Err(Error::Invalid(
                "direct rooms must be created through the dedicated DM endpoint".into(),
            ));
        }
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
    /// channel creation; otherwise returns [`Error::Forbidden`]. Authorization,
    /// room creation, and the creator owner edge share one storage transaction so
    /// a concurrent revocation cannot reuse a stale service-layer decision.
    #[instrument(skip(self), fields(?creator, ?workspace, ?kind))]
    pub async fn create_room_in_workspace(
        &self,
        creator: ParticipantId,
        workspace: WorkspaceId,
        kind: RoomKind,
        name: Option<String>,
    ) -> Result<Room> {
        if kind == RoomKind::Direct {
            return Err(Error::Invalid(
                "direct rooms must be created through the dedicated DM endpoint".into(),
            ));
        }
        let room = self
            .rooms
            .create_in_workspace_authorized(workspace, kind, name, creator)
            .await
            .map_err(map_channel_write_error)?;
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
    /// requires BOTH effective workspace access (active account, current
    /// membership, no workspace deactivation, and mandatory-2FA enrollment for
    /// human participants)
    /// AND room membership ([`can_access_room`]). Returns [`Error::NotFound`] if
    /// the room does not exist, otherwise [`Error::Forbidden`] when access is
    /// denied. Servers should call this before serving any of a room's data.
    #[instrument(skip(self), fields(?participant, ?room, aero.force_sample = true))]
    pub async fn assert_room_access(&self, participant: ParticipantId, room: RoomId) -> Result<()> {
        let workspace = self
            .rooms
            .room_workspace(room)
            .await?
            .ok_or_else(|| Error::NotFound(format!("room {room}")))?;

        let is_workspace_member = self
            .workspaces()?
            .effective_member_role(workspace, participant)
            .await?
            .is_some();
        let is_room_member = self.rooms.is_member(room, participant).await?;

        if can_access_room(is_workspace_member, is_room_member) {
            Ok(())
        } else {
            // Preserve the stable, actionable 2FA error for production services
            // that wire the legacy diagnostic repos. Authorization itself does
            // not depend on these optional seams: `effective_member_role` above
            // always enforces every gate.
            if !is_workspace_member {
                if let Some(deact) = self.deactivations.as_ref() {
                    if deact.is_deactivated(workspace, participant).await? {
                        return Err(Error::Forbidden(format!(
                            "{participant} is deactivated in workspace {workspace}"
                        )));
                    }
                }
                let account = self.participants.get(participant).await?;
                if let Some(totp) = self.totp.as_ref() {
                    if matches!(
                        account.as_ref().map(|participant| participant.kind),
                        Some(ParticipantKind::Human)
                    ) && self.workspaces()?.require_2fa(workspace).await?
                        && !totp.is_activated(participant).await?
                    {
                        return Err(Error::Forbidden(format!(
                            "2fa_required: workspace {workspace} mandates two-factor auth; enroll via /api/me/2fa"
                        )));
                    }
                }
                if account.is_none() {
                    return Err(Error::Forbidden(format!(
                        "{participant} is not an active account"
                    )));
                }
            }
            Err(Error::Forbidden(format!(
                "{participant} may not access room {room}"
            )))
        }
    }

    /// Assert that `room` is a channel.
    ///
    /// Channel-management routes must not reinterpret direct/group rooms as
    /// discoverable channels. Unknown rooms retain the canonical `NotFound`
    /// response; an existing non-channel is a bad request.
    pub async fn assert_channel_kind(&self, room: RoomId) -> Result<()> {
        match self.rooms.room_kind(room).await? {
            Some(RoomKind::Channel) => Ok(()),
            Some(_) => Err(Error::Invalid(format!("room {room} is not a channel"))),
            None => Err(Error::NotFound(format!("room {room}"))),
        }
    }

    /// Canonical channel-management guard: effective room access plus an exact
    /// `RoomKind::Channel` check.
    pub async fn assert_channel_access(
        &self,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<()> {
        self.assert_room_access(participant, room).await?;
        self.assert_channel_kind(room).await
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

    /// Preflight an external or otherwise expensive message-producing action.
    ///
    /// This deliberately repeats the canonical access and announcement-policy
    /// checks before the caller spends provider quota. The eventual
    /// [`ImService::send_message`](Self::send_message) call rechecks both so a
    /// concurrent revocation cannot turn this early decision into authority.
    pub async fn assert_message_send_preflight(
        &self,
        sender: ParticipantId,
        room: RoomId,
    ) -> Result<()> {
        self.assert_room_access(sender, room).await?;
        self.assert_can_post(sender, room).await
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
            .effective_member_role(ws, participant)
            .await?
            .is_some_and(WorkspaceRole::can_administer))
    }

    /// Add a member to a mutable room. `actor` must still be an effective room
    /// Owner/Admin, or an effective workspace Owner/Admin who is also a room
    /// member. `member` must retain a non-guest workspace membership (it may be
    /// deactivated or awaiting mandatory 2FA, in which case it remains unable to
    /// read the room until the corresponding access gate is restored).
    ///
    /// Guest enforcement (single-channel guests): a participant flagged as a guest
    /// in the room's workspace may NOT be added to (or self-join) an arbitrary
    /// public channel through this path — guests are confined to the specific
    /// channel(s) they were explicitly invited to, which the admin guest endpoint
    /// wires up directly. Adding a guest here is denied with [`Error::Forbidden`].
    #[instrument(skip(self), fields(?actor, ?room, ?member))]
    pub async fn add_member(
        &self,
        actor: ParticipantId,
        room: RoomId,
        member: ParticipantId,
    ) -> Result<()> {
        let inserted = self
            .rooms
            .add_member_authorized(room, actor, member)
            .await
            .map_err(map_channel_write_error)?;
        if !inserted {
            return Ok(());
        }
        let event = ImEvent::MemberAdded {
            room,
            participant: member,
        };
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

    /// Atomically create and install a Bot/Agent into a mutable room.
    ///
    /// Storage rechecks the actor's effective room-management authority and
    /// writes the participant, workspace membership, and room membership in one
    /// transaction. Publishing follows the same best-effort domain-event seam as
    /// [`Self::add_member`].
    #[instrument(skip(self, display_name, avatar_url), fields(?actor, ?room, ?kind))]
    pub async fn create_service_identity(
        &self,
        actor: ParticipantId,
        room: RoomId,
        display_name: &str,
        kind: ParticipantKind,
        avatar_url: Option<&str>,
    ) -> Result<Participant> {
        let participant = self
            .rooms
            .create_service_identity_authorized(room, actor, display_name, kind, avatar_url)
            .await
            .map_err(map_channel_write_error)?;
        let event = ImEvent::MemberAdded {
            room,
            participant: participant.id,
        };
        if let Err(err) = publish_event(
            self.bus.as_ref(),
            &format!("{EVENTS_SUBJECT}.room.member_added"),
            &event,
        )
        .await
        {
            warn!(
                ?err,
                %room,
                member = %participant.id,
                "publish service-identity MemberAdded failed"
            );
        }
        Ok(participant)
    }

    /// List rooms the participant belongs to.
    #[instrument(skip(self), fields(?who))]
    pub async fn list_my_rooms(&self, who: ParticipantId) -> Result<Vec<Room>> {
        Ok(self.rooms.rooms_for(who).await?)
    }
}
