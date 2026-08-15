//! Room-lane + recall-lane facade drills (R1/R2/R3/R5): drive the REAL
//! `ImService` room/recall write paths through in-tx audit → 0245/0246
//! trigger enqueue → `claim_due` → real `AuditRelay` → `StubSink` → settle.
//!
//! Why this module exists: the room lane (R1/R2) and the recall lane (R3)
//! have ZERO facade-level coverage — every existing drill scrubs the
//! class-'room' rows its own fixture produces (`scrub_room_lane_rows`), and
//! `recall_tests.rs` covers recall behavior without governance assertions.
//! A regression that drops the `room.create`/`room.archived`/
//! `message.recalled` audit append or the 0245/0246 trigger is invisible to
//! the im-core suite. These drills pin the full chain from a facade call:
//! create/archive/recall → in-tx `AuditRepo::append_in_tx` → 0245/0246 1:1
//! outbox row → `claim_due` → `AuditRelay` → `StubSink` echo → `settle`
//! status 2. A future im-core op that forgets the audit append yields zero
//! rows ⇒ red, never vacuous-green.
//!
//! Lane arbitrations pinned (do not weaken):
//!   * Room lane (0245): `room.create`/`room.archived` → class 'room',
//!     priority 10, outbox rows produced ONLY by the 0245 trigger, action =
//!     local token VERBATIM (no fabricated contract token). Gate-free:
//!     enforcement OFF is the correct fixture state (0245 has no binding
//!     lookup, no runtime gate).
//!   * Recall lane (0246): `message.recalled` rows DO enter the outbox —
//!     1:1 (`event_id` = audit id), class 'message', priority 10 — produced
//!     ONLY by the 0246 trigger. `governance_lane_for("message.recalled")`
//!     stays `None` (the arbitration; pinned at unit level by the aero-ai
//!     suite `unknown_local_token_passes_through_unmapped` /
//!     `admin_class_rows_never_aggregated` — referenced, not duplicated).
//!     Recall rows NEVER fold into the 0242 L1 window (the 1:1 requirement
//!     is load-bearing: recall counts must not corrupt create/edit
//!     aggregates).
//!   * Relay `source_system` per lane: room/recall rows stamp the pinned
//!     `AUDIT_SOURCE_SYSTEM` constant (0245/0246 write it into the payload),
//!     so every relay config here passes the compile-time const — a
//!     binding-value config deads the row at `validate_delivery_payload`
//!     before any POST (D2 pin, same as FR-1).
//!
//! Posture notes (per design §8 / resolution-m1-m2-posture.md §4.4):
//!   * M1: facade drills run JWKS-off (`jwks_uri: None` ⇒
//!     `verify_token_signature` unconditional Ok — the production default).
//!     The signature plane is pinned at facade level by
//!     `drill_posture_jwks_on_*` and at relay-unit level by
//!     `jwks_fetch_failure_requeues_without_any_post` /
//!     `rotation_lag_recovers_via_bypass_before_dead`. Production MUST set
//!     `AERO_AUDIT_JWKS_URL`; the drill suite does not change that default.
//!   * M2: token-endpoint failures are ALL transient by current production
//!     classification (RFC 6749 §5.2 error responses are not distinguished;
//!     any non-200 → Transient → infinite requeue, capped backoff, no
//!     terminal). `drill_posture_token_endpoint_failures_requeue_never_dead`
//!     pins that contract. A terminal class for 401/400 is a DEFERRED
//!     production change (Change Manifest); it must red that drill.

use aero_audit_connector::stub::{SinkBehavior, StubSink};
use aero_common::{
    AuditClaimPayload, AuditId, Block, RoomKind, AUDIT_SOURCE_SYSTEM, GOVERNANCE_CLASS_MESSAGE,
    GOVERNANCE_CLASS_ROOM, LOCAL_ACTION_MESSAGE_RECALLED, LOCAL_ACTION_ROOM_ARCHIVED,
    LOCAL_ACTION_ROOM_CREATE,
};
use uuid::Uuid;

use super::shared::*;
use super::*;

/// One room-create fixture (shared by all six drills): self-isolate → start-
/// of-test singleton re-assert (enforcement OFF — 0245 is gate-free; do NOT
/// seed a binding) → workspace/room. Returns the room + the audit id (1:1
/// with the outbox `event_id`) + the outbox row.
struct RoomFixture {
    ws: WorkspaceId,
    owner: ParticipantId,
    room: aero_common::RoomId,
    /// The `room.create` audit row id == the 0245 outbox `event_id`.
    audit_id: AuditId,
}

async fn room_create_fixture(pool: &PgPool, svc: &crate::service::ImService, prefix: &str) -> RoomFixture {
    self_isolate(pool).await;
    // Start-of-test singleton re-assert (module invariant — no Drop guard;
    // a panicked earlier drill may have left the global ON). Enforcement OFF
    // is correct: 0245 is gate-free (no binding lookup, no runtime gate).
    restore_enforcement_disabled(pool).await;
    let (ws, owner) = workspace_fixture(pool, prefix).await;
    let room = svc
        .create_room_in_workspace(owner, ws, RoomKind::Channel, Some(format!("{prefix}-room")))
        .await
        .expect("create room in workspace")
        .id;
    // Exactly 1 room.create audit row (a dropped `append_in_tx` yields zero
    // rows — loud red, never vacuous).
    let (audit_id, actor, target, detail): (
        Uuid,
        Option<Uuid>,
        String,
        serde_json::Value,
    ) = sqlx::query_as(
        "SELECT id, actor_id, target, detail FROM audit_events
          WHERE workspace_id = $1 AND action = $2",
    )
    .bind(ws.to_uuid())
    .bind(LOCAL_ACTION_ROOM_CREATE)
    .fetch_one(pool)
    .await
    .expect("exactly one room.create audit row");
    assert_eq!(actor, Some(owner.to_uuid()), "audit actor = creator");
    assert_eq!(target, room.to_string(), "audit target = room id");
    assert_eq!(
        detail.get("kind").and_then(serde_json::Value::as_str),
        Some("channel"),
        "detail.kind = channel"
    );
    assert!(
        detail.get("name").is_none(),
        "unvalidated name must not enter the audit payload (G-SEC2)"
    );
    RoomFixture {
        ws,
        owner,
        room,
        audit_id: AuditId::from_uuid(audit_id),
    }
}

/// Assert the 0245 1:1 outbox row shape (shared by R1/R2 and the R5 legs):
/// class 'room', priority 10, status 0, attempts 0, `last_error` NULL, the
/// 16-key envelope, `action` == the leaf token VERBATIM, `idempotency_key`
/// == `event_id`, `source_system` == `AUDIT_SOURCE_SYSTEM`.
async fn assert_room_outbox_shape(
    pool: &PgPool,
    audit_id: AuditId,
    action: &str,
    aggregate_id: WorkspaceId,
    actor_id: Option<Uuid>,
    target: &str,
) {
    let (class, priority, status, attempts, last_error): (String, i16, i32, i64, Option<String>) =
        sqlx::query_as(
            "SELECT class, priority, status, attempts, last_error
               FROM audit_governance_outbox WHERE event_id = $1",
        )
        .bind(audit_id.to_uuid())
        .fetch_one(pool)
        .await
        .expect("outbox row");
    assert_eq!(class, GOVERNANCE_CLASS_ROOM, "class 'room'");
    assert_eq!(priority, 10, "priority 10 (GOVERNANCE_PRIORITY_BACKLOG)");
    assert_eq!(status, 0, "status 0 = enqueued (0245 normative)");
    assert_eq!(attempts, 0, "attempts 0");
    assert!(last_error.is_none(), "last_error NULL");
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM audit_governance_outbox WHERE event_id = $1",
    )
    .bind(audit_id.to_uuid())
    .fetch_one(pool)
    .await
    .expect("outbox payload");
    let obj = payload.as_object().expect("payload is an object");
    for key in ENVELOPE_KEYS {
        assert!(obj.contains_key(key), "missing envelope key {key}");
    }
    assert_eq!(obj.len(), 16, "exactly the 16-key envelope");
    // Typed fail-closed parse (deny_unknown_fields) — an envelope drift reds
    // here, not just at the key-count assert.
    let typed: AuditClaimPayload =
        serde_json::from_value(payload.clone()).expect("payload parses into the leaf twin");
    assert_eq!(
        serde_json::to_value(&typed).unwrap(),
        payload,
        "re-serialized twin equals the stored JSONB (semantic parity)"
    );
    assert_eq!(payload["action"], action, "action == leaf token verbatim");
    let audit_id_text = audit_id.to_uuid().to_string();
    assert_eq!(payload["event_id"], audit_id_text, "envelope event_id");
    assert_eq!(
        payload["idempotency_key"], audit_id_text,
        "idempotency_key == event_id"
    );
    assert_eq!(
        payload["source_system"], AUDIT_SOURCE_SYSTEM,
        "0245 stamps the pinned AUDIT_SOURCE_SYSTEM (not a binding value)"
    );
    assert_eq!(
        payload["aggregate_id"],
        aggregate_id.to_uuid().to_string(),
        "aggregate_id == workspace id"
    );
    if let Some(actor) = actor_id {
        assert_eq!(
            payload["actor"]["id"],
            actor.to_string(),
            "actor.id == the human creator"
        );
        assert_eq!(
            payload["actor"]["type"], "participant",
            "actor.type == participant (never system for a room op)"
        );
    } else {
        assert_eq!(payload["actor"]["id"], "system", "nil actor → system id");
        assert_eq!(payload["actor"]["type"], "system");
    }
    assert_eq!(
        payload["targets"][0]["id"],
        target,
        "targets[0].id == target"
    );
    assert_eq!(payload["targets"][0]["type"], "resource");
}

/// R1 — room-create E2E (acceptance core): `create_room_in_workspace` →
/// in-tx audit → 0245 1:1 outbox row → relay → sink echo → settle status 2.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_room_create_claims_delivers_settles() {
    let pool = pool();
    let svc = service(pool.clone());
    let f = room_create_fixture(&pool, &svc, "drill-room-create").await;
    assert_room_outbox_shape(
        &pool,
        f.audit_id,
        LOCAL_ACTION_ROOM_CREATE,
        f.ws,
        Some(f.owner.to_uuid()),
        &f.room.to_string(),
    )
    .await;

    // Relay leg — D2 pin: the 0245 row carries `source_system =
    // AUDIT_SOURCE_SYSTEM`, so the relay config MUST use the compile-time
    // constant (a binding-value config deads the row pre-POST).
    let stub = StubSink::start().await.expect("start stub");
    let (relay, _repo, _config) = relay_for(&pool, &stub, AUDIT_SOURCE_SYSTEM).await;
    assert_eq!(
        relay.dispatch_batch().await.expect("dispatch"),
        1,
        "claims exactly the room row"
    );
    assert_eq!(stub.posts(), 1, "exactly one POST");
    // Dual-format pin: header = base32 `AuditId` Display, payload `event_id`
    // = `uuid::text` — unequal by design.
    let key_b32 = f.audit_id.to_string();
    assert_ne!(
        key_b32,
        f.audit_id.to_uuid().to_string(),
        "base32 header vs uuid::text payload (dual-format)"
    );
    assert_eq!(
        stub.seen_idempotency_keys().await,
        vec![key_b32],
        "Idempotency-Key header = AuditId base32"
    );
    let row = outbox_row(&pool, f.audit_id).await;
    assert_eq!(row.status, 2, "the stub's 202 + receipt echo settles the row");
    assert!(row.delivered_at.is_some(), "delivered_at stamped");
    assert_eq!(row.attempts, 1, "exactly one claim (no double-claim)");
    assert!(row.claim_token.is_none(), "fencing token cleared on settle");
    assert!(row.lease_expires_at.is_none(), "lease cleared on settle");
    assert!(row.last_error.is_none(), "no error recorded on settle");

    cleanup_facade_rows(&pool, f.ws, &[], &[f.audit_id], &[]).await;
}

/// R1 relay-absent leg (T-11, mirrors FR-3): with NO relay constructed, the
/// room row stays `status 0`, `attempts 0`, never delivered, never dead, and
/// still selectable by the `claim_due` predicate. Structural — no sleeps.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_room_create_relay_absent_stays_status_zero() {
    let pool = pool();
    let svc = service(pool.clone());
    let f = room_create_fixture(&pool, &svc, "drill-room-relay-absent").await;

    let row = outbox_row(&pool, f.audit_id).await;
    assert_eq!(row.status, 0, "stays enqueued without a relay");
    assert_eq!(row.attempts, 0, "zero claims");
    assert!(row.delivered_at.is_none(), "never delivered");
    assert!(row.last_error.is_none(), "no error");
    assert!(
        claim_predicate_holds(&pool, f.audit_id.to_uuid()).await,
        "relay-absent row stays due/claimable"
    );

    cleanup_facade_rows(&pool, f.ws, &[], &[f.audit_id], &[]).await;
}

/// R2 — archive leg: `archive_channel` → `room.archived` audit row in-tx →
/// 0245 1:1 outbox row → relay → settle status 2. Together with R1, both
/// room tokens are proven verbatim through the relay payload.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_room_archive_claims_delivers_settles() {
    let pool = pool();
    let svc = service(pool.clone());
    let f = room_create_fixture(&pool, &svc, "drill-room-archive").await;
    // Scrub the create's OWN class-'room' row so the archive assertions are
    // exact-count (the create row sorts ahead in `claim_due`'s total order).
    scrub_room_lane_rows(&pool).await;

    svc.archive_channel(f.owner, f.room, true)
        .await
        .expect("archive channel commits");

    // Exactly 1 room.archived audit row (actor == owner, detail.archived).
    let (audit_id, actor, target, detail): (
        Uuid,
        Option<Uuid>,
        String,
        serde_json::Value,
    ) = sqlx::query_as(
        "SELECT id, actor_id, target, detail FROM audit_events
          WHERE workspace_id = $1 AND action = $2",
    )
    .bind(f.ws.to_uuid())
    .bind(LOCAL_ACTION_ROOM_ARCHIVED)
    .fetch_one(&pool)
    .await
    .expect("exactly one room.archived audit row");
    assert_eq!(actor, Some(f.owner.to_uuid()), "audit actor = the archiver");
    assert_eq!(target, f.room.to_string(), "audit target = room id");
    assert_eq!(
        detail.get("archived").and_then(serde_json::Value::as_bool),
        Some(true),
        "detail.archived = true"
    );
    let audit_id = AuditId::from_uuid(audit_id);
    assert_room_outbox_shape(
        &pool,
        audit_id,
        LOCAL_ACTION_ROOM_ARCHIVED,
        f.ws,
        Some(f.owner.to_uuid()),
        &f.room.to_string(),
    )
    .await;

    // Same relay leg → settle status 2 (proves room.archived flows verbatim).
    let stub = StubSink::start().await.expect("start stub");
    let (relay, _repo, _config) = relay_for(&pool, &stub, AUDIT_SOURCE_SYSTEM).await;
    assert_eq!(
        relay.dispatch_batch().await.expect("dispatch"),
        1,
        "claims exactly the archive row"
    );
    assert_eq!(stub.posts(), 1, "exactly one POST");
    assert_eq!(
        stub.seen_idempotency_keys().await,
        vec![audit_id.to_string()],
        "Idempotency-Key header = AuditId base32"
    );
    let row = outbox_row(&pool, audit_id).await;
    assert_eq!(row.status, 2, "archive leg settles to status 2");
    assert!(row.delivered_at.is_some(), "delivered_at stamped");
    assert_eq!(row.attempts, 1, "exactly one claim");
    assert!(row.last_error.is_none(), "no error recorded on settle");

    cleanup_facade_rows(&pool, f.ws, &[], &[audit_id], &[]).await;
}

/// R3 — recall leg + unmapped-lane pin: `recall_message` (room owner path) →
/// in-tx `message.recalled` audit append → 0246 1:1 outbox row → relay →
/// settle status 2. Pins the arbitration: recall rows DO enter the outbox
/// (0246 trigger, class 'message', 1:1) while `governance_lane_for(
/// "message.recalled")` stays `None` (unit-pinned in aero-ai; referenced,
/// not duplicated) — and recall NEVER folds into the 0242 L1 window.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_recall_claims_delivers_settles() {
    let pool = pool();
    let svc = service(pool.clone());
    // Fixture: self-isolate + singleton re-assert (enforcement OFF — 0246 is
    // gate-free) + workspace + room + message. The fixture's own writes
    // enqueue a class-'room' row (0245) and a window row (0242) — scrub both
    // lanes before counting so the recall assertions are exact-count.
    let f = room_create_fixture(&pool, &svc, "drill-recall").await;
    let msg = svc
        .send_message(
            f.owner,
            f.room,
            vec![Block::text("drill-recall message body")],
            None,
            None,
        )
        .await
        .expect("send message");
    scrub_room_lane_rows(&pool).await;
    // The 0242 window row (marker-key presence): delete the fixture's own
    // aggregated row so the "no recall folding" count is exact.
    sqlx::query(
        "DELETE FROM audit_governance_outbox
          WHERE payload->>'aggregated' = 'true' AND payload->>'aggregate_id' = $1",
    )
    .bind(f.ws.to_uuid().to_string())
    .execute(&pool)
    .await
    .expect("scrub the 0242 window row");

    // Recall as the room owner/admin (the `recall_authorized` owner arm —
    // window-exempt; with the shared fixture's ZERO window the author arm
    // would also pass, E14 — the drill exercises the owner path and pins the
    // audit actor = the recaller).
    svc.recall_message(f.owner, msg.id)
        .await
        .expect("recall_message commits as room owner");

    // Exactly 1 message.recalled audit row: actor == recaller, target ==
    // message id, detail {room_id, digest} — digest is the REAL producer
    // seam (recomputed equality, DR-2: at most 120 chars, never '{}').
    let (audit_id, actor, target, detail, created_at): (
        Uuid,
        Option<Uuid>,
        String,
        serde_json::Value,
        time::OffsetDateTime,
    ) = sqlx::query_as(
        "SELECT id, actor_id, target, detail, created_at FROM audit_events
          WHERE workspace_id = $1 AND action = $2",
    )
    .bind(f.ws.to_uuid())
    .bind(LOCAL_ACTION_MESSAGE_RECALLED)
    .fetch_one(&pool)
    .await
    .expect("exactly one message.recalled audit row");
    assert_eq!(actor, Some(f.owner.to_uuid()), "actor == the recaller (never system)");
    assert_eq!(target, msg.id.to_string(), "audit target = message id");
    assert_eq!(
        detail.get("room_id").and_then(serde_json::Value::as_str),
        Some(f.room.to_string().as_str()),
        "detail.room_id == room id"
    );
    let expected_digest: String = msg.searchable_text().chars().take(120).collect();
    assert!(!expected_digest.is_empty(), "fixture message has searchable text");
    let digest = detail
        .get("digest")
        .and_then(serde_json::Value::as_str)
        .expect("detail.digest present");
    assert_eq!(
        digest, expected_digest,
        "digest == the real producer seam (searchable_text().chars().take(120))"
    );
    assert_ne!(digest, "{}", "digest is never the empty-detail placeholder");

    // Exactly 1 outbox row (0246 trigger): class 'message', priority 10,
    // status 0, attempts 0, 16-key envelope, action verbatim,
    // source_system = AUDIT_SOURCE_SYSTEM, targets non-empty, actor type
    // participant, and NO L1 marker keys.
    let audit_id = AuditId::from_uuid(audit_id);
    let (class, priority, status, attempts, last_error): (String, i16, i32, i64, Option<String>) =
        sqlx::query_as(
            "SELECT class, priority, status, attempts, last_error
               FROM audit_governance_outbox WHERE event_id = $1",
        )
        .bind(audit_id.to_uuid())
        .fetch_one(&pool)
        .await
        .expect("recall outbox row");
    assert_eq!(class, GOVERNANCE_CLASS_MESSAGE, "class 'message' (0246)");
    assert_eq!(priority, 10, "priority 10 (GOVERNANCE_PRIORITY_BACKLOG)");
    assert_eq!(status, 0, "status 0 = enqueued (0246 normative)");
    assert_eq!(attempts, 0, "attempts 0");
    assert!(last_error.is_none(), "last_error NULL");
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM audit_governance_outbox WHERE event_id = $1",
    )
    .bind(audit_id.to_uuid())
    .fetch_one(&pool)
    .await
    .expect("recall outbox payload");
    let obj = payload.as_object().expect("payload is an object");
    for key in ENVELOPE_KEYS {
        assert!(obj.contains_key(key), "missing envelope key {key}");
    }
    assert_eq!(obj.len(), 16, "exactly the 16-key envelope");
    // Typed fail-closed parse (deny_unknown_fields) — any extra key (an L1
    // marker, a fabricated token) reds here.
    let typed: AuditClaimPayload =
        serde_json::from_value(payload.clone()).expect("payload parses into the leaf twin");
    assert_eq!(
        serde_json::to_value(&typed).unwrap(),
        payload,
        "re-serialized twin equals the stored JSONB (semantic parity)"
    );
    for absent in ["aggregated", "spill", "count", "window_start", "window_end", "first_event_at", "last_event_at"] {
        assert!(
            !obj.contains_key(absent),
            "1:1 recall payload must not carry L1 marker '{absent}'"
        );
    }
    assert_eq!(
        payload["action"], LOCAL_ACTION_MESSAGE_RECALLED,
        "action == leaf LOCAL_ACTION_MESSAGE_RECALLED verbatim"
    );
    let audit_id_text = audit_id.to_uuid().to_string();
    assert_eq!(payload["event_id"], audit_id_text, "envelope event_id");
    assert_eq!(
        payload["idempotency_key"], audit_id_text,
        "idempotency_key == event_id"
    );
    assert_eq!(
        payload["source_system"], AUDIT_SOURCE_SYSTEM,
        "0246 stamps the pinned AUDIT_SOURCE_SYSTEM"
    );
    assert_eq!(
        payload["actor"]["id"],
        f.owner.to_uuid().to_string(),
        "actor.id == the recaller"
    );
    assert_eq!(
        payload["actor"]["type"], "participant",
        "actor.type == participant (recall never system)"
    );
    assert_eq!(
        payload["targets"][0]["id"],
        msg.id.to_string(),
        "targets[0].id == message id"
    );
    assert_eq!(payload["targets"][0]["type"], "resource");

    // Unmapped-lane pin (a): recall NEVER folds into the 0242 window — the
    // outbox holds no aggregated row for the ws, and the recall event_id is
    // not the recomputed window key for the recall's own created_at (DR-4:
    // bind the RECALL audit row's created_at — a widened 0242 at an
    // epoch-boundary straddle would red here).
    let aggregated: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_governance_outbox
          WHERE payload->>'aggregated' = 'true' AND payload->>'aggregate_id' = $1",
    )
    .bind(f.ws.to_uuid().to_string())
    .fetch_one(&pool)
    .await
    .expect("aggregated row count");
    assert_eq!(aggregated, 0, "no 0242 window row for the ws after the recall");
    let window_key = window_row_for(&pool, f.ws, created_at).await;
    assert_ne!(
        audit_id.to_uuid(),
        window_key,
        "recall event_id != the recomputed 0242 window key (1:1 never folds)"
    );

    // Relay leg → settle status 2 (0246 stamps AUDIT_SOURCE_SYSTEM).
    let stub = StubSink::start().await.expect("start stub");
    let (relay, _repo, _config) = relay_for(&pool, &stub, AUDIT_SOURCE_SYSTEM).await;
    assert_eq!(
        relay.dispatch_batch().await.expect("dispatch"),
        1,
        "claims exactly the recall row"
    );
    assert_eq!(stub.posts(), 1, "exactly one POST");
    assert_eq!(
        stub.seen_idempotency_keys().await,
        vec![audit_id.to_string()],
        "Idempotency-Key header = AuditId base32"
    );
    let row = outbox_row(&pool, audit_id).await;
    assert_eq!(row.status, 2, "recall row settles to status 2");
    assert!(row.delivered_at.is_some(), "delivered_at stamped");
    assert_eq!(row.attempts, 1, "exactly one claim");
    assert!(row.last_error.is_none(), "no error recorded on settle");

    cleanup_facade_rows(&pool, f.ws, &[msg.id], &[audit_id], &[]).await;
}

// ---------------------------------------------------------------------------
// R5 — T-11 terminal legs over the ROOM lane (mirror `terminal.rs`, but the
// 422 leg uses the deterministic `force_re_due` — no sleeps). `claim_due` is
// class-agnostic, so the same predicate semantics are proven over the room
// lane; the pre-existing admin-lane drills stay green and untouched.
// ---------------------------------------------------------------------------

/// 403 → IMMEDIATE dead (T-11 fail-closed, zero retries) on a room row: one
/// round, `status == 3`, `attempts == 1`, `delivered_at` NULL, exact
/// Forbidden-arm `last_error`.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_room_403_forbidden_deads_immediately() {
    let pool = pool();
    let svc = service(pool.clone());
    let f = room_create_fixture(&pool, &svc, "drill-room-403").await;

    let stub = StubSink::start().await.expect("start stub");
    stub.set_behavior(SinkBehavior {
        events_status: 403,
        ..SinkBehavior::default()
    })
    .await;
    let (relay, _repo, _config) = relay_for(&pool, &stub, AUDIT_SOURCE_SYSTEM).await;

    assert_eq!(
        relay.dispatch_batch().await.expect("single round"),
        1,
        "the room row is claimed"
    );
    let row = outbox_row(&pool, f.audit_id).await;
    assert_eq!(row.status, 3, "403 → immediate dead (T-11 fail-closed)");
    assert_eq!(row.attempts, 1, "zero retries for the identity fault");
    assert!(row.delivered_at.is_none(), "never delivered");
    assert_eq!(
        row.last_error.as_deref(),
        Some("audit sink rejected the service identity (HTTP 403)"),
        "exact relay Forbidden-arm string (same as the admin lane)"
    );
    assert_eq!(stub.posts(), 1);

    cleanup_facade_rows(&pool, f.ws, &[], &[f.audit_id], &[]).await;
}

/// 422 → dead ≤1 retry on a room row: attempt 1 requeues with backoff,
/// `force_re_due` re-parks deterministically (no sleep), attempt 2 →
/// `status == 3`, `attempts == 2`.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_room_422_permanent_deads_at_attempt_two() {
    let pool = pool();
    let svc = service(pool.clone());
    let f = room_create_fixture(&pool, &svc, "drill-room-422").await;

    let stub = StubSink::start().await.expect("start stub");
    stub.set_behavior(SinkBehavior {
        events_status: 422,
        ..SinkBehavior::default()
    })
    .await;
    let (relay, _repo, _config) = relay_for(&pool, &stub, AUDIT_SOURCE_SYSTEM).await;

    // Round 1: claim → 422 → permanent → requeue (attempt 1, never dead).
    assert_eq!(
        relay.dispatch_batch().await.expect("round 1"),
        1,
        "claims the room row"
    );
    let row = outbox_row(&pool, f.audit_id).await;
    assert_eq!(row.status, 0, "attempt 1 requeues — never dead (≤1 retry)");
    assert_eq!(row.attempts, 1);
    assert!(row.delivered_at.is_none(), "never delivered");
    assert!(
        row.last_error
            .as_deref()
            .is_some_and(|e| e.contains("Unprocessable")),
        "last_error names the permanent class (got {:?})",
        row.last_error
    );
    assert_eq!(stub.posts(), 1, "exactly one real POST in round 1");

    // Deterministic re-due (no sleep): backdate `available_at` past the
    // requeue's backoff(1) park.
    force_re_due(&pool, f.audit_id).await;
    assert_eq!(
        relay.dispatch_batch().await.expect("round 2"),
        1,
        "reclaims after re-due"
    );
    let row = outbox_row(&pool, f.audit_id).await;
    assert_eq!(row.status, 3, "dead terminal at attempt 2 (≤1 retry)");
    assert_eq!(row.attempts, 2, "budget counts claims: exactly two");
    assert!(row.delivered_at.is_none(), "never delivered");
    assert!(
        row.last_error
            .as_deref()
            .is_some_and(|e| e.contains("classified permanent: Unprocessable")),
        "last_error pins the dead cause (got {:?})",
        row.last_error
    );
    assert_eq!(stub.posts(), 2, "exactly two real POSTs — never a third");
    assert!(
        !claim_predicate_holds(&pool, f.audit_id.to_uuid()).await,
        "dead is a real terminal, not a stuck row"
    );

    cleanup_facade_rows(&pool, f.ws, &[], &[f.audit_id], &[]).await;
}
