use super::{message_expiration, message_request_hash, postgres_code_retryable};
use aero_common::{Block, MessageId, RoomId};

#[test]
fn message_request_hash_is_stable_and_binds_every_field() {
    let room = RoomId::new();
    let blocks = vec![Block::text("hello")];
    let base = message_request_hash(room, &blocks, None, None).unwrap();
    assert_eq!(
        base,
        message_request_hash(room, &blocks, None, None).unwrap()
    );
    assert_ne!(
        base,
        message_request_hash(RoomId::new(), &blocks, None, None).unwrap()
    );
    assert_ne!(
        base,
        message_request_hash(room, &[Block::text("changed")], None, None).unwrap()
    );
    assert_ne!(
        base,
        message_request_hash(room, &blocks, Some(MessageId::new()), None).unwrap()
    );
    assert_ne!(
        base,
        message_request_hash(room, &blocks, None, Some(60)).unwrap()
    );
}

#[test]
fn message_request_hash_v1_has_a_golden_vector() {
    let room = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse::<RoomId>().unwrap();
    assert_eq!(
        hex::encode(message_request_hash(room, &[Block::text("hello")], None, None).unwrap()),
        "8358768e73605d23245fe55afbd134fd0a352e99bca724130beb707687abfd8b"
    );
}

#[test]
fn message_expiry_rejects_integer_wraparound() {
    let now = time::OffsetDateTime::UNIX_EPOCH;
    assert_eq!(message_expiration(None, now).unwrap(), None);
    assert_eq!(message_expiration(Some(0), now).unwrap(), None);
    assert_eq!(
        message_expiration(Some(60), now).unwrap(),
        Some(now + time::Duration::seconds(60))
    );
    assert!(message_expiration(Some(u64::MAX), now).is_err());
}

#[test]
fn only_transient_postgres_codes_are_retryable() {
    for code in [
        "08006", "40001", "40P01", "55P03", "53300", "57014", "57P03",
    ] {
        assert!(postgres_code_retryable(code), "{code}");
    }
    for code in ["23503", "23505", "22001", "42601"] {
        assert!(!postgres_code_retryable(code), "{code}");
    }
}
