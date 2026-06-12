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
    message::NewMessage, AiJobKind, AiJobRepo, BlockRepo, CallRepo, DeactivationRepo,
    KeywordAlertRepo, MessageEditRepo, MessageRepo, NotificationPrefsRepo, NotificationRepo,
    ParticipantRepo, PinRepo, ReactionRepo, ReceiptRepo, RoomRepo, ThreadNotificationPrefsRepo,
    ThreadSubscriptionRepo, TotpRepo, UserGroupRepo, WorkspaceMuteRepo, WorkspaceRepo,
};
use async_trait::async_trait;
use futures::StreamExt;
use std::collections::BTreeMap;
use tracing::{instrument, warn, Instrument};

use crate::events::ImEvent;
use crate::moderator::{ModerationVerdict, Moderator};
use crate::seq::{LocalSeqProvider, SeqProvider};
use crate::validation::validate_blocks;

const EVENTS_SUBJECT: &str = "im.events";

/// Max in-flight per-recipient `Notify` publishes inside the detached
/// notification-dispatch task. Bounds the NATS pipelining for a large-room
/// `@everyone` fan-out (ROADMAP 第三版 方向四): concurrent enough to drain 10k
/// publishes quickly, bounded so one send can't monopolize bus connections.
const NOTIFY_PUBLISH_CONCURRENCY: usize = 32;

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

/// Whether a sender may post in a room under its `post_policy` (announcement
/// channels, migration 0030). `everyone` (the default and any UNRECOGNIZED value)
/// always allows — an unknown policy is treated as the open default so a typo
/// never silently locks a channel. `admins` allows only when the sender is the
/// room's creator OR a workspace Admin/Owner. Pure decision function so the rule
/// is unit-tested without a database; [`ImService::assert_can_post`] supplies
/// `is_admin`/`is_creator`.
#[must_use]
pub fn post_allowed(policy: &str, is_admin: bool, is_creator: bool) -> bool {
    match policy {
        "admins" => is_admin || is_creator,
        // "everyone" and any unrecognized policy fall through to the open default.
        _ => true,
    }
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
    /// User-group store (`@-usergroups`). Optional builder
    /// ([`with_user_groups`](Self::with_user_groups)); when present,
    /// [`dispatch_notifications`](Self::dispatch_notifications) expands an
    /// `@handle` group mention into a notification for every group member.
    user_groups: Option<UserGroupRepo>,
    /// Message edit-history store. Optional builder
    /// ([`with_message_edits`](Self::with_message_edits)); when present,
    /// [`edit_message`](Self::edit_message) archives the replaced version before
    /// overwriting it.
    message_edits: Option<MessageEditRepo>,
    /// Keyword / highlight-alert store. Optional builder
    /// ([`with_keyword_alerts`](Self::with_keyword_alerts)); when present,
    /// [`dispatch_notifications`](Self::dispatch_notifications) also notifies any
    /// member subscribed to a keyword the message contains.
    keyword_alerts: Option<KeywordAlertRepo>,
    /// Thread-subscription store. Optional builder
    /// ([`with_thread_subs`](Self::with_thread_subs)); when present,
    /// [`dispatch_notifications`](Self::dispatch_notifications) notifies everyone
    /// who followed a reply's root message (Wave 11).
    thread_subs: Option<ThreadSubscriptionRepo>,
    /// Workspace deactivation store. Optional builder
    /// ([`with_deactivations`](Self::with_deactivations)); when present,
    /// [`assert_room_access`](Self::assert_room_access) denies a member who has
    /// been deactivated in the room's workspace (Wave 14).
    deactivations: Option<DeactivationRepo>,
    /// Workspace 2FA-enforcement store. Optional builder
    /// ([`with_totp`](Self::with_totp)); when present,
    /// [`assert_room_access`](Self::assert_room_access) denies a member of a
    /// `require_2fa` workspace who has not activated TOTP (Wave 24).
    totp: Option<TotpRepo>,
    /// Per-thread notification level store (ROADMAP6 Lane A). Optional builder
    /// ([`with_thread_notification_prefs`](Self::with_thread_notification_prefs));
    /// when present, [`dispatch_notifications`](Self::dispatch_notifications) applies
    /// the per-thread level (`all`/`mentions`/`none`) to thread reply notifications
    /// after the thread-mute step.
    thread_notification_prefs: Option<ThreadNotificationPrefsRepo>,
    /// Workspace-wide mute store (ROADMAP6 Lane A). Optional builder
    /// ([`with_workspace_mutes`](Self::with_workspace_mutes)); when present,
    /// [`dispatch_notifications`](Self::dispatch_notifications) suppresses all
    /// notifications to participants who have muted the room's workspace (checked
    /// first, before any room-level suppression).
    workspace_mutes: Option<WorkspaceMuteRepo>,
    /// User block store (ROADMAP8). Optional builder
    /// ([`with_block_repo`](Self::with_block_repo)); when present,
    /// [`dispatch_notifications`](Self::dispatch_notifications) suppresses
    /// notifications to recipients who have blocked the sender.
    block_repo: Option<BlockRepo>,
    bus: Arc<dyn BusSink>,
    moderator: Arc<dyn Moderator>,
    /// Per-subject event-seq source for publish-time `"seq"` stamping (ROADMAP
    /// 第三版 方向一). Defaults to the process-local [`LocalSeqProvider`];
    /// clustered deployments wire the Redis `INCR`-backed
    /// [`aero_storage::SeqStore`] via [`with_seq`](Self::with_seq) so every node
    /// draws from one per-room sequence. Per-room monotonic; gaps are legal
    /// (consumers use seq only for dedup + relative order).
    seq: Arc<dyn SeqProvider>,
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
            user_groups: None,
            message_edits: None,
            keyword_alerts: None,
            thread_subs: None,
            deactivations: None,
            totp: None,
            thread_notification_prefs: None,
            workspace_mutes: None,
            block_repo: None,
            bus: bus as Arc<dyn BusSink>,
            moderator,
            seq: Arc::new(LocalSeqProvider::new()),
        }
    }

    /// Inject a custom moderator (overrides env default).
    #[must_use]
    pub fn with_moderator(mut self, moderator: Arc<dyn Moderator>) -> Self {
        self.moderator = moderator;
        self
    }

    /// Inject a custom event-seq provider (overrides the process-local default).
    /// `bin/aero-server.rs` passes the Redis-backed [`aero_storage::SeqStore`]
    /// here so the publish-time `"seq"` stamp is cluster-correct: two instances
    /// publishing into the same room never mint the same seq for different
    /// events. Additive builder mirroring [`with_moderator`](Self::with_moderator).
    #[must_use]
    pub fn with_seq(mut self, seq: Arc<dyn SeqProvider>) -> Self {
        self.seq = seq;
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

    /// Wire in the user-group store, enabling `@handle` group-mention fan-out in
    /// [`dispatch_notifications`](Self::dispatch_notifications). Additive builder;
    /// without it, group handles in a message are ignored (only direct
    /// `Block::Mention`s and replies notify).
    #[must_use]
    pub fn with_user_groups(mut self, user_groups: UserGroupRepo) -> Self {
        self.user_groups = Some(user_groups);
        self
    }

    /// Wire in the message edit-history store, enabling prior-version capture in
    /// [`edit_message`](Self::edit_message). Additive builder; without it, edits
    /// still succeed but no history row is written.
    #[must_use]
    pub fn with_message_edits(mut self, message_edits: MessageEditRepo) -> Self {
        self.message_edits = Some(message_edits);
        self
    }

    /// Wire in the keyword-alert store, enabling keyword/highlight notifications in
    /// [`dispatch_notifications`](Self::dispatch_notifications). Additive builder;
    /// without it, keyword subscriptions never fire.
    #[must_use]
    pub fn with_keyword_alerts(mut self, keyword_alerts: KeywordAlertRepo) -> Self {
        self.keyword_alerts = Some(keyword_alerts);
        self
    }

    /// Wire in the thread-subscription store, enabling thread-follow notifications
    /// in [`dispatch_notifications`](Self::dispatch_notifications). Additive
    /// builder; without it, a reply only notifies the root author + @-mentions.
    #[must_use]
    pub fn with_thread_subs(mut self, thread_subs: ThreadSubscriptionRepo) -> Self {
        self.thread_subs = Some(thread_subs);
        self
    }

    /// Wire in the workspace-deactivation store, enabling access revocation in
    /// [`assert_room_access`](Self::assert_room_access). Additive builder; without
    /// it, no deactivation check is applied (every member keeps access).
    #[must_use]
    pub fn with_deactivations(mut self, deactivations: DeactivationRepo) -> Self {
        self.deactivations = Some(deactivations);
        self
    }

    /// Wire the TOTP store so [`assert_room_access`](Self::assert_room_access)
    /// enforces a workspace's `require_2fa` mandate (Wave 24). Additive builder;
    /// without it, no 2FA gate is applied.
    #[must_use]
    pub fn with_totp(mut self, totp: TotpRepo) -> Self {
        self.totp = Some(totp);
        self
    }

    /// Wire in the per-thread notification level store (ROADMAP6 Lane A), enabling
    /// per-thread level suppression (`all`/`mentions`/`none`) in
    /// [`dispatch_notifications`](Self::dispatch_notifications). Additive builder;
    /// without it, all thread reply notifications are delivered (fail-open).
    #[must_use]
    pub fn with_thread_notification_prefs(
        mut self,
        repo: ThreadNotificationPrefsRepo,
    ) -> Self {
        self.thread_notification_prefs = Some(repo);
        self
    }

    /// Wire in the workspace-wide mute store (ROADMAP6 Lane A), enabling workspace
    /// mute suppression (checked first, before room-level suppression) in
    /// [`dispatch_notifications`](Self::dispatch_notifications). Additive builder;
    /// without it, no workspace-mute suppression is applied.
    #[must_use]
    pub fn with_workspace_mutes(mut self, repo: WorkspaceMuteRepo) -> Self {
        self.workspace_mutes = Some(repo);
        self
    }

    /// Wire in the user-block store (ROADMAP8), enabling block-based notification
    /// suppression in [`dispatch_notifications`](Self::dispatch_notifications). When
    /// present, recipients who have blocked the sender receive no notifications.
    /// Additive builder; without it, no block suppression is applied.
    #[must_use]
    pub fn with_block_repo(mut self, repo: BlockRepo) -> Self {
        self.block_repo = Some(repo);
        self
    }

    /// Notification suppression seam for per-channel mute + per-user
    /// Do-Not-Disturb + one-off snooze. Returns whether a notification should be
    /// delivered to `recipient` for `room`: `false` when the recipient has MUTED
    /// the room, is currently inside their daily DND window, OR has an active
    /// one-off snooze (`now < snooze_until`); `true` otherwise. DND/snooze are
    /// evaluated against the current UTC clock for now. Best-effort and
    /// FAIL-OPEN: no prefs store wired, or a lookup error, returns `true` so a
    /// glitch never silently drops a notification.
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
        let now = time::OffsetDateTime::now_utc();
        // One-off snooze (Slack "Pause notifications"): suppress while still
        // active, in ADDITION to the recurring window + per-room mute.
        match prefs.get_snooze(recipient).await {
            Ok(snooze) if aero_storage::notification_prefs::is_snoozed(snooze, now) => return false,
            Ok(_) => {}
            Err(err) => {
                warn!(?err, %recipient, "snooze lookup failed; not suppressing");
                return true;
            }
        }
        let now_minute = aero_storage::notification_prefs::minute_of_day_utc(now);
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
            user_groups: None,
            message_edits: None,
            keyword_alerts: None,
            thread_subs: None,
            deactivations: None,
            totp: None,
            thread_notification_prefs: None,
            workspace_mutes: None,
            block_repo: None,
            bus,
            moderator: Arc::new(crate::moderator::AllowAllModerator),
            seq: Arc::new(LocalSeqProvider::new()),
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
    async fn assert_can_post(&self, sender: ParticipantId, room: RoomId) -> Result<()> {
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
    async fn is_workspace_admin_of_room(
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
        expires_at: Option<time::OffsetDateTime>,
    ) -> Result<Message> {
        let started = std::time::Instant::now();
        if !self.rooms.is_member(room, sender).await? {
            return Err(Error::Forbidden(format!(
                "sender {sender} is not a member of room {room}"
            )));
        }
        // Announcement-channel guard (migration 0030). Cheap: a single policy read
        // that short-circuits for the overwhelmingly common `everyone` rooms.
        self.assert_can_post(sender, room).await?;
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
                expires_at,
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

        // Mention / thread-reply notifications, DETACHED from the send path
        // (ROADMAP 第三版 方向四): a large-room @everyone's prefs reads + per-
        // recipient Notify publishes must not add to send latency. Safe to
        // detach because the message is already persisted AND its Message event
        // published above, and dispatch is best-effort end-to-end (every failure
        // is warn-only). `ImService` is cheaply Clone (Arc'd repos/bus). Tests
        // await the handle so inbox state is deterministic after send returns.
        {
            let svc = self.clone();
            let msg = message.clone();
            let dispatch = tokio::spawn(
                async move { svc.dispatch_notifications(&msg, &recipients).await }
                    .in_current_span(),
            );
            #[cfg(test)]
            if let Err(err) = dispatch.await {
                warn!(?err, "notification dispatch task panicked");
            }
            #[cfg(not(test))]
            drop(dispatch);
        }

        // Best-effort enqueue embed + moderate jobs (AI worker picks them up).
        let searchable = message.searchable_text();
        if !searchable.is_empty() {
            // Tag jobs with the room's workspace for per-tenant cost metering.
            // Best-effort: None falls back to global budget tracking.
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
            // Async AI moderation: classifies after publish so send latency is
            // unaffected. BLOCK verdict triggers soft_delete in the worker.
            if let Err(err) = self
                .ai_jobs
                .enqueue(
                    AiJobKind::Moderate,
                    Some(message.id.to_uuid()),
                    ws,
                    serde_json::json!({"text": searchable}),
                )
                .await
            {
                warn!(?err, message_id = %message.id, "enqueue moderate job failed");
            }
        }

        // Throughput observability (ROADMAP 方向五): count accepted messages —
        // layered by room_type for capacity planning — and record the hot-path
        // latency so dashboards can answer "messages/sec". The room-kind lookup
        // is best-effort (a single PK read); "unknown" on miss never fails send.
        let room_type = match self.rooms.room_kind(room).await {
            Ok(Some(RoomKind::Direct)) => "direct",
            Ok(Some(RoomKind::Group)) => "group",
            Ok(Some(RoomKind::Channel)) => "channel",
            _ => "unknown",
        };
        aero_common::metrics::inc_counter_labeled(
            aero_common::metrics::names::MESSAGES_SENT_TOTAL,
            1,
            &[("room_type", room_type)],
        );
        aero_common::metrics::observe_histogram_labeled(
            aero_common::metrics::names::MESSAGE_PROCESSING_DURATION_SECONDS,
            started.elapsed().as_secs_f64(),
            &[("op", "send")],
        );

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
        let started = std::time::Instant::now();
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

        // Archive the version being replaced (best-effort; never blocks the edit).
        // The capture happens BEFORE the overwrite so `message_edits` accumulates
        // every prior version, oldest-first by `recorded_at`.
        if let Some(history) = self.message_edits.as_ref() {
            match serde_json::to_value(&existing.blocks) {
                Ok(old_blocks) => {
                    if let Err(err) = history.record(id, actor, &old_blocks).await {
                        warn!(?err, %id, "record edit history failed");
                    }
                }
                Err(err) => warn!(?err, %id, "serialize prior blocks for history failed"),
            }
        }

        let updated = self
            .messages
            .edit(id, blocks)
            .await?
            .ok_or_else(|| Error::Conflict("edit raced with delete".into()))?;

        self.publish_room_event(updated.room_id, &RoomEvent::Edited(updated.clone()))
            .await;

        let edit_text = updated.searchable_text();
        if !edit_text.is_empty() {
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
            // Re-moderate the edited content: an edit can introduce harmful text
            // that bypassed the original send-time keyword check. Best-effort.
            if let Err(err) = self
                .ai_jobs
                .enqueue(
                    AiJobKind::Moderate,
                    Some(updated.id.to_uuid()),
                    ws,
                    serde_json::json!({"text": edit_text}),
                )
                .await
            {
                warn!(?err, %id, "enqueue re-moderate failed");
            }
        }
        aero_common::metrics::inc_counter(aero_common::metrics::names::MESSAGES_EDITED_TOTAL, 1);
        aero_common::metrics::observe_histogram_labeled(
            aero_common::metrics::names::MESSAGE_PROCESSING_DURATION_SECONDS,
            started.elapsed().as_secs_f64(),
            &[("op", "edit")],
        );
        Ok(updated)
    }

    /// Soft-delete a message. Sender or room-owner may delete.
    #[instrument(skip(self), fields(?actor, ?id))]
    pub async fn delete_message(&self, actor: ParticipantId, id: MessageId) -> Result<()> {
        let started = std::time::Instant::now();
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
        aero_common::metrics::inc_counter(aero_common::metrics::names::MESSAGES_DELETED_TOTAL, 1);
        aero_common::metrics::observe_histogram_labeled(
            aero_common::metrics::names::MESSAGE_PROCESSING_DURATION_SECONDS,
            started.elapsed().as_secs_f64(),
            &[("op", "delete")],
        );
        Ok(())
    }

    /// System action: soft-delete a message flagged by AI moderation and
    /// broadcast the removal. Unlike [`delete_message`] this bypasses the
    /// sender-only authorization check — the caller is the trusted moderation
    /// pipeline, not a participant. `reason` is logged, not sent to clients.
    ///
    /// When `workspace` resolves to a tenant, the delete + a `message.moderated`
    /// audit row commit (or roll back) together via
    /// [`MessageRepo::soft_delete_moderated`](aero_storage::MessageRepo::soft_delete_moderated)
    /// (ROADMAP 第三版 方向五 审计事务化, moderation path): a moderation deletion
    /// can never succeed while its audit append is lost, so removals stay
    /// independently reviewable. `reason` + `digest` (a content summary captured
    /// before the blocks are cleared) are recorded in the audit detail. A legacy
    /// room with no owning workspace (`None`) keeps the plain (unaudited)
    /// soft-delete, exactly like the user-delete handler's degraded path.
    #[instrument(skip(self), fields(?message_id, reason))]
    pub async fn moderate_delete(
        &self,
        message_id: MessageId,
        workspace: Option<WorkspaceId>,
        reason: &str,
        digest: &str,
    ) -> Result<()> {
        let existing = self
            .messages
            .get(message_id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("message {message_id}")))?;
        if existing.deleted_at.is_some() {
            return Ok(());
        }
        match workspace {
            Some(ws) => {
                let detail = serde_json::json!({
                    "room_id": existing.room_id,
                    "reason": reason,
                    "digest": digest,
                });
                self.messages.soft_delete_moderated(message_id, ws, detail).await?;
            }
            // Legacy room with no audit trail to write into: keep the original
            // (non-transactional) delete, mirroring `delete_message`.
            None => {
                self.messages.soft_delete(message_id).await?;
            }
        }
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
        // Reaction notification: a freshly-ADDED reaction to someone else's message
        // drops a durable inbox entry for the author (never self-notify on your own
        // reaction). Best-effort + gated like every other notification (mute / DND /
        // snooze via `should_notify`); only when a NotificationRepo is wired.
        if op == ReactionOp::Add && actor != msg.sender_id {
            if let Some(repo) = self.notifications.as_ref() {
                if self.should_notify(msg.sender_id, msg.room_id).await {
                    if let Err(err) = repo
                        .insert(
                            msg.sender_id,
                            msg.room_id,
                            message_id,
                            NotificationKind::Reaction,
                            Some(actor),
                        )
                        .await
                    {
                        warn!(?err, recipient = ?msg.sender_id, "persist reaction notification failed");
                    } else {
                        self.publish_room_event(
                            msg.room_id,
                            &RoomEvent::Notify {
                                room_id: msg.room_id,
                                message_id,
                                mentioned: msg.sender_id,
                                by: actor,
                                kind: NotificationKind::Reaction,
                            },
                        )
                        .await;
                    }
                }
            }
        }
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

    /// Broadcast an already-constructed [`RoomEvent`] on the room's subject.
    /// Thin public seam (mirrors [`relay_call_event`](Self::relay_call_event))
    /// letting feature modules that own their own storage — e.g. polls — fan out a
    /// room event without re-implementing the bus plumbing. Best-effort: a publish
    /// failure is logged, never surfaced.
    pub async fn broadcast_room_event(&self, room: RoomId, event: RoomEvent) {
        self.publish_room_event(room, &event).await;
    }

    // ---------------------------------------------------------- internal

    /// Persist + push mention/thread-reply notifications for a freshly-sent
    /// message. Best-effort: a failure is logged and never blocks the send.
    /// Runs DETACHED (spawned by [`send_message`](Self::send_message)) so its
    /// prefs reads and per-recipient Notify publishes stay off the send path.
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

        // Thread followers (Wave 11): when this message is a reply, everyone who
        // explicitly followed its root message is notified (∩ room members, never
        // the sender). Reply-kind; `or_insert` so a stronger direct mention wins.
        if let (Some(subs_repo), Some(root)) = (self.thread_subs.as_ref(), message.reply_to) {
            match subs_repo.subscribers(root).await {
                Ok(subs) => {
                    for s in subs {
                        if s != sender && member_set.contains(&s) {
                            targets.entry(s).or_insert(NotificationKind::Reply);
                        }
                    }
                }
                Err(err) => warn!(?err, %root, "thread subscribers lookup failed"),
            }
        }

        // `@handle` tokens parsed once: reused for broadcast mentions (below) and
        // for user-group resolution (further down).
        let handle_tokens = group_handle_tokens(&message.blocks);

        // Broadcast mentions (`@channel` / `@everyone` / `@here` / `@all`): notify
        // EVERY room member. Pure and store-free (online-only filtering for `@here`
        // would need presence wiring — a documented refinement). `or_insert` never
        // downgrades a stronger direct mention/reply, and mute/DND still applies in
        // the delivery loop below; the sender never notifies themselves.
        if handle_tokens.iter().any(|t| is_broadcast_token(t)) {
            for &m in &member_set {
                if m != sender {
                    targets.entry(m).or_insert(NotificationKind::Mention);
                }
            }
        }

        // Group mentions (`@handle`) + keyword/highlight alerts. Both are weaker
        // signals than an explicit mention/reply, so they only ADD a target
        // (`or_insert` never overrides a stronger kind already recorded), only for
        // room members (a non-member can't see the message), and never the sender.
        // Both need the room's workspace; fetch it once, and only when at least one
        // of the two stores is wired. Best-effort: a lookup error is logged, not
        // fatal.
        if self.user_groups.is_some() || self.keyword_alerts.is_some() {
            if let Some(workspace) = self.rooms.room_workspace(room).await.ok().flatten() {
                if let Some(groups) = self.user_groups.as_ref() {
                    for handle in &handle_tokens {
                        // Broadcast tokens aren't group handles — skip the lookup.
                        if is_broadcast_token(handle) {
                            continue;
                        }
                        let Ok(Some(group)) = groups.resolve(workspace, handle).await else {
                            continue;
                        };
                        match groups.members(group.id).await {
                            Ok(members) => {
                                for m in members {
                                    if m != sender && member_set.contains(&m) {
                                        targets.entry(m).or_insert(NotificationKind::Mention);
                                    }
                                }
                            }
                            Err(err) => {
                                warn!(?err, group = %group.id, "group members lookup failed");
                            }
                        }
                    }
                }
                if let Some(alerts) = self.keyword_alerts.as_ref() {
                    let text = message.searchable_text();
                    if !text.is_empty() {
                        match alerts.matching_subscribers(workspace, &text).await {
                            Ok(subs) => {
                                for s in subs {
                                    if s != sender && member_set.contains(&s) {
                                        targets.entry(s).or_insert(NotificationKind::Mention);
                                    }
                                }
                            }
                            Err(err) => warn!(?err, %room, "keyword subscriber lookup failed"),
                        }
                    }
                }
            }
        }

        // Block filter (ROADMAP8): remove any recipient who has blocked the sender.
        // Best-effort: a lookup failure delivers (fail-open, warn). No-op when
        // the block store is not wired.
        if let Some(block_repo) = self.block_repo.as_ref() {
            match block_repo.blockers_of(sender).await {
                Ok(blockers) => {
                    let blocker_set: std::collections::HashSet<ParticipantId> =
                        blockers.into_iter().collect();
                    targets.retain(|p, _| !blocker_set.contains(p));
                }
                Err(err) => warn!(?err, "block repo lookup failed; not suppressing"),
            }
        }

        // Workspace-wide mute check (ROADMAP6 Lane A): suppress ALL notifications
        // for participants who have muted this room's workspace. Checked FIRST,
        // before any room-level mute or DND check. Best-effort: a failed lookup
        // degrades to "no workspace suppression" (warn + deliver), never a dropped
        // notification. No-op when the store or workspace id is absent.
        let workspace_for_room = if self.workspace_mutes.is_some() {
            self.rooms.room_workspace(room).await.ok().flatten()
        } else {
            None
        };
        let ws_muted_set: std::collections::HashSet<ParticipantId> =
            if let (Some(ws_mute_repo), Some(workspace)) =
                (self.workspace_mutes.as_ref(), workspace_for_room)
            {
                let ids: Vec<ParticipantId> = targets.keys().copied().collect();
                let mut set = std::collections::HashSet::new();
                for id in &ids {
                    match ws_mute_repo.is_muted(*id, workspace).await {
                        Ok(true) => { set.insert(*id); }
                        Ok(false) => {}
                        Err(err) => {
                            warn!(?err, %id, "workspace mute lookup failed; not suppressing");
                        }
                    }
                }
                set
            } else {
                std::collections::HashSet::new()
            };
        // Remove workspace-muted recipients from the target map immediately so
        // subsequent steps (room-level mute, thread-level, DND) skip them.
        targets.retain(|recipient, _| !ws_muted_set.contains(recipient));

        // Thread notification level check (ROADMAP6 Lane A): for thread-reply
        // notifications, apply the per-recipient per-thread level:
        //   "all"      — deliver (default when no row).
        //   "mentions" — deliver only when the recipient is @-mentioned.
        //   "none"     — drop.
        // Applied after workspace-mute filtering, before room-level DND/snooze.
        // Best-effort: a lookup failure delivers (fail-open, warn).
        if let (Some(tnp_repo), Some(root)) =
            (self.thread_notification_prefs.as_ref(), message.reply_to)
        {
            let reply_recipients: Vec<ParticipantId> = targets
                .iter()
                .filter_map(|(p, k)| (*k == NotificationKind::Reply).then_some(*p))
                .collect();

            // Collect mentioned participant ids for the "mentions" level check.
            let mentioned: std::collections::HashSet<ParticipantId> =
                mentioned_participants(&message.blocks).into_iter().collect();

            // Per-recipient level lookup (individual queries; N is bounded by room
            // membership, already O(3N) in the existing prefs batch path). Best-effort:
            // a lookup failure delivers (fail-open, warn).
            let mut to_drop: Vec<ParticipantId> = Vec::new();
            for &recipient in &reply_recipients {
                let level = match tnp_repo.get_level(recipient, root).await {
                    Ok(l) => l,
                    Err(err) => {
                        warn!(?err, %recipient, "thread notification level lookup failed; delivering");
                        "all".to_owned()
                    }
                };
                let deliver = match level.as_str() {
                    "none" => false,
                    "mentions" => mentioned.contains(&recipient),
                    _ => true, // "all" or any unknown value: fail-open
                };
                if !deliver {
                    to_drop.push(recipient);
                }
            }
            for recipient in to_drop {
                targets.remove(&recipient);
            }
        }

        // Filter by mute / DND / snooze, then persist ALL survivors in ONE batch
        // INSERT (ROADMAP 第三版 方向四 — write amplification). Prefs are read in
        // TWO batched queries (`= ANY`) instead of three per recipient, then
        // applied in memory through the same pure helper composition the
        // single-recipient `should_notify` uses — a 10k-member @everyone is now
        // O(1) prefs round-trips instead of O(3N). Fail-open like should_notify:
        // a failed batch lookup degrades to "no suppression" (warn + notify),
        // never to a dropped notification.
        let notifiable: Vec<(ParticipantId, NotificationKind)> = if let Some(prefs) =
            self.prefs.as_ref()
        {
            let ids: Vec<ParticipantId> = targets.keys().copied().collect();
            let muted = match prefs.muted_set(room, &ids).await {
                Ok(set) => set,
                Err(err) => {
                    warn!(?err, %room, "batch mute lookup failed; not suppressing");
                    std::collections::HashSet::new()
                }
            };
            let dnd_rows = match prefs.dnd_snooze_many(&ids).await {
                Ok(rows) => rows,
                Err(err) => {
                    warn!(?err, %room, "batch DND/snooze lookup failed; not suppressing");
                    std::collections::HashMap::new()
                }
            };
            let now = time::OffsetDateTime::now_utc();
            targets
                .into_iter()
                .filter(|(recipient, _)| {
                    // Absent row == no prefs (DndSnooze::default): always delivers.
                    let row = dnd_rows.get(recipient).copied().unwrap_or_default();
                    aero_storage::notification_prefs::should_deliver(
                        muted.contains(recipient),
                        row.dnd,
                        row.snooze_until,
                        now,
                    )
                })
                .collect()
        } else {
            targets.into_iter().collect()
        };
        if let Err(err) = repo.insert_many(room, message.id, Some(sender), &notifiable).await {
            warn!(?err, %room, count = notifiable.len(), "batch persist notifications failed");
            return;
        }
        // Targeted live hints: one Notify per recipient (the WS layer routes each
        // to its single recipient via `explicit_recipients` — push_bot consumes
        // them individually), published with bounded concurrency instead of
        // strictly serially. Best-effort; runs inside the detached dispatch task,
        // off the send hot path.
        futures::stream::iter(notifiable.into_iter().map(|(recipient, kind)| {
            let event = RoomEvent::Notify {
                room_id: room,
                message_id: message.id,
                mentioned: recipient,
                by: sender,
                kind,
            };
            async move { self.publish_room_event(room, &event).await }
        }))
        .buffer_unordered(NOTIFY_PUBLISH_CONCURRENCY)
        .for_each(|()| std::future::ready(()))
        .await;
    }

    /// Publish a room event to the bus (cross-node fan-out).
    ///
    /// **Deliberate design (ROADMAP 方向一):** a publish failure is recorded
    /// (warn + `NATS_PUBLISH_ERRORS_TOTAL`) but NOT propagated to the caller, so
    /// the HTTP/WS send path still returns success. This is intentional and is
    /// *not* the "silent data loss" the ROADMAP cautions against:
    ///
    /// - The message is already durably persisted in Postgres **before** this
    ///   publish (persist-then-broadcast). NATS is the live fan-out, not the
    ///   source of truth.
    /// - Subscribers that missed the live event recover it through the reconnect
    ///   **backfill** protocol (`MessageRepo::list_since` + `?since=` cursor) — so
    ///   no message is lost even when NATS is down.
    /// - Returning `503` here would instead induce **duplicate sends**: the client
    ///   retries, re-inserting the already-persisted message. The publish-error
    ///   counter (alertable) is the correct operational signal; the data path
    ///   self-heals via backfill.
    async fn publish_room_event(&self, room: RoomId, event: &RoomEvent) {
        let subject = Self::room_subject(room);
        // Publish-time seq stamp (ROADMAP 第三版 方向一): mint the per-room seq
        // BEFORE the bytes hit NATS, so an at-least-once redelivery carries the
        // SAME seq and clients can dedup/order Edited/Deleted/Reaction/Typing
        // events that have no id of their own. `None` (provider unavailable)
        // degrades to an unstamped event — never blocks delivery. Existing
        // consumers deserialize `RoomEvent` with serde, which ignores the
        // unknown `"seq"` field (no event type uses `deny_unknown_fields`).
        let seq = self.seq.next_seq(&subject).await;
        let publish = async {
            let bytes = aero_bus::stamped_event_bytes(event, seq)?;
            self.bus.publish_bytes(&subject, bytes.into()).await
        };
        if let Err(err) = publish.await {
            warn!(?err, %subject, "publish RoomEvent failed");
            aero_common::metrics::inc_counter(
                aero_common::metrics::names::NATS_PUBLISH_ERRORS_TOTAL,
                1,
            );
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

/// Whether an `@handle` token is a broadcast/special mention that targets the
/// whole channel rather than a single user-group. `@channel`, `@everyone`, `@all`,
/// and `@here` (the Slack-standard set) all fan out to every room member. Pure, so
/// it is unit-testable without a database. Tokens are already lowercased by
/// [`group_handle_tokens`].
fn is_broadcast_token(token: &str) -> bool {
    matches!(token, "channel" | "everyone" | "all" | "here")
}

/// Extract distinct `@handle` tokens from a message's text blocks — lowercased and
/// de-duplicated in first-appearance order. A handle is `@` followed by one or more
/// ASCII alphanumerics / `_` / `-` (the same alphabet user-group handles are stored
/// in). These are resolved against workspace user-group handles to fan a group
/// mention out to its members; a token that matches no group is simply ignored.
/// Pure, so it is unit-testable without a database or bus.
fn group_handle_tokens(blocks: &[Block]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for b in blocks {
        let Block::Text { content, .. } = b else {
            continue;
        };
        let bytes = content.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'@' {
                let start = i + 1;
                let mut j = start;
                while j < bytes.len()
                    && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'-')
                {
                    j += 1;
                }
                if j > start {
                    // start..j are ASCII byte offsets ⇒ always char boundaries.
                    let token = content[start..j].to_ascii_lowercase();
                    if seen.insert(token.clone()) {
                        out.push(token);
                    }
                }
                i = j.max(start);
            } else {
                i += 1;
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
    fn group_handle_tokens_extracts_lowercases_and_dedups() {
        let blocks = vec![
            Block::text("hey @Eng and @ops, ping @eng again"),
            Block::text("also @on-call_team! and a bare @ and email a@b.com"),
        ];
        let got = group_handle_tokens(&blocks);
        // first-appearance order, lowercased, deduped; "eng" not repeated.
        assert_eq!(got, vec!["eng", "ops", "on-call_team", "b"]);
        // The bare "@ " yields no token; non-text blocks are ignored.
        assert!(group_handle_tokens(&[Block::text("no handles here")]).is_empty());
        assert!(group_handle_tokens(&[Block::Mention { participant: ParticipantId::new() }]).is_empty());
    }

    #[test]
    fn broadcast_tokens_recognized_case_insensitively() {
        // group_handle_tokens lowercases, so is_broadcast_token sees lowercase.
        for t in ["channel", "everyone", "all", "here"] {
            assert!(is_broadcast_token(t), "{t} is a broadcast mention");
        }
        for t in ["eng", "ops", "channels", "everybody", ""] {
            assert!(!is_broadcast_token(t), "{t} is NOT a broadcast mention");
        }
        // End-to-end through the token extractor: "@channel" in text is detected.
        let toks = group_handle_tokens(&[Block::text("hey @Channel ship it")]);
        assert!(toks.iter().any(|t| is_broadcast_token(t)), "@Channel detected");
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

    #[test]
    fn post_allowed_everyone_is_always_true() {
        // The open default ignores admin/creator standing entirely.
        for is_admin in [false, true] {
            for is_creator in [false, true] {
                assert!(
                    post_allowed("everyone", is_admin, is_creator),
                    "everyone always permits (admin={is_admin}, creator={is_creator})"
                );
            }
        }
    }

    #[test]
    fn post_allowed_admins_requires_admin_or_creator() {
        // 'admins' permits exactly the admin OR the creator; a plain member is denied.
        assert!(post_allowed("admins", true, false), "workspace admin may post");
        assert!(post_allowed("admins", false, true), "room creator may post");
        assert!(post_allowed("admins", true, true), "admin+creator may post");
        assert!(
            !post_allowed("admins", false, false),
            "a plain member may not post in an announcements-only channel"
        );
    }

    #[test]
    fn post_allowed_admins_is_logical_or_of_admin_and_creator() {
        for is_admin in [false, true] {
            for is_creator in [false, true] {
                assert_eq!(
                    post_allowed("admins", is_admin, is_creator),
                    is_admin || is_creator,
                    "(admin={is_admin}, creator={is_creator})"
                );
            }
        }
    }

    #[test]
    fn post_allowed_unknown_policy_falls_back_to_everyone() {
        // An unrecognized/typo'd policy fails OPEN (treated as everyone), never a
        // silent lockout — matches the read-side default in `RoomRepo::post_policy`.
        for is_admin in [false, true] {
            for is_creator in [false, true] {
                assert!(
                    post_allowed("bogus", is_admin, is_creator),
                    "unknown policy permits like everyone (admin={is_admin}, creator={is_creator})"
                );
                assert!(post_allowed("", is_admin, is_creator), "empty policy permits");
            }
        }
    }
}
