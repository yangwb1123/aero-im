use super::*;

/// Exact wire text — not just value round-trip. A serialize-as-string /
/// deserialize-from-string pair would pass a value-level round-trip while
/// breaking the integer wire contract (0239 INTEGER + psql Q3 integers).
#[test]
fn outbox_status_serializes_as_integer_text() {
    assert_eq!(serde_json::to_string(&OutboxStatus::Enqueued).unwrap(), "0");
    assert_eq!(serde_json::to_string(&OutboxStatus::Claimed).unwrap(), "1");
    assert_eq!(
        serde_json::to_string(&OutboxStatus::Delivered).unwrap(),
        "2"
    );
    assert_eq!(serde_json::to_string(&OutboxStatus::Dead).unwrap(), "3");
    assert_eq!(
        serde_json::from_str::<OutboxStatus>("0").unwrap(),
        OutboxStatus::Enqueued
    );
    assert_eq!(
        serde_json::from_str::<OutboxStatus>("3").unwrap(),
        OutboxStatus::Dead
    );
}

/// Fail-closed serde: unknown statuses are rejected on the wire (the
/// 0239 CHECK analogue) — even though `from_i32` is fail-open for the
/// scanning path. The asymmetry is deliberate (F1 mitigation).
#[test]
fn outbox_status_serde_rejects_unknown_values() {
    for wire in ["4", "-1", "7", "\"delivered\"", "null"] {
        let error = serde_json::from_str::<OutboxStatus>(wire)
            .err()
            .unwrap_or_else(|| panic!("{wire} must be rejected fail-closed"));
        assert!(!error.to_string().is_empty());
    }
}

/// `from_i32` is fail-open (None on unknown) — the Q3 bucket parser
/// tolerance. `as_i32` mirrors the repr.
#[test]
fn outbox_status_from_i32_is_fail_open_and_as_i32_mirrors_repr() {
    assert_eq!(OutboxStatus::from_i32(0), Some(OutboxStatus::Enqueued));
    assert_eq!(OutboxStatus::from_i32(1), Some(OutboxStatus::Claimed));
    assert_eq!(OutboxStatus::from_i32(2), Some(OutboxStatus::Delivered));
    assert_eq!(OutboxStatus::from_i32(3), Some(OutboxStatus::Dead));
    assert_eq!(OutboxStatus::from_i32(4), None);
    assert_eq!(OutboxStatus::from_i32(-1), None);
    assert_eq!(OutboxStatus::from_i32(i32::MAX), None);
    assert_eq!(OutboxStatus::Enqueued.as_i32(), 0);
    assert_eq!(OutboxStatus::Claimed.as_i32(), 1);
    assert_eq!(OutboxStatus::Delivered.as_i32(), 2);
    assert_eq!(OutboxStatus::Dead.as_i32(), 3);
}

/// Cross-format robustness: `serde_json::Value` integers route through
/// `visit_u64`/`visit_i64` — both must funnel into `from_i32`.
#[test]
fn outbox_status_roundtrips_through_json_value() {
    for (value, expected) in [
        (serde_json::json!(0), OutboxStatus::Enqueued),
        (serde_json::json!(1), OutboxStatus::Claimed),
        (serde_json::json!(2), OutboxStatus::Delivered),
        (serde_json::json!(3), OutboxStatus::Dead),
    ] {
        let status: OutboxStatus = serde_json::from_value(value).unwrap();
        assert_eq!(status, expected);
    }
    assert!(serde_json::from_value::<OutboxStatus>(serde_json::json!(4)).is_err());
    assert!(serde_json::from_value::<OutboxStatus>(serde_json::json!(-1)).is_err());
}

/// `AuditClass` wire spellings are the lowercase forms the 0239 CHECK
/// enumerates; `as_str` stays in lockstep with the serde rename.
#[test]
fn audit_class_roundtrips_lowercase() {
    assert_eq!(
        serde_json::to_string(&AuditClass::Admin).unwrap(),
        "\"admin\""
    );
    assert_eq!(
        serde_json::to_string(&AuditClass::Message).unwrap(),
        "\"message\""
    );
    assert_eq!(
        serde_json::to_string(&AuditClass::Room).unwrap(),
        "\"room\""
    );
    assert_eq!(
        serde_json::from_str::<AuditClass>("\"admin\"").unwrap(),
        AuditClass::Admin
    );
    assert_eq!(
        serde_json::from_str::<AuditClass>("\"message\"").unwrap(),
        AuditClass::Message
    );
    assert_eq!(
        serde_json::from_str::<AuditClass>("\"room\"").unwrap(),
        AuditClass::Room
    );
    assert!(serde_json::from_str::<AuditClass>("\"administrator\"").is_err());
    assert_eq!(AuditClass::Admin.as_str(), "admin");
    assert_eq!(AuditClass::Message.as_str(), "message");
    assert_eq!(AuditClass::Room.as_str(), "room");
}

/// Vocabulary canonical-value pins (the leaf is the single definition
/// point — the migration from governance.rs:197's literal assert).
/// The outbound moderation spelling is pinned to the VOCABULARY's index
/// 0 (not a bare literal): a coordinated flip (A1.3, scripts/coordinated-
/// flip-drill.sh) swaps the vocabulary members and the const follows —
/// this pin stays green and still catches an uncoordinated drift
/// (const ≠ index 0).
#[test]
fn vocabulary_consts_are_pinned() {
    assert_eq!(
        MODERATION_OUTBOUND_ACTION,
        MODERATION_OUTBOUND_VOCABULARY[0]
    );
    assert_ne!(
        MODERATION_OUTBOUND_VOCABULARY[0],
        MODERATION_OUTBOUND_VOCABULARY[1]
    );
    assert_eq!(LOCAL_ACTION_MODERATED, "message.moderated");
    assert_eq!(GOVERNANCE_CLASS_ADMIN, AuditClass::Admin.as_str());
    assert_eq!(GOVERNANCE_CLASS_MESSAGE, AuditClass::Message.as_str());
    assert_eq!(GOVERNANCE_CLASS_ROOM, AuditClass::Room.as_str());
    assert_eq!(GOVERNANCE_CLASS_ADMIN, "admin");
    assert_eq!(GOVERNANCE_CLASS_MESSAGE, "message");
    assert_eq!(GOVERNANCE_CLASS_ROOM, "room");
}

/// L1 vocabulary canonical-value pins (0242 trigger allowlist + envelope
/// — the `SQL literals` are cross-pinned by the aero-storage `db_tests`; a
/// flip here is a one-line edit and the cross-pins follow automatically).
#[test]
fn l1_vocabulary_consts_are_pinned() {
    assert_eq!(LOCAL_ACTION_MESSAGE_CREATE, "message.create");
    assert_eq!(LOCAL_ACTION_MESSAGE_EDIT, "message.edit");
    assert_eq!(AGGREGATED_MESSAGE_ACTION, "message.batch");
    assert_eq!(AUDIT_SOURCE_SYSTEM, "aero-im.source");
    assert_eq!(L1_WINDOW_SECONDS, 60);
    assert_ne!(
        LOCAL_ACTION_MESSAGE_CREATE, LOCAL_ACTION_MODERATED,
        "the L1 allowlist tokens are disjoint from the moderation token"
    );
}

/// Room-lane vocabulary canonical-value pins (0245 trigger allowlist —
/// the SQL literals are cross-pinned by the aero-storage `db_tests`; a
/// flip here is a one-line edit and the cross-pins follow automatically).
#[test]
fn room_vocabulary_consts_are_pinned() {
    assert_eq!(LOCAL_ACTION_ROOM_CREATE, "room.create");
    assert_eq!(LOCAL_ACTION_ROOM_ARCHIVED, "room.archived");
    assert_eq!(LOCAL_ACTION_MESSAGE_RECALLED, "message.recalled");
    assert_ne!(
        LOCAL_ACTION_ROOM_CREATE, LOCAL_ACTION_ROOM_ARCHIVED,
        "the two room allowlist tokens are distinct"
    );
    assert_ne!(
        LOCAL_ACTION_ROOM_CREATE, LOCAL_ACTION_MODERATED,
        "the room tokens are disjoint from the moderation token"
    );
    assert_ne!(
        LOCAL_ACTION_ROOM_CREATE, LOCAL_ACTION_MESSAGE_CREATE,
        "the room tokens are disjoint from the L1 allowlist tokens"
    );
    assert_ne!(
        LOCAL_ACTION_MESSAGE_RECALLED, LOCAL_ACTION_MODERATED,
        "the recall token is disjoint from the moderation token"
    );
    assert_ne!(
        LOCAL_ACTION_MESSAGE_RECALLED, LOCAL_ACTION_MESSAGE_DELETED,
        "the recall token is disjoint from the R-D2 message.deleted token"
    );
    assert_eq!(
        LOCAL_ACTION_MESSAGE_DELETED, "message.deleted",
        "R-D2 delete token value pin (leaf single definition)"
    );
    assert_ne!(
        LOCAL_ACTION_MESSAGE_DELETED, LOCAL_ACTION_MESSAGE_CREATE,
        "the delete token is disjoint from the L1 allowlist tokens"
    );
    assert_ne!(
        LOCAL_ACTION_MESSAGE_DELETED, LOCAL_ACTION_MESSAGE_EDIT,
        "the delete token is disjoint from the L1 allowlist tokens"
    );
    assert_ne!(
        LOCAL_ACTION_MESSAGE_DELETED, LOCAL_ACTION_MODERATED,
        "the delete token is disjoint from the moderation token (exact-token gate)"
    );
    assert_ne!(
        LOCAL_ACTION_MESSAGE_DELETED, LOCAL_ACTION_ROOM_CREATE,
        "the delete token is disjoint from the room tokens"
    );
    assert_ne!(
        LOCAL_ACTION_MESSAGE_DELETED, LOCAL_ACTION_ROOM_ARCHIVED,
        "the delete token is disjoint from the room tokens"
    );
    assert_ne!(
        LOCAL_ACTION_MESSAGE_DELETED, LOCAL_ACTION_MESSAGE_RECALLED,
        "the delete token is disjoint from the recall token"
    );
    assert_ne!(
        LOCAL_ACTION_MESSAGE_RECALLED, LOCAL_ACTION_MESSAGE_CREATE,
        "the recall token is disjoint from the L1 allowlist tokens"
    );
    assert_ne!(
        LOCAL_ACTION_MESSAGE_RECALLED, LOCAL_ACTION_MESSAGE_EDIT,
        "the recall token is disjoint from the L1 allowlist tokens"
    );
}

// ---- Typed outbound claim payload: exact-wire-text + fail-closed serde
// pins (mirrors the `outbox_status_*` family). ----

fn sample_payload() -> AuditClaimPayload {
    AuditClaimPayload::new(
        "0195b7d3-5a11-7000-8000-000000000001".to_owned(),
        "aero-im".to_owned(),
        "2026-08-08T16:59:31.123456+00:00".to_owned(),
        AuditActor::system(),
        vec![],
        "0195b7d3-5a11-7000-8000-000000000002".to_owned(),
        LOCAL_ACTION_MESSAGE_DELETED.to_owned(),
        serde_json::json!({ "reason": "spam" }),
    )
}

fn sample_aggregate_payload(spill: Option<bool>) -> AuditAggregatePayload {
    AuditAggregatePayload {
        event_id: "0195b7d3-5a11-7000-8000-000000000010".to_owned(),
        source_system: AUDIT_SOURCE_SYSTEM.to_owned(),
        event_type: AUDIT_EVENT_TYPE.to_owned(),
        schema_id: AUDIT_SCHEMA_ID.to_owned(),
        schema_version: AUDIT_SCHEMA_VERSION,
        occurred_at: "2026-08-08T16:59:00+00:00".to_owned(),
        aggregate_type: AUDIT_AGGREGATE_TYPE.to_owned(),
        aggregate_id: "0195b7d3-5a11-7000-8000-000000000011".to_owned(),
        action: AGGREGATED_MESSAGE_ACTION.to_owned(),
        outcome: AUDIT_OUTCOME_SUCCESS.to_owned(),
        data_classification: AUDIT_DATA_CLASSIFICATION.to_owned(),
        retention_class: AUDIT_RETENTION_CLASS.to_owned(),
        idempotency_key: "0195b7d3-5a11-7000-8000-000000000010".to_owned(),
        count: 5,
        aggregated: true,
        spill,
        window_start: "2026-08-08T16:59:00+00:00".to_owned(),
        window_end: "2026-08-08T17:00:00+00:00".to_owned(),
        first_event_at: "2026-08-08T16:59:31.123456+00:00".to_owned(),
        last_event_at: "2026-08-08T16:59:42.5+00:00".to_owned(),
    }
}

/// The aggregate wire order and omission of the optional spill marker are
/// pinned against migration 0242's `jsonb_build_object` document order.
#[test]
fn audit_aggregate_payload_serializes_exact_wire_text() {
    let wire = serde_json::to_string(&sample_aggregate_payload(None)).unwrap();
    assert_eq!(
        wire,
        r#"{"event_id":"0195b7d3-5a11-7000-8000-000000000010","source_system":"aero-im.source","event_type":"aero.im.security","schema_id":"aero.im.security","schema_version":1,"occurred_at":"2026-08-08T16:59:00+00:00","aggregate_type":"workspace","aggregate_id":"0195b7d3-5a11-7000-8000-000000000011","action":"message.batch","outcome":"success","data_classification":"confidential","retention_class":"security","idempotency_key":"0195b7d3-5a11-7000-8000-000000000010","count":5,"aggregated":true,"window_start":"2026-08-08T16:59:00+00:00","window_end":"2026-08-08T17:00:00+00:00","first_event_at":"2026-08-08T16:59:31.123456+00:00","last_event_at":"2026-08-08T16:59:42.5+00:00"}"#
    );

    let spill = serde_json::to_value(sample_aggregate_payload(Some(true))).unwrap();
    assert_eq!(spill["spill"], true);
}

#[test]
fn audit_aggregate_payload_roundtrips_and_rejects_drift() {
    let original = sample_aggregate_payload(Some(true));
    let value = serde_json::to_value(&original).unwrap();
    let parsed: AuditAggregatePayload = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(parsed, original);
    assert_eq!(serde_json::to_value(&parsed).unwrap(), value);

    let mut with_actor = value;
    with_actor["actor"] = serde_json::json!({ "id": "user", "type": "participant" });
    assert!(
        serde_json::from_value::<AuditAggregatePayload>(with_actor).is_err(),
        "aggregate envelopes must reject 0239-only actor fields"
    );
}

/// Exact wire text — key names, key order, and value spellings pinned
/// against the 0239 `jsonb_build_object` document order (the field-name
/// pin: a renamed key or a `schema_version` serialized as `"1"` fails
/// here before the parity drill ever runs).
#[test]
fn audit_claim_payload_serializes_exact_wire_text() {
    let wire = serde_json::to_string(&sample_payload()).unwrap();
    assert_eq!(
        wire,
        r#"{"event_id":"0195b7d3-5a11-7000-8000-000000000001","source_system":"aero-im","event_type":"aero.im.security","schema_id":"aero.im.security","schema_version":1,"occurred_at":"2026-08-08T16:59:31.123456+00:00","actor":{"id":"system","type":"system"},"targets":[],"aggregate_type":"workspace","aggregate_id":"0195b7d3-5a11-7000-8000-000000000002","action":"message.deleted","outcome":"success","payload":{"reason":"spam"},"data_classification":"confidential","retention_class":"security","idempotency_key":"0195b7d3-5a11-7000-8000-000000000001"}"#
    );
}

/// `to_value` → `from_value` → `to_value` is byte-identical (the
/// JSON-Value round-trip the storage drill relies on).
#[test]
fn audit_claim_payload_roundtrips_through_json_value() {
    let original = sample_payload();
    let value = serde_json::to_value(&original).unwrap();
    let parsed: AuditClaimPayload = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(parsed, original);
    assert_eq!(serde_json::to_value(&parsed).unwrap(), value);
}

/// Fail-closed serde: an unknown top-level key (a future SQL-side
/// addition) or an unknown nested key inside `actor`/`targets` is `Err` —
/// the drift alarm that forces the leaf to track the SQL envelope.
#[test]
fn audit_claim_payload_serde_rejects_unknown_field_names() {
    let mut with_tenant = serde_json::to_value(sample_payload()).unwrap();
    with_tenant
        .as_object_mut()
        .unwrap()
        .insert("tenant_id".to_owned(), serde_json::json!("t-1"));
    let error = serde_json::from_value::<AuditClaimPayload>(with_tenant)
        .err()
        .unwrap();
    assert!(!error.to_string().is_empty());

    // Nested structs are fail-closed too (design: all three structs).
    let mut bad_actor = serde_json::to_value(sample_payload()).unwrap();
    bad_actor["actor"]["tenant_id"] = serde_json::json!("t-1");
    assert!(
        serde_json::from_value::<AuditClaimPayload>(bad_actor).is_err(),
        "unknown key inside actor must be rejected"
    );
    let mut bad_target = serde_json::to_value(sample_payload()).unwrap();
    bad_target["targets"] = serde_json::json!([{ "id": "m-1", "type": "resource", "x": 1 }]);
    assert!(
        serde_json::from_value::<AuditClaimPayload>(bad_target).is_err(),
        "unknown key inside targets[] must be rejected"
    );

    // The `schema_version` wire type is a JSON NUMBER: `"1"` (string) is
    // a drift, not a tolerated alternate spelling.
    let mut string_version = serde_json::to_value(sample_payload()).unwrap();
    string_version["schema_version"] = serde_json::json!("1");
    assert!(
        serde_json::from_value::<AuditClaimPayload>(string_version).is_err(),
        "schema_version as a string must be rejected (0239: JSON number 1)"
    );
}

/// The constructor fills the invariant fields from the `AUDIT_*` consts
/// — asserted via field access AND via the serialized text (the 16-key
/// contract a producer sees).
#[test]
fn audit_claim_payload_constructor_pins_wire_consts() {
    let payload = sample_payload();
    assert_eq!(payload.event_type, AUDIT_EVENT_TYPE);
    assert_eq!(payload.schema_id, AUDIT_SCHEMA_ID);
    assert_eq!(payload.schema_version, AUDIT_SCHEMA_VERSION);
    assert_eq!(payload.aggregate_type, AUDIT_AGGREGATE_TYPE);
    assert_eq!(payload.outcome, AUDIT_OUTCOME_SUCCESS);
    assert_eq!(payload.data_classification, AUDIT_DATA_CLASSIFICATION);
    assert_eq!(payload.retention_class, AUDIT_RETENTION_CLASS);
    assert_eq!(
        payload.idempotency_key, payload.event_id,
        "idempotency_key must equal event_id (0239: both = audit_events.id::text)"
    );
    let value = serde_json::to_value(&payload).unwrap();
    assert_eq!(value["event_type"], AUDIT_EVENT_TYPE);
    assert_eq!(value["schema_id"], AUDIT_SCHEMA_ID);
    assert_eq!(value["schema_version"], serde_json::json!(1));
    assert_eq!(value["aggregate_type"], AUDIT_AGGREGATE_TYPE);
    assert_eq!(value["outcome"], AUDIT_OUTCOME_SUCCESS);
    assert_eq!(value["data_classification"], AUDIT_DATA_CLASSIFICATION);
    assert_eq!(value["retention_class"], AUDIT_RETENTION_CLASS);
    assert_eq!(value["idempotency_key"], value["event_id"]);

    // Helper spellings (single literal site): participant + resource.
    let actor = AuditActor::participant("0195b7d3-5a11-7000-8000-000000000003");
    assert_eq!(actor.kind, AUDIT_ACTOR_TYPE_PARTICIPANT);
    assert_eq!(actor.kind, "participant");
    let target = AuditTarget::resource("m-1");
    assert_eq!(target.kind, AUDIT_TARGET_TYPE_RESOURCE);
    assert_eq!(target.kind, "resource");
    assert_eq!(AuditActor::system().kind, AUDIT_ACTOR_TYPE_SYSTEM);
    assert_eq!(AuditActor::system().id, "system");
}

/// R-D2 §10.4: the canonical `occurred_at` wire spelling — the PG
/// `timestamptz → jsonb` text form (`+00:00` offset, trailing-zero-\
/// trimmed fraction, omitted when zero). A `Z` suffix, a padded
/// fraction, or a dangling `.0` reds here — the drift
/// `audit_wire_occurred_at` exists to kill (the `time` crate's Rfc3339
/// emits `Z`). Values pinned against live PG output verified at design
/// time (docker `aero-postgres`): `.5` → `.5`, `.120000` → `.12`,
/// `.000010` → `.00001`, zero → no fraction.
#[test]
fn audit_wire_occurred_at_canonical_spelling() {
    let parse = |s: &str| {
        time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
            .expect("fixed ts")
    };
    assert_eq!(
        audit_wire_occurred_at(parse("2026-08-15T12:34:56Z")),
        "2026-08-15T12:34:56+00:00",
        "zero subsecond → no fraction (PG never emits .0)"
    );
    assert_eq!(
        audit_wire_occurred_at(parse("2026-08-15T12:34:56.5Z")),
        "2026-08-15T12:34:56.5+00:00",
        "single digit, never padded"
    );
    assert_eq!(
        audit_wire_occurred_at(parse("2026-08-15T12:34:56.123456Z")),
        "2026-08-15T12:34:56.123456+00:00",
        "full microseconds"
    );
    assert_eq!(
        audit_wire_occurred_at(parse("2026-08-15T12:34:56.120000Z")),
        "2026-08-15T12:34:56.12+00:00",
        "trailing zeros trimmed (PG trim, never padded)"
    );
    assert_eq!(
        audit_wire_occurred_at(parse("2026-08-15T12:34:56.000010Z")),
        "2026-08-15T12:34:56.00001+00:00",
        "internal zeros preserved, only trailing trimmed"
    );
    // A non-UTC input is normalized to +00:00 (PG renders UTC).
    assert_eq!(
        audit_wire_occurred_at(parse("2026-08-15T12:34:56.5+02:00")),
        "2026-08-15T10:34:56.5+00:00",
        "offset normalized to UTC"
    );
}
