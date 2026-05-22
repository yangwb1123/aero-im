//! Persistence layer — Postgres (sqlx) + Redis (fred).
//!
//! Repository pattern: each entity has a `*Repo` struct holding a `PgPool`,
//! offering high-level methods that other crates call. SQL stays inside this crate.

pub mod ai_job;
pub mod blob;
pub mod blob_store;
pub mod cache;
pub mod call;
pub mod db;
pub mod message;
pub mod mls;
pub mod participant;
pub mod presence;
pub mod reaction;
pub mod receipt;
pub mod room;
pub mod stream;

pub use ai_job::{AiJob, AiJobKind, AiJobRepo, AiJobStatus};
pub use blob::{BlobRepo, NewBlob};
pub use blob_store::{BlobStore, BlobStoreError, LocalFsBlobStore};
pub use cache::{Cache, RedisCache};
pub use call::CallRepo;
pub use db::{connect_pg, migrate, PgPool};
pub use message::{MessageRepo, NewMessage, SearchHit};
pub use mls::{KeyPackageRepo, MlsGroupRepo};
pub use participant::ParticipantRepo;
pub use presence::PresenceStore;
pub use reaction::ReactionRepo;
pub use receipt::ReceiptRepo;
pub use room::RoomRepo;
pub use stream::{NewStream, StreamRepo};
