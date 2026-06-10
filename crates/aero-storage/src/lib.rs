//! Persistence layer — Postgres (sqlx) + Redis (fred).
//!
//! Repository pattern: each entity has a `*Repo` struct holding a `PgPool`,
//! offering high-level methods that other crates call. SQL stays inside this crate.

pub mod ai_context;
pub mod ai_job;
pub mod audit;
pub mod blob;
pub mod blob_store;
pub mod s3_blob_store;
pub mod bookmark;
pub mod bookmark_collection;
pub mod cache;
pub mod channel_section;
pub mod call;
pub mod call_transcript;
pub mod db;
pub mod draft;
pub mod emoji;
pub mod invitation;
pub mod live;
pub mod live_presence;
pub mod message;
pub mod mls;
pub mod notification;
pub mod notification_prefs;
pub mod participant;
pub mod pat;
pub mod pin;
pub mod poll;
pub mod presence;
pub mod message_receipt;
pub mod reaction;
pub mod receipt;
pub mod room;
pub mod saved_search;
pub mod scim;
pub mod seq;
pub mod sso;
pub mod stream;
pub mod stream_chat_settings;
pub mod stream_mod;
pub mod stream_route;
pub mod unfurl;
pub mod user_status;
pub mod vod;
pub mod webhook;
pub mod workspace;
pub mod scheduled;
pub mod scheduled_stream;
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
pub mod dm;
pub mod recurring_message;
pub mod reaction_detail;
pub mod default_channel;
// Wave 13 (0043 + read-only group_dm): group DM, channel join requests.
pub mod group_dm;
pub mod join_request;
// Wave 14 (0044-0046): TOTP 2FA secrets, workspace deactivation, message templates.
pub mod totp;
pub mod deactivation;
pub mod message_template;
// Wave 15 (0047 + read-only search/role): revoked tokens, search operators, channel roles.
pub mod revoked_token;
pub mod search_query;
pub mod room_role;

// ---- Wave 16 ----
pub mod analytics;
pub mod canvas;
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
pub mod stream_stats;

// ---- Wave 19 ----
pub mod info_barrier;

// ---- Wave 21 ----
pub mod activity_feed;
pub mod auth_session;

// ---- Wave 22 (Round 8) ----
pub mod password_reset;

pub use ai_context::{AiContextStore, MAX_TURNS as AI_CONTEXT_MAX_TURNS};
pub use ai_job::{AiJob, AiJobKind, AiJobRepo, AiJobStatus};
pub use audit::{events_to_csv, AuditEvent, AuditRepo, AUDIT_CSV_HEADER};
pub use blob::{BlobRepo, NewBlob};
pub use blob_store::{BlobStore, BlobStoreError, LocalFsBlobStore};
pub use s3_blob_store::{blob_store_from_env, blob_store_from_env_checked, S3BlobStore, S3Config};
pub use bookmark::{BookmarkRepo, SavedMessage};
pub use bookmark_collection::{BookmarkCollection, BookmarkCollectionRepo};
pub use cache::{Cache, RedisCache};
pub use call::CallRepo;
pub use call_transcript::{CallTranscriptRepo, TranscriptLine};
pub use channel_section::{ChannelSection, ChannelSectionRepo};
pub use db::{connect_pg, migrate, PgPool};
pub use draft::{Draft, DraftRepo};
pub use emoji::{is_valid_emoji_name, CustomEmoji, EmojiRepo};
// `generate_token`/`hash_token` are NOT re-exported at the crate root (the webhook
// module exports same-named helpers); invitation consumers use the
// `aero_storage::invitation::` path.
pub use invitation::{invitation_is_redeemable, Invitation, InvitationRepo};
pub use live::LiveRepo;
pub use live_presence::{CallRosterStore, StreamViewerStore, DEFAULT_TTL as LIVE_PRESENCE_TTL};
pub use message::{MessageRepo, NewMessage, SearchHit};
pub use mls::{KeyPackageRepo, MlsGroupRepo};
pub use notification::NotificationRepo;
pub use notification_prefs::NotificationPrefsRepo;
pub use participant::ParticipantRepo;
pub use pat::{PatRepo, PatSummary};
pub use pin::PinRepo;
pub use poll::{PollRepo, VoteError as PollVoteError};
pub use presence::PresenceStore;
pub use reaction::ReactionRepo;
pub use message_receipt::{MessageReader, MessageReceiptRepo};
pub use receipt::ReceiptRepo;
pub use room::RoomRepo;
pub use saved_search::{SavedSearch, SavedSearchRepo};
// SCIM token helpers (`generate_token`/`hash_token`) are intentionally NOT
// re-exported at the crate root: the webhook module exports same-named helpers,
// so SCIM consumers reach these via the `aero_storage::scim::` path instead.
pub use scim::{ScimRepo, ScimUserRow};
pub use seq::SeqStore;
pub use sso::SsoRepo;
pub use stream::{NewStream, StreamRepo};
pub use stream_chat_settings::{slow_mode_violation, StreamChatSettings, StreamChatSettingsRepo};
pub use stream_mod::{ban_active, StreamBan, StreamModRepo};
pub use stream_route::{redirect_base, StreamRouteRegistry, DEFAULT_TTL as STREAM_ROUTE_TTL};
pub use unfurl::{UnfurlRepo, Unfurler};
pub use user_status::UserStatusRepo;
pub use vod::{playback_url, VodRepo};
pub use webhook::{
    build_delivery, event_matches, generate_secret, generate_token, hash_token, sign_payload,
    Delivery, FakeSender, IncomingHook, IncomingHookSummary, OutgoingHookSummary, OutgoingTarget,
    ReqwestSender, WebhookRepo, WebhookSender, SIGNATURE_HEADER, TIMESTAMP_HEADER,
};
pub use workspace::{
    retention_cutoff, role_can_assign, role_can_invite, role_can_manage_member, role_can_remove,
    validate_retention_days, RoomExport, WorkspaceExport, WorkspaceRepo, EXPORT_MESSAGES_PER_ROOM,
    MIN_RETENTION_DAYS,
};
pub use scheduled::{ScheduledMessage, ScheduledRepo};
pub use scheduled_stream::{ScheduledStream, ScheduledStreamRepo};
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
pub use dm::DmRepo;
pub use recurring_message::{next_occurrence, RecurringMessage, RecurringMessageRepo};
pub use reaction_detail::{EmojiReactors, ReactionDetailRepo};
pub use default_channel::DefaultChannelRepo;
// Wave 13 re-exports.
pub use group_dm::GroupDmRepo;
pub use join_request::{JoinRequest, JoinRequestRepo};
// Wave 14 re-exports.
pub use totp::TotpRepo;
pub use deactivation::{DeactivatedMember, DeactivationRepo};
pub use message_template::{MessageTemplate, MessageTemplateRepo};
// Wave 15 re-exports. (revoked_token::hash_token is NOT re-exported at the crate
// root — it collides with scim/webhook; reach it via aero_storage::revoked_token::.)
pub use revoked_token::RevokedTokenRepo;
pub use search_query::{parse_search_query, AdvancedSearchRepo, ParsedQuery};
pub use room_role::RoomRoleRepo;

// ---- Wave 16 re-exports ----
pub use analytics::{
    AnalyticsRepo, ChannelMessageCount, DayCount, WorkspaceAnalytics, MAX_TIMELINE_DAYS,
    MAX_TOP_CHANNELS,
};
pub use canvas::{Canvas, CanvasRepo};
pub use channel_bookmark::{ChannelBookmark, ChannelBookmarkRepo};
pub use creator_subscription::{CreatorSubscription, CreatorTier, SubscriptionRepo};
pub use directory::{DirectoryEntry, DirectoryRepo};
pub use stream_category::{StreamCategory, StreamCategoryRepo};

// ---- Wave 17 re-exports ----
pub use approval::{Approval, ApprovalRepo};
pub use legal_hold::{LegalHold, LegalHoldRepo};
pub use org_chart::{OrgChartRepo, DEFAULT_CHAIN_DEPTH, MAX_CHAIN_DEPTH};
pub use out_of_office::{within_window, OutOfOffice, OutOfOfficeRepo};
pub use task::{validate_status, Task, TaskRepo};
pub use workspace_files::{WorkspaceFile, WorkspaceFileRepo};

// ---- Wave 18 re-exports ----
pub use clip::{Clip, ClipRepo};
pub use stream_stats::{StreamAnalytics, StreamStatsRepo};

// ---- Wave 19 re-exports ----
pub use info_barrier::{BarrierRepo, InfoBarrier};

// ---- Wave 21 re-exports ----
pub use activity_feed::{ActivityEntry, ActivityFeedRepo};
pub use auth_session::{AuthSession, SessionRepo};
pub use password_reset::{
    generate_token as generate_reset_token, hash_reset_token, PasswordResetRepo,
    DEFAULT_TTL as RESET_TOKEN_TTL,
};

// ---- ROADMAP 方向二 — mobile push ----
pub mod push_token;
pub use push_token::{PushPlatform, PushToken, PushTokenRepo};

// ---- ROADMAP 方向四 — GDPR blob GC ----
pub mod blob_gc;
pub use blob_gc::BlobGcRepo;

// ---- ROADMAP 方向四 — GDPR async full export ----
pub mod export_job;
pub use export_job::{link_is_valid, ExportJob, ExportJobRepo, EXPORT_LINK_TTL};

// ---- ROADMAP 方向五 — password reuse history ----
pub mod password_history;
pub use password_history::{PasswordHistoryRepo, HISTORY_DEPTH as PASSWORD_HISTORY_DEPTH};

// ---- Concurrent-viewer history sampling (peak/avg concurrent viewers) ----
pub mod stream_viewer_sample;
pub use stream_viewer_sample::{StreamViewerSampleRepo, ViewerStats};

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
    apply_contribution, is_expired as hype_train_is_expired, HypeTrainRepo, HypeTrainSession,
    Escalation, MAX_LEVEL as HYPE_TRAIN_MAX_LEVEL, UNITS_PER_LEVEL as HYPE_TRAIN_UNITS_PER_LEVEL,
    WINDOW_SECS as HYPE_TRAIN_WINDOW_SECS,
};
pub use raid::{Raid, RaidRepo};
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
