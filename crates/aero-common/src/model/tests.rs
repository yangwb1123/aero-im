use super::*;
use crate::ids::*;
use pretty_assertions::assert_eq;
use time::OffsetDateTime;

#[test]
fn notify_batch_accessors_expose_all_recipients_and_room() {
    let room = RoomId::new();
    let a = ParticipantId::new();
    let b = ParticipantId::new();
    let ev = RoomEvent::NotifyBatch {
        room_id: room,
        message_id: MessageId::new(),
        by: ParticipantId::new(),
        delivery_id: uuid::Uuid::new_v4(),
        recipients: vec![
            NotifyTarget {
                participant: a,
                kind: NotificationKind::Mention,
            },
            NotifyTarget {
                participant: b,
                kind: NotificationKind::Reply,
            },
        ],
    };
    // Fan-out targets every recipient (not just one), and the room resolves —
    // so the bus listener delivers to all of them (ROADMAP 方向二).
    assert_eq!(ev.explicit_recipients(), vec![a, b]);
    assert_eq!(ev.room_id(), Some(room));
}

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
fn block_button_roundtrip() {
    // An action button (no url) with a style.
    let b = Block::Button {
        action_id: "approve".into(),
        label: "Approve".into(),
        style: Some("primary".into()),
        url: None,
    };
    let j = serde_json::to_string(&b).unwrap();
    assert!(j.contains("\"type\":\"button\""));
    assert!(j.contains("\"action_id\":\"approve\""));
    // url is None → skipped on the wire.
    assert!(!j.contains("\"url\""));
    let back: Block = serde_json::from_str(&j).unwrap();
    match &back {
        Block::Button {
            action_id,
            label,
            style,
            url,
        } => {
            assert_eq!(action_id, "approve");
            assert_eq!(label, "Approve");
            assert_eq!(style.as_deref(), Some("primary"));
            assert_eq!(url.as_deref(), None);
        }
        _ => panic!("wrong variant"),
    }
    // A link button (url set, no style) round-trips too.
    let link = Block::Button {
        action_id: "docs".into(),
        label: "Open docs".into(),
        style: None,
        url: Some("https://example.com".into()),
    };
    let lj = serde_json::to_string(&link).unwrap();
    let lback: Block = serde_json::from_str(&lj).unwrap();
    assert!(matches!(
        lback,
        Block::Button { url: Some(u), style: None, .. } if u == "https://example.com"
    ));
}

#[test]
fn block_select_roundtrip() {
    let b = Block::Select {
        action_id: "priority".into(),
        placeholder: Some("Pick one".into()),
        options: vec![
            SelectOption {
                value: "lo".into(),
                label: "Low".into(),
            },
            SelectOption {
                value: "hi".into(),
                label: "High".into(),
            },
        ],
    };
    let j = serde_json::to_string(&b).unwrap();
    assert!(j.contains("\"type\":\"select\""));
    let back: Block = serde_json::from_str(&j).unwrap();
    match &back {
        Block::Select {
            action_id,
            placeholder,
            options,
        } => {
            assert_eq!(action_id, "priority");
            assert_eq!(placeholder.as_deref(), Some("Pick one"));
            assert_eq!(options.len(), 2);
            assert_eq!(options[0].value, "lo");
            assert_eq!(options[1].label, "High");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn interactive_blocks_tolerate_unknown_future_fields() {
    // A forward-compatible payload: a future server adds fields we don't know.
    // serde must ignore them rather than fail (default behavior, asserted here
    // so a future #[serde(deny_unknown_fields)] regression is caught).
    let button_json = r#"{
            "type": "button",
            "action_id": "a1",
            "label": "Go",
            "confirm": {"title": "Sure?"},
            "accessibility_label": "go button"
        }"#;
    let b: Block = serde_json::from_str(button_json).unwrap();
    assert!(matches!(b, Block::Button { action_id, .. } if action_id == "a1"));

    let select_json = r#"{
            "type": "select",
            "action_id": "s1",
            "options": [{"value": "v", "label": "L", "description": "future"}],
            "max_selected": 3
        }"#;
    let s: Block = serde_json::from_str(select_json).unwrap();
    match s {
        Block::Select {
            action_id,
            options,
            placeholder,
        } => {
            assert_eq!(action_id, "s1");
            assert!(placeholder.is_none());
            assert_eq!(options.len(), 1);
            assert_eq!(options[0].value, "v");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn interactive_block_labels_are_searchable() {
    // Button label + Select option labels both feed the search projection.
    let msg = Message {
        id: MessageId::new(),
        room_id: RoomId::new(),
        sender_id: ParticipantId::new(),
        blocks: vec![
            Block::text("Choose a plan"),
            Block::Button {
                action_id: "buy".into(),
                label: "Buy now".into(),
                style: None,
                url: None,
            },
            Block::Select {
                action_id: "tier".into(),
                placeholder: None,
                options: vec![
                    SelectOption {
                        value: "pro".into(),
                        label: "Pro tier".into(),
                    },
                    SelectOption {
                        value: "ent".into(),
                        label: "Enterprise tier".into(),
                    },
                ],
            },
        ],
        reply_to: None,
        metadata: serde_json::Value::Null,
        created_at: OffsetDateTime::UNIX_EPOCH,
        edited_at: None,
        deleted_at: None,
        recalled_at: None,
        recalled_by: None,
        expires_at: None,
        version: 1,
    };
    let text = msg.searchable_text();
    assert!(text.contains("Choose a plan"));
    assert!(
        text.contains("Buy now"),
        "button label searchable: {text:?}"
    );
    assert!(
        text.contains("Pro tier"),
        "select option searchable: {text:?}"
    );
    assert!(text.contains("Enterprise tier"));
}

#[test]
fn message_has_action_matches_only_real_components() {
    let blocks = vec![
        Block::text("not interactive"),
        Block::Button {
            action_id: "click_me".into(),
            label: "Click".into(),
            style: None,
            url: None,
        },
        Block::Select {
            action_id: "pick".into(),
            placeholder: None,
            options: vec![SelectOption {
                value: "a".into(),
                label: "A".into(),
            }],
        },
    ];
    assert!(message_has_action(&blocks, "click_me"));
    assert!(message_has_action(&blocks, "pick"));
    assert!(!message_has_action(&blocks, "missing"));
    // A plain-text-only message has no actions.
    assert!(!message_has_action(&[Block::text("hi")], "click_me"));
    // An empty block list never matches.
    assert!(!message_has_action(&[], "click_me"));
}

#[test]
fn interaction_event_tagged_and_routes_to_room() {
    let room = RoomId::new();
    let ev = RoomEvent::Interaction {
        room_id: room,
        message_id: MessageId::new(),
        participant: ParticipantId::new(),
        action_id: "approve".into(),
    };
    // Fans out to the whole room (poster included) — no explicit recipients.
    assert!(ev.explicit_recipients().is_empty());
    assert_eq!(ev.room_id(), Some(room));
    let j = serde_json::to_string(&ev).unwrap();
    assert!(j.contains("\"kind\":\"interaction\""));
    assert!(j.contains("\"action_id\":\"approve\""));
    let back: RoomEvent = serde_json::from_str(&j).unwrap();
    assert_eq!(back.room_id(), Some(room));
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
        leg_generation: 7,
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
        leg_generation: 7,
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
    let ev = RoomEvent::Membership {
        room_id: room,
        participant: who,
        op: MembershipOp::Join,
    };
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
        RoomEvent::Membership {
            op: MembershipOp::Join,
            ..
        }
    ));

    // Leave round-trips too.
    let leave = RoomEvent::Membership {
        room_id: room,
        participant: who,
        op: MembershipOp::Leave,
    };
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
    assert!(matches!(
        back,
        RoomEvent::Poll {
            op: PollOp::Created,
            ..
        }
    ));

    // Voted / Closed round-trip too.
    let voted = RoomEvent::Poll {
        room_id: room,
        poll_id: crate::ids::PollId::new(),
        op: PollOp::Voted,
    };
    assert!(serde_json::to_string(&voted)
        .unwrap()
        .contains("\"op\":\"voted\""));
    let closed = RoomEvent::Poll {
        room_id: room,
        poll_id: crate::ids::PollId::new(),
        op: PollOp::Closed,
    };
    assert!(serde_json::to_string(&closed)
        .unwrap()
        .contains("\"op\":\"closed\""));
}

#[test]
fn canvas_op_event_roundtrips_without_bus_seq_collision() {
    let room = RoomId::new();
    let canvas = CanvasId::new();
    let op_id = uuid::Uuid::new_v4();
    let author = ParticipantId::new();
    let ev = RoomEvent::CanvasOp {
        room_id: room,
        canvas_id: canvas,
        op_id,
        op_seq: 17,
        author_id: author,
        op: serde_json::json!({"type": "insert", "at": 2, "text": "hi"}),
    };

    assert!(ev.explicit_recipients().is_empty());
    assert_eq!(ev.room_id(), Some(room));

    let wire = serde_json::to_value(&ev).unwrap();
    assert_eq!(wire["kind"], "canvas_op");
    assert_eq!(wire["room_id"], room.to_string());
    assert_eq!(wire["canvas_id"], canvas.to_string());
    assert_eq!(wire["op_id"], op_id.to_string());
    assert_eq!(wire["op_seq"], 17);
    assert_eq!(wire["author_id"], author.to_string());
    assert_eq!(wire["op"]["type"], "insert");
    assert!(
        wire.get("seq").is_none(),
        "the room-bus seq remains a separate envelope stamp"
    );

    let back: RoomEvent = serde_json::from_value(wire).unwrap();
    assert_eq!(back.room_id(), Some(room));
    assert!(matches!(
        back,
        RoomEvent::CanvasOp {
            canvas_id,
            op_id: restored_id,
            op_seq: 17,
            author_id,
            ..
        } if canvas_id == canvas && restored_id == op_id && author_id == author
    ));
}

#[test]
fn searchable_text_concatenates_blocks() {
    let m = Message {
        id: MessageId::new(),
        room_id: RoomId::new(),
        sender_id: ParticipantId::new(),
        blocks: vec![
            Block::text("hello"),
            Block::Code {
                lang: "rs".into(),
                content: "fn main() {}".into(),
            },
            Block::Thought {
                content: "secret".into(),
                hidden: true,
            },
        ],
        reply_to: None,
        metadata: serde_json::Value::Null,
        created_at: time::OffsetDateTime::now_utc(),
        edited_at: None,
        deleted_at: None,
        recalled_at: None,
        recalled_by: None,
        expires_at: None,
        version: 1,
    };
    assert_eq!(m.searchable_text(), "hello\nfn main() {}");
}

#[test]
fn file_attachment_name_is_searchable() {
    let m = Message {
        id: MessageId::new(),
        room_id: RoomId::new(),
        sender_id: ParticipantId::new(),
        blocks: vec![
            Block::text("see attached"),
            Block::File {
                blob_id: BlobId::new(),
                kind: FileKind::Document,
                name: "deploy-runbook.pdf".into(),
                size: 4096,
            },
        ],
        reply_to: None,
        metadata: serde_json::Value::Null,
        created_at: time::OffsetDateTime::now_utc(),
        edited_at: None,
        deleted_at: None,
        recalled_at: None,
        recalled_by: None,
        expires_at: None,
        version: 1,
    };
    let text = m.searchable_text();
    assert!(
        text.contains("deploy-runbook.pdf"),
        "attachment file name is indexed: {text}"
    );
    assert!(
        text.contains("see attached"),
        "surrounding text still indexed"
    );
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

// ---------- Recall (撤回) ----------

/// `RoomEvent::Recalled` is a tuple variant carrying the full updated message:
/// wire tag must be `recalled`, the placeholder body must round-trip, and the
/// accessors must fan out to the whole room (`explicit_recipients` empty) and
/// resolve the room from the carried message.
#[test]
fn recalled_event_roundtrips_and_fans_to_room() {
    let m = Message {
        id: MessageId::new(),
        room_id: RoomId::new(),
        sender_id: ParticipantId::new(),
        blocks: vec![Block::text(RECALLED_MESSAGE_PLACEHOLDER)],
        reply_to: None,
        metadata: serde_json::Value::Null,
        created_at: time::OffsetDateTime::now_utc(),
        edited_at: None,
        deleted_at: None,
        recalled_at: Some(time::OffsetDateTime::now_utc()),
        recalled_by: Some(ParticipantId::new()),
        expires_at: None,
        version: 2,
    };
    let ev = RoomEvent::Recalled(m.clone());
    let j = serde_json::to_string(&ev).unwrap();
    assert!(
        j.contains("\"kind\":\"recalled\""),
        "wire tag is the snake_case discriminant: {j}"
    );
    let back: RoomEvent = serde_json::from_str(&j).unwrap();
    let RoomEvent::Recalled(back_m) = back else {
        panic!("expected Recalled variant");
    };
    assert_eq!(back_m.id, m.id);
    assert_eq!(
        serde_json::to_value(&back_m.blocks).unwrap(),
        serde_json::to_value(&m.blocks).unwrap(),
        "placeholder blocks round-trip"
    );
    assert_eq!(back_m.recalled_at, m.recalled_at);
    assert_eq!(back_m.recalled_by, m.recalled_by);
    assert_eq!(back_m.version, 2);
    assert!(
        ev.explicit_recipients().is_empty(),
        "recall fans out to all room members"
    );
    assert_eq!(ev.room_id(), Some(m.room_id));
    assert!(m.recalled());
}

/// Pre-recall JSON (no `recalled_*` fields) must keep deserializing: the new
/// fields are additive and optional, so old clients/rows are untouched.
#[test]
fn message_deserializes_without_recall_fields() {
    let j = r#"{"id":"01HZXZY0Z0Z0Z0Z0Z0Z0Z0Z0Z0","room_id":"01HZXZY0Z0Z0Z0Z0Z0Z0Z0Z0Z0","sender_id":"01HZXZY0Z0Z0Z0Z0Z0Z0Z0Z0Z0","blocks":[{"type":"text","content":"hi"}],"created_at":"2026-01-01T00:00:00Z","version":1}"#;
    let m: Message = serde_json::from_str(j).unwrap();
    assert!(!m.recalled());
    assert!(m.recalled_at.is_none());
    assert!(m.recalled_by.is_none());
}

/// The system placeholder is a single plain text block; a recalled message's
/// searchable projection is the placeholder text (the DB clears the dedicated
/// `searchable_text` column, so the placeholder never reaches FTS).
#[test]
fn recalled_placeholder_is_single_text_block() {
    let blocks = vec![Block::text(RECALLED_MESSAGE_PLACEHOLDER)];
    let m = Message {
        id: MessageId::new(),
        room_id: RoomId::new(),
        sender_id: ParticipantId::new(),
        blocks,
        reply_to: None,
        metadata: serde_json::Value::Null,
        created_at: time::OffsetDateTime::now_utc(),
        edited_at: None,
        deleted_at: None,
        recalled_at: Some(time::OffsetDateTime::now_utc()),
        recalled_by: Some(ParticipantId::new()),
        expires_at: None,
        version: 1,
    };
    assert_eq!(m.searchable_text(), RECALLED_MESSAGE_PLACEHOLDER);
    assert!(!RECALLED_MESSAGE_PLACEHOLDER.is_empty());
}
