//! JSON frame serialization for WebSocket messages.
//!
//! Extracted from `ws/ws_impl.rs` as part of REFACTOR_PLAN.md Step 4.
//! Uses `ServerFrame` and `Message` from the parent module.

use aero_common::{NotificationKind, RoomEvent};

use super::ws_impl::ServerFrame;

/// Stamp a bus seq onto a ServerFrame JSON value and serialize.
fn stamped_frame_json(frame: &ServerFrame<'_>, seq: Option<u64>) -> String {
    match serde_json::to_value(frame) {
        Ok(mut value) => {
            aero_bus::stamp_seq(&mut value, seq);
            value.to_string()
        }
        Err(_) => "{\"type\":\"error\",\"code\":\"serialize\",\"msg\":\"\"}".into(),
    }
}

/// Translate a `RoomEvent` into the JSON wire frame the browser expects.
pub fn room_event_to_frame_json(event: &RoomEvent, seq: Option<u64>) -> String {
    let frame: ServerFrame<'_> = match event.clone() {
        RoomEvent::Message(env) => ServerFrame::Message { message: env.message },
        RoomEvent::Edited(m) => ServerFrame::Edited { message: m },
        RoomEvent::Deleted { room_id, message_id, by } => {
            ServerFrame::Deleted { room_id, message_id, by }
        }
        RoomEvent::Reaction { room_id, message_id, participant, emoji, op } => {
            ServerFrame::Reaction { room_id, message_id, participant, emoji, op }
        }
        RoomEvent::Read { room_id, participant, last_message_id, at } => {
            ServerFrame::Read { room_id, participant, last_message_id, at }
        }
        RoomEvent::Typing { room_id, participant, on } => {
            ServerFrame::Typing { room_id, participant, on }
        }
        RoomEvent::Notify { room_id, message_id, mentioned, by, kind } => {
            ServerFrame::Notify { room_id, message_id, mentioned, by, notify_kind: kind }
        }
        RoomEvent::NotifyBatch { room_id, message_id, by, delivery_id: _, recipients } => {
            let (mentioned, notify_kind) = recipients
                .first()
                .map_or((by, NotificationKind::Mention), |t| (t.participant, t.kind));
            ServerFrame::Notify { room_id, message_id, mentioned, by, notify_kind }
        }
        RoomEvent::Pin { room_id, message_id, by, op } => {
            ServerFrame::Pin { room_id, message_id, by, op }
        }
        RoomEvent::Membership { room_id, participant, op } => {
            ServerFrame::Membership { room_id, participant, op }
        }
        RoomEvent::Call(call) => ServerFrame::Call { event: call },
        RoomEvent::Poll { room_id, poll_id, op } => ServerFrame::Poll { room_id, poll_id, op },
        RoomEvent::MessageSeen { room_id, message_id, participant } => {
            ServerFrame::MessageSeen { room_id, message_id, participant }
        }
        RoomEvent::Interaction { room_id, message_id, participant, action_id } => {
            ServerFrame::Interaction { room_id, message_id, participant, action_id }
        }
    };
    stamped_frame_json(&frame, seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    #[test]
    fn seq_stamp_is_injected_and_omitted() {
        use aero_common::RoomId;
        let frame = ServerFrame::Typing {
            room_id: RoomId::new(),
            participant: aero_common::ParticipantId::new(),
            on: true,
        };
        // With seq.
        let with: serde_json::Value =
            serde_json::from_str(&stamped_frame_json(&frame, Some(9))).unwrap();
        assert_eq!(with.get("type").and_then(|v| v.as_str()), Some("typing"));
        assert_eq!(with.get("seq").and_then(serde_json::Value::as_u64), Some(9));
        // Without seq.
        let without: serde_json::Value =
            serde_json::from_str(&stamped_frame_json(&frame, None)).unwrap();
        assert!(without.get("seq").is_none());
    }
}
