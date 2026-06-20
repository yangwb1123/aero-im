//! Core domain model — mirrors the SQL schema in `migrations/`.
//!
//! These types are wire-format AND storage-format. The `Block` enum is intentionally
//! the same shape as Slack Block Kit / Discord Embeds / LLM tool-call payloads,
//! so messages can flow through the system without lossy transformations.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{BlobId, MessageId, NotificationId, ParticipantId, PollId, RoomId};
use super::block::Block;

// ---------- Message ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub room_id: RoomId,
    pub sender_id: ParticipantId,
    pub blocks: Vec<Block>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<MessageId>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub edited_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub deleted_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
}

impl Message {
    /// Concatenated searchable text used for embedding/full-text index. Folds in
    /// each block's primary [`searchable_text`](Block::searchable_text) plus any
    /// [`extra_searchable_text`](Block::extra_searchable_text) (e.g. `Select`
    /// option labels), in block order.
    #[must_use]
    pub fn searchable_text(&self) -> String {
        self.blocks
            .iter()
            .flat_map(|b| {
                b.searchable_text()
                    .into_iter()
                    .chain(b.extra_searchable_text())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Envelope used on the NATS event bus and WebSocket wire.
/// Wraps a message with routing metadata so consumers can fan-out without re-reading the DB.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEnvelope {
    pub message: Message,
    /// Recipients (room members at publish time); used by Agents to avoid loops.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recipients: Vec<ParticipantId>,
}

// ---------- Reactions (P2) ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reaction {
    pub message_id: MessageId,
    pub participant_id: ParticipantId,
    pub emoji: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Aggregate of reactions on a message — one entry per unique emoji.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionSummary {
    pub emoji: String,
    pub count: u32,
    pub participants: Vec<ParticipantId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReactionOp {
    Add,
    Remove,
}

// ---------- Channel membership (channels) ----------

/// Whether a `RoomEvent::Membership` records a participant joining or leaving a
/// channel. Carried on the room-scoped event so members keep their roster in sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MembershipOp {
    Join,
    Leave,
}

// ---------- Read receipts (P2) ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadReceipt {
    pub room_id: RoomId,
    pub participant_id: ParticipantId,
    pub last_read_message_id: MessageId,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

// ---------- Threads ----------

/// Aggregate view of a thread (a root message + its replies), used to render the
/// "N replies" affordance without fetching the whole reply set. Produced by the
/// message repository from the existing `messages.reply_to` linkage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSummary {
    /// The root message the replies hang off of.
    pub root_id: MessageId,
    /// Number of (non-deleted) replies in the thread.
    pub reply_count: u32,
    /// Distinct repliers (capped) — used to render participant avatars.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repliers: Vec<ParticipantId>,
    /// Id of the most recent reply (a time-sortable ULID), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reply_id: Option<MessageId>,
    /// Timestamp of the most recent reply, if any.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_reply_at: Option<OffsetDateTime>,
}
