//! `ImService` — the IM business facade.
//!
//! Coordinates the storage repositories and the event bus. All business
//! invariants (membership checks, validation, authorization) live here so
//! HTTP/WS handlers stay thin.
//!
//! See `docs/specs/2026-05-22-aero-im-design.md` §4.2 for the protocol contract.
use std::sync::Arc;
use aero_bus::EventBus;
use aero_common::{
    Block, Message, NotificationKind, NotifyTarget, ParticipantId, RoomEvent, RoomId, WorkspaceRole,
};
use aero_storage::{
    AiJobRepo, AutoModRuleRepo, BlockRepo, CallRepo,
    DeactivationRepo, KeywordAlertRepo, MessageEditRepo, MessageRepo, NotificationBundleRepo,
    NotificationPrefsRepo, NotificationRepo, ParticipantRepo, PinRepo, ReactionRepo, ReceiptRepo, RoomRepo,
    ThreadMuteRepo, ThreadNotificationPrefsRepo, ThreadSubscriptionRepo, TotpRepo, UserGroupRepo,
    WorkspaceMuteRepo, WorkspaceNotifDefaultsRepo, WorkspaceRepo,
};
use std::collections::BTreeMap;
use tracing::warn;
use crate::service::BusSink;
use crate::moderator::Moderator;
use crate::seq::{LocalSeqProvider, SeqProvider};
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
/// Fingerprint a message's content for the behavioral spam guard's duplicate /
/// cross-room-blast detection. Identical block content hashes identically within a
/// process (a non-stable-across-runs `DefaultHasher` is fine — the guard is
/// per-process and short-window).
pub(crate) fn spam_content_hash(blocks: &[Block]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match serde_json::to_string(blocks) {
        Ok(s) => s.hash(&mut h),
        // Should never fail for Block; fall back to length so distinct-size
        // messages still differ.
        Err(_) => blocks.len().hash(&mut h),
    }
    h.finish()
}
/// The notification batch a [`RoomEvent::NotifyBatch`] carries — used only to
/// derive a deterministic `delivery_id`. The same message produces one mention
/// batch and (separately) one reply batch; they must derive *different*
/// delivery ids so a recipient legitimately on both is not collapsed.
#[derive(Clone, Copy)]
pub(crate) enum NotifyBatchKind {
    Mention,
    Reply,
}

impl NotifyBatchKind {
    /// The discriminator woven into the v5 name so the two batches of one
    /// message never share a `delivery_id`.
    fn tag(self) -> &'static str {
        match self {
            NotifyBatchKind::Mention => "mention",
            NotifyBatchKind::Reply => "reply",
        }
    }
}

/// A fixed, private namespace UUID for deriving NotifyBatch idempotency tokens.
/// Any constant non-nil UUID works; it just has to be stable across processes
/// and releases so a redelivered batch derives the same id. (Random v4, minted
/// once and frozen here.)
const NOTIFY_DELIVERY_NAMESPACE: uuid::Uuid =
    uuid::uuid!("6f1d2e3c-9b4a-4d57-8a2e-1c0f7b6a4d9e");

/// Derive a **deterministic** `delivery_id` for a NotifyBatch from the message
/// it concerns plus which batch (mention vs reply) it is. The same message's
/// same batch always derives the same UUID, so a NATS redelivery after a
/// consumer crash produces an identical `delivery_id` and
/// `NotificationRepo::insert_many`'s `ON CONFLICT (delivery_id, participant_id)`
/// de-duplicates the re-expanded rows (mig 0137). A random id (the old code)
/// changed on every retry and so could never collide — the index never fired.
#[must_use]
pub(crate) fn notify_delivery_id(
    message: aero_common::MessageId,
    kind: NotifyBatchKind,
) -> uuid::Uuid {
    let name = format!("{}:{}", message.to_uuid(), kind.tag());
    uuid::Uuid::new_v5(&NOTIFY_DELIVERY_NAMESPACE, name.as_bytes())
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
/// Sentinel `workspace` label value for a message whose owning tenant could not
/// be resolved (legacy room, or a best-effort lookup that errored / missed).
/// Mirrors `aero_ai::metrics`'s workspace-less label: collapse to one bounded
/// series rather than dropping the data or inventing an unbounded label.
pub(crate) const WORKSPACE_NONE: &str = "none";
/// Whether the opt-in per-tenant message-rate metric is enabled, read ONCE from
/// `AERO_PER_TENANT_METRICS` and cached for the process lifetime. OPT-IN (default
/// OFF) because a `workspace` label multiplies `aero_messages_sent_total`'s
/// cardinality by the active-tenant count — a deliberate operator choice. Resolved
/// at most once so the send hot path never re-reads env (effectively boot-time
/// config) — ROADMAP5 方向二.
pub(crate) fn per_tenant_metrics_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("AERO_PER_TENANT_METRICS").is_ok_and(|v| env_truthy(&v)))
}
/// Whether an env-var value is "truthy": `"1"` or (case-insensitively) `"true"`.
/// Anything else — empty, `"0"`, `"false"`, `"yes"` — is false.
#[must_use]
fn env_truthy(v: &str) -> bool {
    v == "1" || v.eq_ignore_ascii_case("true")
}
/// IM business facade. Cheap to clone (repositories wrap `Arc<PgPool>`).
#[derive(Clone)]
pub struct ImService {
    pub(crate) rooms: RoomRepo,
    /// Tenancy repo for workspace-scoped authorization. Optional so the existing
    /// constructors stay signature-compatible; wire it via
    /// [`with_workspaces`](ImService::with_workspaces) to enable the
    /// workspace-scoped methods.
    pub(crate) workspaces: Option<WorkspaceRepo>,
    pub(crate) messages: MessageRepo,
    #[allow(dead_code)] // Reserved for mention lookups, P3+.
    pub(crate) participants: ParticipantRepo,
    pub(crate) receipts: ReceiptRepo,
    pub(crate) reactions: ReactionRepo,
    pub(crate) calls: CallRepo,
    pub(crate) ai_jobs: AiJobRepo,
    /// Notification inbox (mentions / thread replies). Optional so the existing
    /// constructors stay signature-compatible; wire it via
    /// [`with_notifications`](ImService::with_notifications) to persist + push
    /// notifications on send. Without it, sends still succeed (no inbox writes).
    pub(crate) notifications: Option<NotificationRepo>,
    /// Notification bundle queue (ROADMAP7 方向三). Optional builder
    /// ([`with_notification_bundles`](Self::with_notification_bundles));
    /// when present, reply notifications are deferred into bundle rows
    /// and periodically flushed as AggregateReply.
    pub(crate) notification_bundles: Option<NotificationBundleRepo>,
    /// Pinned-messages store. Optional builder ([`with_pins`](ImService::with_pins));
    /// the pin/unpin/list methods return an internal error if it isn't wired.
    pub(crate) pins: Option<PinRepo>,
    /// Notification preferences (per-channel mute + per-user Do-Not-Disturb).
    /// Optional builder ([`with_notification_prefs`](ImService::with_notification_prefs));
    /// when present, [`should_notify`](ImService::should_notify) suppresses
    /// notifications to muted/DND recipients. Without it, nothing is suppressed.
    pub(crate) prefs: Option<NotificationPrefsRepo>,
    /// User-group store (`@-usergroups`). Optional builder
    /// ([`with_user_groups`](Self::with_user_groups)); when present,
    /// [`dispatch_notifications`](Self::dispatch_notifications) expands an
    /// `@handle` group mention into a notification for every group member.
    pub(crate) user_groups: Option<UserGroupRepo>,
    /// Message edit-history store. Optional builder
    /// ([`with_message_edits`](Self::with_message_edits)); when present,
    /// [`edit_message`](Self::edit_message) archives the replaced version before
    /// overwriting it.
    pub(crate) message_edits: Option<MessageEditRepo>,
    /// Keyword / highlight-alert store. Optional builder
    /// ([`with_keyword_alerts`](Self::with_keyword_alerts)); when present,
    /// [`dispatch_notifications`](Self::dispatch_notifications) also notifies any
    /// member subscribed to a keyword the message contains.
    pub(crate) keyword_alerts: Option<KeywordAlertRepo>,
    /// Thread-subscription store. Optional builder
    /// ([`with_thread_subs`](Self::with_thread_subs)); when present,
    /// [`dispatch_notifications`](Self::dispatch_notifications) notifies everyone
    /// who followed a reply's root message (Wave 11).
    pub(crate) thread_subs: Option<ThreadSubscriptionRepo>,
    /// Per-user thread-mute store (migration 0088) — the inverse of
    /// [`thread_subs`](Self::thread_subs). Optional builder
    /// ([`with_thread_mutes`](Self::with_thread_mutes)); when present,
    /// [`dispatch_notifications`](Self::dispatch_notifications) SUBTRACTS everyone
    /// who muted a reply's root message from the notification fan-out (after the
    /// workspace-mute step, before the per-thread level step). Muting suppresses
    /// only the NOTIFICATION, never the room broadcast of the reply.
    pub(crate) thread_mutes: Option<ThreadMuteRepo>,
    /// Workspace deactivation store. Optional builder
    /// ([`with_deactivations`](Self::with_deactivations)); when present,
    /// [`assert_room_access`](Self::assert_room_access) denies a member who has
    /// been deactivated in the room's workspace (Wave 14).
    pub(crate) deactivations: Option<DeactivationRepo>,
    /// Workspace 2FA-enforcement store. Optional builder
    /// ([`with_totp`](Self::with_totp)); when present,
    /// [`assert_room_access`](Self::assert_room_access) denies a member of a
    /// `require_2fa` workspace who has not activated TOTP (Wave 24).
    pub(crate) totp: Option<TotpRepo>,
    /// Per-thread notification level store (ROADMAP6 Lane A). Optional builder
    /// ([`with_thread_notification_prefs`](Self::with_thread_notification_prefs));
    /// when present, [`dispatch_notifications`](Self::dispatch_notifications) applies
    /// the per-thread level (`all`/`mentions`/`none`) to thread reply notifications
    /// after the thread-mute step.
    pub(crate) thread_notification_prefs: Option<ThreadNotificationPrefsRepo>,
    /// Workspace-wide mute store (ROADMAP6 Lane A). Optional builder
    /// ([`with_workspace_mutes`](Self::with_workspace_mutes)); when present,
    /// [`dispatch_notifications`](Self::dispatch_notifications) suppresses all
    /// notifications to participants who have muted the room's workspace (checked
    /// first, before any room-level suppression).
    pub(crate) workspace_mutes: Option<WorkspaceMuteRepo>,
    /// User block store (ROADMAP8). Optional builder
    /// ([`with_block_repo`](Self::with_block_repo)); when present,
    /// [`dispatch_notifications`](Self::dispatch_notifications) suppresses
    /// notifications to recipients who have blocked the sender.
    pub(crate) block_repo: Option<BlockRepo>,
    /// Auto-moderation rules engine (ROADMAP10 migration 0111). Optional builder
    /// ([`with_auto_mod_rules`](Self::with_auto_mod_rules)); when present,
    /// [`send_message`](Self::send_message) enforces workspace-level text rules at
    /// send time (the `"block"` action rejects the message before persistence).
    pub(crate) auto_mod_rules: Option<AutoModRuleRepo>,
    /// Workspace-level default notification level store (ROADMAP12 migration 0119).
    /// Optional builder ([`with_workspace_notif_defaults`](Self::with_workspace_notif_defaults));
    /// when present, [`join_channel`](Self::join_channel) and
    /// [`add_member`](Self::add_member) apply the workspace default level to new
    /// members that have no existing `channel_notification_prefs` row.
    pub(crate) workspace_notif_defaults: Option<WorkspaceNotifDefaultsRepo>,
    /// Cluster-wide room-presence store (Redis sorted set; see
    /// [`aero_storage::PresenceStore`]). Optional builder
    /// ([`with_presence`](Self::with_presence)); when present,
    /// [`dispatch_notifications`](Self::dispatch_notifications) restricts an
    /// `@here` broadcast to the room's *online* members (Slack semantics),
    /// while `@channel`/`@everyone`/`@all` still fan out to all members.
    /// Strictly fail-open: a presence lookup error, or an empty online set
    /// (which would otherwise silently drop everyone), falls back to notifying
    /// every member — `@here` must never lose a notification because presence
    /// is unreachable.
    pub(crate) presence: Option<aero_storage::PresenceStore>,
    pub(crate) bus: Arc<dyn BusSink>,
    pub(crate) moderator: Arc<dyn Moderator>,
    /// Optional behavioral spam/flood guard (ROADMAP5 方向五). When wired,
    /// [`send_message`](Self::send_message) throttles a sender whose recent send
    /// behaviour (rate / same-content cross-room blast / duplicates) crosses the
    /// thresholds. `None` = no behavioural throttling.
    pub(crate) spam_guard: Option<Arc<crate::SpamGuard>>,
    /// Optional PII detector (ROADMAP5 方向五). When wired,
    /// [`send_message`](Self::send_message) blocks an outbound message whose text
    /// carries structurally plausible PII (SSN / Luhn-valid card / email / phone),
    /// so it never reaches the FTS index, AI embeddings, or exports. `None` = no
    /// PII screening.
    pub(crate) pii_detector: Option<Arc<crate::PiiDetector>>,
    /// Per-subject event-seq source for publish-time `"seq"` stamping (ROADMAP
    /// 第三版 方向一). Defaults to the process-local [`LocalSeqProvider`];
    /// clustered deployments wire the Redis `INCR`-backed
    /// [`aero_storage::SeqStore`] via [`with_seq`](Self::with_seq) so every node
    /// draws from one per-room sequence. Per-room monotonic; gaps are legal
    /// (consumers use seq only for dedup + relative order).
    pub(crate) seq: Arc<dyn SeqProvider>,
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
            notification_bundles: None,
            pins: None,
            prefs: None,
            user_groups: None,
            message_edits: None,
            keyword_alerts: None,
            thread_subs: None,
            thread_mutes: None,
            deactivations: None,
            totp: None,
            thread_notification_prefs: None,
            workspace_mutes: None,
            block_repo: None,
            auto_mod_rules: None,
            workspace_notif_defaults: None,
            presence: None,
            bus: bus as Arc<dyn BusSink>,
            moderator,
            spam_guard: None,
            pii_detector: None,
            seq: Arc::new(LocalSeqProvider::new()),
        }
    }
    /// Inject a custom moderator (overrides env default).
    #[must_use]
    pub fn with_moderator(mut self, moderator: Arc<dyn Moderator>) -> Self {
        self.moderator = moderator;
        self
    }
    /// Wire a behavioral spam/flood guard (ROADMAP5 方向五). Additive — without it
    /// `send_message` does no behavioural throttling.
    #[must_use]
    pub fn with_spam_guard(mut self, guard: Arc<crate::SpamGuard>) -> Self {
        self.spam_guard = Some(guard);
        self
    }
    /// Wire a PII detector (ROADMAP5 方向五). Additive — without it `send_message`
    /// does no PII screening.
    #[must_use]
    pub fn with_pii_detector(mut self, detector: Arc<crate::PiiDetector>) -> Self {
        self.pii_detector = Some(detector);
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
    /// Wire in the notification bundle queue (ROADMAP7 方向三), enabling
    /// deferred reply aggregation. Additive builder; without it, reply
    /// notifications are sent immediately (no aggregation) — matching the
    /// legacy behaviour.
    #[must_use]
    pub fn with_notification_bundles(mut self, bundles: NotificationBundleRepo) -> Self {
        self.notification_bundles = Some(bundles);
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
    /// Wire in the per-user thread-mute store (migration 0088), the inverse of
    /// [`with_thread_subs`](Self::with_thread_subs): when present,
    /// [`dispatch_notifications`](Self::dispatch_notifications) removes everyone who
    /// muted a reply's root message from the notification fan-out. Additive builder;
    /// without it, no thread-mute suppression is applied (fail-open).
    #[must_use]
    pub fn with_thread_mutes(mut self, thread_mutes: ThreadMuteRepo) -> Self {
        self.thread_mutes = Some(thread_mutes);
        self
    }
    /// Wire in the cluster-wide room-presence store
    /// ([`aero_storage::PresenceStore`]), enabling Slack-style `@here` semantics
    /// in [`dispatch_notifications`](Self::dispatch_notifications): an `@here`
    /// broadcast then notifies only the room's *online* members, whereas
    /// `@channel`/`@everyone`/`@all` continue to notify everyone. Additive
    /// builder; without it, `@here` keeps its legacy behaviour of notifying all
    /// members. The filter is strictly fail-open — a presence lookup error (or
    /// an empty online roster) falls back to the full member set, so `@here`
    /// never drops a notification because Redis is unreachable.
    #[must_use]
    pub fn with_presence(mut self, presence: aero_storage::PresenceStore) -> Self {
        self.presence = Some(presence);
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

    /// Wire in the auto-moderation rules repo (ROADMAP10), enabling workspace-level
    /// text-pattern enforcement at send time. When present,
    /// [`send_message`](Self::send_message) fetches the workspace's rules and rejects
    /// any message matched by a `"block"` rule. Additive builder; without it, no
    /// auto-mod rules are applied.
    #[must_use]
    pub fn with_auto_mod_rules(mut self, repo: AutoModRuleRepo) -> Self {
        self.auto_mod_rules = Some(repo);
        self
    }

    /// Wire in the workspace notification defaults repo (ROADMAP12 migration 0119).
    /// When present, [`join_channel`](Self::join_channel) applies the workspace
    /// default notification level to the joining participant if they have no
    /// existing `channel_notification_prefs` row for that room. Best-effort and
    /// fail-open: errors here never block the join.
    #[must_use]
    pub fn with_workspace_notif_defaults(mut self, repo: WorkspaceNotifDefaultsRepo) -> Self {
        self.workspace_notif_defaults = Some(repo);
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
    pub(crate) async fn should_notify(&self, recipient: ParticipantId, room: RoomId) -> bool {
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
            notification_bundles: None,
            pins: None,
            prefs: None,
            user_groups: None,
            message_edits: None,
            keyword_alerts: None,
            thread_subs: None,
            thread_mutes: None,
            deactivations: None,
            totp: None,
            thread_notification_prefs: None,
            workspace_mutes: None,
            block_repo: None,
            auto_mod_rules: None,
            workspace_notif_defaults: None,
            presence: None,
            bus,
            moderator: Arc::new(crate::moderator::AllowAllModerator),
            spam_guard: None,
            pii_detector: None,
            seq: Arc::new(LocalSeqProvider::new()),
        }
    }

    /// NATS subject used for per-room broadcast (see spec §4.2).
    #[must_use]
    pub fn room_subject(room: RoomId) -> String {
        format!("im.room.{room}")
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
    pub(crate) async fn dispatch_notifications(&self, message: &Message, members: &[ParticipantId]) {
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

        // Broadcast mentions. Two flavours with distinct fan-out (Slack semantics):
        //   * `@channel` / `@everyone` / `@all` → notify EVERY room member.
        //   * `@here`                           → notify only the room's *online*
        //                                          members (when presence is wired).
        // `or_insert` never downgrades a stronger direct mention/reply, and
        // mute/DND still applies in the delivery loop below; the sender never
        // notifies themselves.
        let wants_all = handle_tokens.iter().any(|t| is_all_broadcast_token(t));
        let wants_here = handle_tokens.iter().any(|t| is_here_token(t));
        if wants_all || wants_here {
            // `@channel`-class tokens dominate: they always mean everyone, so an
            // accompanying `@here` can only ever be a subset and adds nothing. Only
            // when `@here` is the *sole* broadcast do we narrow to online members.
            let recipients: std::collections::BTreeSet<ParticipantId> = if wants_all {
                member_set.clone()
            } else {
                // `@here` only. Restrict to online members — but strictly
                // fail-open: a presence lookup error, no wired presence store, or
                // an empty online roster all fall back to the full member set so
                // `@here` never silently drops a notification.
                self.here_recipients(room, &member_set).await
            };
            for &m in &recipients {
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
                // One index-backed batch query instead of N serial is_muted
                // round-trips (ROADMAP 方向二): a large-room @everyone previously
                // scaled linearly with membership. Fail-open: a lookup error
                // suppresses nobody (delivers), matching the prior per-id loop.
                match ws_mute_repo.muted_participants(workspace, &ids).await {
                    Ok(set) => set,
                    Err(err) => {
                        warn!(?err, "workspace mute batch lookup failed; not suppressing");
                        std::collections::HashSet::new()
                    }
                }
            } else {
                std::collections::HashSet::new()
            };
        // Remove workspace-muted recipients from the target map immediately so
        // subsequent steps (room-level mute, thread-level, DND) skip them.
        targets.retain(|recipient, _| !ws_muted_set.contains(recipient));

        // Thread-mute check (migration 0088): for a REPLY, subtract everyone who
        // explicitly muted its root message — the exact inverse of the thread-follow
        // fan-out above. A mute suppresses the NOTIFICATION only; the reply is still
        // broadcast to the room. Applied after the workspace-mute step, before the
        // per-thread level step. Best-effort and FAIL-OPEN: no store wired, or a
        // lookup error, suppresses nobody (warn + deliver). No-op for non-replies.
        if let (Some(mute_repo), Some(root)) = (self.thread_mutes.as_ref(), message.reply_to) {
            match mute_repo.muted_by(root).await {
                Ok(muters) => targets.retain(|recipient, _| !muters.contains(recipient)),
                Err(err) => warn!(?err, %root, "thread mute lookup failed; not suppressing"),
            }
        }

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

            // ONE batch query for every reply-recipient's thread level instead of
            // an O(R) per-recipient get_level loop (ROADMAP 方向二). Fail-open: a
            // batch error yields an empty map, so a missing level defaults to
            // "all" and delivers.
            let levels = tnp_repo
                .levels_for(root, &reply_recipients)
                .await
                .unwrap_or_else(|err| {
                    warn!(?err, "thread notification level batch lookup failed; delivering");
                    std::collections::HashMap::new()
                });
            let mut to_drop: Vec<ParticipantId> = Vec::new();
            for &recipient in &reply_recipients {
                let level = levels.get(&recipient).map_or("all", String::as_str);
                let deliver = match level {
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
        // Split notifiable into mentions (immediate insert) and replies (bundle).
        let (mentions, replies): (Vec<_>, Vec<_>) = notifiable
            .into_iter()
            .partition(|(_, kind)| matches!(kind, NotificationKind::Mention));

        // Mentions: insert immediately and publish NotifyBatch.
        if !mentions.is_empty() {
            // Deterministic idempotency token: the same message's mention batch
            // always derives the same id, so a redelivered batch de-dups via
            // insert_many's ON CONFLICT (delivery_id, participant_id).
            let delivery_id = notify_delivery_id(message.id, NotifyBatchKind::Mention);
            if let Err(err) = repo
                .insert_many(room, message.id, Some(sender), &mentions, Some(delivery_id))
                .await
            {
                warn!(?err, %room, count = mentions.len(), "mention batch persist failed");
            } else {
                let recipients: Vec<NotifyTarget> = mentions
                    .into_iter()
                    .map(|(participant, kind)| NotifyTarget { participant, kind })
                    .collect();
                self.publish_room_event(
                    room,
                    &RoomEvent::NotifyBatch {
                        room_id: room,
                        message_id: message.id,
                        by: sender,
                        delivery_id,
                        recipients,
                    },
                )
                .await;
            }
        }

        // Replies: defer into notification bundles for periodic aggregation.
        if let Some(bundle_repo) = &self.notification_bundles {
            let thread_root = message.reply_to;
            for (recipient, kind) in &replies {
                if let Err(err) = bundle_repo
                    .insert(*recipient, room, message.id, *kind, Some(sender), thread_root)
                    .await
                {
                    warn!(?err, %recipient, "notification bundle insert failed");
                }
            }
        } else {
            // No bundle store: insert replies immediately (legacy behaviour).
            // Reply batch gets its OWN deterministic id (distinct namespace tag
            // from the mention batch) so a recipient on both is not collapsed.
            let delivery_id = notify_delivery_id(message.id, NotifyBatchKind::Reply);
            if let Err(err) = repo
                .insert_many(room, message.id, Some(sender), &replies, Some(delivery_id))
                .await
            {
                warn!(?err, %room, count = replies.len(), "reply batch persist failed");
            } else {
                let recipients: Vec<NotifyTarget> = replies
                    .into_iter()
                    .map(|(participant, kind)| NotifyTarget { participant, kind })
                    .collect();
                self.publish_room_event(
                    room,
                    &RoomEvent::NotifyBatch {
                        room_id: room,
                        message_id: message.id,
                        by: sender,
                        delivery_id,
                        recipients,
                    },
                )
                .await;
            }
        }
}

    /// Resolve the recipient set for an `@here` broadcast: the room's *online*
    /// members (Slack semantics) when a presence store is wired, else the full
    /// member set. Strictly **fail-open** — `@here` must never drop a
    /// notification because presence is unreachable, so every degraded path
    /// (no presence store, a Redis/lookup error, or an empty online roster)
    /// returns the full `member_set`. Online members not in `member_set` (e.g. a
    /// stale presence entry for someone who has left the room) are intersected
    /// out, so the result is always a subset of the room membership.
    async fn here_recipients(
        &self,
        room: RoomId,
        member_set: &std::collections::BTreeSet<ParticipantId>,
    ) -> std::collections::BTreeSet<ParticipantId> {
        let Some(presence) = self.presence.as_ref() else {
            // Presence not wired: legacy behaviour — notify all members.
            return member_set.clone();
        };
        match presence.members(room).await {
            Ok(online) if !online.is_empty() => {
                let intersected: std::collections::BTreeSet<ParticipantId> =
                    online.into_iter().filter(|p| member_set.contains(p)).collect();
                // An online roster that shares nobody with the room membership is
                // almost certainly a stale/degraded read, not a genuinely empty
                // audience — fail open to all members rather than notify no one.
                if intersected.is_empty() {
                    member_set.clone()
                } else {
                    intersected
                }
            }
            Ok(_) => {
                // Empty online roster (or pruned to nothing): treat as a degraded
                // read and fall back to all members — never silently drop @here.
                member_set.clone()
            }
            Err(err) => {
                warn!(?err, %room, "presence lookup for @here failed; notifying all members");
                member_set.clone()
            }
        }
    }

    /// Flush expired notification bundles and publish NotifyBatch events for
    /// the resulting aggregated notifications. No-op when the bundle store is
    /// not wired. Best-effort: failures are logged, never fatal.
    pub async fn flush_notification_bundles(&self) {
        let Some(bundle_repo) = &self.notification_bundles else { return };
        let result = match bundle_repo.flush().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "notification bundle flush failed");
                return;
            }
        };
        if result.notifications_inserted == 0 {
            return;
        }
        // Group by room and publish one NotifyBatch per room.
        let mut by_room: std::collections::BTreeMap<RoomId, Vec<NotifyTarget>> =
            std::collections::BTreeMap::new();
        for n in &result.inserted {
            by_room
                .entry(n.room)
                .or_default()
                .push(NotifyTarget { participant: n.participant, kind: n.kind });
        }
        for (room, recipients) in &by_room {
            self.publish_room_event(
                *room,
                &RoomEvent::NotifyBatch {
                    room_id: *room,
                    message_id: aero_common::MessageId::new(),
                    by: aero_common::ParticipantId::new(),
                    delivery_id: aero_common::MessageId::new().to_uuid(),
                    recipients: recipients.clone(),
                },
            )
            .await;
        }
    }
}

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
/// and `@here` (the Slack-standard set) are all broadcasts (none is a user-group
/// handle to resolve). Pure, so it is unit-testable without a database. Tokens are
/// already lowercased by [`group_handle_tokens`].
fn is_broadcast_token(token: &str) -> bool {
    is_all_broadcast_token(token) || is_here_token(token)
}

/// The "notify everyone" broadcast tokens: `@channel`, `@everyone`, `@all`. These
/// fan out to every room member regardless of presence. Distinct from
/// [`is_here_token`], which (when presence is wired) narrows to online members.
fn is_all_broadcast_token(token: &str) -> bool {
    matches!(token, "channel" | "everyone" | "all")
}

/// The `@here` broadcast token: notify only *online* room members (Slack
/// semantics) when a presence store is wired, else fall back to all members.
fn is_here_token(token: &str) -> bool {
    token == "here"
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
    fn notify_delivery_id_is_deterministic_per_message_and_kind() {
        let m1 = aero_common::MessageId::new();
        let m2 = aero_common::MessageId::new();

        // Same message + same batch kind => identical id (so a redelivery of the
        // same batch derives the same delivery_id and ON CONFLICT can de-dup).
        assert_eq!(
            notify_delivery_id(m1, NotifyBatchKind::Mention),
            notify_delivery_id(m1, NotifyBatchKind::Mention),
            "deterministic for (message, kind)"
        );
        assert_eq!(
            notify_delivery_id(m1, NotifyBatchKind::Reply),
            notify_delivery_id(m1, NotifyBatchKind::Reply),
        );

        // Mention vs reply of the SAME message must differ — a recipient
        // legitimately on both batches must not be collapsed.
        assert_ne!(
            notify_delivery_id(m1, NotifyBatchKind::Mention),
            notify_delivery_id(m1, NotifyBatchKind::Reply),
            "mention batch and reply batch get distinct ids"
        );

        // Different messages get different ids (same kind).
        assert_ne!(
            notify_delivery_id(m1, NotifyBatchKind::Mention),
            notify_delivery_id(m2, NotifyBatchKind::Mention),
            "distinct messages => distinct ids"
        );

        // Derived ids are non-nil v5 UUIDs.
        let id = notify_delivery_id(m1, NotifyBatchKind::Mention);
        assert_ne!(id, uuid::Uuid::nil());
        assert_eq!(id.get_version(), Some(uuid::Version::Sha1));
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
    fn here_is_distinct_from_channel_class_broadcasts() {
        // `@here` is its own (online-only) flavour, NOT a "notify everyone" token.
        assert!(is_here_token("here"));
        assert!(!is_all_broadcast_token("here"), "@here must NOT fan out to all");

        // `@channel` / `@everyone` / `@all` stay full-fan-out and are NOT `@here`.
        for t in ["channel", "everyone", "all"] {
            assert!(is_all_broadcast_token(t), "{t} fans out to everyone");
            assert!(!is_here_token(t), "{t} is not @here");
        }

        // Every token in both classes is still a broadcast (skipped by group
        // resolution); a non-broadcast handle is neither.
        for t in ["here", "channel", "everyone", "all"] {
            assert!(is_broadcast_token(t));
        }
        for t in ["eng", "ops", "here_team", ""] {
            assert!(!is_here_token(t), "{t} is not @here");
            assert!(!is_all_broadcast_token(t));
        }
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
            crate::service::events::publish_event(
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
    fn env_truthy_accepts_one_and_case_insensitive_true_only() {
        for v in ["1", "true", "True", "TRUE", "tRuE"] {
            assert!(env_truthy(v), "{v:?} should be truthy");
        }
        for v in ["", "0", "false", "False", "yes", "on", "2", "truee", " true"] {
            assert!(!env_truthy(v), "{v:?} should NOT be truthy");
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
