//! `send_markdown` frame serde tests, moved verbatim from `ws_impl`.
//! Loaded via `#[cfg(test)] mod send_markdown_tests;` so the module name is
//! preserved (`ws::ws_impl::send_markdown_tests`). This file IS the module
//! body, so the inner items are reproduced exactly (indentation included).

use super::{
    server_shutdown_close_frame, ClientFrame, DeliveryRoomBarrier, ServerFrame, WS_CAPABILITIES,
};
use aero_common::{
    markdown::parse_markdown_to_blocks, Block, Message, MessageId, ParticipantId, RoomId, SpanStyle,
};

/// The new `send_markdown` frame deserializes with the documented field set
/// (markdown text + optional reply_to / expires_after_secs) and is distinct
/// from `send_message`. Pure serde round-trip — no I/O, no `AppState`.
#[test]
fn send_markdown_frame_deserializes() {
    // `RoomId` is serde-transparent over `Ulid`, so the wire value is a ULID
    // string (matching the other ws frame tests), not a UUID.
    let frame: ClientFrame = serde_json::from_str(
        r#"{
                "type":"send_markdown",
                "room_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "markdown":"hello **world**",
                "expires_after_secs":60
            }"#,
    )
    .expect("parse send_markdown frame");
    match frame {
        ClientFrame::SendMarkdown {
            markdown,
            reply_to,
            expires_after_secs,
            ..
        } => {
            assert_eq!(markdown, "hello **world**");
            assert!(reply_to.is_none(), "reply_to defaults to None when absent");
            assert_eq!(expires_after_secs, Some(60));
        }
        _ => panic!("expected SendMarkdown variant"),
    }
}

#[test]
fn client_message_id_and_ack_capability_are_explicit_on_the_wire() {
    let client_message_id = uuid::Uuid::new_v4();
    let frame: ClientFrame = serde_json::from_value(serde_json::json!({
        "type": "send_message",
        "room_id": RoomId::new(),
        "blocks": [{"type": "text", "content": "hello"}],
        "client_message_id": client_message_id,
    }))
    .unwrap();
    match frame {
        ClientFrame::SendMessage {
            client_message_id: parsed,
            ..
        } => {
            assert_eq!(parsed, Some(client_message_id));
        }
        _ => panic!("expected SendMessage"),
    }

    let welcome = serde_json::to_value(ServerFrame::Welcome {
        participant: ParticipantId::new(),
        capabilities: WS_CAPABILITIES,
    })
    .unwrap();
    let capabilities = welcome["capabilities"]
        .as_array()
        .expect("capability array");
    assert!(capabilities.iter().any(|value| value == "message_ack_v1"));
    assert!(capabilities
        .iter()
        .any(|value| value == "delivery_cursor_v2"));
    assert!(capabilities.iter().any(|value| value == "call_sfu_v1"));
    assert!(capabilities.iter().any(|value| value == "call_sfu_v2"));
    let room = RoomId::new();
    let ready = serde_json::to_value(ServerFrame::DeliveryReady {
        rooms: vec![DeliveryRoomBarrier {
            room_id: room,
            delivery_ordinal: 42,
        }],
    })
    .unwrap();
    assert_eq!(
        ready,
        serde_json::json!({
            "type": "delivery_ready",
            "rooms": [{"room_id": room, "delivery_ordinal": 42}]
        })
    );

    let message = Message {
        id: MessageId::new(),
        room_id: RoomId::new(),
        sender_id: ParticipantId::new(),
        blocks: vec![Block::text("hello")],
        reply_to: None,
        metadata: serde_json::Value::Null,
        created_at: time::OffsetDateTime::now_utc(),
        edited_at: None,
        deleted_at: None,
        expires_at: None,
        version: 1,
    };
    let ack = serde_json::to_value(ServerFrame::MessageAck {
        client_message_id,
        message,
        deduplicated: true,
    })
    .unwrap();
    assert_eq!(ack["type"], "message_ack");
    assert_eq!(ack["client_message_id"], client_message_id.to_string());
    assert_eq!(ack["deduplicated"], true);
}

#[test]
fn server_owned_sfu_signaling_has_distinct_wire_frames() {
    let call_id = aero_common::CallId::new();
    let room_id = RoomId::new();
    let offer: ClientFrame = serde_json::from_value(serde_json::json!({
        "type": "call_sfu_offer",
        "call_id": call_id,
        "room_id": room_id,
        "sdp": "v=0\r\n",
    }))
    .expect("parse explicit SFU offer");
    assert!(matches!(
        offer,
        ClientFrame::CallSfuOffer {
            call_id: parsed_call,
            room_id: parsed_room,
            ..
        } if parsed_call == call_id && parsed_room == room_id
    ));

    let answer = serde_json::to_value(ServerFrame::CallSfuAnswer {
        call_id,
        sdp: "v=0\r\n".into(),
        local_addr: "127.0.0.1:5000".into(),
        mids: vec!["0".into(), "1".into()],
        session_generation: 9,
        revision: 3,
        publishers: Vec::new(),
        required_recv_slots: 4,
    })
    .unwrap();
    assert_eq!(answer["type"], "call_sfu_answer");
    assert_eq!(answer["mids"], serde_json::json!(["0", "1"]));
    assert_eq!(answer["revision"], 3);
    assert_eq!(answer["required_recv_slots"], 4);

    let ack = serde_json::to_value(ServerFrame::CallSfuIceAck {
        call_id,
        session_generation: 9,
    })
    .unwrap();
    assert_eq!(ack["type"], "call_sfu_ice_ack");

    let publisher = ParticipantId::new();
    let subscribe: ClientFrame = serde_json::from_value(serde_json::json!({
        "type": "call_sfu_subscribe",
        "call_id": call_id,
        "room_id": room_id,
        "session_generation": 9,
        "revision": 3,
        "tracks": [{
            "publisher": publisher,
            "pub_mid": "0",
            "out_mid": "recv-audio-1"
        }]
    }))
    .expect("parse explicit publisher-scoped route");
    assert!(matches!(
        subscribe,
        ClientFrame::CallSfuSubscribe {
            revision: 3,
            tracks,
            ..
        } if tracks[0].publisher == publisher && tracks[0].out_mid == "recv-audio-1"
    ));
}

#[test]
fn graceful_shutdown_uses_rfc6455_going_away_close() {
    let axum::extract::ws::Message::Close(Some(frame)) = server_shutdown_close_frame() else {
        panic!("shutdown must produce a Close frame");
    };
    assert_eq!(frame.code, 1001);
    assert_eq!(frame.reason, "server shutdown");
}

/// The edge transform the `SendMarkdown` arm performs before the shared
/// dispatch: the markdown body parses into exactly the `Vec<Block>` the
/// structured `SendMessage` path would have carried. Verifies the bold span
/// survives so the rich-text actually reaches `send_message`.
#[test]
fn send_markdown_body_parses_to_expected_blocks() {
    let blocks = parse_markdown_to_blocks("hello **world**");
    assert_eq!(blocks.len(), 1);
    match &blocks[0] {
        Block::Text { content, spans } => {
            assert_eq!(content, "hello world");
            assert_eq!(spans.len(), 1);
            assert_eq!((spans[0].start, spans[0].end), (6, 11));
            assert!(matches!(spans[0].style, SpanStyle::Bold));
        }
        other => panic!("expected text block, got {other:?}"),
    }
}

/// @mention follow-up contract: a leading `@name` parses to a nil-id
/// `Block::Mention` marker (display-name → ParticipantId resolution is a
/// deferred follow-up), and any trailing text still lands verbatim so the
/// message body is never lost.
#[test]
fn send_markdown_mention_lands_as_nil_id_marker() {
    let blocks = parse_markdown_to_blocks("@alice ping");
    assert_eq!(blocks.len(), 2);
    match &blocks[0] {
        Block::Mention { participant } => {
            assert_eq!(
                *participant,
                ParticipantId::nil(),
                "mention id is nil pending resolution"
            );
        }
        other => panic!("expected mention block, got {other:?}"),
    }
    assert!(
        matches!(&blocks[1], Block::Text { .. }),
        "trailing text preserved"
    );
}
