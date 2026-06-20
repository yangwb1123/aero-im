//! Core domain model — mirrors the SQL schema in `migrations/`.
//!
//! These types are wire-format AND storage-format. The `Block` enum is intentionally
//! the same shape as Slack Block Kit / Discord Embeds / LLM tool-call payloads,
//! so messages can flow through the system without lossy transformations.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{BlobId, MessageId, NotificationId, ParticipantId, PollId, RoomId};
use super::block::FileKind;
use super::message::Message;
use super::message::MembershipOp;
use super::message::MessageEnvelope;
use super::message::ReactionOp;
use super::notification::{NotificationKind, NotifyTarget};
use super::blob::PinOp;
use super::media::{CallEvent, PollOp};

// ---------- Unified room-scoped event (NATS + WS wire) ----------

/// Every per-room real-time event flows through this tagged enum on the NATS
/// `im.room.{room_id}` subject. Subscribers fan out by recipient list.
///
/// The web client receives the same shape (minus envelope-level routing fields)
/// over the WebSocket as `{"type":"room_event", "event": <RoomEvent>}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoomEvent {
    /// A new message was sent.
    Message(MessageEnvelope),
    /// An existing message was edited.
    Edited(Message),
    /// A message was soft-deleted.
    Deleted {
        room_id: RoomId,
        message_id: MessageId,
        by: ParticipantId,
    },
    /// A reaction was toggled.
    Reaction {
        room_id: RoomId,
        message_id: MessageId,
        participant: ParticipantId,
        emoji: String,
        op: ReactionOp,
    },
    /// A read receipt was updated.
    Read {
        room_id: RoomId,
        participant: ParticipantId,
        last_message_id: MessageId,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
    /// Best-effort typing indicator (no DB persistence).
    Typing {
        room_id: RoomId,
        participant: ParticipantId,
        on: bool,
    },
    /// A participant was notified (mentioned or replied-to). Targeted to the
    /// `mentioned` recipient only so their client can raise a badge/toast even
    /// while viewing another room. The durable copy lives in `notifications`.
    Notify {
        room_id: RoomId,
        message_id: MessageId,
        mentioned: ParticipantId,
        by: ParticipantId,
        // Renamed on the wire to avoid colliding with `RoomEvent`'s `kind` tag
        // (the same fix `CallEvent` uses for `call_kind`). Clients read
        // `notify_kind`.
        #[serde(rename = "notify_kind")]
        kind: NotificationKind,
    },
    /// A batch of per-recipient notifications for ONE message, published as a
    /// SINGLE bus event that the receiving node expands into one targeted
    /// `Notify`-shaped frame per recipient. Collapses a broadcast `@everyone`
    /// from O(N) NATS publishes to one (ROADMAP 方向二). Server-internal: never
    /// delivered to clients verbatim — each recipient still receives an ordinary
    /// `notify` frame, so no client change is needed and recipients stay private.
    NotifyBatch {
        room_id: RoomId,
        message_id: MessageId,
        by: ParticipantId,
        /// Idempotency token: each NotifyBatch publish carries a unique ULID so
        /// a redelivery after a consumer crash produces zero duplicate
        /// notifications (ON CONFLICT on (delivery_id, participant_id)).
        delivery_id: uuid::Uuid,
        recipients: Vec<NotifyTarget>,
    },
    /// A message was pinned or unpinned. Fans out to the whole room so every
    /// member's pinned-panel stays in sync.
    Pin {
        room_id: RoomId,
        message_id: MessageId,
        by: ParticipantId,
        op: PinOp,
    },
    /// A participant joined or left a channel. Fans out to the whole room so
    /// every member's roster stays in sync. `op` carries join-vs-leave.
    Membership {
        room_id: RoomId,
        participant: ParticipantId,
        op: MembershipOp,
    },
    /// WebRTC signaling (P3/P6).
    Call(CallEvent),
    /// A poll was created, voted on, or closed. Fans out to the whole room so
    /// every member's tally stays live. `op` carries which transition occurred.
    Poll {
        room_id: RoomId,
        poll_id: PollId,
        op: PollOp,
    },
    /// A participant acknowledged seeing a SPECIFIC message ("Seen by …"). Fans
    /// out to the whole room so every member's per-message read indicator stays
    /// live. Distinct from [`RoomEvent::Read`], which moves the per-room unread
    /// cursor; this carries the individual message a reader has now seen.
    MessageSeen {
        room_id: RoomId,
        message_id: MessageId,
        participant: ParticipantId,
    },
    /// A participant interacted with an interactive [`Block`] (clicked a
    /// `Button` or picked a `Select` option) on a message. Fans out to the whole
    /// room so the poster's client — typically a bot/app/webhook integration —
    /// sees the click live and can react (the durable record lives in
    /// `block_interactions`). `action_id` identifies which component was hit.
    Interaction {
        room_id: RoomId,
        message_id: MessageId,
        participant: ParticipantId,
        action_id: String,
    },
}

impl RoomEvent {
    /// The set of intended recipients for this event. Empty means "fan out to all
    /// room members" — the bus subscriber will look up membership.
    #[must_use]
    pub fn explicit_recipients(&self) -> Vec<ParticipantId> {
        match self {
            RoomEvent::Message(e) => e.recipients.clone(),
            RoomEvent::Notify { mentioned, .. } => vec![*mentioned],
            RoomEvent::NotifyBatch { recipients, .. } => {
                recipients.iter().map(|t| t.participant).collect()
            }
            RoomEvent::Call(CallEvent::Invite { to, .. }) => to.clone(),
            RoomEvent::Call(
                CallEvent::Answer { to, .. }
                | CallEvent::Ice { to, .. }
                | CallEvent::Roster { to, .. }
                | CallEvent::Offer { to, .. },
            ) => vec![*to],
            _ => Vec::new(),
        }
    }

    #[must_use]
    pub fn room_id(&self) -> Option<RoomId> {
        match self {
            RoomEvent::Message(e) => Some(e.message.room_id),
            RoomEvent::Edited(m) => Some(m.room_id),
            RoomEvent::Deleted { room_id, .. }
            | RoomEvent::Reaction { room_id, .. }
            | RoomEvent::Read { room_id, .. }
            | RoomEvent::Typing { room_id, .. }
            | RoomEvent::Notify { room_id, .. }
            | RoomEvent::NotifyBatch { room_id, .. }
            | RoomEvent::Pin { room_id, .. }
            | RoomEvent::Membership { room_id, .. }
            | RoomEvent::Poll { room_id, .. }
            | RoomEvent::MessageSeen { room_id, .. }
            | RoomEvent::Interaction { room_id, .. } => Some(*room_id),
            RoomEvent::Call(
                CallEvent::Invite { room_id, .. }
                | CallEvent::End { room_id, .. }
                | CallEvent::Caption { room_id, .. }
                | CallEvent::Join { room_id, .. }
                | CallEvent::Leave { room_id, .. },
            ) => Some(*room_id),
            RoomEvent::Call(
                CallEvent::Answer { .. }
                | CallEvent::Ice { .. }
                | CallEvent::Roster { .. }
                | CallEvent::Offer { .. },
            ) => None,
        }
    }
}
