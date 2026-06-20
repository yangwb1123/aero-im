//! Core domain model — mirrors the SQL schema in `migrations/`.
//!
//! These types are wire-format AND storage-format. The `Block` enum is intentionally
//! the same shape as Slack Block Kit / Discord Embeds / LLM tool-call payloads,
//! so messages can flow through the system without lossy transformations.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{BlobId, MessageId, NotificationId, ParticipantId, PollId, RoomId};

// ---------- Notifications (mentions & thread replies) ----------

/// Why a participant was notified about a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotificationKind {
    /// The message contains a `Block::Mention` targeting the recipient.
    Mention,
    /// The message is a `reply_to` a message the recipient sent.
    Reply,
    /// Someone added an emoji reaction to a message the recipient authored.
    Reaction,
    /// A new message matched one of the recipient's monitored saved searches
    /// (ROADMAP5 方向三 — periodic saved-search digest).
    #[serde(rename = "saved_search")]
    SavedSearch,
    /// Multiple people replied to the same thread you follow, aggregated as one
    /// notification instead of N individual Reply events (ROADMAP6 方向一).
    /// The aggregated `n` count is stored in the notification's metadata.
    #[serde(rename = "aggregate_reply")]
    AggregateReply,
}

/// One recipient of a [`RoomEvent::NotifyBatch`] — who to notify and why.
/// Server-internal only; the receiving node expands each into an individual
/// `notify` frame, so a client never sees the other recipients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyTarget {
    pub participant: ParticipantId,
    #[serde(rename = "notify_kind")]
    pub kind: NotificationKind,
}

impl NotificationKind {
    /// Lowercase DB/wire token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mention => "mention",
            Self::Reply => "reply",
            Self::Reaction => "reaction",
            Self::SavedSearch => "saved_search",
            Self::AggregateReply => "aggregate_reply",
        }
    }

    /// Parse a DB/wire token, defaulting unknown values to [`Self::Mention`].
    #[must_use]
    pub fn from_str_lenient(s: &str) -> Self {
        match s {
            "reply" => Self::Reply,
            "reaction" => Self::Reaction,
            "saved_search" => Self::SavedSearch,
            "aggregate_reply" => Self::AggregateReply,
            _ => Self::Mention,
        }
    }
}

/// Heuristic importance score for a notification kind, in `[0.0, 1.0]`.
/// Used by the push layer to prioritise delivery and by clients to surface
/// high-importance notifications visually.
#[must_use]
pub fn importance_for(kind: &NotificationKind) -> f32 {
    match kind {
        NotificationKind::Mention => 1.0,
        NotificationKind::Reply => 0.8,
        NotificationKind::AggregateReply => 0.7,
        NotificationKind::SavedSearch => 0.5,
        NotificationKind::Reaction => 0.3,
    }
}

/// A durable inbox entry: "you were mentioned / replied to in this message".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub id: NotificationId,
    /// The recipient whose inbox this lands in.
    pub participant_id: ParticipantId,
    pub room_id: RoomId,
    pub message_id: MessageId,
    pub kind: NotificationKind,
    /// Who triggered it (the message sender), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<ParticipantId>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// `None` until the recipient marks it read.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub read_at: Option<OffsetDateTime>,
    /// When `kind == AggregateReply`, the count of replies folded into this
    /// notification (ROADMAP7 方向三). `None` for non-aggregate notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate_count: Option<u32>,
    /// Heuristic importance score (0.0 = low, 1.0 = highest).
    /// Computed from the kind and other signals at insert time.
    pub importance_score: f32,
}

/// Per-room unread tally for a participant: total unread messages plus the
/// subset that mention them. Drives the sidebar unread + mention badges.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomUnread {
    pub room_id: RoomId,
    /// Unread messages (created after the participant's read receipt, not their own).
    pub unread: u32,
    /// Unread notifications (mentions/replies) in this room.
    pub mentions: u32,
}
