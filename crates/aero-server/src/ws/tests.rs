use crate::ws::ws_impl::{
    access_participant, authoritative_count, backfill_room_ids, cursor_backfill_plan,
    initial_backfill_page, parse_resume_cursor, same_lang, truncation_cursor, ClientFrame,
    BACKFILL_PER_ROOM_LIMIT, INITIAL_BACKFILL_PER_ROOM_LIMIT,
};
use aero_auth::{Claims, TokenKind};
use aero_common::{MessageId, ParticipantId, Room, RoomId, RoomKind};
use aero_storage::DeliveryCursor;
use ulid::Ulid;

#[test]
fn websocket_accepts_only_access_claims() {
    let participant = ParticipantId::new();
    let mut claims = Claims {
        sub: participant.to_string(),
        iss: "aero-im".into(),
        iat: 1,
        exp: u64::MAX,
        kind: TokenKind::Access,
        jti: "test".into(),
        sid: Some(aero_common::SessionId::new().to_string()),
    };
    assert_eq!(access_participant(&claims).unwrap(), participant);
    claims.sid = None;
    assert!(access_participant(&claims).is_err());
    claims.sid = Some(aero_common::SessionId::new().to_string());
    claims.kind = TokenKind::Refresh;
    assert!(access_participant(&claims).is_err());
}

#[test]
fn watch_stream_frame_carries_optional_since_cursor() {
    // Legacy client (no `since`) → None, so the replay behaviour is unchanged.
    let without: ClientFrame =
        serde_json::from_str(r#"{"type":"watch_stream","stream_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV"}"#)
            .expect("parse watch_stream without since");
    match without {
        ClientFrame::WatchStream { since, .. } => assert!(since.is_none()),
        _ => panic!("expected WatchStream"),
    }
    // With `since` → carried through so the watch handler can replay catch-up.
    let with: ClientFrame = serde_json::from_str(
            r#"{"type":"watch_stream","stream_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","since":"01ARZ3NDEKTSV4RRFFQ69G5FZZ"}"#,
        )
        .expect("parse watch_stream with since");
    match with {
        ClientFrame::WatchStream { since, .. } => {
            assert_eq!(since.as_deref(), Some("01ARZ3NDEKTSV4RRFFQ69G5FZZ"));
        }
        _ => panic!("expected WatchStream"),
    }
}

#[test]
fn same_lang_matches_on_primary_subtag() {
    assert!(same_lang("en", "en-US"));
    assert!(same_lang("zh-CN", "zh-Hans"));
    assert!(same_lang("EN", "en"));
    assert!(!same_lang("en", "zh"));
    assert!(!same_lang("zh-CN", "en-US"));
}

#[test]
fn resume_cursor_is_best_effort() {
    // Absent and malformed cursors both degrade to "no backfill" (None), never
    // an error — backfill is purely additive and must not fail the upgrade.
    assert!(parse_resume_cursor(None).is_none());
    assert!(parse_resume_cursor(Some("")).is_none());
    assert!(parse_resume_cursor(Some("not-a-ulid")).is_none());
    // A valid id round-trips, with surrounding whitespace tolerated.
    let id = MessageId::new();
    assert_eq!(parse_resume_cursor(Some(&id.to_string())), Some(id));
    assert_eq!(parse_resume_cursor(Some(&format!("  {id}  "))), Some(id));
}

#[test]
fn authoritative_count_prefers_redis_then_falls_back() {
    // Redis Ok is authoritative even when it disagrees with the local count.
    assert_eq!(authoritative_count(Ok(7), 3), 7);
    // Redis error ⇒ fall back to the process-local count (no worse than today).
    assert_eq!(authoritative_count(Err(anyhow::anyhow!("down")), 3), 3);
    // A count that overflows u32 saturates rather than wrapping/panicking.
    assert_eq!(
        authoritative_count(Ok(u64::from(u32::MAX) + 1), 0),
        u32::MAX
    );
    // Zero from Redis is honored (e.g. last viewer just left, cluster-wide).
    assert_eq!(authoritative_count(Ok(0), 9), 0);
}

fn room_with_id(id: RoomId) -> Room {
    Room {
        id,
        kind: RoomKind::Group,
        name: None,
        created_by: ParticipantId::new(),
        created_at: time::OffsetDateTime::UNIX_EPOCH,
    }
}

#[test]
fn backfill_selects_every_room_id_in_order() {
    // Empty membership ⇒ nothing to replay.
    assert!(backfill_room_ids(&[]).is_empty());
    // Otherwise: exactly the ids of every room the participant belongs to,
    // order-preserving (so replay follows the membership listing order).
    let a = RoomId::new();
    let b = RoomId::new();
    let c = RoomId::new();
    let rooms = [room_with_id(a), room_with_id(b), room_with_id(c)];
    assert_eq!(backfill_room_ids(&rooms), vec![a, b, c]);
}

#[test]
fn backfill_per_room_limit_matches_keyset_window() {
    // The replay cap equals the storage keyset clamp ceiling, so a single
    // reconnect never over-replays beyond one page per room.
    assert_eq!(BACKFILL_PER_ROOM_LIMIT, 200);
    // Sanity: it is a usable id-orderable cursor type (compile-time check that
    // the backfill path keys on a time-sortable MessageId/Ulid).
    let _ = Ulid::new();
    assert_eq!(
        INITIAL_BACKFILL_PER_ROOM_LIMIT, 50,
        "a cursor-less room receives one normal-size newest-history page"
    );
}

#[test]
fn cursor_backfill_plan_includes_rooms_without_a_cursor() {
    let participant = ParticipantId::new();
    let with_cursor = RoomId::new();
    let first_connection = RoomId::new();
    let another_first_connection = RoomId::new();
    let stale_left_room = RoomId::new();
    let message = MessageId::new();
    let cursors = [
        DeliveryCursor {
            room_id: with_cursor,
            participant_id: participant,
            last_delivered_message_id: message,
            last_delivery_ordinal: 42,
            last_seq: 10,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        },
        DeliveryCursor {
            room_id: stale_left_room,
            participant_id: participant,
            last_delivered_message_id: MessageId::new(),
            last_delivery_ordinal: 99,
            last_seq: 99,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        },
    ];
    assert_eq!(
            cursor_backfill_plan(
                &[
                    with_cursor,
                    first_connection,
                    another_first_connection
                ],
                &cursors,
            ),
            vec![
                (with_cursor, Some(42)),
                (first_connection, None),
                (another_first_connection, None),
            ],
            "every current room is planned; no-cursor rooms get bounded initial replay and stale rooms drop"
        );
}

#[test]
fn initial_cursorless_backfill_is_newest_bounded_and_chronological() {
    let (page, truncated) = initial_backfill_page(vec![6, 5, 4, 3, 2, 1], 3);
    assert_eq!(page, vec![4, 5, 6]);
    assert!(truncated);

    let (short, truncated) = initial_backfill_page(vec![2, 1], 3);
    assert_eq!(short, vec![1, 2]);
    assert!(!truncated);
}

#[test]
fn truncation_cursor_fires_only_when_replay_hits_the_cap() {
    let last = MessageId::new();
    // Under the cap (including zero rows): complete replay, no signal.
    assert_eq!(truncation_cursor(0, 200, Some(last)), None);
    assert_eq!(truncation_cursor(199, 200, Some(last)), None);
    // Exactly at the cap: possibly cut short → continue from the last id.
    assert_eq!(truncation_cursor(200, 200, Some(last)), Some(last));
    // Defensive: above the cap still signals (storage clamps, but a future
    // limit change must fail safe toward "tell the client to continue").
    assert_eq!(truncation_cursor(201, 200, Some(last)), Some(last));
    // No last id (empty replay) can never produce a cursor.
    assert_eq!(truncation_cursor::<MessageId>(200, 200, None), None);
}
