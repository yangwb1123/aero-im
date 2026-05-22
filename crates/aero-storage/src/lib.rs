//! Persistence layer — Postgres (sqlx) + Redis (fred).
//!
//! Repository pattern: each entity has a `*Repo` struct holding a `PgPool`,
//! offering high-level methods that other crates call. SQL stays inside this crate.

pub mod cache;
pub mod db;
pub mod participant;
pub mod room;
pub mod message;
pub mod presence;

pub use cache::{Cache, RedisCache};
pub use db::{connect_pg, migrate, PgPool};
pub use message::MessageRepo;
pub use participant::ParticipantRepo;
pub use presence::PresenceStore;
pub use room::RoomRepo;
