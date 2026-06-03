//! Persistence layer — Postgres (sqlx) + Redis (fred).
//!
//! Repository pattern: each entity has a `*Repo` struct holding a `PgPool`,
//! offering high-level methods that other crates call. SQL stays inside this crate.

pub mod ai_job;
pub mod audit;
pub mod blob;
pub mod blob_store;
pub mod cache;
pub mod call;
pub mod db;
pub mod live;
pub mod live_presence;
pub mod message;
pub mod mls;
pub mod participant;
pub mod presence;
pub mod reaction;
pub mod receipt;
pub mod room;
pub mod sso;
pub mod stream;
pub mod stream_route;
pub mod workspace;

pub use ai_job::{AiJob, AiJobKind, AiJobRepo, AiJobStatus};
pub use audit::{AuditEvent, AuditRepo};
pub use blob::{BlobRepo, NewBlob};
pub use blob_store::{BlobStore, BlobStoreError, LocalFsBlobStore};
pub use cache::{Cache, RedisCache};
pub use call::CallRepo;
pub use db::{connect_pg, migrate, PgPool};
pub use live::LiveRepo;
pub use live_presence::{CallRosterStore, StreamViewerStore, DEFAULT_TTL as LIVE_PRESENCE_TTL};
pub use message::{MessageRepo, NewMessage, SearchHit};
pub use mls::{KeyPackageRepo, MlsGroupRepo};
pub use participant::ParticipantRepo;
pub use presence::PresenceStore;
pub use reaction::ReactionRepo;
pub use receipt::ReceiptRepo;
pub use room::RoomRepo;
pub use sso::SsoRepo;
pub use stream::{NewStream, StreamRepo};
pub use stream_route::{redirect_base, StreamRouteRegistry, DEFAULT_TTL as STREAM_ROUTE_TTL};
pub use workspace::{
    retention_cutoff, role_can_assign, role_can_invite, role_can_manage_member, role_can_remove,
    validate_retention_days, RoomExport, WorkspaceExport, WorkspaceRepo, EXPORT_MESSAGES_PER_ROOM,
    MIN_RETENTION_DAYS,
};
