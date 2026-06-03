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
pub mod call;
pub mod db;
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
pub mod presence;
pub mod reaction;
pub mod receipt;
pub mod room;
pub mod scim;
pub mod sso;
pub mod stream;
pub mod stream_route;
pub mod user_status;
pub mod webhook;
pub mod workspace;
pub mod scheduled;

pub use ai_job::{AiJob, AiJobKind, AiJobRepo, AiJobStatus};
pub use audit::{AuditEvent, AuditRepo};
pub use blob::{BlobRepo, NewBlob};
pub use blob_store::{BlobStore, BlobStoreError, LocalFsBlobStore};
pub use bookmark::{BookmarkRepo, SavedMessage};
pub use cache::{Cache, RedisCache};
pub use call::CallRepo;
pub use db::{connect_pg, migrate, PgPool};
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
pub use presence::PresenceStore;
pub use reaction::ReactionRepo;
pub use receipt::ReceiptRepo;
pub use room::RoomRepo;
// SCIM token helpers (`generate_token`/`hash_token`) are intentionally NOT
// re-exported at the crate root: the webhook module exports same-named helpers,
// so SCIM consumers reach these via the `aero_storage::scim::` path instead.
pub use scim::{ScimRepo, ScimUserRow};
pub use sso::SsoRepo;
pub use stream::{NewStream, StreamRepo};
pub use stream_route::{redirect_base, StreamRouteRegistry, DEFAULT_TTL as STREAM_ROUTE_TTL};
pub use user_status::UserStatusRepo;
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
