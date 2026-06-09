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
}

impl NotificationKind {
    /// Lowercase DB/wire token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mention => "mention",
            Self::Reply => "reply",
            Self::Reaction => "reaction",
        }
    }

    /// Parse a DB/wire token, defaulting unknown values to [`Self::Mention`].
    #[must_use]
    pub fn from_str_lenient(s: &str) -> Self {
        match s {
            "reply" => Self::Reply,
            "reaction" => Self::Reaction,
            _ => Self::Mention,
        }
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

// ---------- Live streams (P4/P5) ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamStatus {
    Idle,
    Live,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamProtocol {
    Rtmp,
    Whip,
    Srt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stream {
    pub id: ulid::Ulid,
    pub owner_id: ParticipantId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_id: Option<RoomId>,
    pub title: String,
    pub stream_key: String,
    pub status: StreamStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hls_path: Option<String>,
    pub protocol: StreamProtocol,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub ended_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// A finalized stream recording / VOD: a snapshot of a stream's HLS playlist,
/// retained after the live stream ends so members can play it back later. The
/// `hls_path` is the same playlist path the live pipeline wrote (under the
/// configured `hls_dir`); a `playback_url` is rendered from it at the HTTP edge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vod {
    pub id: crate::ids::VodId,
    /// The stream this recording was finalized from. Not a hard FK — a VOD may
    /// outlive its stream row.
    pub stream_id: ulid::Ulid,
    pub owner_id: ParticipantId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_id: Option<RoomId>,
    pub title: String,
    /// Playlist path (e.g. `/hls/<stream_id>/index.m3u8`) the VOD plays back from.
    pub hls_path: String,
    /// Recording duration in whole seconds, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u32>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

// ---------- Call sessions + signaling (P3/P6) ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallKind {
    Audio,
    Video,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallMode {
    P2p,
    Sfu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CallId(pub ulid::Ulid);

impl CallId {
    #[must_use]
    pub fn new() -> Self {
        Self(ulid::Ulid::new())
    }
    #[must_use]
    pub fn to_uuid(&self) -> uuid::Uuid {
        uuid::Uuid::from_u128(self.0 .0)
    }
    #[must_use]
    pub fn from_uuid(u: uuid::Uuid) -> Self {
        Self(ulid::Ulid(u.as_u128()))
    }
}

impl Default for CallId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for CallId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::str::FromStr for CallId {
    type Err = ulid::DecodeError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(ulid::Ulid::from_str(s)?))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallSession {
    pub id: CallId,
    pub room_id: RoomId,
    pub initiator: ParticipantId,
    pub kind: CallKind,
    pub mode: CallMode,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub ended_at: Option<OffsetDateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
}

/// WebRTC signaling event. Routed via `RoomEvent::Call(...)` on the bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CallEvent {
    Invite {
        call_id: CallId,
        room_id: RoomId,
        from: ParticipantId,
        to: Vec<ParticipantId>,
        // Renamed on the wire to avoid colliding with `RoomEvent`'s `kind` tag
        // when this enum is flattened into `RoomEvent::Call` on the bus.
        #[serde(rename = "call_kind")]
        kind: CallKind,
        sdp: String,
    },
    Answer {
        call_id: CallId,
        from: ParticipantId,
        to: ParticipantId,
        sdp: String,
    },
    Ice {
        call_id: CallId,
        from: ParticipantId,
        to: ParticipantId,
        candidate: serde_json::Value,
    },
    End {
        call_id: CallId,
        room_id: RoomId,
        by: ParticipantId,
        reason: String,
    },
    /// Live caption line (P3 实时字幕翻译). Broadcast to the room during a call.
    /// `text` is the recognized speech; `translated` is filled by the server's
    /// AI backend for final lines when a target language differs from `lang`.
    Caption {
        call_id: CallId,
        room_id: RoomId,
        from: ParticipantId,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lang: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        translated: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        translated_lang: Option<String>,
        is_final: bool,
    },
    /// Group call (P6 mesh): a participant joined. Broadcast to the room so
    /// existing members can establish a peer connection to the newcomer.
    Join {
        call_id: CallId,
        room_id: RoomId,
        from: ParticipantId,
        #[serde(rename = "call_kind")]
        kind: CallKind,
    },
    /// Group call: a participant left. Peers tear down the connection to them.
    Leave {
        call_id: CallId,
        room_id: RoomId,
        from: ParticipantId,
    },
    /// Group call: server → joiner, listing the members already in the call so
    /// the joiner knows whom to connect to (glare-free: lower id offers).
    Roster {
        call_id: CallId,
        to: ParticipantId,
        members: Vec<ParticipantId>,
        #[serde(rename = "call_kind")]
        kind: CallKind,
    },
    /// Group call: a per-pair mesh offer (distinct from the 1:1 `Invite`, which
    /// prompts the callee — `Offer` is auto-answered within an active call).
    Offer {
        call_id: CallId,
        from: ParticipantId,
        to: ParticipantId,
        sdp: String,
    },
}

// ---------- Polls ----------

/// A poll created in a room: a question with 2..=10 options, single- or
/// multi-choice. Members vote; everyone sees the live tally; the creator closes
/// it. Backs `migrations/0023_polls.sql`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Poll {
    pub id: PollId,
    pub room_id: RoomId,
    pub created_by: ParticipantId,
    pub question: String,
    /// Ordered option labels; a vote references one by its index.
    pub options: Vec<String>,
    /// `true` ⇒ a participant may pick several options; `false` ⇒ exactly one.
    pub multi: bool,
    /// Set once the creator closes the poll; `None` while it accepts votes.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub closed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl Poll {
    /// Whether the poll is closed (no longer accepting votes).
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed_at.is_some()
    }
}

/// A poll plus its current per-option vote counts. `counts[i]` is the number of
/// votes for `poll.options[i]`; `total` is the sum (a multi-choice poll can have
/// more votes than voters).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollTally {
    pub poll: Poll,
    pub counts: Vec<u32>,
    pub total: u32,
}

/// What happened to a poll, carried on [`RoomEvent::Poll`] so clients refresh the
/// tally. Renamed nowhere needed — `op` is the field name (the enum tag is `kind`
/// on `RoomEvent`, and this is a plain field, not flattened).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PollOp {
    /// A poll was created.
    Created,
    /// A vote was cast or changed.
    Voted,
    /// The poll was closed by its creator.
    Closed,
}

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
}

impl RoomEvent {
    /// The set of intended recipients for this event. Empty means "fan out to all
    /// room members" — the bus subscriber will look up membership.
    #[must_use]
    pub fn explicit_recipients(&self) -> Vec<ParticipantId> {
        match self {
            RoomEvent::Message(e) => e.recipients.clone(),
            RoomEvent::Notify { mentioned, .. } => vec![*mentioned],
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
            | RoomEvent::Pin { room_id, .. }
            | RoomEvent::Membership { room_id, .. }
            | RoomEvent::Poll { room_id, .. } => Some(*room_id),
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

// ---------- User custom status + presence ----------

/// A user's coarse presence preference. This is the DURABLE, user-chosen
/// preference shown on a profile — distinct from the ephemeral Redis online
/// tracking in `aero_storage::PresenceStore` (which records whether a client is
/// currently connected to a room). Serialized as a lowercase token on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Presence {
    /// Available / online by choice.
    Active,
    /// Stepped away but still reachable.
    Away,
    /// Appears offline by choice.
    Offline,
}

impl Presence {
    /// Lowercase DB/wire token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Away => "away",
            Self::Offline => "offline",
        }
    }

    /// Parse a DB/wire token, defaulting unknown/empty values to [`Self::Active`].
    /// Mirrors `NotificationKind::from_str_lenient` so a malformed stored token
    /// never fails a read.
    #[must_use]
    pub fn from_str_lenient(s: &str) -> Self {
        match s {
            "away" => Self::Away,
            "offline" => Self::Offline,
            _ => Self::Active,
        }
    }
}

/// A user-set custom status (emoji + free text, like Slack's "🏝️ On vacation")
/// plus a coarse [`Presence`] preference. One per participant. The custom status
/// (`emoji`/`text`) may auto-expire at `expires_at`; once it has passed, readers
/// treat the custom status as cleared (emoji/text become `None`) while keeping
/// the `presence` preference. Backs `migrations/0022_user_status.sql`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserStatus {
    pub participant_id: ParticipantId,
    /// Emoji shorthand, e.g. `:palm_tree:`. `None` when no custom status is set
    /// (or it has expired).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
    /// Free-form status text, e.g. "On vacation". `None` when unset/expired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub presence: Presence,
    /// Optional auto-expiry for the custom status. `None` means it never expires.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl UserStatus {
    /// Whether the custom status (`emoji` + `text`) has auto-expired as of `now`.
    ///
    /// `expires_at == None` (no expiry) is never expired. Otherwise it is expired
    /// once `now` has reached or passed `expires_at` (the boundary is inclusive,
    /// so an instant *equal* to `expires_at` counts as expired). This is the
    /// canonical, DB-agnostic counterpart to the storage layer's row-level check,
    /// so any in-memory reader (event payload, cache, test) applies the same rule.
    #[must_use]
    pub fn is_custom_status_expired(&self, now: OffsetDateTime) -> bool {
        matches!(self.expires_at, Some(at) if now >= at)
    }

    /// Return a normalized copy as a reader should see it at `now`: if the custom
    /// status has expired, `emoji`, `text`, and `expires_at` are dropped while the
    /// coarse [`Presence`] preference and `updated_at` are kept (a user who set
    /// "away until 5pm" stays away after 5pm; only the decoration drops). When not
    /// expired, the status is returned unchanged.
    ///
    /// This owns the contract promised in this type's docs so non-DB code paths do
    /// not have to re-implement the expiry-clearing rule.
    #[must_use]
    pub fn with_expiry_applied(mut self, now: OffsetDateTime) -> Self {
        if self.is_custom_status_expired(now) {
            self.emoji = None;
            self.text = None;
            self.expires_at = None;
        }
        self
    }

    /// The emoji a reader should see at `now` — `None` once the custom status has
    /// expired, without allocating a normalized copy.
    #[must_use]
    pub fn effective_emoji(&self, now: OffsetDateTime) -> Option<&str> {
        if self.is_custom_status_expired(now) {
            None
        } else {
            self.emoji.as_deref()
        }
    }

    /// The status text a reader should see at `now` — `None` once the custom
    /// status has expired, without allocating a normalized copy.
    #[must_use]
    pub fn effective_text(&self, now: OffsetDateTime) -> Option<&str> {
        if self.is_custom_status_expired(now) {
            None
        } else {
            self.text.as_deref()
        }
    }
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
    fn caption_event_tagged_and_routes_to_room() {
        let room = RoomId::new();
        let ev = RoomEvent::Call(CallEvent::Caption {
            call_id: CallId::new(),
            room_id: room,
            from: ParticipantId::new(),
            text: "你好".into(),
            lang: Some("zh-CN".into()),
            translated: Some("hello".into()),
            translated_lang: Some("en".into()),
            is_final: true,
        });
        // Fans out to all room members (no explicit recipient list).
        assert!(ev.explicit_recipients().is_empty());
        assert_eq!(ev.room_id(), Some(room));

        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains("\"kind\":\"call\""));
        assert!(j.contains("\"op\":\"caption\""));
        let back: RoomEvent = serde_json::from_str(&j).unwrap();
        assert_eq!(back.room_id(), Some(room));
    }

    #[test]
    fn group_call_events_route_correctly() {
        let room = RoomId::new();
        let joiner = ParticipantId::new();
        let call = CallId::new();

        // Join broadcasts to the whole room.
        let join = RoomEvent::Call(CallEvent::Join {
            call_id: call,
            room_id: room,
            from: joiner,
            kind: CallKind::Video,
        });
        assert!(join.explicit_recipients().is_empty());
        assert_eq!(join.room_id(), Some(room));

        // Roster is targeted to the joiner only.
        let peer = ParticipantId::new();
        let roster = RoomEvent::Call(CallEvent::Roster {
            call_id: call,
            to: joiner,
            members: vec![peer],
            kind: CallKind::Video,
        });
        assert_eq!(roster.explicit_recipients(), vec![joiner]);
        assert_eq!(roster.room_id(), None);

        // Offer is targeted to one peer.
        let offer = RoomEvent::Call(CallEvent::Offer {
            call_id: call,
            from: joiner,
            to: peer,
            sdp: "v=0\r\n".into(),
        });
        assert_eq!(offer.explicit_recipients(), vec![peer]);

        let j = serde_json::to_string(&join).unwrap();
        assert!(j.contains("\"op\":\"join\""));
        let back: RoomEvent = serde_json::from_str(&j).unwrap();
        assert_eq!(back.room_id(), Some(room));
    }

    #[test]
    fn membership_event_tagged_and_fans_to_room() {
        let room = RoomId::new();
        let who = ParticipantId::new();
        let ev = RoomEvent::Membership { room_id: room, participant: who, op: MembershipOp::Join };
        // No explicit recipient list ⇒ fans out to the whole room.
        assert!(ev.explicit_recipients().is_empty());
        assert_eq!(ev.room_id(), Some(room));

        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains("\"kind\":\"membership\""));
        assert!(j.contains("\"op\":\"join\""));
        let back: RoomEvent = serde_json::from_str(&j).unwrap();
        assert_eq!(back.room_id(), Some(room));
        assert!(matches!(
            back,
            RoomEvent::Membership { op: MembershipOp::Join, .. }
        ));

        // Leave round-trips too.
        let leave = RoomEvent::Membership { room_id: room, participant: who, op: MembershipOp::Leave };
        let j = serde_json::to_string(&leave).unwrap();
        assert!(j.contains("\"op\":\"leave\""));
    }

    #[test]
    fn poll_event_tagged_and_fans_to_room() {
        let room = RoomId::new();
        let ev = RoomEvent::Poll {
            room_id: room,
            poll_id: crate::ids::PollId::new(),
            op: PollOp::Created,
        };
        // No explicit recipient list ⇒ fans out to the whole room.
        assert!(ev.explicit_recipients().is_empty());
        assert_eq!(ev.room_id(), Some(room));

        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains("\"kind\":\"poll\""));
        assert!(j.contains("\"op\":\"created\""));
        let back: RoomEvent = serde_json::from_str(&j).unwrap();
        assert_eq!(back.room_id(), Some(room));
        assert!(matches!(back, RoomEvent::Poll { op: PollOp::Created, .. }));

        // Voted / Closed round-trip too.
        let voted = RoomEvent::Poll { room_id: room, poll_id: crate::ids::PollId::new(), op: PollOp::Voted };
        assert!(serde_json::to_string(&voted).unwrap().contains("\"op\":\"voted\""));
        let closed = RoomEvent::Poll { room_id: room, poll_id: crate::ids::PollId::new(), op: PollOp::Closed };
        assert!(serde_json::to_string(&closed).unwrap().contains("\"op\":\"closed\""));
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

    #[test]
    fn presence_token_roundtrip_and_lenient_parse() {
        for p in [Presence::Active, Presence::Away, Presence::Offline] {
            assert_eq!(Presence::from_str_lenient(p.as_str()), p);
        }
        // Lowercase serde tokens.
        assert_eq!(serde_json::to_string(&Presence::Away).unwrap(), "\"away\"");
        // Unknown / empty tokens default to Active (never fails a read).
        assert_eq!(Presence::from_str_lenient("bogus"), Presence::Active);
        assert_eq!(Presence::from_str_lenient(""), Presence::Active);
    }

    #[test]
    fn user_status_expiry_clears_custom_keeps_presence() {
        use time::Duration;
        let set_at = time::OffsetDateTime::UNIX_EPOCH;
        let expiry = set_at + Duration::hours(1);
        let status = UserStatus {
            participant_id: ParticipantId::new(),
            emoji: Some(":palm_tree:".into()),
            text: Some("On vacation".into()),
            presence: Presence::Away,
            expires_at: Some(expiry),
            updated_at: set_at,
        };

        // Before expiry: nothing is cleared.
        let before = expiry - Duration::seconds(1);
        assert!(!status.is_custom_status_expired(before));
        assert_eq!(status.effective_emoji(before), Some(":palm_tree:"));
        assert_eq!(status.effective_text(before), Some("On vacation"));
        let normalized_before = status.clone().with_expiry_applied(before);
        assert_eq!(normalized_before.emoji.as_deref(), Some(":palm_tree:"));
        assert_eq!(normalized_before.expires_at, Some(expiry));

        // Exactly at the boundary: expired (inclusive), so custom status drops.
        assert!(status.is_custom_status_expired(expiry));

        // After expiry: emoji/text/expires_at cleared, presence + updated_at kept.
        let after = expiry + Duration::seconds(1);
        assert!(status.is_custom_status_expired(after));
        assert_eq!(status.effective_emoji(after), None);
        assert_eq!(status.effective_text(after), None);
        let normalized = status.clone().with_expiry_applied(after);
        assert_eq!(normalized.emoji, None);
        assert_eq!(normalized.text, None);
        assert_eq!(normalized.expires_at, None);
        assert_eq!(normalized.presence, Presence::Away, "presence is preserved");
        assert_eq!(normalized.updated_at, set_at, "updated_at is preserved");
        assert_eq!(normalized.participant_id, status.participant_id);
    }

    #[test]
    fn user_status_no_expiry_never_clears() {
        let status = UserStatus {
            participant_id: ParticipantId::new(),
            emoji: Some(":wave:".into()),
            text: Some("around".into()),
            presence: Presence::Active,
            expires_at: None,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        // A status with no expiry is never expired, even far in the future.
        let far_future = time::OffsetDateTime::UNIX_EPOCH + time::Duration::days(3650);
        assert!(!status.is_custom_status_expired(far_future));
        assert_eq!(status.effective_emoji(far_future), Some(":wave:"));
        let same = status.clone().with_expiry_applied(far_future);
        assert_eq!(same.emoji.as_deref(), Some(":wave:"));
        assert_eq!(same.text.as_deref(), Some("around"));
        assert_eq!(same.expires_at, None);
    }

    #[test]
    fn user_status_json_roundtrip() {
        let status = UserStatus {
            participant_id: ParticipantId::new(),
            emoji: Some(":palm_tree:".into()),
            text: Some("On vacation".into()),
            presence: Presence::Away,
            expires_at: None,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let j = serde_json::to_string(&status).unwrap();
        assert!(j.contains("\"presence\":\"away\""));
        assert!(j.contains(":palm_tree:"));
        let back: UserStatus = serde_json::from_str(&j).unwrap();
        assert_eq!(back.participant_id, status.participant_id);
        assert_eq!(back.emoji.as_deref(), Some(":palm_tree:"));
        assert_eq!(back.presence, Presence::Away);
        assert!(back.expires_at.is_none());
    }
}
