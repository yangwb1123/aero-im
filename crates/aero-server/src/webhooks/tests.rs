use super::*;
use aero_common::{Message, MessageEnvelope, MessageId, ParticipantId};
use aero_storage::FakeSender;

#[test]
fn outgoing_event_filters_are_normalized_validated_and_deduplicated() {
    let raw = vec![
        " Message ".to_owned(),
        "message".to_owned(),
        "REACTION".to_owned(),
        " notify ".to_owned(),
    ];
    assert_eq!(
        normalize_outgoing_events(Some(&raw)).unwrap(),
        vec!["message", "reaction", "notify"]
    );
    assert!(normalize_outgoing_events(None).unwrap().is_empty());
    assert!(normalize_outgoing_events(Some(&[])).unwrap().is_empty());

    let unknown = vec!["notify_batch".to_owned()];
    assert_eq!(
        normalize_outgoing_events(Some(&unknown))
            .unwrap_err()
            .status_code(),
        400
    );
    let too_long = vec!["x".repeat(MAX_EVENT_KIND_CHARS + 1)];
    assert_eq!(
        normalize_outgoing_events(Some(&too_long))
            .unwrap_err()
            .status_code(),
        400
    );
}

#[test]
fn outgoing_event_raw_count_is_bounded_before_deduplication() {
    let repeated = vec!["message".to_owned(); MAX_OUTGOING_EVENT_FILTERS + 1];
    assert_eq!(
        normalize_outgoing_events(Some(&repeated))
            .unwrap_err()
            .status_code(),
        400
    );
}

#[test]
fn webhook_labels_and_incoming_bot_names_are_trimmed_and_bounded() {
    assert_eq!(
        normalize_webhook_label(Some("  deploy hook  ")).unwrap(),
        Some("deploy hook".to_owned())
    );
    assert_eq!(normalize_webhook_label(Some(" \n ")).unwrap(), None);
    assert_eq!(normalize_webhook_label(None).unwrap(), None);
    assert!(normalize_webhook_label(Some(&"界".repeat(MAX_WEBHOOK_LABEL_CHARS + 1))).is_err());

    assert_eq!(
        crate::routes::agents::validate_bot_name("  Incoming Bot  ").unwrap(),
        "Incoming Bot"
    );
    assert!(crate::routes::agents::validate_bot_name(&"界".repeat(65)).is_err());
    assert!(crate::routes::agents::validate_bot_name("   ").is_err());
}

#[test]
fn webhook_write_errors_map_to_http_contract() {
    assert_eq!(
        map_webhook_write_error(WebhookWriteError::FixedMembership, "room").status_code(),
        400
    );
    assert_eq!(
        map_webhook_write_error(WebhookWriteError::Forbidden, "webhook").status_code(),
        403
    );
    assert_eq!(
        map_webhook_write_error(WebhookWriteError::NotFound, "webhook").status_code(),
        404
    );
    assert_eq!(
        map_webhook_write_error(
            WebhookWriteError::Storage(sqlx::Error::RowNotFound),
            "webhook"
        )
        .status_code(),
        500
    );
}

#[test]
fn webhook_ip_block_list_rejects_internal_allows_public() {
    use std::net::IpAddr;
    let blocked = [
        "127.0.0.1",
        "10.1.2.3",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.169.254",
        "0.0.0.0",
        "::1",
        "fe80::1",
        "fc00::1",
        "::ffff:127.0.0.1",
    ];
    for ip in blocked {
        assert!(
            aero_storage::webhook::webhook_ip_is_blocked(ip.parse::<IpAddr>().unwrap()),
            "{ip} must be blocked"
        );
    }
    let allowed = [
        "8.8.8.8",
        "1.1.1.1",
        "93.184.216.34",
        "2606:4700:4700::1111",
    ];
    for ip in allowed {
        assert!(
            !aero_storage::webhook::webhook_ip_is_blocked(ip.parse::<IpAddr>().unwrap()),
            "{ip} must be allowed"
        );
    }
}

#[tokio::test]
async fn webhook_url_safety_rejects_loopback_and_localhost() {
    assert!(assert_webhook_url_safe("http://127.0.0.1:8080/x")
        .await
        .is_err());
    assert!(
        assert_webhook_url_safe("http://169.254.169.254/latest/meta-data/")
            .await
            .is_err()
    );
    assert!(assert_webhook_url_safe("http://[::1]:443/").await.is_err());
    assert!(assert_webhook_url_safe("http://localhost/hook")
        .await
        .is_err());
    assert!(assert_webhook_url_safe("http://user:pass@10.0.0.5/hook")
        .await
        .is_err());
    assert!(assert_webhook_url_safe("https://internal.local/hook")
        .await
        .is_err());
    assert!(assert_webhook_url_safe("https://8.8.8.8/hook")
        .await
        .is_ok());
}

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
        recalled_at: None,
        recalled_by: None,
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
fn body_text_becomes_single_text_block() {
    let blocks = blocks_from_body(IncomingBody {
        text: Some("  hello  ".into()),
        blocks: None,
    })
    .unwrap();
    assert_eq!(blocks.len(), 1);
    match &blocks[0] {
        Block::Text { content, .. } => assert_eq!(content, "hello"),
        other => panic!("expected text block, got {other:?}"),
    }
}

#[test]
fn body_blocks_pass_through_and_text_wins_over_blocks() {
    let blocks = blocks_from_body(IncomingBody {
        text: Some("win".into()),
        blocks: Some(vec![Block::text("lose")]),
    })
    .unwrap();
    assert_eq!(blocks.len(), 1);
    match &blocks[0] {
        Block::Text { content, .. } => assert_eq!(content, "win"),
        other => panic!("expected text block, got {other:?}"),
    }
    let only = blocks_from_body(IncomingBody {
        text: None,
        blocks: Some(vec![Block::text("a"), Block::text("b")]),
    })
    .unwrap();
    assert_eq!(only.len(), 2);
}

#[test]
fn body_empty_is_rejected_as_invalid() {
    let err = blocks_from_body(IncomingBody {
        text: None,
        blocks: None,
    })
    .unwrap_err();
    assert_eq!(err.status_code(), 400);
    let err = blocks_from_body(IncomingBody {
        text: Some("   ".into()),
        blocks: Some(vec![]),
    })
    .unwrap_err();
    assert_eq!(err.status_code(), 400);
}

#[test]
fn event_kind_matches_wire_discriminant() {
    assert_eq!(event_kind(&message_event(RoomId::new())), "message");
    let room = RoomId::new();
    assert_eq!(
        event_kind(&RoomEvent::Typing {
            room_id: room,
            participant: ParticipantId::new(),
            on: true,
        }),
        "typing"
    );
    let ev = message_event(room);
    let json = serde_json::to_value(&ev).unwrap();
    assert_eq!(json["kind"], event_kind(&ev));
}

#[tokio::test]
async fn dispatch_skips_events_without_a_room() {
    use aero_common::{CallEvent, CallId};
    let sender = FakeSender::new(200);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://u:p@localhost/db")
        .unwrap();
    let repo = WebhookRepo::new(pool.clone());
    let deliveries = WebhookDeliveryRepo::new(pool);
    let answer = RoomEvent::Call(CallEvent::Answer {
        call_id: CallId::new(),
        from: ParticipantId::new(),
        to: ParticipantId::new(),
        sdp: String::new(),
    });
    let limiter = DeliveryLimiter::new(runtime::DeliveryConcurrency::new(2, 1));
    assert!(
        dispatch_event(
            &repo,
            &deliveries,
            &sender,
            &limiter,
            &CancellationToken::new(),
            &answer,
            0,
        )
        .await
    );
    assert!(sender.calls().is_empty());
}

#[test]
fn is_success_only_for_2xx() {
    assert!(is_success(200));
    assert!(is_success(202));
    assert!(is_success(299));
    assert!(!is_success(199));
    assert!(!is_success(300));
    assert!(!is_success(404));
    assert!(!is_success(500));
}

#[test]
fn claim_defer_deadline_never_precedes_breaker_gate() {
    let deadline = defer_deadline(100, 0, Some(500));
    assert_eq!(deadline.unix_timestamp(), 500);
    assert_eq!(
        defer_deadline(100, 2, None).unix_timestamp(),
        160,
        "without a breaker gate the HTTP-attempt backoff applies"
    );
    assert_eq!(
        defer_deadline(100, 3, Some(150)).unix_timestamp(),
        220,
        "a shorter breaker gate cannot reduce ordinary backoff"
    );
}

#[test]
fn correlation_id_is_message_id_for_messages_else_none() {
    let ev = message_event(RoomId::new());
    let RoomEvent::Message(env) = &ev else {
        unreachable!()
    };
    assert_eq!(event_correlation_id(&ev), Some(env.message.id.to_string()));
    let typing = RoomEvent::Typing {
        room_id: RoomId::new(),
        participant: ParticipantId::new(),
        on: true,
    };
    assert_eq!(event_correlation_id(&typing), None);
}
