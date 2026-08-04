use super::*;
use aero_common::{Block, CallEvent, CallId, Message, MessageEnvelope, MessageId, ParticipantId};

fn message_event(room: RoomId) -> RoomEvent {
    let msg = Message {
        id: MessageId::new(),
        room_id: room,
        sender_id: ParticipantId::new(),
        blocks: vec![Block::text("hi")],
        reply_to: None,
        metadata: serde_json::Value::Null,
        created_at: time::OffsetDateTime::UNIX_EPOCH,
        edited_at: None,
        deleted_at: None,
        expires_at: None,
        version: 1,
    };
    RoomEvent::Message(MessageEnvelope {
        message: msg,
        delivery_ordinal: Some(1),
        client_message_id: None,
        recipients: Vec::new(),
    })
}

#[test]
fn event_type_matches_wire_discriminant() {
    let room = RoomId::new();
    let ev = message_event(room);
    assert_eq!(event_type(&ev), "message");
    let json = serde_json::to_value(&ev).unwrap();
    assert_eq!(json["kind"], event_type(&ev));

    assert_eq!(
        event_type(&RoomEvent::Interaction {
            room_id: room,
            message_id: MessageId::new(),
            participant: ParticipantId::new(),
            action_id: "approve".into(),
        }),
        "interaction"
    );
    assert_eq!(
        event_type(&RoomEvent::Typing {
            room_id: room,
            participant: ParticipantId::new(),
            on: true,
        }),
        "typing"
    );
}

#[test]
fn action_id_only_for_interaction() {
    let room = RoomId::new();
    let interaction = RoomEvent::Interaction {
        room_id: room,
        message_id: MessageId::new(),
        participant: ParticipantId::new(),
        action_id: "btn_approve".into(),
    };
    assert_eq!(event_action_id(&interaction), Some("btn_approve"));
    assert_eq!(event_action_id(&message_event(room)), None);
}

#[test]
fn empty_filter_matches_everything() {
    let room = RoomId::new();
    assert!(filter_matches(&serde_json::json!({}), room, None, None));
    assert!(!filter_matches(&serde_json::Value::Null, room, None, None));
    assert!(!filter_matches(
        &serde_json::json!("garbage"),
        room,
        None,
        None
    ));
}

#[test]
fn room_filter_matches_only_its_room() {
    let room = RoomId::new();
    let other = RoomId::new();
    let filter = serde_json::json!({ "room_id": room.to_string() });
    assert!(filter_matches(&filter, room, None, None));
    assert!(!filter_matches(&filter, other, None, None));
}

#[test]
fn workspace_filter_requires_resolved_matching_workspace() {
    let room = RoomId::new();
    let workspace = WorkspaceId::new();
    let other_workspace = WorkspaceId::new();
    let filter = serde_json::json!({ "workspace_id": workspace.to_string() });
    assert!(filter_matches(&filter, room, Some(workspace), None));
    assert!(!filter_matches(&filter, room, Some(other_workspace), None));
    assert!(!filter_matches(&filter, room, None, None));
}

#[test]
fn action_id_filter_gates_interactions() {
    let room = RoomId::new();
    let filter = serde_json::json!({ "action_id": "approve" });
    assert!(filter_matches(&filter, room, None, Some("approve")));
    assert!(!filter_matches(&filter, room, None, Some("reject")));
    assert!(!filter_matches(&filter, room, None, None));
}

#[test]
fn multiple_filter_keys_are_anded() {
    let room = RoomId::new();
    let workspace = WorkspaceId::new();
    let filter = serde_json::json!({
        "room_id": room.to_string(),
        "workspace_id": workspace.to_string(),
        "action_id": "go",
    });
    assert!(filter_matches(&filter, room, Some(workspace), Some("go")));
    assert!(!filter_matches(
        &filter,
        RoomId::new(),
        Some(workspace),
        Some("go")
    ));
    assert!(!filter_matches(
        &filter,
        room,
        Some(WorkspaceId::new()),
        Some("go")
    ));
    assert!(!filter_matches(&filter, room, Some(workspace), Some("no")));
}

#[test]
fn malformed_present_filter_fields_fail_closed() {
    let room = RoomId::new();
    let workspace = WorkspaceId::new();
    assert!(!filter_matches(
        &serde_json::json!({ "room_id": 7 }),
        room,
        Some(workspace),
        None
    ));
    assert!(!filter_matches(
        &serde_json::json!({ "workspace_id": true }),
        room,
        Some(workspace),
        None
    ));
    assert!(!filter_matches(
        &serde_json::json!({ "action_id": ["approve"] }),
        room,
        Some(workspace),
        Some("approve")
    ));
}

#[test]
fn roomless_event_has_no_subscription_scope() {
    let answer = RoomEvent::Call(CallEvent::Answer {
        call_id: CallId::new(),
        from: ParticipantId::new(),
        to: ParticipantId::new(),
        sdp: String::new(),
    });
    assert!(answer.room_id().is_none());
}

#[test]
fn raw_producer_event_id_survives_delivery_bytes() {
    let event_id = Uuid::new_v4();
    let room = RoomId::new();
    let participant = ParticipantId::new();
    let raw = serde_json::to_vec(&serde_json::json!({
        "kind": "typing",
        "room_id": room,
        "participant": participant,
        "on": true,
        "event_id": event_id,
        "seq": 9
    }))
    .unwrap();
    assert_eq!(
        crate::consumer_event_receipt::extract_event_id(&raw),
        Some(event_id)
    );
    let typed: RoomEvent = serde_json::from_slice(&raw).unwrap();
    assert!(
        serde_json::to_value(typed)
            .unwrap()
            .get("event_id")
            .is_none(),
        "typed decoding intentionally drops producer envelope metadata"
    );

    let delivery = build_delivery_from_bytes(
        "https://example.test/hook",
        "secret",
        &raw,
        &[("Content-Type".into(), "application/json".into())],
        123,
    );
    assert_eq!(delivery.body, raw);
    let delivered: serde_json::Value = serde_json::from_slice(&delivery.body).unwrap();
    assert_eq!(delivered["event_id"], event_id.to_string());
}

#[test]
fn retention_defaults_keep_dlq_and_attempt_history_for_ninety_days() {
    let config = RetentionConfig::from_values(None, None, None);
    assert_eq!(
        config.delivered_age,
        Duration::days(DEFAULT_DELIVERED_RETENTION_DAYS)
    );
    assert_eq!(config.dlq_age, Duration::days(90));
    assert_eq!(config.dlq_age, Duration::days(DEFAULT_DLQ_RETENTION_DAYS));
    assert_eq!(config.interval, StdDuration::from_secs(DEFAULT_SWEEP_SECS));

    let now = OffsetDateTime::UNIX_EPOCH + Duration::days(200);
    let (delivered_cutoff, dlq_cutoff) = config.cutoffs(now);
    assert_eq!(
        delivered_cutoff,
        now - Duration::days(DEFAULT_DELIVERED_RETENTION_DAYS)
    );
    assert_eq!(dlq_cutoff, now - Duration::days(DEFAULT_DLQ_RETENTION_DAYS));
}

#[test]
fn retention_env_values_are_parsed_and_bounded_without_global_env_mutation() {
    let configured = RetentionConfig::from_values(Some("14"), Some("120"), Some("900"));
    assert_eq!(configured.delivered_age, Duration::days(14));
    assert_eq!(configured.dlq_age, Duration::days(120));
    assert_eq!(configured.interval, StdDuration::from_secs(900));

    let bounded = RetentionConfig::from_values(Some("0"), Some("99999"), Some("1"));
    assert_eq!(bounded.delivered_age, Duration::days(1));
    assert_eq!(bounded.dlq_age, Duration::days(3_650));
    assert_eq!(bounded.interval, StdDuration::from_secs(60));

    let invalid = RetentionConfig::from_values(Some("bad"), Some("bad"), Some("bad"));
    assert_eq!(
        invalid.delivered_age,
        Duration::days(DEFAULT_DELIVERED_RETENTION_DAYS)
    );
    assert_eq!(invalid.dlq_age, Duration::days(DEFAULT_DLQ_RETENTION_DAYS));
    assert_eq!(invalid.interval, StdDuration::from_secs(DEFAULT_SWEEP_SECS));
}

#[test]
fn candidate_truncation_metric_has_a_stable_server_local_name() {
    assert_eq!(
        BOT_CANDIDATE_TRUNCATIONS_TOTAL,
        "aero_bot_candidate_truncations_total"
    );
    metrics::inc_counter(BOT_CANDIDATE_TRUNCATIONS_TOTAL, 1);
    assert!(metrics::render_prometheus().contains(BOT_CANDIDATE_TRUNCATIONS_TOTAL));
}
