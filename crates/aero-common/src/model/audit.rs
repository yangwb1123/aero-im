//! B5 contract vocabulary — Rust-side single source of truth for the audit
//! relay outbox wire contract.
//!
//! DB-side twin: the 0239 CHECK constraints
//! (`migrations/0239_audit_governance_outbox.sql`). SQL cannot import Rust,
//! so the CHECKs stay the DB single source; aero-storage `db_tests`
//! (`audit_governance.rs`) cross-pin the two sides (leaf ↔ DDL).
//!
//! This module is the only legal spelling of the status/class/action-token
//! vocabulary in Rust — `scripts/truth-check.sh` hard-fails on any
//! `admin.content.flag` literal outside this file (AC4 regression guard).

/// Outbox lifecycle status. 0239 twin: `CHECK (status IN (0,1,2,3))`; Q3
/// psql output (`audit_provision.rs`) is the same integers.
///
/// Explicit discriminants `= 0..3` prevent reorder drift; a new variant must
/// pick a new value (a status code is never reused once shipped).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum OutboxStatus {
    Enqueued = 0,
    Claimed = 1,
    Delivered = 2,
    Dead = 3,
}

impl OutboxStatus {
    /// `const fn` so the derived `STATUS_*` aliases (aero-audit-connector
    /// `pg.rs`) can call it in const context.
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        self as i32
    }

    /// Fail-open parse: unknown → `None`（Q3 bucket parsing keeps its
    /// "unknown statuses are ignored" semantics — a scanning tool tolerates
    /// future statuses, the wire contract does not).
    ///
    /// Deliberate asymmetry with `Deserialize` (fail-closed): parsing paths
    /// (buckets) tolerate unknown rows and keep scanning; the wire path
    /// rejects unknown values, mirroring the 0239 CHECK.
    #[must_use]
    pub const fn from_i32(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Enqueued),
            1 => Some(Self::Claimed),
            2 => Some(Self::Delivered),
            3 => Some(Self::Dead),
            _ => None,
        }
    }
}

impl serde::Serialize for OutboxStatus {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_i32(self.as_i32())
    }
}

/// Hand-written `Deserialize` (a derive cannot coexist with the manual impl
/// above, so the only drift path would be deleting impl *and* tests
/// together). Entry point is pinned to `deserialize_i32`; `visit_i32` is the
/// primary arm and `visit_i64`/`visit_u64` funnel through `i32::try_from` —
/// `serde_json` routes small positive integers to `visit_u64` and negatives to
/// `visit_i64`, so an `i32`-only visitor would Err on every real JSON
/// integer. Unknown values → Err (fail-closed: the wire contract rejects
/// what the 0239 CHECK rejects).
impl<'de> serde::Deserialize<'de> for OutboxStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct StatusVisitor;

        impl serde::de::Visitor<'_> for StatusVisitor {
            type Value = OutboxStatus;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an outbox status integer 0..=3")
            }

            fn visit_i32<E>(self, value: i32) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                OutboxStatus::from_i32(value).ok_or_else(|| {
                    E::custom(format!(
                        "unknown outbox status {value} (0239 CHECK allows 0..=3)"
                    ))
                })
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let value = i32::try_from(value)
                    .map_err(|_| E::custom(format!("outbox status {value} is out of i32 range")))?;
                self.visit_i32(value)
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let value = i32::try_from(value)
                    .map_err(|_| E::custom(format!("outbox status {value} is out of i32 range")))?;
                self.visit_i32(value)
            }
        }

        deserializer.deserialize_i32(StatusVisitor)
    }
}

/// Governance class. 0239 twin: `CHECK (class IN ('admin','message','room'))`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditClass {
    Message,
    Room,
    Admin,
}

impl AuditClass {
    /// `const fn` — the `GOVERNANCE_CLASS_*` derived constants depend on it
    /// (and the serde `rename_all = "lowercase"` must stay in lockstep: the
    /// spellings below are the wire/DEFAULT forms).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Room => "room",
            Self::Admin => "admin",
        }
    }
}

// ---- Action-token vocabulary (the only legal literal site in Rust) ----

/// Single outbound contract token: the 0239 trigger/0241 reconciler hardcode
/// this in SQL (their INSERT literals are pinned against this constant by
/// the aero-storage `db_tests` — leaf ↔ DDL cross-pin — and by truth-check
/// rule 3g's static SQL-literal guard). The contract proposal listed both
/// `admin.content.flag` and `admin.moderation.action`; locked here as ONE
/// constant. A flip is a COORDINATED edit: this leaf + the 0239/0241 SQL
/// literals + the priority drill's contract-vocabulary pair (index 0) + the
/// test-integration drill fixture, all in one commit (truth-check rule 3g
/// and the drill's exact-equality bail red a partial flip).
pub const MODERATION_OUTBOUND_ACTION: &str = "admin.content.flag";

/// The two contract-legal outbound spellings, in canonical order: index 0 =
/// the pinned [`MODERATION_OUTBOUND_ACTION`], index 1 = the sibling. A
/// coordinated flip swaps the members (index 0 becomes the sibling) — the
/// single vocabulary both the priority drill's pair and the
/// `claim_validation` drift twin derive their "the other spelling" from, so
/// A1.3 (scripts/coordinated-flip-drill.sh) stays executable without a
/// third hardcoded copy.
pub const MODERATION_OUTBOUND_VOCABULARY: [&str; 2] =
    ["admin.content.flag", "admin.moderation.action"];

/// Local audit token produced by every moderation finalize producer
/// (`AiWorker::handle_moderate`, `ImService::moderate_delete`, report
/// review). Call sites spell the token through this name, never a bare
/// literal.
pub const LOCAL_ACTION_MODERATED: &str = "message.moderated";

// ---- L1 window-aggregation vocabulary (migration 0242) ----
// The 0242 trigger `aero_enqueue_l1_aggregate_audit` allowlists these local
// tokens on the SQL side (its literals are pinned against these constants by
// the aero-storage `db_tests` — leaf ↔ DDL cross-pin, same mechanism as
// `MODERATION_OUTBOUND_ACTION`). `message.create`/`message.edit` stay
// UNMAPPED in `governance_lane_for` (L1 is a SQL-side allowlist, not a
// governance lane — see `aero_ai::governance`).

/// L1 trigger allowlist token: message create (aggregatable into the
/// per-(workspace,class,window) outbox row).
pub const LOCAL_ACTION_MESSAGE_CREATE: &str = "message.create";
/// L1 trigger allowlist token: message edit (second allowlist entry).
pub const LOCAL_ACTION_MESSAGE_EDIT: &str = "message.edit";
/// L1 outbound action token carried by window AND spill payloads (0242
/// envelope). The leaf wire-contract's R3 token; the sibling slice's
/// [PROPOSED] `MESSAGE_OUTBOUND_ACTION` "message.activity" folds into this
/// single token (same rule as the single-0242 arbitration).
pub const AGGREGATED_MESSAGE_ACTION: &str = "message.batch";
/// 0242 envelope `source_system` value — REQUIRED by the connector's
/// `validate_delivery_payload` (aero-audit-connector client.rs; `PayloadGuard`
/// permanent class = dead after ≤1 retry if absent/mismatched). L1 has no
/// binding row, so the trigger writes this pinned constant. Deployment
/// invariant: `AERO_AUDIT_SOURCE_SYSTEM` must equal this const (same
/// single-value convention as 1:1 binding rows / drill configs / harness).
pub const AUDIT_SOURCE_SYSTEM: &str = "aero-im.source";
/// Fixed L1 window in seconds (0242 window key divisor). 60s per the B5-1
/// sibling design D1.
pub const L1_WINDOW_SECONDS: i64 = 60;

// ---- Room-lane vocabulary (migration 0245) ----
// The 0245 trigger `aero_enqueue_room_audit` allowlists these local tokens on
// the SQL side (its literals are pinned against these constants by the
// aero-storage `db_tests` — leaf ↔ DDL cross-pin, same mechanism as
// `LOCAL_ACTION_MESSAGE_CREATE`/`LOCAL_ACTION_MESSAGE_EDIT` above). The two
// tokens are the room-family exemplars named in the 0239-era pins
// (governance.rs / audit_governance.rs); a FUTURE room token is a schema
// change: one leaf const + one SQL literal + one migration, never a silent
// widening of the 0245 allowlist.

/// 0245 trigger allowlist token: room create (1:1 class-'room' outbox row).
pub const LOCAL_ACTION_ROOM_CREATE: &str = "room.create";
/// 0245 trigger allowlist token: room archived (second allowlist entry).
pub const LOCAL_ACTION_ROOM_ARCHIVED: &str = "room.archived";

// ---- Message-recall vocabulary (migration 0246) ----
// The 0246 trigger `aero_enqueue_message_recall_audit` allowlists this local
// token on the SQL side (its literal is pinned against this constant by the
// aero-storage `db_tests` — leaf ↔ DDL cross-pin, same mechanism as
// `LOCAL_ACTION_ROOM_CREATE`/`LOCAL_ACTION_ROOM_ARCHIVED` above).
// `message.recalled` is the ONLY production-site recall token
// (crates/aero-storage/src/message/authorization.rs, in-tx via
// `AuditRepo::append_in_tx`); it stays UNMAPPED in `governance_lane_for`
// (SQL-side message lane, like the L1 tokens — no governance arm).

/// 0246 trigger allowlist token: message recall (1:1 class-'message' outbox
/// row — never folded into the 0242 L1 window; the 1:1 requirement is
/// load-bearing).
pub const LOCAL_ACTION_MESSAGE_RECALLED: &str = "message.recalled";

// ---- Class derived aliases ----
// Keep the `aero_ai::governance::GOVERNANCE_CLASS_*` chain and the 0239 DDL
// comment pins textually stable while the definition lives here (the leaf is
// the single definition point).

/// Admin class: moderation rows must stay 1:1 (`event_id` = `audit_events.id`).
pub const GOVERNANCE_CLASS_ADMIN: &str = AuditClass::Admin.as_str();
/// High-volume message backlog class (L1-aggregatable).
pub const GOVERNANCE_CLASS_MESSAGE: &str = AuditClass::Message.as_str();
/// Room lifecycle class.
pub const GOVERNANCE_CLASS_ROOM: &str = AuditClass::Room.as_str();

// ---- Typed outbound claim payload (Rust twin of the 0239 SQL-built wire
// envelope) ----
//
// The 0239 trigger `aero_enqueue_governance_audit` is the sole producer of
// the governance outbox payload for every non-moderation action and builds
// it EXCLUSIVELY in SQL (`jsonb_build_object`, 16 keys — see
// `migrations/0239_audit_governance_outbox.sql`). The connector forwards it
// untyped (`Claim.payload: serde_json::Value`), so no Rust code is
// compile-time pinned to the wire shape. `aero-storage` cannot import
// `aero-ai`; the only crate shared by `aero-storage` producers and the
// connector is this leaf — hence the typed twin lives here. The parity drill
// (`aero-storage/src/audit_governance.rs`
// `rust_produced_payload_matches_0239_envelope`) cross-pins leaf ↔ DDL.
//
// `deny_unknown_fields` is a deliberate breaking point: a future SQL-side
// envelope addition fails the drill's fail-closed parse until this struct is
// updated — the intended drift alarm, not a defect.

/// Wire-constant vocabulary of the 0239 envelope — this module is the single
/// legal literal site in Rust (the SQL trigger spells the same values).
pub const AUDIT_EVENT_TYPE: &str = "aero.im.security";
pub const AUDIT_SCHEMA_ID: &str = "aero.im.security";
pub const AUDIT_SCHEMA_VERSION: u32 = 1;
pub const AUDIT_AGGREGATE_TYPE: &str = "workspace";
pub const AUDIT_OUTCOME_SUCCESS: &str = "success";
pub const AUDIT_DATA_CLASSIFICATION: &str = "confidential";
pub const AUDIT_RETENTION_CLASS: &str = "security";
pub const AUDIT_ACTOR_TYPE_SYSTEM: &str = "system";
pub const AUDIT_ACTOR_TYPE_PARTICIPANT: &str = "participant";
pub const AUDIT_TARGET_TYPE_RESOURCE: &str = "resource";

/// Envelope `actor` sub-object. 0239 spelling: `{id, type}` where `id` is
/// `actor_id::text` (hyphenated UUID) or `'system'` and `type` is
/// `'participant'`/`'system'`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditActor {
    pub id: String,
    /// Wire key is `type` (serde rename reproduces it exactly).
    #[serde(rename = "type")]
    pub kind: String,
}

impl AuditActor {
    /// The system actor (server-initiated actions: moderation finalize,
    /// retention sweep). 0239: `actor_id IS NULL → {id:'system', type:'system'}`.
    #[must_use]
    pub fn system() -> Self {
        Self {
            id: AUDIT_ACTOR_TYPE_SYSTEM.to_owned(),
            kind: AUDIT_ACTOR_TYPE_SYSTEM.to_owned(),
        }
    }

    /// A human actor; `id` is the participant's hyphenated UUID text
    /// (`participant_id::text`).
    #[must_use]
    pub fn participant(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: AUDIT_ACTOR_TYPE_PARTICIPANT.to_owned(),
        }
    }
}

/// Envelope `targets[]` element. 0239 spelling: `[{id, type:'resource'}]` for
/// a non-NULL `audit_events.target`, `[]` otherwise.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditTarget {
    pub id: String,
    /// Wire key is `type` (serde rename reproduces it exactly).
    #[serde(rename = "type")]
    pub kind: String,
}

impl AuditTarget {
    /// A resource target (e.g. a message id, verbatim `audit_events.target`).
    #[must_use]
    pub fn resource(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: AUDIT_TARGET_TYPE_RESOURCE.to_owned(),
        }
    }
}

/// Rust twin of the 0239 SQL-built wire envelope — the full 16-key payload
/// `jsonb_build_object` produces in `aero_enqueue_governance_audit` (field
/// order = 0239 document order, pinned by the exact-wire-text test).
///
/// Consumers are *producers* of `message.*`/`room.*` governance rows (the
/// trigger only maps `message.moderated`); `occurred_at` is a `String`
/// because PG serializes timestamptz→jsonb as ISO-8601-with-offset, which no
/// Rust formatter reproduces byte-identically (the drill derives the expected
/// spelling from `to_jsonb(created_at)`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditClaimPayload {
    pub event_id: String,
    pub source_system: String,
    pub event_type: String,
    pub schema_id: String,
    pub schema_version: u32,
    pub occurred_at: String,
    pub actor: AuditActor,
    pub targets: Vec<AuditTarget>,
    pub aggregate_type: String,
    pub aggregate_id: String,
    pub action: String,
    pub outcome: String,
    pub payload: serde_json::Value,
    pub data_classification: String,
    pub retention_class: String,
    pub idempotency_key: String,
}

impl AuditClaimPayload {
    /// Producer-facing constructor: takes only the variable inputs and fills
    /// the eight invariant fields from the `AUDIT_*` consts above, so a
    /// producer can never spell a wire constant. `idempotency_key` is
    /// enforced equal to `event_id` (0239: both = `audit_events.id::text`;
    /// the sink's Idempotency-Key header).
    #[allow(clippy::too_many_arguments)] // 8 variable inputs; repo precedent (audit.rs:238)
    #[must_use]
    pub fn new(
        event_id: String,
        source_system: String,
        occurred_at: String,
        actor: AuditActor,
        targets: Vec<AuditTarget>,
        aggregate_id: String,
        action: String,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            idempotency_key: event_id.clone(),
            event_id,
            source_system,
            event_type: AUDIT_EVENT_TYPE.to_owned(),
            schema_id: AUDIT_SCHEMA_ID.to_owned(),
            schema_version: AUDIT_SCHEMA_VERSION,
            occurred_at,
            actor,
            targets,
            aggregate_type: AUDIT_AGGREGATE_TYPE.to_owned(),
            aggregate_id,
            action,
            outcome: AUDIT_OUTCOME_SUCCESS.to_owned(),
            payload,
            data_classification: AUDIT_DATA_CLASSIFICATION.to_owned(),
            retention_class: AUDIT_RETENTION_CLASS.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
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
        assert_eq!(MODERATION_OUTBOUND_ACTION, MODERATION_OUTBOUND_VOCABULARY[0]);
        assert_ne!(MODERATION_OUTBOUND_VOCABULARY[0], MODERATION_OUTBOUND_VOCABULARY[1]);
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
            LOCAL_ACTION_MESSAGE_RECALLED, "message.deleted",
            "the recall token is disjoint from the R-D2-excluded message.deleted token"
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
            "message.deleted".to_owned(),
            serde_json::json!({ "reason": "spam" }),
        )
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
}
