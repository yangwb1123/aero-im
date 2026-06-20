//! Core domain model — mirrors the SQL schema in `migrations/`.
//!
//! These types are wire-format AND storage-format. The `Block` enum is intentionally
//! the same shape as Slack Block Kit / Discord Embeds / LLM tool-call payloads,
//! so messages can flow through the system without lossy transformations.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{BlobId, MessageId, NotificationId, ParticipantId, PollId, RoomId};

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
