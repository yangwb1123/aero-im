//! High-level IM events published on the `im.events.*` `JetStream` subject family.
//!
//! Per-room message broadcasts go on `im.room.{room_id}` as [`MessageEnvelope`]; this
//! module covers the *control-plane* events (room lifecycle, membership) that downstream
//! consumers (search indexer, audit log, presence) need to react to.

use aero_common::{MessageEnvelope, ParticipantId, Room, RoomId};
use serde::{Deserialize, Serialize};

/// Tagged union of high-level IM events.
///
/// Wire format: `{"type":"<variant>", ...}` — consumers can switch on the discriminator
/// without deserializing the payload twice.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImEvent {
    /// A message has been persisted and is being fanned out. Also published on
    /// `im.room.{room_id}` as a bare [`MessageEnvelope`]; this variant exists so the
    /// audit/search consumers can subscribe to a single `im.events.>` stream.
    MessageSent(MessageEnvelope),

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
