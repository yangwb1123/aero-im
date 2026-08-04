//! Core domain model — mirrors the SQL schema in `migrations/`.
//!
//! These types are wire-format AND storage-format. The `Block` enum is intentionally
//! the same shape as Slack Block Kit / Discord Embeds / LLM tool-call payloads,
//! so messages can flow through the system without lossy transformations.

use serde::{Deserialize, Serialize};

use crate::ids::{BlobId, ParticipantId};

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

/// One choice in a [`Block::Select`] dropdown: a stable machine `value` recorded
/// when chosen, plus a human-readable `label` rendered in the menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectOption {
    /// The machine value recorded as the interaction's `value` when this option is
    /// chosen (e.g. `"approve"`).
    pub value: String,
    /// The human-readable label shown in the dropdown (e.g. `"Approve request"`).
    pub label: String,
}

/// The atomic unit of message content. Inspired by Slack Block Kit but extended for AI.
///
/// A message is `Vec<Block>` — clients render blocks in order. AI agents produce and
/// consume the same shape, so no translation layer is needed between user input,
/// stored history, and LLM context windows.
///
/// The interactive variants ([`Button`](Block::Button), [`Select`](Block::Select))
/// let a bot/webhook/app post an *actionable* message: a participant clicks a
/// button or picks an option and the server records the interaction (see the
/// `interactions` HTTP surface), broadcasting a
/// [`RoomEvent::Interaction`](RoomEvent::Interaction) so the poster sees it live.
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
    /// An interactive button. A `url`-bearing button is a plain link the client
    /// opens; a `url`-less button is an *action* — clicking it POSTs to the
    /// interactions endpoint, which records the click against `action_id`. `style`
    /// is an optional render hint (`primary` | `danger` | `default`).
    Button {
        action_id: String,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        style: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    /// An interactive single-choice dropdown. Picking an option POSTs to the
    /// interactions endpoint with the chosen option's `value`, recorded against
    /// `action_id`. `placeholder` is the prompt shown before a choice is made.
    Select {
        action_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        options: Vec<SelectOption>,
    },
}

impl Block {
    pub fn text(content: impl Into<String>) -> Self {
        Self::Text {
            content: content.into(),
            spans: Vec::new(),
        }
    }

    /// Returns the primary plain-text projection used for fulltext indexing and
    /// embedding. Hidden Thought blocks are excluded. An interactive
    /// [`Button`](Self::Button) contributes its visible `label` (so a "Approve"
    /// button is searchable like any other text). A [`Select`](Self::Select)
    /// dropdown's *option* labels are multiple, so they are surfaced separately via
    /// [`extra_searchable_text`](Self::extra_searchable_text) (which
    /// [`Message::searchable_text`](Message::searchable_text) also folds in).
    #[must_use]
    pub fn searchable_text(&self) -> Option<&str> {
        match self {
            Self::Text { content, .. } | Self::Code { content, .. } => Some(content),
            Self::Voice {
                transcript: Some(t),
                ..
            } => Some(t),
            Self::Thought {
                content,
                hidden: false,
            } => Some(content),
            Self::Button { label, .. } => Some(label),
            // Index the attachment's file name so a message carrying e.g.
            // "deploy-runbook.pdf" is findable by name (ROADMAP5 方向三: file search
            // — voice transcripts are already indexed above; files were the gap).
            Self::File { name, .. } => Some(name),
            _ => None,
        }
    }

    /// Additional searchable spans that don't fit the single-`&str`
    /// [`searchable_text`](Self::searchable_text) shape — today the option labels of
    /// a [`Select`](Self::Select) dropdown, so the menu's human-readable choices are
    /// indexed/embedded alongside the rest of a message. Empty for every other
    /// variant.
    #[must_use]
    pub fn extra_searchable_text(&self) -> Vec<&str> {
        match self {
            Self::Select { options, .. } => options.iter().map(|o| o.label.as_str()).collect(),
            _ => Vec::new(),
        }
    }

    /// The interactive `action_id` this block carries, if it is an interactive
    /// component (a `Button` or a `Select`); `None` for every static block. Used
    /// to verify an interaction targets a real component on a message.
    #[must_use]
    pub fn action_id(&self) -> Option<&str> {
        match self {
            Self::Button { action_id, .. } | Self::Select { action_id, .. } => Some(action_id),
            _ => None,
        }
    }
}

/// Whether `blocks` contains an interactive [`Block`] (a `Button` or `Select`)
/// whose `action_id` equals `action_id`. Pure — the server uses it to verify an
/// inbound interaction actually targets a component on the message before
/// recording it (otherwise the interaction is a 404). A `url`-only link button
/// still counts: it has an `action_id` and can be recorded if a client posts one.
#[must_use]
pub fn message_has_action(blocks: &[Block], action_id: &str) -> bool {
    blocks.iter().any(|b| b.action_id() == Some(action_id))
}
