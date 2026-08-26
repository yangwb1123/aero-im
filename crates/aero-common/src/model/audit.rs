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

// ---- Message-delete vocabulary (R-D2 Rust writer) ----
// `message.deleted` is NOT trigger-owned (0245/0246 headers declare the
// carve-out): the governance outbox row is produced by the Rust writer
// (`AuditGovernanceOutboxRepo::append_message_delete_in_tx`) from the
// soft-delete choke point — never by an AFTER-INSERT allowlist.

/// R-D2: user-delete token. NOT trigger-owned (0245/0246 carve-out); the
/// governance outbox row is produced by the Rust writer
/// (`AuditGovernanceOutboxRepo::append_message_delete_in_tx`) from the
/// soft-delete choke point — never by an AFTER-INSERT allowlist. Stays
/// UNMAPPED in `governance_lane_for` (aero-ai) — the writer is a direct
/// outbox write, not a lane mapping.
pub const LOCAL_ACTION_MESSAGE_DELETED: &str = "message.deleted";

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

/// Canonical `occurred_at` wire spelling shared by every Rust producer of
/// the 0239 envelope (R-D2 §10.4): PG renders `timestamptz → jsonb` as
/// ISO-8601-with-offset with `+00:00` (never `Z`) and a trailing-zero-\
/// trimmed subsecond fraction that is omitted when zero (e.g.
/// `2026-08-15T12:34:56.123456+00:00`, `…56.5+00:00`, `…56+00:00`) — the
/// `time` crate's `Rfc3339` well-known format instead emits `Z`, which
/// drifts the wire contract vs trigger-produced rows. The 0239 trigger's
/// `jsonb_build_object('occurred_at', NEW.created_at)` and this helper are
/// the single canonical spelling; AC-3 byte-asserts both sides against it.
#[must_use]
pub fn audit_wire_occurred_at(ts: time::OffsetDateTime) -> String {
    // `[subsecond digits:1+]` reproduces PG's trailing-zero trim exactly
    // (`.000010` → `.00001`); a zero subsecond renders no fraction at all
    // (PG: `…56+00:00`, never `…56.0+00:00`) — hence the two format items.
    static WITH_FRACTION: &[time::format_description::BorrowedFormatItem<'static>] = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:1+]+00:00"
    );
    static WITHOUT_FRACTION: &[time::format_description::BorrowedFormatItem<'static>] =
        time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]+00:00");
    let utc = ts.to_offset(time::UtcOffset::UTC);
    if utc.nanosecond() == 0 {
        utc.format(WITHOUT_FRACTION)
            .expect("offset datetime formats")
    } else {
        utc.format(WITH_FRACTION).expect("offset datetime formats")
    }
}
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

/// Rust twin of the 0242 SQL-built L1 aggregate envelope. This remains a
/// separate strict shape because aggregate rows deliberately do not carry the
/// actor/targets/payload fields required by `AuditClaimPayload`; making those
/// fields optional would weaken the 0239 drift alarm.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditAggregatePayload {
    pub event_id: String,
    pub source_system: String,
    pub event_type: String,
    pub schema_id: String,
    pub schema_version: u32,
    pub occurred_at: String,
    pub aggregate_type: String,
    pub aggregate_id: String,
    pub action: String,
    pub outcome: String,
    pub data_classification: String,
    pub retention_class: String,
    pub idempotency_key: String,
    pub count: i64,
    pub aggregated: bool,
    /// Only spill rows carry this discriminator. A missing key is distinct
    /// from `spill: false` and is preserved on serialization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spill: Option<bool>,
    pub window_start: String,
    pub window_end: String,
    pub first_event_at: String,
    pub last_event_at: String,
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
mod tests;
