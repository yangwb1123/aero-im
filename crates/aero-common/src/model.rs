//! Core domain model — mirrors the SQL schema in `migrations/`.
//!
//! These types are wire-format AND storage-format. The `Block` enum is intentionally
//! the same shape as Slack Block Kit / Discord Embeds / LLM tool-call payloads,
//! so messages can flow through the system without lossy transformations.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{BlobId, MessageId, ParticipantId, RoomId};

// ---------- Participant ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParticipantKind {
    Human,
    Agent,
    Bot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Participant {
    pub id: ParticipantId,
    pub kind: ParticipantKind,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub created_by: Option<ParticipantId>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

// ---------- Room ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoomKind {
    Direct,
    Group,
    Channel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Room {
    pub id: RoomId,
    pub kind: RoomKind,
    pub name: Option<String>,
    pub created_by: ParticipantId,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

// ---------- Block model ----------

/// A single inline span inside a `Text` block — used for bold/italic/link decoration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: u32,
    pub end: u32,
    pub style: SpanStyle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanStyle {
    Bold,
    Italic,
    Strikethrough,
    Code,
    Link { href: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    Image,
    Video,
    Audio,
    Document,
    Other,
}

/// The atomic unit of message content. Inspired by Slack Block Kit but extended for AI.
///
/// A message is `Vec<Block>` — clients render blocks in order. AI agents produce and
/// consume the same shape, so no translation layer is needed between user input,
/// stored history, and LLM context windows.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        content: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        spans: Vec<Span>,
    },
    Mention {
        participant: ParticipantId,
    },
    Code {
        lang: String,
        content: String,
    },
    File {
        blob_id: BlobId,
        kind: FileKind,
        name: String,
        size: u64,
    },
    Voice {
        blob_id: BlobId,
        duration_ms: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transcript: Option<String>,
    },
    Card {
        schema: String,
        payload: serde_json::Value,
    },
    ToolCall {
        tool: String,
        args: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
    },
    Thought {
        content: String,
        #[serde(default)]
        hidden: bool,
    },
}

impl Block {
    pub fn text(content: impl Into<String>) -> Self {
        Self::Text { content: content.into(), spans: Vec::new() }
    }

    /// Returns the plain-text projection used for fulltext indexing and embedding.
    /// Hidden Thought blocks are excluded.
    #[must_use]
    pub fn searchable_text(&self) -> Option<&str> {
        match self {
            Self::Text { content, .. } | Self::Code { content, .. } => Some(content),
            Self::Voice { transcript: Some(t), .. } => Some(t),
            Self::Thought { content, hidden: false } => Some(content),
            _ => None,
        }
    }
}

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
}

impl Message {
    /// Concatenated searchable text used for embedding/full-text index.
    #[must_use]
    pub fn searchable_text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(Block::searchable_text)
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

// ---------- Tests ----------

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn block_text_roundtrip() {
        let b = Block::text("hi");
        let j = serde_json::to_string(&b).unwrap();
        assert!(j.contains("\"type\":\"text\""));
        assert!(j.contains("\"content\":\"hi\""));
        let back: Block = serde_json::from_str(&j).unwrap();
        assert!(matches!(back, Block::Text { ref content, .. } if content == "hi"));
    }

    #[test]
    fn block_toolcall_roundtrip() {
        let b = Block::ToolCall {
            tool: "search".into(),
            args: serde_json::json!({"q": "rust"}),
            result: None,
        };
        let j = serde_json::to_string(&b).unwrap();
        let back: Block = serde_json::from_str(&j).unwrap();
        match back {
            Block::ToolCall { tool, .. } => assert_eq!(tool, "search"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn searchable_text_concatenates_blocks() {
        let m = Message {
            id: MessageId::new(),
            room_id: RoomId::new(),
            sender_id: ParticipantId::new(),
            blocks: vec![
                Block::text("hello"),
                Block::Code { lang: "rs".into(), content: "fn main() {}".into() },
                Block::Thought { content: "secret".into(), hidden: true },
            ],
            reply_to: None,
            metadata: serde_json::Value::Null,
            created_at: time::OffsetDateTime::now_utc(),
            edited_at: None,
            deleted_at: None,
        };
        assert_eq!(m.searchable_text(), "hello\nfn main() {}");
    }
}
