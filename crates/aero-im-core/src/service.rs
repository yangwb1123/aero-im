//! `ImService` — the IM business facade.
//!
//! Coordinates the storage repositories and the event bus. All business
//! invariants (membership checks, validation, authorization) live here so
//! HTTP/WS handlers stay thin.
//!
//! See `docs/specs/2026-05-22-aero-im-design.md` §4.2 for the protocol contract.

use std::sync::Arc;

use aero_bus::traits::BusError;
use aero_bus::EventBus;
use aero_common::{
    Block, CallEvent, CallId, CallKind, CallMode, CallSession, Error, MembershipOp, Message,
    MessageEnvelope, MessageId, NotificationKind, ParticipantId, ReactionOp, ReactionSummary,
    ReadReceipt, Result, Room, RoomEvent, RoomId, RoomKind, WorkspaceId, WorkspaceRole,
};
use aero_common::{PinOp, PinnedMessage};
use aero_storage::{
    message::NewMessage, AiJobKind, AiJobRepo, CallRepo, MessageRepo, NotificationPrefsRepo,
    NotificationRepo, ParticipantRepo, PinRepo, ReactionRepo, ReceiptRepo, RoomRepo, WorkspaceRepo,
};
use async_trait::async_trait;
use std::collections::BTreeMap;
use tracing::{instrument, warn};

use crate::events::ImEvent;
use crate::moderator::{ModerationVerdict, Moderator};
use crate::validation::validate_blocks;

const EVENTS_SUBJECT: &str = "im.events";

/// Whether a workspace member holding `role` may create a channel (room) in that
/// workspace. Members and above may; guests may not. Pure decision function so it
/// is exhaustively unit-testable without a database.
#[must_use]
pub fn can_create_channel(role: WorkspaceRole) -> bool {
    role.at_least(WorkspaceRole::Member)
}

/// Whether a participant may access a room's data, given (a) whether they belong
/// to the room's workspace and (b) whether they are a member of the room itself.
/// Both must hold — workspace membership alone is not enough, nor is a stale room
/// membership in a workspace they've been removed from. Pure decision function,
/// unit-tested as a truth table.
#[must_use]
pub fn can_access_room(is_workspace_member: bool, is_room_member: bool) -> bool {
    is_workspace_member && is_room_member
}

/// Whether a workspace member may join a channel given (a) it is public
/// (not private) and (b) it is not archived. Both must hold: a private or
/// archived channel is not openly joinable. Pure decision function, unit-tested
/// as a truth table.
#[must_use]
pub fn can_join_public_channel(is_private: bool, is_archived: bool) -> bool {
    !is_private && !is_archived
}

/// Object-safe view of [`EventBus`] used internally for dependency injection.
///
/// The upstream [`EventBus`] trait declares a generic default method (`publish_json<T>`)
/// which makes it not dyn-compatible. Rather than patching `aero-bus`, we wrap any
/// [`EventBus`] implementation in this object-safe shim.
#[async_trait]
pub trait BusSink: Send + Sync + 'static {
    async fn publish_bytes(&self, subject: &str, payload: bytes::Bytes)
        -> std::result::Result<(), BusError>;
}

#[async_trait]
impl<T> BusSink for T
where
    T: EventBus + 'static,
{
    async fn publish_bytes(
        &self,
        subject: &str,
        payload: bytes::Bytes,
    ) -> std::result::Result<(), BusError> {
        EventBus::publish(self, subject, payload).await
    }
}

async fn publish_event<T: serde::Serialize>(
    bus: &dyn BusSink,
    subject: &str,
    value: &T,
) -> std::result::Result<(), BusError> {
    let bytes = serde_json::to_vec(value)?;
    bus.publish_bytes(subject, bytes.into()).await
}

/// IM business facade. Cheap to clone (repositories wrap `Arc<PgPool>`).
#[derive(Clone)]
pub struct ImService {
    rooms: RoomRepo,
    /// Tenancy repo for workspace-scoped authorization. Optional so the existing
    /// constructors stay signature-compatible; wire it via
    /// [`with_workspaces`](ImService::with_workspaces) to enable the
    /// workspace-scoped methods.
    workspaces: Option<WorkspaceRepo>,
    messages: MessageRepo,
    #[allow(dead_code)] // Reserved for mention lookups, P3+.
    participants: ParticipantRepo,
    receipts: ReceiptRepo,
    reactions: ReactionRepo,
    calls: CallRepo,
    ai_jobs: AiJobRepo,
    /// Notification inbox (mentions / thread replies). Optional so the existing
    /// constructors stay signature-compatible; wire it via
    /// [`with_notifications`](ImService::with_notifications) to persist + push
    /// notifications on send. Without it, sends still succeed (no inbox writes).
    notifications: Option<NotificationRepo>,
    /// Pinned-messages store. Optional builder ([`with_pins`](ImService::with_pins));
    /// the pin/unpin/list methods return an internal error if it isn't wired.
    pins: Option<PinRepo>,
    /// Notification preferences (per-channel mute + per-user Do-Not-Disturb).
    /// Optional builder ([`with_notification_prefs`](ImService::with_notification_prefs));
    /// when present, [`should_notify`](ImService::should_notify) suppresses
    /// notifications to muted/DND recipients. Without it, nothing is suppressed.
    prefs: Option<NotificationPrefsRepo>,
    bus: Arc<dyn BusSink>,
    moderator: Arc<dyn Moderator>,
}

impl ImService {
    /// Construct from any [`EventBus`] implementation. Repositories are passed
    /// in to keep this crate independent of `PgPool`/Redis types.
    ///
    /// Picks a default moderator from `AERO_BLOCKED_WORDS` env at construction
    /// time, or `AllowAllModerator` if unset.
    #[allow(clippy::too_many_arguments)]
    pub fn new<B: EventBus + 'static>(
        rooms: RoomRepo,
        messages: MessageRepo,
        participants: ParticipantRepo,
        receipts: ReceiptRepo,
        reactions: ReactionRepo,
        calls: CallRepo,
        ai_jobs: AiJobRepo,
        bus: Arc<B>,
    ) -> Self {
        let moderator: Arc<dyn Moderator> = match crate::moderator::KeywordModerator::from_env() {
            Some(m) => Arc::new(m),
            None => Arc::new(crate::moderator::AllowAllModerator),
        };
        Self {
            rooms,
            workspaces: None,
            messages,
            participants,
            receipts,
            reactions,
            calls,
            ai_jobs,
            notifications: None,
            pins: None,
            prefs: None,
            bus: bus as Arc<dyn BusSink>,
            moderator,
        }
    }

    /// Inject a custom moderator (overrides env default).
    #[must_use]
    pub fn with_moderator(mut self, moderator: Arc<dyn Moderator>) -> Self {
        self.moderator = moderator;
        self
    }

    /// Wire in the workspace repository, enabling the workspace-scoped methods
    /// ([`create_room_in_workspace`](ImService::create_room_in_workspace),
    /// [`assert_room_access`](ImService::assert_room_access)). Additive builder,
    /// mirroring [`with_moderator`](ImService::with_moderator); without it those
    /// methods return an internal error rather than silently skipping tenant
    /// checks.
    #[must_use]
    pub fn with_workspaces(mut self, workspaces: WorkspaceRepo) -> Self {
        self.workspaces = Some(workspaces);
        self
    }

    /// Wire in the notification inbox, enabling mention/reply notifications on
    /// [`send_message`](Self::send_message). Additive builder mirroring
    /// [`with_workspaces`](Self::with_workspaces); without it, sends succeed but
    /// write no inbox entries.
    #[must_use]
    pub fn with_notifications(mut self, notifications: NotificationRepo) -> Self {
        self.notifications = Some(notifications);
        self
    }

    /// Wire in the pinned-messages store, enabling
    /// [`pin_message`](Self::pin_message) / [`unpin_message`](Self::unpin_message)
    /// / [`list_pins`](Self::list_pins). Additive builder.
    #[must_use]
    pub fn with_pins(mut self, pins: PinRepo) -> Self {
        self.pins = Some(pins);
        self
    }

    /// Wire in the notification-preferences store (per-channel mute + per-user
    /// Do-Not-Disturb), enabling the suppression seam
    /// ([`should_notify`](Self::should_notify)). Additive builder; without it,
    /// nothing is suppressed and every notification recipient is notified.
    #[must_use]
    pub fn with_notification_prefs(mut self, prefs: NotificationPrefsRepo) -> Self {
        self.prefs = Some(prefs);
        self
    }

    /// Notification suppression seam for per-channel mute + per-user
    /// Do-Not-Disturb. Returns whether a notification should be delivered to
    /// `recipient` for `room`: `false` when the recipient has MUTED the room OR
    /// is currently inside their DND window, `true` otherwise. DND is evaluated
    /// against the current UTC minute-of-day for now. Best-effort and FAIL-OPEN:
    /// no prefs store wired, or a lookup error, returns `true` so a glitch never
    /// silently drops a notification.
    async fn should_notify(&self, recipient: ParticipantId, room: RoomId) -> bool {
        let Some(prefs) = self.prefs.as_ref() else {
            return true;
        };
        let is_muted = match prefs.is_muted(recipient, room).await {
            Ok(m) => m,
            Err(err) => {
                warn!(?err, %recipient, %room, "mute lookup failed; not suppressing");
                return true;
            }
        };
        let dnd = match prefs.get_dnd(recipient).await {
            Ok(d) => d,
            Err(err) => {
                warn!(?err, %recipient, "DND lookup failed; not suppressing");
                return true;
            }
        };
        let now_minute =
            aero_storage::notification_prefs::minute_of_day_utc(time::OffsetDateTime::now_utc());
        !aero_storage::notification_prefs::should_suppress(is_muted, dnd, now_minute)
    }

    /// Lower-level constructor for tests with a pre-built bus sink.
    #[allow(clippy::too_many_arguments)]
    pub fn from_sink(
        rooms: RoomRepo,
        messages: MessageRepo,
        participants: ParticipantRepo,
        receipts: ReceiptRepo,
        reactions: ReactionRepo,
        calls: CallRepo,
        ai_jobs: AiJobRepo,
        bus: Arc<dyn BusSink>,
    ) -> Self {
        Self {
            rooms,
            workspaces: None,
            messages,
            participants,
            receipts,
            reactions,
            calls,
            ai_jobs,
            notifications: None,
            pins: None,
            prefs: None,
            bus,
            moderator: Arc::new(crate::moderator::AllowAllModerator),
        }
    }

    /// NATS subject used for per-room broadcast (see spec §4.2).
    #[must_use]
    pub fn room_subject(room: RoomId) -> String {
        format!("im.room.{room}")
    }

    // ---------------------------------------------------------- ROOM lifecycle

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
    fn workspaces(&self) -> Result<&WorkspaceRepo> {
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
    #[instrument(skip(self), fields(?participant, ?room))]
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

        let is_workspace_member = self.workspaces()?.is_member(workspace, participant).await?;
        let is_room_member = self.rooms.is_member(room, participant).await?;

        if can_access_room(is_workspace_member, is_room_member) {
            Ok(())
        } else {
            Err(Error::Forbidden(format!(
                "{participant} may not access room {room}"
            )))
        }
    }

    /// Add a member to a room. `actor` must themselves be a member.
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

    // ---------------------------------------------------------- CHANNELS

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

    /// List the public, joinable channels of a workspace. Requires the actor be a
    /// member of the workspace.
    #[instrument(skip(self), fields(?actor, ?workspace))]
    pub async fn list_workspace_channels(
        &self,
        actor: ParticipantId,
        workspace: WorkspaceId,
    ) -> Result<Vec<Room>> {
        if !self.workspaces()?.is_member(workspace, actor).await? {
            return Err(Error::Forbidden(format!(
                "{actor} is not a member of workspace {workspace}"
            )));
        }
        Ok(self.rooms.list_public_channels(workspace).await?)
    }

    /// Snapshot the current member set of a room (for callee fan-out, etc).
    pub async fn room_members(&self, room: RoomId) -> Result<Vec<ParticipantId>> {
        Ok(self.rooms.members(room).await?)
    }

    // ---------------------------------------------------------- MESSAGES

    /// Persist a new message and broadcast it. Returns the persisted message.
    /// Bus failures are *not* fatal — the message is durable in PG.
    #[instrument(
        skip(self, blocks),
        fields(?sender, ?room, block_count = blocks.len(), reply_to = ?reply_to)
    )]
    pub async fn send_message(
        &self,
        sender: ParticipantId,
        room: RoomId,
        blocks: Vec<Block>,
        reply_to: Option<MessageId>,
    ) -> Result<Message> {
        if !self.rooms.is_member(room, sender).await? {
            return Err(Error::Forbidden(format!(
                "sender {sender} is not a member of room {room}"
            )));
        }
        validate_blocks(&blocks)?;
        if let ModerationVerdict::Block(reason) = self.moderator.check(&blocks) {
            return Err(Error::Invalid(reason));
        }

        let message = self
            .messages
            .insert(NewMessage {
                room_id: room,
                sender_id: sender,
                blocks,
                reply_to,
                metadata: serde_json::Value::Null,
            })
            .await?;

        let recipients = self.rooms.members(room).await.unwrap_or_else(|err| {
            warn!(?err, %room, "fetching recipients failed; publishing without fan-out hint");
            Vec::new()
        });
        let envelope = MessageEnvelope {
            message: message.clone(),
            recipients: recipients.clone(),
        };

        self.publish_room_event(room, &RoomEvent::Message(envelope)).await;

        // Mention / thread-reply notifications (best-effort; never blocks the send).
        self.dispatch_notifications(&message, &recipients).await;

        // Best-effort enqueue an embed job (AI worker will pick it up).
        if !message.searchable_text().is_empty() {
            // Tag the embed job with the room's workspace so the AI worker can
            // meter paid-API spend per tenant (best-effort; None bills globally).
            let ws = self.rooms.room_workspace(room).await.ok().flatten().map(|w| w.to_uuid());
            if let Err(err) = self
                .ai_jobs
                .enqueue(
                    AiJobKind::Embed,
                    Some(message.id.to_uuid()),
                    ws,
                    serde_json::json!({"room_id": room.to_string()}),
                )
                .await
            {
                warn!(?err, message_id = %message.id, "enqueue embed job failed");
            }
        }

        Ok(message)
    }

    /// Edit a message. Only the sender may edit; soft-deleted messages refuse.
    #[instrument(skip(self, blocks), fields(?actor, ?id))]
    pub async fn edit_message(
        &self,
        actor: ParticipantId,
        id: MessageId,
        blocks: Vec<Block>,
    ) -> Result<Message> {
        let existing = self
            .messages
            .get(id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("message {id}")))?;
        if existing.deleted_at.is_some() {
            return Err(Error::Conflict("message is deleted".into()));
        }
        if existing.sender_id != actor {
            return Err(Error::Forbidden("only sender may edit".into()));
        }
        validate_blocks(&blocks)?;

        let updated = self
            .messages
            .edit(id, blocks)
            .await?
            .ok_or_else(|| Error::Conflict("edit raced with delete".into()))?;

        self.publish_room_event(updated.room_id, &RoomEvent::Edited(updated.clone()))
            .await;

        if !updated.searchable_text().is_empty() {
            let ws = self
                .rooms
                .room_workspace(updated.room_id)
                .await
                .ok()
                .flatten()
                .map(|w| w.to_uuid());
            if let Err(err) = self
                .ai_jobs
                .enqueue(
                    AiJobKind::Embed,
                    Some(updated.id.to_uuid()),
                    ws,
                    serde_json::json!({"room_id": updated.room_id.to_string()}),
                )
                .await
            {
                warn!(?err, %id, "enqueue re-embed failed");
            }
        }
        Ok(updated)
    }

    /// Soft-delete a message. Sender or room-owner may delete.
    #[instrument(skip(self), fields(?actor, ?id))]
    pub async fn delete_message(&self, actor: ParticipantId, id: MessageId) -> Result<()> {
        let existing = self
            .messages
            .get(id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("message {id}")))?;
        if existing.deleted_at.is_some() {
            return Ok(());
        }
        if existing.sender_id != actor && !self.rooms.is_member(existing.room_id, actor).await? {
            return Err(Error::Forbidden("only sender or room member may delete".into()));
        }
        if existing.sender_id != actor {
            return Err(Error::Forbidden("only sender may delete in P2".into()));
        }
        self.messages.soft_delete(id).await?;
        self.publish_room_event(
            existing.room_id,
            &RoomEvent::Deleted {
                room_id: existing.room_id,
                message_id: id,
                by: actor,
            },
        )
        .await;
        Ok(())
    }

    /// System action: soft-delete a message flagged by AI moderation and
    /// broadcast the removal. Unlike [`delete_message`] this bypasses the
    /// sender-only authorization check — the caller is the trusted moderation
    /// pipeline, not a participant. `reason` is logged, not sent to clients.
    #[instrument(skip(self), fields(?message_id, reason))]
    pub async fn moderate_delete(&self, message_id: MessageId, reason: &str) -> Result<()> {
        let existing = self
            .messages
            .get(message_id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("message {message_id}")))?;
        if existing.deleted_at.is_some() {
            return Ok(());
        }
        self.messages.soft_delete(message_id).await?;
        warn!(%message_id, reason, "message removed by AI moderation");
        self.publish_room_event(
            existing.room_id,
            &RoomEvent::Deleted {
                room_id: existing.room_id,
                message_id,
                by: existing.sender_id,
            },
        )
        .await;
        Ok(())
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

    /// Fetch reaction aggregates for a batch of messages.
    pub async fn reactions_for(
        &self,
        message_ids: &[MessageId],
    ) -> Result<BTreeMap<MessageId, Vec<ReactionSummary>>> {
        Ok(self.reactions.summaries_for(message_ids).await?)
    }

    // ---------------------------------------------------------- REACTIONS

    /// Toggle a reaction on a message. Caller must be a member of the message's room.
    #[instrument(skip(self), fields(?actor, ?message_id, emoji))]
    pub async fn toggle_reaction(
        &self,
        actor: ParticipantId,
        message_id: MessageId,
        emoji: &str,
    ) -> Result<ReactionOp> {
        let msg = self
            .messages
            .get(message_id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("message {message_id}")))?;
        if !self.rooms.is_member(msg.room_id, actor).await? {
            return Err(Error::Forbidden("not a room member".into()));
        }
        if emoji.is_empty() || emoji.len() > 32 {
            return Err(Error::Invalid("emoji length".into()));
        }
        let op = self.reactions.toggle(message_id, actor, emoji).await?;
        self.publish_room_event(
            msg.room_id,
            &RoomEvent::Reaction {
                room_id: msg.room_id,
                message_id,
                participant: actor,
                emoji: emoji.to_owned(),
                op,
            },
        )
        .await;
        Ok(op)
    }

    // ---------------------------------------------------------- READ RECEIPTS

    /// Move a participant's read cursor in a room.
    #[instrument(skip(self), fields(?actor, ?room, ?last_read))]
    pub async fn mark_read(
        &self,
        actor: ParticipantId,
        room: RoomId,
        last_read: MessageId,
    ) -> Result<ReadReceipt> {
        if !self.rooms.is_member(room, actor).await? {
            return Err(Error::Forbidden("not a room member".into()));
        }
        let receipt = self.receipts.mark_read(room, actor, last_read).await?;
        self.publish_room_event(
            room,
            &RoomEvent::Read {
                room_id: room,
                participant: actor,
                last_message_id: last_read,
                at: receipt.updated_at,
            },
        )
        .await;
        Ok(receipt)
    }

    pub async fn receipts_for(&self, room: RoomId) -> Result<Vec<ReadReceipt>> {
        Ok(self.receipts.list_for_room(room).await?)
    }

    // ---------------------------------------------------------- TYPING

    /// Broadcast a typing indicator. No persistence.
    pub async fn typing(
        &self,
        actor: ParticipantId,
        room: RoomId,
        on: bool,
    ) -> Result<()> {
        if !self.rooms.is_member(room, actor).await? {
            return Err(Error::Forbidden("not a room member".into()));
        }
        self.publish_room_event(
            room,
            &RoomEvent::Typing { room_id: room, participant: actor, on },
        )
        .await;
        Ok(())
    }

    // ---------------------------------------------------------- CALLS (P3)

    /// Start a call session (1:1 or group) and broadcast an invite to the callees.
    #[instrument(skip(self, sdp), fields(?initiator, ?room, ?kind, ?mode))]
    pub async fn start_call(
        &self,
        initiator: ParticipantId,
        room: RoomId,
        kind: CallKind,
        mode: CallMode,
        sdp: String,
    ) -> Result<CallSession> {
        if !self.rooms.is_member(room, initiator).await? {
            return Err(Error::Forbidden("not a room member".into()));
        }
        let mut callees = self.rooms.members(room).await?;
        callees.retain(|p| *p != initiator);
        let call_id = CallId::new();
        let session = self
            .calls
            .start(call_id, room, initiator, kind, mode, &callees)
            .await?;
        self.publish_room_event(
            room,
            &RoomEvent::Call(CallEvent::Invite {
                call_id,
                room_id: room,
                from: initiator,
                to: callees,
                kind,
                sdp,
            }),
        )
        .await;
        Ok(session)
    }

    /// Forward an answer/ICE/end signaling event to the targeted participant(s).
    /// `room` is required for routing on the per-room subject.
    pub async fn relay_call_event(&self, room: RoomId, event: CallEvent) -> Result<()> {
        if let CallEvent::End { call_id, reason, .. } = &event {
            if let Err(err) = self.calls.end(*call_id, reason).await {
                warn!(?err, %call_id, "persist call end failed");
            }
        }
        self.publish_room_event(room, &RoomEvent::Call(event)).await;
        Ok(())
    }

    // ---------------------------------------------------------- PINS

    /// Reference to the wired pin store, or a clear internal error if the service
    /// was built without [`with_pins`](Self::with_pins).
    fn pins(&self) -> Result<&PinRepo> {
        self.pins.as_ref().ok_or_else(|| {
            Error::Internal(anyhow::anyhow!(
                "ImService used for a pin operation without a PinRepo (call ImService::with_pins)"
            ))
        })
    }

    /// Pin a message in a room. Requires the actor to have room access and the
    /// message to actually belong to the room. Broadcasts `RoomEvent::Pin` on a
    /// newly-created pin. Returns `true` if a new pin was created (idempotent).
    #[instrument(skip(self), fields(?actor, ?room, ?message))]
    pub async fn pin_message(
        &self,
        actor: ParticipantId,
        room: RoomId,
        message: MessageId,
    ) -> Result<bool> {
        self.assert_room_access(actor, room).await?;
        // The message must exist, not be deleted, and belong to this room — else a
        // member of room A could pin room B's message into their panel.
        let msg = self
            .messages
            .get(message)
            .await?
            .filter(|m| m.deleted_at.is_none())
            .ok_or_else(|| Error::NotFound(format!("message {message}")))?;
        if msg.room_id != room {
            return Err(Error::Invalid(format!(
                "message {message} does not belong to room {room}"
            )));
        }
        let created = self.pins()?.pin(room, message, actor).await?;
        if created {
            self.publish_room_event(
                room,
                &RoomEvent::Pin { room_id: room, message_id: message, by: actor, op: PinOp::Pin },
            )
            .await;
        }
        Ok(created)
    }

    /// Unpin a message. Requires room access. Broadcasts `RoomEvent::Pin`
    /// (`Unpin`) when a pin was actually removed. Returns `true` if removed.
    #[instrument(skip(self), fields(?actor, ?room, ?message))]
    pub async fn unpin_message(
        &self,
        actor: ParticipantId,
        room: RoomId,
        message: MessageId,
    ) -> Result<bool> {
        self.assert_room_access(actor, room).await?;
        let removed = self.pins()?.unpin(room, message).await?;
        if removed {
            self.publish_room_event(
                room,
                &RoomEvent::Pin { room_id: room, message_id: message, by: actor, op: PinOp::Unpin },
            )
            .await;
        }
        Ok(removed)
    }

    /// List a room's pinned messages (newest first). Requires room access.
    pub async fn list_pins(
        &self,
        actor: ParticipantId,
        room: RoomId,
    ) -> Result<Vec<PinnedMessage>> {
        self.assert_room_access(actor, room).await?;
        Ok(self.pins()?.list_for_room(room).await?)
    }

    // ---------------------------------------------------------- internal

    /// Persist + push mention/thread-reply notifications for a freshly-sent
    /// message. Best-effort: a failure is logged and never blocks the send.
    /// No-op when the notification inbox isn't wired
    /// ([`with_notifications`](Self::with_notifications)). `members` is the room's
    /// member set — a mention or reply targeting someone outside it (e.g. a stale
    /// `@`) is skipped, and the sender never notifies themselves. An explicit
    /// `@`-mention outranks a thread reply for the same recipient.
    async fn dispatch_notifications(&self, message: &Message, members: &[ParticipantId]) {
        let Some(repo) = self.notifications.as_ref() else {
            return;
        };
        let sender = message.sender_id;
        let room = message.room_id;
        let member_set: std::collections::BTreeSet<ParticipantId> =
            members.iter().copied().collect();

        // recipient -> kind. BTreeMap insert order: reply first, then mentions
        // overwrite (an explicit mention is the stronger signal).
        let mut targets: BTreeMap<ParticipantId, NotificationKind> = BTreeMap::new();

        if let Some(parent_id) = message.reply_to {
            if let Ok(Some(parent)) = self.messages.get(parent_id).await {
                let author = parent.sender_id;
                if author != sender && member_set.contains(&author) {
                    targets.insert(author, NotificationKind::Reply);
                }
            }
        }
        for p in mentioned_participants(&message.blocks) {
            if p != sender && member_set.contains(&p) {
                targets.insert(p, NotificationKind::Mention);
            }
        }

        for (recipient, kind) in targets {
            // Per-channel mute / Do-Not-Disturb: skip recipients who have silenced
            // this room or are in a DND window (fail-open if prefs unavailable).
            if !self.should_notify(recipient, room).await {
                continue;
            }
            if let Err(err) = repo
                .insert(recipient, room, message.id, kind, Some(sender))
                .await
            {
                warn!(?err, %recipient, "persist notification failed");
                continue;
            }
            self.publish_room_event(
                room,
                &RoomEvent::Notify {
                    room_id: room,
                    message_id: message.id,
                    mentioned: recipient,
                    by: sender,
                    kind,
                },
            )
            .await;
        }
    }

    async fn publish_room_event(&self, room: RoomId, event: &RoomEvent) {
        let subject = Self::room_subject(room);
        if let Err(err) = publish_event(self.bus.as_ref(), &subject, event).await {
            warn!(?err, %subject, "publish RoomEvent failed");
        }
    }
}

/// Extract the distinct participants `@`-mentioned in a message's blocks, in
/// first-appearance order. Pure helper so mention parsing is unit-testable
/// without a database or bus.
fn mentioned_participants(blocks: &[Block]) -> Vec<ParticipantId> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for b in blocks {
        if let Block::Mention { participant } = b {
            if seen.insert(*participant) {
                out.push(*participant);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::MockBus;

    #[test]
    fn room_subject_is_well_formed() {
        let room = RoomId::new();
        let subject = ImService::room_subject(room);
        assert!(subject.starts_with("im.room."));
        assert!(subject.ends_with(&room.to_string()));
    }

    #[test]
    fn mentioned_participants_dedups_in_order_and_ignores_non_mentions() {
        let a = ParticipantId::new();
        let b = ParticipantId::new();
        let blocks = vec![
            Block::text("hey"),
            Block::Mention { participant: a },
            Block::text("and"),
            Block::Mention { participant: b },
            // duplicate mention of `a` is collapsed
            Block::Mention { participant: a },
        ];
        let got = mentioned_participants(&blocks);
        assert_eq!(got, vec![a, b], "distinct mentions, first-appearance order");

        // No mentions => empty.
        assert!(mentioned_participants(&[Block::text("plain")]).is_empty());
    }

    #[test]
    fn mock_bus_records_publish() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let bus = Arc::new(MockBus::default());
        rt.block_on(async {
            bus.publish_json("im.test", &serde_json::json!({"k": "v"}))
                .await
                .unwrap();
        });
        let log = bus.published.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].0, "im.test");
    }

    #[test]
    fn publish_event_helper_through_dyn() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let bus = Arc::new(MockBus::default());
        let dyn_bus: Arc<dyn super::BusSink> = bus.clone();
        rt.block_on(async {
            super::publish_event(
                dyn_bus.as_ref(),
                "im.events.room.created",
                &serde_json::json!({"ok": true}),
            )
            .await
            .unwrap();
        });
        let log = bus.published.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].0, "im.events.room.created");
        assert!(std::str::from_utf8(&log[0].1).unwrap().contains("\"ok\":true"));
    }

    // ---- Pure tenancy-authorization decisions (DB-free, exhaustive) ----

    const ALL_ROLES: [WorkspaceRole; 4] = [
        WorkspaceRole::Guest,
        WorkspaceRole::Member,
        WorkspaceRole::Admin,
        WorkspaceRole::Owner,
    ];

    #[test]
    fn can_create_channel_is_member_and_above() {
        assert!(!can_create_channel(WorkspaceRole::Guest));
        assert!(can_create_channel(WorkspaceRole::Member));
        assert!(can_create_channel(WorkspaceRole::Admin));
        assert!(can_create_channel(WorkspaceRole::Owner));
    }

    #[test]
    fn can_create_channel_matches_at_least_member_for_all_roles() {
        for r in ALL_ROLES {
            assert_eq!(
                can_create_channel(r),
                r.at_least(WorkspaceRole::Member),
                "role {r:?}"
            );
        }
    }

    #[test]
    fn can_access_room_requires_both_memberships() {
        // Full truth table over (workspace_member, room_member).
        assert!(!can_access_room(false, false));
        assert!(!can_access_room(true, false));
        assert!(!can_access_room(false, true));
        assert!(can_access_room(true, true));
    }

    #[test]
    fn can_access_room_is_logical_and() {
        for ws in [false, true] {
            for room in [false, true] {
                assert_eq!(can_access_room(ws, room), ws && room, "({ws}, {room})");
            }
        }
    }

    #[test]
    fn can_join_public_channel_requires_public_and_not_archived() {
        // Truth table over (is_private, is_archived): joinable only when public
        // AND not archived.
        assert!(can_join_public_channel(false, false));
        assert!(!can_join_public_channel(true, false));
        assert!(!can_join_public_channel(false, true));
        assert!(!can_join_public_channel(true, true));
    }

    #[test]
    fn can_join_public_channel_is_neither_private_nor_archived() {
        for private in [false, true] {
            for archived in [false, true] {
                assert_eq!(
                    can_join_public_channel(private, archived),
                    !private && !archived,
                    "({private}, {archived})"
                );
            }
        }
    }
}
