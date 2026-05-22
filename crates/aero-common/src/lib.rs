//! Shared contracts for the Aero workspace.
//!
//! This crate has **no business logic**. It defines:
//! - Strongly-typed IDs (`ParticipantId`, `RoomId`, `MessageId`, `BlobId`)
//! - The `Block`/`Message`/`Room`/`Participant` data model (mirrors DB schema)
//! - Cross-cutting `Error` and `Result` aliases
//! - Configuration loading (`AppConfig`)
//! - Tracing/OTel initialization
//!
//! Other crates depend on this one and re-export domain-specific types as needed.

pub mod config;
pub mod error;
pub mod ids;
pub mod model;
pub mod telemetry;
pub mod time;

pub use error::{Error, Result};
pub use ids::{BlobId, MessageId, ParticipantId, RoomId};
pub use model::{
    Block, FileKind, Message, MessageEnvelope, Participant, ParticipantKind, Room, RoomKind, Span,
    SpanStyle,
};
