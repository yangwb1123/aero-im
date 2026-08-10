//! JSON frame serialization for WebSocket messages.
//!
//! Extracted from `ws/ws_impl.rs` as part of `REFACTOR_PLAN.md` Step 4.
//! Uses `ServerFrame` and `Message` from the parent module.

use aero_common::{NotificationKind, RoomEvent};

use super::ws_impl::ServerFrame;

/// Stamp a bus seq onto a `ServerFrame` JSON value and serialize.
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
        RoomEvent::Message(env) => ServerFrame::Message {
            message: env.message,
            delivery_ordinal: env.delivery_ordinal,
            client_message_id: env.client_message_id,
        },
        RoomEvent::Edited(m) => ServerFrame::Edited { message: m },
        RoomEvent::Recalled(m) => ServerFrame::Recalled { message: m },
        RoomEvent::Deleted {
            room_id,
            message_id,
            by,
        } => ServerFrame::Deleted {
            room_id,
            message_id,
            by,
        },
        RoomEvent::Reaction {
            room_id,
            message_id,
            participant,
            emoji,
            op,
        } => ServerFrame::Reaction {
            room_id,
            message_id,
            participant,
            emoji,
            op,
        },
        RoomEvent::Read {
            room_id,
            participant,
            last_message_id,
            at,
        } => ServerFrame::Read {
            room_id,
            participant,
            last_message_id,
            at,
        },
        RoomEvent::Typing {
            room_id,
            participant,
            on,
        } => ServerFrame::Typing {
            room_id,
            participant,
            on,
        },
        RoomEvent::Notify {
            room_id,
            message_id,
            mentioned,
            by,
            kind,
        } => ServerFrame::Notify {
            room_id,
            message_id,
            mentioned,
            by,
            notify_kind: kind,
        },
        RoomEvent::NotifyBatch {
            room_id,
            message_id,
            by,
            delivery_id: _,
            recipients,
        } => {
            let (mentioned, notify_kind) = recipients
                .first()
                .map_or((by, NotificationKind::Mention), |t| (t.participant, t.kind));
            ServerFrame::Notify {
                room_id,
                message_id,
                mentioned,
                by,
                notify_kind,
            }
        }
        RoomEvent::Pin {
            room_id,
            message_id,
            by,
            op,
        } => ServerFrame::Pin {
            room_id,
            message_id,
            by,
            op,
        },
        RoomEvent::Membership {
            room_id,
            participant,
            op,
        } => ServerFrame::Membership {
            room_id,
            participant,
            op,
        },
        RoomEvent::Call(call) => ServerFrame::Call { event: call },
        RoomEvent::Poll {
            room_id,
            poll_id,
            op,
        } => ServerFrame::Poll {
            room_id,
            poll_id,
            op,
        },
        RoomEvent::CanvasOp {
            room_id,
            canvas_id,
            op_id,
            op_seq,
            author_id,
            op,
        } => ServerFrame::CanvasOp {
            room_id,
            canvas_id,
            op_id,
            op_seq,
            author_id,
            op,
        },
        RoomEvent::MessageSeen {
            room_id,
            message_id,
            participant,
        } => ServerFrame::MessageSeen {
            room_id,
            message_id,
            participant,
        },
        RoomEvent::Interaction {
            room_id,
            message_id,
            participant,
            action_id,
        } => ServerFrame::Interaction {
            room_id,
            message_id,
            participant,
            action_id,
        },
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

    #[test]
    fn canvas_frame_keeps_room_bus_seq_separate_from_persisted_op_seq() {
        let room = aero_common::RoomId::new();
        let canvas = aero_common::CanvasId::new();
        let op_id = uuid::Uuid::new_v4();
        let author = aero_common::ParticipantId::new();
        let event = RoomEvent::CanvasOp {
            room_id: room,
            canvas_id: canvas,
            op_id,
            op_seq: 23,
            author_id: author,
            op: serde_json::json!({"type": "insert", "text": "hello"}),
        };

        let frame: serde_json::Value =
            serde_json::from_str(&room_event_to_frame_json(&event, Some(91))).unwrap();
        assert_eq!(frame["type"], "canvas_op");
        assert_eq!(frame["room_id"], room.to_string());
        assert_eq!(frame["canvas_id"], canvas.to_string());
        assert_eq!(frame["op_id"], op_id.to_string());
        assert_eq!(frame["author_id"], author.to_string());
        assert_eq!(frame["seq"], 91, "room-bus delivery order");
        assert_eq!(frame["op_seq"], 23, "durable per-canvas recovery cursor");
        assert_eq!(frame["op"]["type"], "insert");
    }
}

/// Contract test (acceptance point): a `Recalled` room event must serialize
/// to the exact WS wire shape the web client renders — `type:"recalled"`,
/// the full updated message with the system placeholder body, `recalled_at`
/// / `recalled_by` present, and the bus `seq` stamped — and must not be
/// confused with the `edited` frame.
#[test]
fn recalled_frame_shape_carries_placeholder_message() {
    use aero_common::{Block, Message, MessageId, ParticipantId, RECALLED_MESSAGE_PLACEHOLDER};

    let recalled_at = time::OffsetDateTime::now_utc();
    let message = Message {
        id: MessageId::new(),
        room_id: aero_common::RoomId::new(),
        sender_id: ParticipantId::new(),
        blocks: vec![Block::text(RECALLED_MESSAGE_PLACEHOLDER)],
        reply_to: None,
        metadata: serde_json::Value::Null,
        created_at: time::OffsetDateTime::UNIX_EPOCH,
        edited_at: None,
        deleted_at: None,
        recalled_at: Some(recalled_at),
        recalled_by: Some(ParticipantId::new()),
        expires_at: None,
        version: 2,
    };
    let frame: serde_json::Value = serde_json::from_str(&room_event_to_frame_json(
        &aero_common::RoomEvent::Recalled(message.clone()),
        Some(7),
    ))
    .unwrap();

    assert_eq!(frame["type"], "recalled", "frame discriminant");
    assert_eq!(frame["seq"], 7, "bus seq stamped for at-least-once dedup");
    assert_eq!(frame["message"]["id"], message.id.to_string());
    assert_eq!(frame["message"]["room_id"], message.room_id.to_string());
    assert_eq!(
        frame["message"]["blocks"][0]["content"], RECALLED_MESSAGE_PLACEHOLDER,
        "clients render the placeholder directly from the event"
    );
    assert!(
        frame["message"]["recalled_at"].is_string(),
        "recalled_at serialized (RFC3339)"
    );
    assert_eq!(
        frame["message"]["recalled_by"],
        message.recalled_by.unwrap().to_string()
    );
    assert_eq!(frame["message"]["version"], 2);
    assert!(
        frame["message"]["deleted_at"].is_null(),
        "recall is not a tombstone"
    );

    // The `edited` frame must remain distinguishable.
    let edited: serde_json::Value = serde_json::from_str(&room_event_to_frame_json(
        &aero_common::RoomEvent::Edited(message),
        None,
    ))
    .unwrap();
    assert_eq!(edited["type"], "edited");
}
