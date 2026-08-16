//! Persistence layer — Postgres (sqlx) + Redis (fred).
//!
//! Repository pattern: each entity has a `*Repo` struct holding a `PgPool`,
//! offering high-level methods that other crates call. SQL stays inside this crate.

pub mod aero_vault_blob_store;
pub mod ai_context;
pub mod ai_job;
pub mod ai_usage;
pub mod audit;
pub mod audit_governance;
pub mod audit_relay_provision;
pub mod blob;
pub mod blob_store;
pub mod block_interaction;
pub mod bookmark;
pub mod bookmark_collection;
pub mod bot;
pub mod bot_delivery_outbox;
pub mod cache;
pub mod call;
pub mod call_transcript;
pub mod channel_section;
pub mod consumer_event_receipt;
pub mod db;
#[cfg(test)]
mod deferred_reply_scope_tests;
pub mod delivery_cursor;
pub mod draft;
pub mod emoji;
pub mod event_outbox;
pub mod identity_migration;
pub mod integration;
pub mod invitation;
pub mod live;
pub mod live_presence;
pub mod message;
pub mod message_receipt;
pub mod message_reports;
pub mod message_side_effect;
pub mod mls;
pub mod notification;
pub mod notification_bundle;
pub mod notification_prefs;
pub mod ownership;
pub mod participant;
pub mod pat;
pub mod pin;
pub mod poll;
pub mod presence;
pub mod query_router;
pub mod reaction;
pub mod receipt;
pub mod region_blob_store;
pub mod registration;
pub mod room;
pub mod s3_blob_store;
pub mod saved_search;
pub mod scheduled;
pub mod scheduled_stream;
pub mod scim;
pub mod seq;
pub mod snaplink_commercial;
#[cfg(test)]
mod snaplink_commercial_db_tests;
pub mod sso;
pub mod stream;
pub mod stream_chat_settings;
pub mod stream_go_live_outbox;
pub mod stream_mod;
pub mod stream_route;
pub mod unfurl;
pub mod user_status;
pub mod vod;
pub mod webhook;
pub mod workspace;
// Wave 10 (0033-0038): user groups, channel favorites, profiles, message edit
// history, keyword alerts, workspace announcements.
pub mod announcement;
pub mod channel_favorite;
pub mod keyword_alert;
pub mod message_edit;
pub mod profile;
pub mod user_group;
// Wave 11 (0039-0040 + read-only file index): files tab, stream follow, thread follow.
pub mod file_index;
pub mod stream_follow;
pub mod thread_subscription;
// Wave 12 (0041-0042 + read-only dm/reaction-detail): dm, recurring msgs, reaction detail, default channels.
pub mod default_channel;
pub mod dm;
pub mod reaction_detail;
pub mod recurring_message;
// Wave 13 (0043 + read-only group_dm): group DM, channel join requests.
pub mod group_dm;
pub mod join_request;
// Wave 14 (0044-0046): TOTP 2FA secrets, workspace deactivation, message templates.
pub mod deactivation;
pub mod message_template;
pub mod totp;
// Wave 15 (0047 + read-only search/role): revoked tokens, search operators, channel roles.
pub mod revoked_token;
pub mod room_role;
pub mod search_feedback;
pub mod search_query;

// ---- Wave 16 ----
pub mod analytics;
pub mod canvas;
pub mod canvas_op;
pub mod channel_bookmark;
pub mod creator_subscription;
pub mod directory;
pub mod stream_category;

// ---- Wave 17 ----
pub mod approval;
pub mod legal_hold;
pub mod org_chart;
pub mod out_of_office;
pub mod task;
pub mod workspace_files;

// ---- Wave 18 ----
pub mod clip;
pub mod clip_collections;
pub mod stream_stats;

// ---- Wave 19 ----
pub mod info_barrier;

// ---- Wave 21 ----
pub mod activity_feed;
pub mod auth_session;
pub mod login_event;
// ROADMAP5 方向五 — persistent failed-login trail (migration 0140). Distinct
// from `login_event` (successes only): catches slow credential-stuffing that
// stays under the in-process LoginThrottle and survives restarts / spans nodes.
pub mod login_failure;

// ---- Wave 22 (Round 8) ----
pub mod credential_rotation;
pub mod password_reset;

pub use aero_vault_blob_store::{AeroVaultAuth, AeroVaultBlobStore, AeroVaultConfig};
pub use ai_context::{AiContextStore, MAX_TURNS as AI_CONTEXT_MAX_TURNS};
pub use ai_job::{AiJob, AiJobKind, AiJobRepo, AiJobStatus};
pub use ai_usage::{
    ai_usage_backoff, AiUsageOutboxClaim, AiUsageRepo, UsageByKind, UsageCharge,
    UsageFinalizeOutcome, UsageOutcome, UsageReservation, UsageReserveOutcome, UsageRow,
};
pub use audit::{events_to_csv, AuditEvent, AuditRepo, AUDIT_CSV_HEADER};
pub use audit_relay_provision::{AuditRelayProvisionRepo, ProvisionCheck};
pub use blob::{BlobRepo, BlobStorageScope, NewBlob};
pub use blob_store::{BlobStore, BlobStoreError, LocalFsBlobStore};
pub use block_interaction::{BlockInteraction, BlockInteractionRepo};
pub use bookmark::{BookmarkRepo, SavedMessage};
pub use bookmark_collection::{BookmarkCollection, BookmarkCollectionRepo};
pub use bot::{
    Bot, BotDelivery, BotEventSubscription, BotRepo, BotWriteError, DeliveryStatus,
    MatchedSubscription, MAX_BOTS_PER_OWNER, MAX_BOTS_PER_WORKSPACE_PAGE, MAX_BOT_EVENT_CANDIDATES,
    MAX_BOT_EXTERNAL_SUBSCRIPTIONS_PER_WORKSPACE, MAX_BOT_SUBSCRIPTIONS_PER_BOT,
};
pub use bot_delivery_outbox::{
    bot_delivery_backoff, BotDeliveryOutbox, BotDeliveryOutboxRepo, BotDeliveryRetentionSweep,
    BOT_DELIVERY_MAX_ATTEMPTS,
};
pub use cache::{Cache, RedisCache};
pub use call::CallRepo;
pub use call_transcript::{CallTranscriptRepo, TranscriptLine};
pub use channel_section::{ChannelSection, ChannelSectionRepo};
pub use consumer_event_receipt::{ConsumerEventClaim, ConsumerEventReceiptRepo};
pub use db::{connect_pg, migrate, PgPool};
pub use draft::{Draft, DraftRepo};
pub use emoji::{is_valid_emoji_name, CustomEmoji, EmojiRepo};
pub use event_outbox::{outbox_backoff_delay, EventOutboxRepo, EventOutboxRow};
pub use identity_migration::{
    ExternalIdentityKey, IdentityMigrationOutcome, IdentityMigrationRepo, IdentityMigrationRequest,
    IDENTITY_MIGRATED_AUDIT_ACTION, IDENTITY_MIGRATED_REASON,
};
pub use region_blob_store::{PersistedRegionBlobStore, RegionRouter, DEFAULT_STORAGE_REGION};
pub use s3_blob_store::{blob_store_from_env, blob_store_from_env_checked, S3BlobStore, S3Config};
// `generate_token`/`hash_token` are NOT re-exported at the crate root (the webhook
// module exports same-named helpers); invitation consumers use the
// `aero_storage::invitation::` path.
pub use delivery_cursor::{DeliveryCursor, DeliveryCursorRepo};
pub use integration::{
    IntegrationInstallation, IntegrationPublishOutcome, IntegrationReplayProbe, IntegrationRepo,
    IntegrationTarget, NewIntegrationInstallation, NewIntegrationNotification,
    ResolvedIntegrationTarget, UpdateIntegrationInstallation, MAX_INTEGRATIONS_PER_WORKSPACE,
    MAX_INTEGRATION_ROOMS,
};
pub use invitation::{
    invitation_is_redeemable, Invitation, InvitationAcceptError, InvitationAcceptance,
    InvitationRepo,
};
pub use live::LiveRepo;
pub use live_presence::{CallRosterStore, StreamViewerStore, DEFAULT_TTL as LIVE_PRESENCE_TTL};
pub use message::events::{OutboxedMessageDelete, OutboxedMessageEdit};
pub use message::{
    MessageIdempotency, MessageInsertOutcome, MessageRepo, NewMessage, OutboxedMessageInsert,
    SearchHit,
};
pub use message_receipt::{MessageReader, MessageReceiptRepo};
pub use message_reports::{MessageReport, MessageReportRepo, MessageReportReview};
pub use message_side_effect::{MessageSideEffectJob, MessageSideEffectKind, MessageSideEffectRepo};
pub use mls::{KeyPackageRepo, MlsGroupRepo};
pub use notification::NotificationRepo;
pub use notification_bundle::{FlushResult, InsertedNotification, NotificationBundleRepo};
pub use notification_prefs::NotificationPrefsRepo;
pub use ownership::{
    is_channel_effective_owner_violation, is_workspace_owner_violation,
    CHANNEL_EFFECTIVE_OWNER_CONSTRAINT, WORKSPACE_OWNER_CONSTRAINT,
};
pub use participant::{ParticipantDeleteError, ParticipantRepo};
pub use pat::{PatRepo, PatSummary};
pub use pin::PinRepo;
pub use poll::{PollRepo, VoteError as PollVoteError};
pub use presence::PresenceStore;
pub use query_router::{QueryConsistency, QueryRouter, QueryTarget};
pub use reaction::ReactionRepo;
pub use receipt::ReceiptRepo;
pub use room::{
    ChannelMetaPatch, RoomMemberRole, RoomMembershipWriteError, RoomRepo,
    MAX_CHANNEL_DESCRIPTION_CHARS, MAX_CHANNEL_TOPIC_CHARS,
};
pub use saved_search::{MonitoredSearch, SavedSearch, SavedSearchMonitorUpdate, SavedSearchRepo};
// SCIM token helpers (`generate_token`/`hash_token`) are intentionally NOT
// re-exported at the crate root: the webhook module exports same-named helpers,
// so SCIM consumers reach these via the `aero_storage::scim::` path instead.
pub use scheduled::{ScheduledMessage, ScheduledRepo};
pub use scheduled_stream::{
    ScheduledStream, ScheduledStreamRepo, ScheduledStreamWriteError,
    MAX_SCHEDULED_STREAMS_PER_CREATOR, MAX_SCHEDULED_STREAM_DESCRIPTION_CHARS,
    MAX_SCHEDULED_STREAM_TITLE_CHARS, MAX_UPCOMING_SCHEDULED_STREAMS_PAGE,
};
pub use scim::{
    ScimRepo, ScimTokenRecord, ScimTokenWriteError, ScimUserRow, ScimUserWriteError,
    MAX_SCIM_TOKENS_PER_WORKSPACE,
};
pub use seq::SeqStore;
pub use snaplink_commercial::{
    commercial_delivery_backoff, ProjectionOutcome, SnaplinkBindingSpec, SnaplinkCommercialRepo,
    SnaplinkDeliveryClaim, SnaplinkDeliveryDestination, SnaplinkEntitlementProjection,
    SnaplinkLimitProjection,
};
pub use sso::{SsoRepo, SsoResolveError};
pub use stream::{NewStream, StreamRepo, StreamWriteError};
pub use stream_chat_settings::{slow_mode_violation, StreamChatSettings, StreamChatSettingsRepo};
pub use stream_go_live_outbox::{
    GoLiveTransition, MarkLiveOutcome, StreamGoLiveOutboxRepo, StreamGoLiveOutboxRow,
};
pub use stream_mod::{ban_active, StreamBan, StreamModRepo};
pub use stream_route::{redirect_base, StreamRouteRegistry, DEFAULT_TTL as STREAM_ROUTE_TTL};
pub use unfurl::{UnfurlRepo, Unfurler};
pub use user_status::UserStatusRepo;
pub use vod::{playback_url, VodRepo};
pub use webhook::{
    build_delivery, event_matches, generate_secret, generate_token, hash_token, outcome_of,
    parse_retry_after, sign_payload, BreakerState, Delivery, DeliveryOutcome, DeliveryResponse,
    FakeSender, IncomingHook, IncomingHookSummary, OutgoingHookSummary, OutgoingTarget,
    ReqwestSender, WebhookRepo, WebhookSender, BREAKER_BASE_COOLDOWN_SECS,
    BREAKER_FAILURE_THRESHOLD, BREAKER_MAX_COOLDOWN_SECS, BREAKER_RATE_LIMIT_COOLDOWN_SECS,
    SIGNATURE_HEADER, TIMESTAMP_HEADER,
};
pub use workspace::{
    retention_cutoff, role_can_assign, role_can_invite, role_can_manage_member, role_can_remove,
    validate_retention_days, GuestMembershipWriteError, RoomExport, WorkspaceExport,
    WorkspaceMemberWriteError, WorkspaceRepo, EXPORT_MESSAGES_PER_ROOM, MIN_RETENTION_DAYS,
};
// Wave 10 re-exports.
pub use announcement::{is_active, Announcement, AnnouncementRepo};
pub use channel_favorite::ChannelFavoriteRepo;
pub use keyword_alert::{normalize_keyword, KeywordAlert, KeywordAlertRepo};
pub use message_edit::{MessageEdit, MessageEditRepo};
pub use profile::{Profile, ProfileRepo};
pub use user_group::{normalize_handle, UserGroup, UserGroupRepo};
// Wave 11 re-exports.
pub use file_index::{FileIndexRepo, SharedFile};
pub use stream_follow::StreamFollowRepo;
pub use thread_subscription::ThreadSubscriptionRepo;
// Wave 12 re-exports.
pub use default_channel::{DefaultChannelRepo, DefaultChannelWriteError};
pub use dm::{DmRepo, DmWriteError};
pub use reaction_detail::{EmojiReactors, ReactionDetailRepo};
pub use recurring_message::{next_occurrence, RecurringMessage, RecurringMessageRepo};
// Wave 13 re-exports.
pub use group_dm::{GroupDmRepo, GroupDmWriteError, MAX_GROUP_DM_NAME_CHARS};
pub use join_request::{JoinRequest, JoinRequestRepo, JoinRequestWriteError};
// Wave 14 re-exports.
pub use deactivation::{DeactivatedMember, DeactivationRepo, DeactivationWriteError};
pub use message_template::{MessageTemplate, MessageTemplateRepo};
pub use totp::{RecoveryCodeRepo, TotpRepo, TotpWriteError};
// Wave 15 re-exports. (revoked_token::hash_token is NOT re-exported at the crate
// root — it collides with scim/webhook; reach it via aero_storage::revoked_token::.)
pub use revoked_token::RevokedTokenRepo;
pub use room_role::RoomRoleRepo;
pub use search_feedback::{CtrStats, SearchFeedbackRepo};
pub use search_query::{
    parse_search_query, AdvancedSearchRepo, FacetCount, ParsedQuery, SearchCursor, SearchFacets,
};

// ---- Wave 16 re-exports ----
pub use analytics::{
    AnalyticsRepo, ChannelAnalytics, ChannelMessageCount, DayCount, ReactionStat,
    WorkspaceAnalytics, WorkspaceSummary, MAX_TIMELINE_DAYS, MAX_TOP_CHANNELS,
};
pub use canvas::{Canvas, CanvasRepo};
pub use canvas_op::{CanvasOp, CanvasOpAppend, CanvasOpRepo};
pub use channel_bookmark::{ChannelBookmark, ChannelBookmarkRepo};
pub use creator_subscription::{CreatorSubscription, CreatorTier, SubscriptionRepo};
pub use directory::{DirectoryEntry, DirectoryRepo};
pub use stream_category::{StreamCategory, StreamCategoryRepo};

// ---- Wave 17 re-exports ----
pub use approval::{Approval, ApprovalRepo, ApprovalWriteError};
pub use legal_hold::{LegalHold, LegalHoldRepo};
pub use org_chart::{OrgChartRepo, OrgChartWriteError, DEFAULT_CHAIN_DEPTH, MAX_CHAIN_DEPTH};
pub use out_of_office::{within_window, OutOfOffice, OutOfOfficeRepo};
pub use task::{validate_status, Task, TaskRepo, TaskWriteError};
pub use workspace_files::{WorkspaceFile, WorkspaceFileRepo};

// ---- Wave 18 re-exports ----
pub use clip::{Clip, ClipRepo};
pub use clip_collections::{ClipCollectionRepo, CollectionRow};
pub use stream_stats::{StreamAnalytics, StreamStatsRepo};

// ---- Wave 19 re-exports ----
pub use info_barrier::{BarrierRepo, InfoBarrier};

// ---- Wave 21 re-exports ----
pub use activity_feed::{ActivityEntry, ActivityFeedRepo};
pub use auth_session::{AuthSession, RefreshRotation, SessionRepo};
pub use credential_rotation::{ChangePasswordResult, CredentialRotationRepo, ResetPasswordResult};
pub use login_event::{LoginEvent, LoginEventRepo};
pub use login_failure::{LoginFailure, LoginFailureRepo};
pub use password_reset::{
    generate_token as generate_reset_token, hash_reset_token, PasswordResetRepo,
    DEFAULT_TTL as RESET_TOKEN_TTL,
};
pub use registration::{NewRegistration, NewRegistrationAudit, RegistrationRepo};

// ---- ROADMAP 方向二 — mobile push ----
pub mod push_token;
pub use push_token::{
    PushPlatform, PushToken, PushTokenRegistrationError, PushTokenRepo,
    MAX_PUSH_TOKENS_PER_PARTICIPANT, MAX_PUSH_TOKEN_BYTES,
};

// ---- ROADMAP 方向四 — GDPR blob GC ----
pub mod blob_gc;
pub use blob_gc::{BlobGcItem, BlobGcRepo};

// ---- ROADMAP 方向四 — GDPR async full export ----
pub mod export_job;
pub use export_job::{link_is_valid, ExportJob, ExportJobRepo, EXPORT_LINK_TTL};

// ---- ROADMAP 方向五 — password reuse history ----
pub mod password_history;
pub use password_history::{PasswordHistoryRepo, HISTORY_DEPTH as PASSWORD_HISTORY_DEPTH};

// ---- Concurrent-viewer history sampling (peak/avg concurrent viewers) ----
pub mod stream_viewer_sample;
pub use stream_viewer_sample::{
    RetentionPoint, RollupOutcome, StreamViewerSampleRepo, ViewerStats,
};

// ---- Workspace IP / network allowlist (authorized networks) ----
pub mod ip_allowlist;
pub use ip_allowlist::{ip_in_cidr, is_allowed, IpAllowEntry, IpAllowlistRepo};

// ---- Per-workspace rate ceiling: Redis fixed-window counter (租户公平) ----
pub mod ws_rate;
pub use ws_rate::WsRateStore;

// ---- ROADMAP3 方向二 — cross-node group-call routing ----
pub mod call_route;
pub use call_route::{CallRouteRegistry, DEFAULT_TTL as CALL_ROUTE_TTL};

// ---- Interactive-live / creator parity (migrations 0079-0083) ----
// Hype train / combo gifts, raids, VOD chapters, stream-moderator role assignment.
// (Subscriber-badge for stream chat lives on the existing `live` module: 0082.)
pub mod hype_train;
pub mod raid;
pub mod stream_moderator;
pub mod vod_chapter;
pub use hype_train::{
    apply_contribution, is_expired as hype_train_is_expired, Escalation, HypeTrainLeaderRow,
    HypeTrainRepo, HypeTrainSession, MAX_LEVEL as HYPE_TRAIN_MAX_LEVEL,
    UNITS_PER_LEVEL as HYPE_TRAIN_UNITS_PER_LEVEL, WINDOW_SECS as HYPE_TRAIN_WINDOW_SECS,
};
pub use raid::{Raid, RaidAnalytics, RaidRepo};
pub use stream_moderator::{StreamModerator, StreamModeratorRepo};
pub use vod_chapter::{VodChapter, VodChapterRepo};

// ---- AI-native: scheduled / recurring AI digest subscriptions (0084) ----
pub mod digest_subscription;
pub use digest_subscription::{
    next_run_at as digest_next_run_at, validate_frequency as validate_digest_frequency,
    DigestSubscription, DigestSubscriptionRepo, DigestTarget, FREQ_DAILY, FREQ_WEEKLY,
};

// ---- Operability: webhook delivery retry/DLQ + per-tenant usage reports ----
pub mod webhook_delivery;
pub use webhook_delivery::{
    backoff_delay, is_dead_at, next_attempt_at, WebhookDelivery, WebhookDeliveryRepo,
    MAX_ATTEMPTS as WEBHOOK_MAX_ATTEMPTS,
};
pub mod usage_report;
pub use usage_report::{AiKindUsage, UsageReport, UsageReportRepo};

// ---- Live / creator economy (migrations 0089-0091) ----
// Channel points + custom-reward redemption, goal/bounty bars, ban/timeout appeals.
pub mod channel_points;
pub use channel_points::{
    is_resolution_status, ChannelPointsRepo, EarnHistoryRow, RedeemError, Redemption, Reward,
};
pub mod goals;
pub use goals::{
    is_valid_metric as is_valid_goal_metric, Goal, GoalCreateError, GoalRepo,
    MAX_ACTIVE_GOALS_PER_STREAM, MAX_GOAL_DESCRIPTION_CHARS, MAX_GOAL_TITLE_CHARS,
};
pub mod ban_appeals;
pub use ban_appeals::{AppealError, BanAppeal, BanAppealRepo};

// ---- Community predictions / channel betting (migration 0092) ----
// Creator opens a prediction (2+ outcomes); viewers STAKE channel points on one;
// the creator LOCKS then RESOLVES; winners are paid proportionally from the pool.
pub mod predictions;
pub use predictions::{
    Prediction, PredictionAnalytics, PredictionOutcome, PredictionRepo, ResolveError, StakeError,
    ViewerPredictionRow,
};

// ---- ROADMAP6 Lane A — thread notification prefs + workspace mutes ----
pub mod thread_notification_prefs;
pub mod workspace_mutes;
pub use thread_notification_prefs::ThreadNotificationPrefsRepo;
pub use workspace_mutes::WorkspaceMuteRepo;

// (ClipCollectionRepo + CollectionRow already re-exported above in Wave 18 re-exports)

// ---- ROADMAP7 Lane A — channel topic history + thread read state ----
pub mod thread_read_state;
pub mod topic_history;
pub use thread_read_state::ThreadReadStateRepo;
pub use topic_history::TopicHistoryRepo;

// ---- User-level blocking / ignoring (migration 0106) ----
pub mod user_blocks;
pub use user_blocks::BlockRepo;

// ---- ROADMAP9 — extended subscription tier levels (migration 0109) ----
pub mod subscription_tier;
pub use subscription_tier::{SubscriptionTier, SubscriptionTierRepo};

// ---- ROADMAP10 — auto-mod rules engine (migration 0111) ----
pub mod auto_mod;
pub use auto_mod::{AutoModRule, AutoModRuleRepo};

// ---- ROADMAP10 — user-level report flow (migration 0112) ----
pub mod user_report;
pub use user_report::{UserReport, UserReportRepo};

// ---- ROADMAP11 Feature 2 — workspace custom emoji (migration 0115) ----
pub mod workspace_emoji;
pub use workspace_emoji::{WorkspaceEmojiRepo, WorkspaceEmojiRow};

// ---- ROADMAP12 — workspace notification defaults (migration 0119) ----
pub mod workspace_notif_defaults;
pub use workspace_notif_defaults::WorkspaceNotifDefaultsRepo;

// ---- Per-user thread MUTES — inverse of thread-follow (migration 0088) ----
pub mod thread_mute;
pub use thread_mute::ThreadMuteRepo;

// ---- Persistent cross-room AI user profile (持久跨房 AI 用户画像, migration 0145) ----
// PRIVACY-SENSITIVE: opt-in (off by default), GDPR-erasable (DELETEd explicitly
// from `delete_participant`), transparent (plain readable fields). See the module
// docs for the full privacy posture.
pub mod ai_profile;
pub use ai_profile::{AiProfile, AiProfileRepo};
