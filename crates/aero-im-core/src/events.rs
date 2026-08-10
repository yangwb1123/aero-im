//! High-level IM events published on the `im.events.*` `JetStream` subject family.
//!
//! Per-room real-time events go on `im.room.{room_id}` as [`RoomEvent`] (see
//! `aero_common::RoomEvent`); this module covers the *control-plane* events
//! (room lifecycle, membership) that downstream consumers (audit log, presence,
//! analytics) need to react to.

use aero_common::{MessageEnvelope, ParticipantId, Room, RoomId};
use serde::{Deserialize, Serialize};

/// Tagged union of high-level IM control-plane events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImEvent {
    /// A message was persisted. Also broadcast on `im.room.{id}` as a `RoomEvent::Message`.
    /// Boxed: `Message` grew recall fields; the variant is wire-only (never
    /// constructed in-process), and `Box` is serde-transparent on the wire.
    MessageSent(Box<MessageEnvelope>),

    /// A new room was created.
    RoomCreated(Room),

    /// A participant joined (or was added to) a room.
    MemberAdded {
        room: RoomId,
        participant: ParticipantId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::{ParticipantId, RoomId, RoomKind};

    #[test]
    fn member_added_roundtrip() {
        let ev = ImEvent::MemberAdded {
            room: RoomId::new(),
            participant: ParticipantId::new(),
        };
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains("\"type\":\"member_added\""));
        let _: ImEvent = serde_json::from_str(&j).unwrap();
    }

    #[test]
    fn room_created_roundtrip() {
        let ev = ImEvent::RoomCreated(Room {
            id: RoomId::new(),
            kind: RoomKind::Group,
            name: Some("dev".into()),
            created_by: ParticipantId::new(),
            created_at: time::OffsetDateTime::now_utc(),
        });
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains("\"type\":\"room_created\""));
        let _: ImEvent = serde_json::from_str(&j).unwrap();
    }
}
