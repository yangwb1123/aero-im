//! Per-subject event sequence stamping (ROADMAP 第三版 方向一 — 实时投递完整性).
//!
//! NATS `JetStream` gives at-least-once delivery, so a redelivery (or a
//! multi-instance failover replay) can hand the same `RoomEvent`/`StreamEvent`
//! to a consumer twice. Most variants (`Edited`, `Deleted`, `Reaction`,
//! `Typing`, …) carry no unique id of their own, leaving clients nothing to
//! dedup on.
//!
//! The fix is a **publish-time** sequence stamp: the publisher serializes the
//! event, inserts a top-level `"seq": <u64>` key next to the enum's `kind` tag,
//! and publishes those bytes. Because the stamp happens once — at publish, not
//! at delivery — every redelivery of the same event carries the **same** seq,
//! which is exactly what makes it a dedup key.
//!
//! Contract:
//! - **Per-subject monotonic**: each `im.room.{id}` / `live.stream.{id}` subject
//!   has its own counter (process-local or Redis `INCR`, see the providers in
//!   `aero-im-core` / `aero-storage`).
//! - **Gaps are legal**: only dedup and *relative* order matter; consumers must
//!   never wait for a missing seq.
//! - **Backward/forward compatible**: serde ignores unknown fields by default
//!   (no event type in this workspace uses `deny_unknown_fields`), so old
//!   consumers deserialize a stamped payload unchanged, and new consumers treat
//!   an unstamped payload as "no seq" and pass it through.

use serde::Serialize;

/// Insert a top-level `"seq"` key into a JSON object value. No-op when `seq` is
/// `None` (publisher chose/failed to stamp) or when the value is not an object
/// (nowhere to put a sibling key without changing the payload's shape).
pub fn stamp_seq(value: &mut serde_json::Value, seq: Option<u64>) {
    if let (Some(seq), serde_json::Value::Object(map)) = (seq, value) {
        map.insert("seq".into(), serde_json::Value::from(seq));
    }
}

/// Serialize `event` and stamp it with `seq`: the publish-side envelope helper.
///
/// # Errors
/// Returns the underlying `serde_json` error if `event` fails to serialize.
pub fn stamped_event_bytes<T: Serialize>(
    event: &T,
    seq: Option<u64>,
) -> serde_json::Result<Vec<u8>> {
    let mut value = serde_json::to_value(event)?;
    stamp_seq(&mut value, seq);
    serde_json::to_vec(&value)
}

/// Read the `"seq"` stamp off a parsed payload, if present and a valid `u64`.
/// `None` covers legacy (unstamped) payloads and garbage values alike — the
/// consumer then simply passes the event through without dedup.
#[must_use]
pub fn extract_seq(value: &serde_json::Value) -> Option<u64> {
    value.get("seq").and_then(serde_json::Value::as_u64)
}

/// Insert a top-level W3C `"traceparent"` key into a JSON object value, so a
/// consumer can continue the producer's distributed trace across the bus (ROADMAP5
/// 方向二). Rides the same envelope mechanism as [`stamp_seq`]: a stamped sibling
/// key that existing serde consumers ignore. No-op when `traceparent` is
/// `None`/empty or the value is not an object.
pub fn stamp_traceparent(value: &mut serde_json::Value, traceparent: Option<&str>) {
    if let (Some(tp), serde_json::Value::Object(map)) = (traceparent, value) {
        if !tp.is_empty() {
            map.insert("traceparent".into(), serde_json::Value::from(tp));
        }
    }
}

/// Read the `"traceparent"` stamp off a parsed payload, if present. `None` for
/// legacy/untraced payloads. Mirrors [`extract_seq`].
#[must_use]
pub fn extract_traceparent(value: &serde_json::Value) -> Option<String> {
    value.get("traceparent").and_then(serde_json::Value::as_str).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::{ParticipantId, RoomEvent, RoomId};
    use pretty_assertions::assert_eq;

    fn typing_event() -> RoomEvent {
        RoomEvent::Typing {
            room_id: RoomId::new(),
            participant: ParticipantId::new(),
            on: true,
        }
    }

    #[test]
    fn stamp_inserts_top_level_seq_next_to_kind_tag() {
        let bytes = stamped_event_bytes(&typing_event(), Some(42)).expect("serialize");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(value.get("kind").and_then(|v| v.as_str()), Some("typing"));
        assert_eq!(extract_seq(&value), Some(42));
    }

    #[test]
    fn stamped_payload_still_round_trips_as_room_event() {
        // The whole point of the design: existing consumers deserialize the
        // stamped bytes with serde, which ignores the unknown `seq` field.
        let event = typing_event();
        let bytes = stamped_event_bytes(&event, Some(7)).expect("serialize");
        let back: RoomEvent = serde_json::from_slice(&bytes).expect("round-trip");
        match back {
            RoomEvent::Typing { on, .. } => assert!(on),
            other => panic!("expected Typing, got {other:?}"),
        }
    }

    #[test]
    fn none_seq_leaves_payload_unstamped() {
        let bytes = stamped_event_bytes(&typing_event(), None).expect("serialize");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(extract_seq(&value), None);
        assert!(value.get("seq").is_none());
    }

    #[test]
    fn non_object_payloads_pass_through_untouched() {
        // A bare scalar has nowhere to hold a sibling key; stamping must not
        // corrupt it (or panic).
        let bytes = stamped_event_bytes(&"hello", Some(1)).expect("serialize");
        assert_eq!(bytes, b"\"hello\"");
    }

    #[test]
    fn extract_rejects_non_u64_stamps() {
        let value: serde_json::Value = serde_json::json!({ "kind": "typing", "seq": "nope" });
        assert_eq!(extract_seq(&value), None);
        let negative: serde_json::Value = serde_json::json!({ "seq": -3 });
        assert_eq!(extract_seq(&negative), None);
    }

    #[test]
    fn traceparent_stamp_round_trips_and_serde_ignores_it() {
        const TP: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
        let mut value = serde_json::to_value(typing_event()).unwrap();
        stamp_traceparent(&mut value, Some(TP));
        assert_eq!(extract_traceparent(&value).as_deref(), Some(TP));
        // A stamped payload still deserializes — serde ignores the unknown key.
        let bytes = serde_json::to_vec(&value).unwrap();
        let back: RoomEvent = serde_json::from_slice(&bytes).expect("round-trip");
        assert!(matches!(back, RoomEvent::Typing { on: true, .. }));
        // None / empty are no-ops (legacy/untraced).
        let mut bare = serde_json::to_value(typing_event()).unwrap();
        stamp_traceparent(&mut bare, None);
        stamp_traceparent(&mut bare, Some(""));
        assert_eq!(extract_traceparent(&bare), None);
    }
}
