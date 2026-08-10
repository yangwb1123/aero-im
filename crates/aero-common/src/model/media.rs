//! Core domain model — mirrors the SQL schema in `migrations/`.
//!
//! These types are wire-format AND storage-format. The `Block` enum is intentionally
//! the same shape as Slack Block Kit / Discord Embeds / LLM tool-call payloads,
//! so messages can flow through the system without lossy transformations.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{ParticipantId, PollId, RoomId};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallMode {
    #[default]
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

/// Media kind of one SFU-published track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SfuMediaKind {
    Audio,
    Video,
}

/// One track a participant publishes into the server SFU.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SfuPublishedTrack {
    /// Publisher-side SDP media id.
    pub mid: String,
    pub media_kind: SfuMediaKind,
}

/// A publisher and the tracks currently negotiated on its server media leg.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SfuPublisherDescription {
    pub participant: ParticipantId,
    pub tracks: Vec<SfuPublishedTrack>,
}

/// Explicit SFU route requested by one subscriber.
///
/// `(publisher, pub_mid)` identifies the source even when every browser uses
/// common media ids such as `"0"` and `"1"`; `out_mid` is a distinct
/// sendrecv/recvonly transceiver in the subscriber's own SDP.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SfuSubscription {
    pub publisher: ParticipantId,
    pub pub_mid: String,
    pub out_mid: String,
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
        /// Negotiated topology. Defaults to P2P when decoding legacy events.
        #[serde(default)]
        mode: CallMode,
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
        /// Durable `call_participants` incarnation. Newer generations
        /// supersede delayed leave/publisher events from an older node.
        #[serde(default)]
        leg_generation: i64,
    },
    /// Server-authored SFU publisher topology update. Every node folds this
    /// idempotently into its local topology revision, then emits a
    /// `call_sfu_renegotiate` snapshot to locally-connected call members.
    SfuPublisher {
        call_id: CallId,
        room_id: RoomId,
        publisher: SfuPublisherDescription,
        active: bool,
        #[serde(default)]
        leg_generation: i64,
    },
    /// Group call: a participant left. Peers tear down the connection to them.
    Leave {
        call_id: CallId,
        room_id: RoomId,
        from: ParticipantId,
        #[serde(default)]
        leg_generation: i64,
    },
    /// Group call: server → joiner, listing the members already in the call so
    /// the joiner knows whom to connect to (glare-free: lower id offers).
    Roster {
        call_id: CallId,
        to: ParticipantId,
        members: Vec<ParticipantId>,
        #[serde(rename = "call_kind")]
        kind: CallKind,
        /// The targeted joiner's durable incarnation.
        #[serde(default)]
        leg_generation: i64,
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
/// it. Backs `migrations/0023_polls.sql` + `0096_polls_anonymous.sql`.
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
    /// `true` ⇒ voter identities are hidden; only totals are returned. Defaults to
    /// `false` (public). Set at creation time and cannot be changed afterwards.
    #[serde(default)]
    pub anonymous: bool,
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
