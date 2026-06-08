//! Persistence layer — Postgres (sqlx) + Redis (fred).
//!
//! Repository pattern: each entity has a `*Repo` struct holding a `PgPool`,
//! offering high-level methods that other crates call. SQL stays inside this crate.

pub mod ai_job;
pub mod audit;
pub mod blob;
pub mod blob_store;
pub mod bookmark;
pub mod cache;
pub mod channel_section;
pub mod call;
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
pub mod reaction;
pub mod receipt;
pub mod room;
pub mod saved_search;
pub mod scim;
pub mod sso;
pub mod stream;
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

pub use ai_job::{AiJob, AiJobKind, AiJobRepo, AiJobStatus};
pub use audit::{AuditEvent, AuditRepo};
pub use blob::{BlobRepo, NewBlob};
pub use blob_store::{BlobStore, BlobStoreError, LocalFsBlobStore};
pub use bookmark::{BookmarkRepo, SavedMessage};
pub use cache::{Cache, RedisCache};
pub use call::CallRepo;
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
pub use receipt::ReceiptRepo;
pub use room::RoomRepo;
pub use saved_search::{SavedSearch, SavedSearchRepo};
// SCIM token helpers (`generate_token`/`hash_token`) are intentionally NOT
// re-exported at the crate root: the webhook module exports same-named helpers,
// so SCIM consumers reach these via the `aero_storage::scim::` path instead.
pub use scim::{ScimRepo, ScimUserRow};
pub use sso::SsoRepo;
pub use stream::{NewStream, StreamRepo};
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
