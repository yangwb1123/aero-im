//! Live-stream interactivity model — danmaku (bullet chat), virtual gifts, and
//! viewer presence (P4 互动直播).
//!
//! These events flow on the NATS `live.stream.{stream_id}` subject and reach
//! watching clients over the WebSocket as `{"type":"stream_event","event": …}`.
//! Unlike [`RoomEvent`](crate::RoomEvent) they fan out to *stream watchers*
//! (tracked in the server Hub), not room members — a public stream can be
//! watched by anyone.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use ulid::Ulid;

use crate::ids::ParticipantId;
use crate::model::StreamStatus;

// ---------- Gifts ----------

/// A virtual gift definition. The catalog is fixed at build time (see
/// [`gift_catalog`]); *sent* gifts are persisted in `stream_gifts` for the
/// leaderboard + replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gift {
    /// Stable identifier used on the wire and in storage, e.g. `"rocket"`.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Emoji / glyph rendered in the gift bar and the float animation.
    pub icon: String,
    /// Price in coins — also the score added to the sender's leaderboard total.
    pub coins: u32,
}

/// The fixed gift catalog. Kept small + deterministic so the client can render a
/// gift bar without a round-trip and the server can validate `gift_id` cheaply.
#[must_use]
pub fn gift_catalog() -> Vec<Gift> {
    [
        ("heart", "红心", "❤️", 1u32),
        ("rose", "玫瑰", "🌹", 5),
        ("beer", "啤酒", "🍺", 10),
        ("rocket", "火箭", "🚀", 100),
        ("crown", "皇冠", "👑", 500),
        ("dragon", "巨龙", "🐉", 2000),
    ]
    .into_iter()
    .map(|(id, name, icon, coins)| Gift {
        id: id.to_owned(),
        name: name.to_owned(),
        icon: icon.to_owned(),
        coins,
    })
    .collect()
}

/// Look up a gift by id in the catalog.
#[must_use]
pub fn gift_by_id(id: &str) -> Option<Gift> {
    gift_catalog().into_iter().find(|g| g.id == id)
}

// ---------- Wire / storage rows ----------

/// One danmaku / chat line on a stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChatLine {
    pub id: Ulid,
    pub stream_id: Ulid,
    pub sender_id: ParticipantId,
    pub sender_name: String,
    pub body: String,
    /// Whether the sender had an active creator subscription to the stream owner
    /// at post time (Twitch-style subscriber badge). Defaults to `false` for older
    /// rows / clients that omit it on the wire.
    #[serde(default)]
    pub is_subscriber: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// A gift that was sent on a stream (already priced + attributed). Cosmetic
/// fields (`gift_name`/`gift_icon`) come from the catalog at read time so the
/// store keeps only `gift_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamGiftLine {
    pub id: Ulid,
    pub stream_id: Ulid,
    pub sender_id: ParticipantId,
    pub sender_name: String,
    pub gift_id: String,
    pub gift_name: String,
    pub gift_icon: String,
    pub qty: u32,
    /// Total coins for this send = `qty * gift.coins`.
    pub coins: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// One row of the per-stream gift leaderboard (top spenders).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiftLeaderRow {
    pub sender_id: ParticipantId,
    pub sender_name: String,
    pub total_coins: u64,
    pub total_qty: u64,
}

// ---------- Unified per-stream event (NATS + WS wire) ----------

/// Every per-stream real-time event flows through this tagged enum on the NATS
/// `live.stream.{stream_id}` subject. The bus listener fans out to local
/// watchers of that stream (tracked in the Hub), independent of room membership.
///
/// The web client receives the same shape over the WebSocket as
/// `{"type":"stream_event","event": <StreamEvent>}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StreamEvent {
    /// A new danmaku / chat line.
    Chat(StreamChatLine),
    /// A gift was sent.
    Gift(StreamGiftLine),
    /// Live viewer count changed.
    Viewers { stream_id: Ulid, count: u32 },
    /// Stream lifecycle transition (went live / ended).
    Status { stream_id: Ulid, status: StreamStatus },
    /// Hype-train momentum update: a gift advanced (or kept alive) the train.
    /// `level` is the current escalation level, `contribution` the running total
    /// of units fed into the train, and `expires_at` the instant the train lapses
    /// without further contributions (RFC 3339 on the wire). Clients render the
    /// escalating meter from this (Twitch Hype Train).
    HypeTrain {
        stream_id: Ulid,
        level: u32,
        contribution: u32,
        #[serde(with = "time::serde::rfc3339")]
        expires_at: OffsetDateTime,
    },
    /// A raid was launched on this (source) stream: the owner is sending their
    /// `viewer_count` viewers to `target_stream_id`. Watchers' clients use this to
    /// redirect to the target stream (Twitch/Kick raid).
    Raid {
        stream_id: Ulid,
        target_stream_id: Ulid,
        viewer_count: u32,
    },
}

impl StreamEvent {
    /// The stream this event belongs to — used by the bus listener to fan out
    /// to the right set of watchers.
    #[must_use]
    pub fn stream_id(&self) -> Ulid {
        match self {
            StreamEvent::Chat(c) => c.stream_id,
            StreamEvent::Gift(g) => g.stream_id,
            StreamEvent::Viewers { stream_id, .. }
            | StreamEvent::Status { stream_id, .. }
            | StreamEvent::HypeTrain { stream_id, .. }
            | StreamEvent::Raid { stream_id, .. } => *stream_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_nonempty_and_unique() {
        let cat = gift_catalog();
        assert!(!cat.is_empty());
        let mut ids: Vec<&str> = cat.iter().map(|g| g.id.as_str()).collect();
        ids.sort_unstable();
        let len = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), len, "gift ids must be unique");
    }

    #[test]
    fn gift_lookup_roundtrip() {
        assert_eq!(gift_by_id("rocket").unwrap().coins, 100);
        assert!(gift_by_id("nope").is_none());
    }

    #[test]
    fn stream_event_tag_and_stream_id() {
        let sid = Ulid::new();
        let ev = StreamEvent::Viewers { stream_id: sid, count: 7 };
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains("\"kind\":\"viewers\""));
        assert_eq!(ev.stream_id(), sid);

        let back: StreamEvent = serde_json::from_str(&j).unwrap();
        assert!(matches!(back, StreamEvent::Viewers { count: 7, .. }));
    }

    #[test]
    fn chat_line_roundtrips_as_string_ids() {
        let line = StreamChatLine {
            id: Ulid::new(),
            stream_id: Ulid::new(),
            sender_id: ParticipantId::new(),
            sender_name: "Ada".into(),
            body: "hi".into(),
            is_subscriber: false,
            created_at: OffsetDateTime::now_utc(),
        };
        let j = serde_json::to_string(&line).unwrap();
        // ULIDs serialize as 26-char Crockford strings, not byte arrays.
        assert!(j.contains(&line.id.to_string()));
        let back: StreamChatLine = serde_json::from_str(&j).unwrap();
        assert_eq!(back.body, "hi");
    }
}
