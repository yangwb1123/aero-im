//! Message repository — CRUD, search, thread queries, sweep.
//!
//! Split from monolithic `message.rs` (1819 lines) as part of REFACTOR_PLAN.md
//! Step 2. Each method group lives in its own sub-module.
//!
//! The struct definitions (`MessageRepo`, `NewMessage`, `SearchHit`) and
//! `MessageRepo::new` live in this module; implementations in sub-modules.

use aero_common::{Block, Message, MessageId, ParticipantId, RoomId};
use sqlx::PgPool;

/// Core CRUD: insert, get, edit, soft-delete, voice transcript, embedding.
pub mod crud;
/// Query methods: list_recent, changes_since, list_since, by_sender, etc.
pub mod query;
/// Full-text + vector search across rooms and workspaces.
pub mod search;
/// Thread operations: replies, summary, participants.
pub mod thread;
/// Unread-count aggregation.
pub mod unread;
/// Expiry sweeps: ephemeral + retention.
pub mod sweep;
pub mod orig;
/// Original monolithic module (for incremental extraction).

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

pub(crate) const EXPORT_SENDER_CAP: i64 = 500;

/// Maximum rows returned by [`MessageRepo::by_sender`] (personal GDPR export).

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
    expires_at: Option<time::OffsetDateTime>,
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
    pub(crate) expires_at: Option<time::OffsetDateTime>,
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
                expires_at: r.expires_at,
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
            expires_at: r.expires_at,
        }
    }
}




