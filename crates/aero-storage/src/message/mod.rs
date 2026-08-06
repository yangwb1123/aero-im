//! Message repository — CRUD, search, thread queries, sweep.
//!
//! Split from monolithic `message.rs` (1819 lines) as part of `REFACTOR_PLAN.md`
//! Step 2. Each method group lives in its own sub-module.
//!
//! The struct definitions (`MessageRepo`, `NewMessage`, `SearchHit`) and
//! `MessageRepo::new` live in this module; implementations in sub-modules.

use aero_common::{Block, Message, MessageId, ParticipantId, RoomId};
use sqlx::PgPool;

pub(crate) mod authorization;
/// Core CRUD: insert, get, edit, soft-delete, voice transcript, embedding.
pub mod crud;
/// Transactional edit/delete plus durable message-aggregate events.
pub mod events;
/// Sender-scoped client-message idempotency ledger.
pub mod idempotency;
pub mod orig;
/// Query methods: list_recent, changes_since, list_since, by_sender, etc.
pub mod query;

#[cfg(test)]
mod authorization_tests;
#[cfg(test)]
mod barrier_tests;
#[cfg(test)]
mod recall_tests;
#[cfg(test)]
mod reply_scope_tests;
/// Full-text + vector search across rooms and workspaces.
pub mod search;
/// Expiry sweeps: ephemeral + retention.
pub mod sweep;
/// Thread operations: replies, summary, participants.
pub mod thread;
/// Unread-count aggregation.
pub mod unread;

// ---------------------------------------------------------------------------
// Public types (moved here from the original message.rs so sub-modules can
// reference them without circular dependency).
// ---------------------------------------------------------------------------

/// Thin wrapper around a connection pool.
#[derive(Clone)]
pub struct MessageRepo {
    pub pool: PgPool,
}

/// Input bundle for inserting a new message.
#[derive(Debug, Clone)]
pub struct NewMessage {
    pub room_id: RoomId,
    pub sender_id: ParticipantId,
    pub blocks: Vec<Block>,
    pub reply_to: Option<MessageId>,
    pub metadata: serde_json::Value,
    pub expires_at: Option<time::OffsetDateTime>,
}

/// Optional sender-scoped key attached to an outboxed message insert.
#[derive(Debug, Clone, Copy)]
pub struct MessageIdempotency {
    pub client_message_id: uuid::Uuid,
    pub request_hash: [u8; 32],
}

impl MessageIdempotency {
    #[must_use]
    pub const fn new(client_message_id: uuid::Uuid, request_hash: [u8; 32]) -> Self {
        Self {
            client_message_id,
            request_hash,
        }
    }
}

/// Result of an idempotent message insert.
#[derive(Debug, Clone)]
pub enum MessageInsertOutcome {
    /// This request created the canonical message.
    Created(Message),
    /// The same sender/key/payload was already committed.
    Existing(Message),
}

impl MessageInsertOutcome {
    #[must_use]
    pub fn message(&self) -> &Message {
        match self {
            Self::Created(message) | Self::Existing(message) => message,
        }
    }

    #[must_use]
    pub const fn deduplicated(&self) -> bool {
        matches!(self, Self::Existing(_))
    }

    #[must_use]
    pub fn into_message(self) -> Message {
        match self {
            Self::Created(message) | Self::Existing(message) => message,
        }
    }
}

/// Atomic message/outbox insert result.
///
/// `outbox_id` always identifies the canonical message's retained outbox row,
/// including when [`outcome`](Self::outcome) is deduplicated.
#[derive(Debug, Clone)]
pub struct OutboxedMessageInsert {
    pub outcome: MessageInsertOutcome,
    pub outbox_id: uuid::Uuid,
}

impl OutboxedMessageInsert {
    #[must_use]
    pub fn message(&self) -> &Message {
        self.outcome.message()
    }

    #[must_use]
    pub const fn deduplicated(&self) -> bool {
        self.outcome.deduplicated()
    }

    #[must_use]
    pub fn into_message(self) -> Message {
        self.outcome.into_message()
    }
}

/// Search result row carrying the message + a relevance score.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub message: Message,
    pub score: f32,
    pub headline: Option<String>,
}

impl MessageRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// pgvector HNSW `ef_search` to use for a vector query — the size of the
/// candidate list the index walk maintains.
pub(crate) fn hnsw_ef_search(limit: i64) -> i64 {
    if let Ok(v) = std::env::var("AERO_HNSW_EF_SEARCH") {
        if let Ok(n) = v.parse::<i64>() {
            return n.clamp(1, 1000);
        }
    }
    (limit.saturating_mul(4)).clamp(100, 400)
}

pub(crate) fn searchable_of(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter_map(Block::searchable_text)
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn attached_blob_ids(blocks_json: &serde_json::Value) -> Vec<aero_common::BlobId> {
    let blocks: Vec<Block> = serde_json::from_value(blocks_json.clone()).unwrap_or_default();
    blocks
        .iter()
        .filter_map(|b| match b {
            Block::File { blob_id, .. } | Block::Voice { blob_id, .. } => Some(*blob_id),
            _ => None,
        })
        .collect()
}

/// Gate round-3 B1: the recall history snapshot must never carry byte
/// references. The recall transaction enqueues the original attachment blobs
/// for GC (attachment bytes are content — removed), while `message_edits` is
/// invisible to the GC live-reference scan; a snapshot with `blob_id`s would
/// point at destroyed bytes. `File`/`Voice` blocks become plain text (the
/// voice transcript survives as text evidence); everything else passes
/// through untouched.
pub(crate) fn redact_blocks_for_recall_snapshot(blocks: &[Block]) -> Vec<Block> {
    blocks
        .iter()
        .map(|block| match block {
            Block::File { .. } => Block::text("[附件已移除]"),
            Block::Voice {
                transcript: Some(transcript),
                ..
            } => Block::text(transcript.clone()),
            Block::Voice {
                transcript: None, ..
            } => Block::text("[语音已移除]"),
            other => other.clone(),
        })
        .collect()
}

pub(crate) const EXPORT_SENDER_CAP: i64 = 500;

// ---------- Types shared by sub-modules ----------

/// Row type for [`SearchHit`] queries — carries the extra `score` column.
#[derive(sqlx::FromRow)]
pub(crate) struct ScoredMessageRow {
    id: uuid::Uuid,
    room_id: uuid::Uuid,
    sender_id: uuid::Uuid,
    blocks: serde_json::Value,
    reply_to: Option<uuid::Uuid>,
    metadata: serde_json::Value,
    created_at: time::OffsetDateTime,
    edited_at: Option<time::OffsetDateTime>,
    deleted_at: Option<time::OffsetDateTime>,
    recalled_at: Option<time::OffsetDateTime>,
    recalled_by: Option<uuid::Uuid>,
    expires_at: Option<time::OffsetDateTime>,
    version: i32,
}

/// Row type for `list_recent` / `list_since` etc. — maps directly to `messages` columns.
#[derive(sqlx::FromRow, Debug)]
pub(crate) struct MessageRow {
    pub(crate) id: uuid::Uuid,
    pub(crate) room_id: uuid::Uuid,
    pub(crate) sender_id: uuid::Uuid,
    pub(crate) blocks: serde_json::Value,
    pub(crate) reply_to: Option<uuid::Uuid>,
    pub(crate) metadata: serde_json::Value,
    pub(crate) created_at: time::OffsetDateTime,
    pub(crate) edited_at: Option<time::OffsetDateTime>,
    pub(crate) deleted_at: Option<time::OffsetDateTime>,
    pub(crate) recalled_at: Option<time::OffsetDateTime>,
    pub(crate) recalled_by: Option<uuid::Uuid>,
    pub(crate) expires_at: Option<time::OffsetDateTime>,
    pub(crate) version: i32,
}

#[allow(deprecated)]
impl From<ScoredMessageRow> for SearchHit {
    fn from(r: ScoredMessageRow) -> Self {
        Self {
            message: Message {
                id: MessageId::from_uuid(r.id),
                room_id: RoomId::from_uuid(r.room_id),
                sender_id: aero_common::ParticipantId::from_uuid(r.sender_id),
                blocks: serde_json::from_value(r.blocks).unwrap_or_default(),
                reply_to: r.reply_to.map(MessageId::from_uuid),
                metadata: r.metadata,
                created_at: r.created_at,
                edited_at: r.edited_at,
                deleted_at: r.deleted_at,
                recalled_at: r.recalled_at,
                recalled_by: r.recalled_by.map(ParticipantId::from_uuid),
                expires_at: r.expires_at,
                version: r.version,
            },
            score: 0.0,
            headline: None,
        }
    }
}

impl From<MessageRow> for Message {
    fn from(r: MessageRow) -> Self {
        Self {
            id: MessageId::from_uuid(r.id),
            room_id: RoomId::from_uuid(r.room_id),
            sender_id: aero_common::ParticipantId::from_uuid(r.sender_id),
            blocks: serde_json::from_value(r.blocks).unwrap_or_default(),
            reply_to: r.reply_to.map(MessageId::from_uuid),
            metadata: r.metadata,
            created_at: r.created_at,
            edited_at: r.edited_at,
            deleted_at: r.deleted_at,
            recalled_at: r.recalled_at,
            recalled_by: r.recalled_by.map(ParticipantId::from_uuid),
            expires_at: r.expires_at,
            version: r.version,
        }
    }
}
