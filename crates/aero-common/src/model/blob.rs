//! Core domain model — mirrors the SQL schema in `migrations/`.
//!
//! These types are wire-format AND storage-format. The `Block` enum is intentionally
//! the same shape as Slack Block Kit / Discord Embeds / LLM tool-call payloads,
//! so messages can flow through the system without lossy transformations.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{BlobId, ParticipantId, RoomId};
use super::block::FileKind;
use super::message::Message;

// ---------- Pinned messages ----------

/// Whether a pin event pinned or unpinned a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PinOp {
    Pin,
    Unpin,
}

/// A pinned message record (provenance + the message itself when listed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinnedMessage {
    pub room_id: RoomId,
    /// The pinned message (joined in on listing).
    pub message: Message,
    pub pinned_by: ParticipantId,
    #[serde(with = "time::serde::rfc3339")]
    pub pinned_at: OffsetDateTime,
}

// ---------- Blobs (P2) ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blob {
    pub id: BlobId,
    pub owner_id: ParticipantId,
    pub kind: FileKind,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub sha256: Option<String>,
    pub storage_key: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub finalized_at: Option<OffsetDateTime>,
}
