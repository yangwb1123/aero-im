//! Facade-level B5-1 message-lane drill (FR-1…FR-4, FR-6, FR-7): drive the
//! REAL `ImService::send_message`/`edit_message`/`moderate_delete` through
//! in-tx audit → 0242 L1 window → `claim_due` → real `AuditRelay` →
//! `StubSink`, plus the window lane's crash/reclaim and permanent-dead
//! terminals.
//!
//! Why this module exists: the admin lane is pinned at facade level
//! (`moderated_row`), but the message lane is pinned only at repo level
//! (`aero-storage producer/send.rs` drives `MessageRepo::insert_outboxed`
//! directly, bypassing `ImService` — the facade the platform calls), and the
//! suite's `scrub_seam_rows` deletes the facade-produced seam rows before any
//! assertion. These drills pin the full chain from a facade call:
//! send/edit → in-tx `AuditRepo::append_in_tx` → 0242 window row →
//! `claim_due` → `AuditRelay` → `StubSink` echo → `settle` status 2. A
//! future im-core op that forgets the audit append yields zero window rows
//! ⇒ red, never vacuous-green.
//!
//! Load-bearing pins (do not weaken): D1 — window payloads are the 19-key
//! AGGREGATED shape (no `actor`/`targets`/`payload`/`spill`): pinned
//! Value-level ONLY, a typed `AuditClaimPayload` parse is FORBIDDEN
//! (`deny_unknown_fields` rejects the aggregated shape by design). D2 —
//! window-row relays ALWAYS pass `aero_common::AUDIT_SOURCE_SYSTEM` (a
//! binding-value config deads the row at `validate_delivery_payload` before
//! any POST). D3 — `scrub_room_lane_rows` after every facade write (the
//! 0245 class-'room' row sorts ahead of the window row in `claim_due`'s
//! total order; no-op when 0245 absent). Timestamps compare via cast-back
//! SQL; start-of-test singleton re-assert + `cleanup_facade_rows` restore
//! (no Drop guard). `--test-threads=1` runbook.

use aero_audit_connector::stub::{DuplicateBehavior, SinkBehavior, StubSink};
use aero_common::{
    AGGREGATED_MESSAGE_ACTION, AUDIT_DATA_CLASSIFICATION, AUDIT_EVENT_TYPE, AUDIT_OUTCOME_SUCCESS,
    AUDIT_RETENTION_CLASS, AUDIT_SCHEMA_ID, AUDIT_SCHEMA_VERSION, AUDIT_SOURCE_SYSTEM,
    GOVERNANCE_CLASS_MESSAGE, LOCAL_ACTION_MESSAGE_CREATE, LOCAL_ACTION_MESSAGE_EDIT,
};
use uuid::Uuid;

use super::shared::*;
use super::*;

// ----- Shared helpers live in `shared.rs` (D2 hoist): `scrub_room_lane_rows`,
// `window_row_for`, `claim_predicate_holds`, `force_re_due`, `cleanup_facade_rows`,
// `outbox_row`/`RowState`, `ENVELOPE_KEYS`. Imported via `super::shared::*`.

// ----- Shared window fixture (FR-1/FR-3/FR-4/FR-6/FR-7; FR-2 does its own setup — its moderation lane needs seed BEFORE writes). -----

struct FacadeWindow {
    ws: WorkspaceId,
    owner: ParticipantId,
    msg_id: MessageId,
    /// Recomputed 0242 window PK (`md5(ws|message|epoch)::uuid`).
    key: Uuid,
    /// `AuditId::from_uuid(key)` — the base32 header-spelling source.
    key_audit_id: AuditId,
    /// The send's audit `created_at` (the key-recompute source).
    created_at: time::OffsetDateTime,
}

/// One facade window-row setup (shared by the five message-only legs):
/// `self_isolate` + enforcement re-assert → workspace/room/message →
/// `scrub_room_lane_rows` → audit-seam pin (exactly 1 `message.create`) →
/// window-key pin (`event_id == recomputed key`, class message, priority 10,
/// status 0, attempts 0). Enforcement OFF is correct — 0242 has no gate.
async fn facade_window_fixture(
    pool: &PgPool,
    svc: &crate::service::ImService,
    prefix: &str,
) -> FacadeWindow {
    self_isolate(pool).await;
    // Start-of-test singleton re-assert (module invariant — no Drop guard;
    // a panicked earlier drill may have left the global ON).
    restore_enforcement_disabled(pool).await;
    let (ws, owner) = workspace_fixture(pool, prefix).await;
    let room = svc
        .create_room_in_workspace(owner, ws, RoomKind::Channel, Some(format!("{prefix}-room")))
        .await
        .expect("create room in workspace")
        .id;
    let msg = svc
        .send_message(
            owner,
            room,
            vec![Block::text(format!("{prefix} facade drill message"))],
            None,
            None,
        )
        .await
        .expect("send message");
    scrub_room_lane_rows(pool).await;
    // Audit seam: exactly 1 message.create row (a dropped `append_in_tx`
    // yields zero rows — loud red, never vacuous).
    let (created_at, target, actor): (time::OffsetDateTime, String, Option<Uuid>) =
        sqlx::query_as(
            "SELECT created_at, target, actor_id FROM audit_events
              WHERE workspace_id = $1 AND action = $2",
        )
        .bind(ws.to_uuid())
        .bind(LOCAL_ACTION_MESSAGE_CREATE)
        .fetch_one(pool)
        .await
        .expect("exactly one message.create audit row");
    assert_eq!(target, msg.id.to_string(), "audit target = message id");
    assert_eq!(actor, Some(owner.to_uuid()), "audit actor = sender");
    // Window key pin: recompute the 0242 md5 preimage from the
    // server-stamped created_at (strongest key-derivation pin).
    let key = window_row_for(pool, ws, created_at).await;
    let (event_id, class, priority, status, attempts, last_error): (
        Uuid,
        String,
        i16,
        i32,
        i64,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT event_id, class, priority, status, attempts, last_error
           FROM audit_governance_outbox
          WHERE payload->>'aggregate_id' = $1 AND class = 'message'",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(pool)
    .await
    .expect("exactly one window row (0242 aggregated)");
    assert_eq!(event_id, key, "outbox event_id == recomputed 0242 window key");
    assert_eq!(class, GOVERNANCE_CLASS_MESSAGE, "class 'message'");
    assert_eq!(priority, 10, "priority 10 (GOVERNANCE_PRIORITY_BACKLOG)");
    assert_eq!(status, 0, "status 0 = enqueued (0242 normative)");
    assert_eq!(attempts, 0, "attempts 0");
    assert!(last_error.is_none(), "last_error NULL");
    FacadeWindow {
        ws,
        owner,
        msg_id: msg.id,
        key,
        key_audit_id: AuditId::from_uuid(key),
        created_at,
    }
}

// ----- FR-1 — send chain E2E: facade send → audit → 0242 window → claim → relay → stub echo → settle (acceptance core). -----

/// `svc.send_message` commits exactly 1 `message.create` audit row + exactly
/// 1 class-'message' priority-10 status-0 window row whose `event_id` equals
/// the recomputed md5 key; `claim_due` returns it; the real `AuditRelay`
/// delivers `action == message.batch` to the `StubSink`; the 202 echo
/// settles the row to status 2. Exact-count/exact-value — never vacuous.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_facade_send_window_claims_delivers_settles() {
    let pool = pool();
    let svc = service(pool.clone());
    let w = facade_window_fixture(&pool, &svc, "drill-facade-send").await;

    // Full Value-level payload pins (D1: 19-key AGGREGATED shape — a typed
    // `AuditClaimPayload` parse is FORBIDDEN here; `deny_unknown_fields`
    // would reject it by design).
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM audit_governance_outbox WHERE event_id = $1",
    )
    .bind(w.key)
    .fetch_one(&pool)
    .await
    .expect("window payload");
    let obj = payload.as_object().expect("payload is an object");
    for absent in ["actor", "targets", "payload", "spill"] {
        assert!(
            !obj.contains_key(absent),
            "aggregated shape: '{absent}' must be absent (19-key window payload)"
        );
    }
    assert_eq!(
        payload["action"], AGGREGATED_MESSAGE_ACTION,
        "envelope action = message.batch"
    );
    // Split asserts (never an `a == b == c` chain) — both payload keys are
    // set from `v_event_id::text` by 0242, pinned against the window PK.
    assert_eq!(
        payload["idempotency_key"],
        w.key.to_string(),
        "idempotency_key == window event_id uuid::text"
    );
    assert_eq!(
        payload["event_id"],
        w.key.to_string(),
        "payload event_id == window PK uuid::text"
    );
    assert_eq!(
        payload["source_system"], AUDIT_SOURCE_SYSTEM,
        "source_system written by the trigger (PayloadGuard passes)"
    );
    assert_eq!(
        payload["aggregate_id"],
        w.ws.to_uuid().to_string(),
        "aggregate_id == workspace id"
    );
    assert_eq!(payload["count"].as_i64(), Some(1), "window count 1");
    assert_eq!(
        payload["aggregated"].as_bool(),
        Some(true),
        "aggregated true"
    );
    assert_eq!(payload["event_type"], AUDIT_EVENT_TYPE);
    assert_eq!(payload["schema_id"], AUDIT_SCHEMA_ID);
    assert_eq!(payload["schema_version"], AUDIT_SCHEMA_VERSION);
    assert_eq!(payload["outcome"], AUDIT_OUTCOME_SUCCESS);
    assert_eq!(payload["data_classification"], AUDIT_DATA_CLASSIFICATION);
    assert_eq!(payload["retention_class"], AUDIT_RETENTION_CLASS);
    // Window span + event stamps via cast-back SQL (avoids the JSONB ISO
    // spelling trap: `to_jsonb(ts)::text` quotes, `ts::text` spaces).
    let span_ok: bool = sqlx::query_scalar(
        "SELECT (payload->>'window_start')::timestamptz = to_timestamp(floor(extract(epoch FROM $1::timestamptz) / 60)::bigint * 60) \
                AND (payload->>'window_end')::timestamptz = to_timestamp(floor(extract(epoch FROM $1::timestamptz) / 60)::bigint * 60 + 60) \
                AND (payload->>'occurred_at')::timestamptz = (payload->>'window_start')::timestamptz \
                AND (payload->>'first_event_at')::timestamptz = $1 \
                AND (payload->>'last_event_at')::timestamptz = $1 \
           FROM audit_governance_outbox WHERE event_id = $2",
    )
    .bind(w.created_at)
    .bind(w.key)
    .fetch_one(&pool)
    .await
    .expect("window span probe");
    assert!(span_ok, "window_start/end span the 60s grid; occurred_at == window_start; first/last == created_at");

    // Relay leg — D2 pin: the window row carries `source_system =
    // AUDIT_SOURCE_SYSTEM`, so the relay config MUST use the compile-time
    // constant (a binding-value config deads the row pre-POST).
    let stub = StubSink::start().await.expect("start stub");
    let (relay, _repo, _config) = relay_for(&pool, &stub, AUDIT_SOURCE_SYSTEM).await;
    assert_eq!(
        relay.dispatch_batch().await.expect("dispatch"),
        1,
        "claims exactly the window row"
    );
    assert_eq!(stub.posts(), 1, "exactly one POST");
    // Dual-format pin: header = base32 `AuditId` Display, payload
    // `event_id` = `uuid::text` — unequal by design.
    let key_b32 = AuditId::from_uuid(w.key).to_string();
    assert_ne!(
        key_b32,
        w.key.to_string(),
        "base32 header vs uuid::text payload (dual-format)"
    );
    assert_eq!(
        stub.seen_idempotency_keys().await,
        vec![key_b32],
        "Idempotency-Key header = AuditId base32"
    );
    let row = outbox_row(&pool, w.key_audit_id).await;
    assert_eq!(
        row.status, 2,
        "the stub's 202 + receipt echo settles the row"
    );
    assert!(row.delivered_at.is_some(), "delivered_at stamped");
    assert_eq!(row.attempts, 1, "exactly one claim (no double-claim)");
    assert!(row.claim_token.is_none(), "fencing token cleared on settle");
    assert!(row.lease_expires_at.is_none(), "lease cleared on settle");
    assert!(row.last_error.is_none(), "no error recorded on settle");

    cleanup_facade_rows(&pool, w.ws, &[w.msg_id], &[w.key_audit_id], &[]).await;
}

// ----- FR-2 — moderation-priority leg: priority 100 preempts the window row in the same batch. -----

/// With a seeded `message.moderated` row (priority 100) and the 0242 window
/// row (priority 10) both due in one batch, two sequential `claim_due(lease,
/// 1)` calls serve the moderation row FIRST and the window row SECOND —
/// strict precedence regardless of enqueue order. `min_service_floor(1) = 0`
/// ⇒ arm A = top-1 of the total order; round 2 excludes the leased row.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_facade_moderation_preempts_window_in_same_batch() {
    let pool = pool();
    let svc = service(pool.clone());
    self_isolate(&pool).await;
    let (ws, owner) = workspace_fixture(&pool, "drill-facade-preempt").await;
    // Enforcement ON + binding + entitlement BEFORE the writes (mandatory:
    // Gate 2 RAISEs without a binding; 0235 metering RAISEs on the INSERT).
    let source = seed_governance_enforcement(&pool, ws).await;
    assert_eq!(source, format!("source-{ws}"), "binding source pin");
    let room = svc
        .create_room_in_workspace(
            owner,
            ws,
            RoomKind::Channel,
            Some("drill-facade-preempt-room".into()),
        )
        .await
        .expect("create room in workspace")
        .id;
    let msg = svc
        .send_message(
            owner,
            room,
            vec![Block::text("facade preempt message")],
            None,
            None,
        )
        .await
        .expect("send message")
        .id;
    // Priority-100 moderation row through the REAL service seam (its own
    // sanity gate asserts 1 audit + 1 outbox row, class 'admin', prio 100).
    let mod_id = moderated_row(&pool, &svc, ws, msg).await;
    scrub_room_lane_rows(&pool).await;
    // Recompute the window key from the send's audit row (exactly 1).
    let created_at: time::OffsetDateTime = sqlx::query_scalar(
        "SELECT created_at FROM audit_events
          WHERE workspace_id = $1 AND action = $2",
    )
    .bind(ws.to_uuid())
    .bind(LOCAL_ACTION_MESSAGE_CREATE)
    .fetch_one(&pool)
    .await
    .expect("send audit created_at");
    let key = window_row_for(&pool, ws, created_at).await;

    let repo = PgOutboxRepo::new(pool.clone());
    // Round 1: limit 1 ⇒ K = min_service_floor(1) = 0 ⇒ arm A = top-1 — the
    // priority-100 row preempts the earlier-enqueued priority-10 window row.
    let round1 = repo
        .claim_due(time::Duration::seconds(30), 1)
        .await
        .expect("round 1");
    assert_eq!(round1.len(), 1, "exactly one row per limit-1 claim");
    assert_eq!(
        round1[0].event_id, mod_id,
        "moderation row claimed first (priority DESC preempts FIFO)"
    );
    assert_eq!(
        round1[0].priority, 100,
        "priority 100 (GOVERNANCE_PRIORITY_MODERATION)"
    );
    assert_eq!(round1[0].class, "admin", "admin lane");
    assert_eq!(round1[0].attempts, 1, "first claim records attempts == 1");
    // Round 2: the moderation row is leased (status 1) → only the window
    // row is left due.
    let round2 = repo
        .claim_due(time::Duration::seconds(30), 1)
        .await
        .expect("round 2");
    assert_eq!(round2.len(), 1, "exactly one row per limit-1 claim");
    assert_eq!(
        round2[0].event_id,
        AuditId::from_uuid(key),
        "window row claimed second"
    );
    assert_eq!(
        round2[0].priority, 10,
        "priority 10 (GOVERNANCE_PRIORITY_BACKLOG)"
    );
    assert_eq!(round2[0].class, "message", "message lane");
    assert_eq!(round2[0].attempts, 1, "first claim records attempts == 1");

    cleanup_facade_rows(&pool, ws, &[msg], &[AuditId::from_uuid(key)], &[mod_id]).await;
}

// ----- FR-3 — T-11 relay-absent leg: window row stays status 0 without a relay. -----

/// With NO relay constructed, the window row remains `status 0`, `attempts
/// 0`, `delivered_at NULL`, `last_error NULL`, and the claim predicate
/// (pg.rs `claim_due` WHERE verbatim) still selects it — never delivered,
/// never dead, never silently dropped. No sleep: "no relay touches it" is
/// structural (no relay loop is spawned).
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_facade_window_relay_absent_stays_status_zero() {
    let pool = pool();
    let svc = service(pool.clone());
    let w = facade_window_fixture(&pool, &svc, "drill-facade-relay-absent").await;

    let row = outbox_row(&pool, w.key_audit_id).await;
    assert_eq!(row.status, 0, "stays enqueued without a relay");
    assert_eq!(row.attempts, 0, "zero claims");
    assert!(row.delivered_at.is_none(), "never delivered");
    assert!(row.last_error.is_none(), "no error");
    // T-11 claim-predicate mirror (pg.rs claim CTE WHERE verbatim): the row
    // is due/claimable, not lost.
    assert!(
        claim_predicate_holds(&pool, w.key).await,
        "relay-absent row stays due/claimable"
    );

    cleanup_facade_rows(&pool, w.ws, &[w.msg_id], &[w.key_audit_id], &[]).await;
}

// ----- FR-4 — facade edit seam merge leg: `message.edit` merges into the SAME 60s window row (count 1→2). -----

/// `svc.edit_message` commits exactly 1 `message.edit` audit row that merges
/// into the existing window row: exactly 1 window row, `count == 2`,
/// `first_event_at` stays the send's `created_at`, `last_event_at` advances
/// to the edit's. SUM conservation pins the parity side.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_facade_edit_merges_into_same_window() {
    let pool = pool();
    let svc = service(pool.clone());
    let w = facade_window_fixture(&pool, &svc, "drill-facade-edit").await;
    // Version-fallback path (messages.rs) — ms-spacing keeps both events in
    // one 60s window (E3 precedent; a boundary hit reds loudly).
    let edited = svc
        .edit_message(
            w.owner,
            w.msg_id,
            vec![Block::text("edited content")],
            None,
        )
        .await
        .expect("edit commits (version-fallback path)");
    assert_eq!(edited.id, w.msg_id, "edit returns the same message");

    // Audit seam: exactly 1 create + exactly 1 edit row for the ws.
    let create_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_events
          WHERE workspace_id = $1 AND action = $2",
    )
    .bind(w.ws.to_uuid())
    .bind(LOCAL_ACTION_MESSAGE_CREATE)
    .fetch_one(&pool)
    .await
    .expect("message.create audit count");
    assert_eq!(create_count, 1, "exactly one message.create audit row");
    let (edit_target, edit_actor, edit_created): (String, Option<Uuid>, time::OffsetDateTime) =
        sqlx::query_as(
            "SELECT target, actor_id, created_at FROM audit_events
              WHERE workspace_id = $1 AND action = $2",
        )
        .bind(w.ws.to_uuid())
        .bind(LOCAL_ACTION_MESSAGE_EDIT)
        .fetch_one(&pool)
        .await
        .expect("exactly one message.edit audit row");
    assert_eq!(edit_target, w.msg_id.to_string(), "edit target = message id");
    assert_eq!(edit_actor, Some(w.owner.to_uuid()), "edit actor = editor");

    // Window row: exactly 1, merged count == 2, first/last stamps advanced.
    let window_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_governance_outbox
          WHERE payload->>'aggregate_id' = $1 AND class = 'message'",
    )
    .bind(w.ws.to_uuid().to_string())
    .fetch_one(&pool)
    .await
    .expect("window row count");
    assert_eq!(
        window_count, 1,
        "exactly one window row — the edit merges into it, never a second row"
    );
    let (count, status, priority, class, first_ok, last_ok): (
        i64,
        i32,
        i16,
        String,
        bool,
        bool,
    ) = sqlx::query_as(
        "SELECT (payload->>'count')::bigint, status, priority, class,
                (payload->>'first_event_at')::timestamptz = $2,
                (payload->>'last_event_at')::timestamptz = $3
           FROM audit_governance_outbox WHERE event_id = $1",
    )
    .bind(w.key)
    .bind(w.created_at)
    .bind(edit_created)
    .fetch_one(&pool)
    .await
    .expect("merged window row");
    assert_eq!(
        (count, status, priority, class.as_str()),
        (2, 0, 10, "message"),
        "send + edit merge into the same 60s window (count 1→2, status 0, \
         priority 10, class message)"
    );
    assert!(first_ok, "first_event_at stays the send's created_at");
    assert!(last_ok, "last_event_at advanced to the edit's created_at");

    // SUM conservation (spill-inclusive, E3 precedent): two events, two
    // counts.
    let sum: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM((payload->>'count')::bigint), 0)::bigint
           FROM audit_governance_outbox
          WHERE payload->>'aggregate_id' = $1 AND class = 'message'
            AND ((payload->>'aggregated') = 'true' OR (payload->>'spill') = 'true')",
    )
    .bind(w.ws.to_uuid().to_string())
    .fetch_one(&pool)
    .await
    .expect("window sum");
    assert_eq!(sum, 2, "SUM conservation: two events, two counts");

    cleanup_facade_rows(&pool, w.ws, &[w.msg_id], &[w.key_audit_id], &[]).await;
}

// ----- FR-6 — crash/reclaim leg (qa_lead M1): crash → lease expiry → reclaim → same base32 key → settle. -----

/// The crash window for the window lane (admin-lane R11 mirror): claim →
/// real 202 + receipt → crash before `settle` → lease forced past expiry →
/// reclaim through the REAL relay with a rotated token → the conforming
/// sink replays the original receipt → settle status 2 / attempts 2.
/// Acceptance: `seen_idempotency_keys == [key_base32, key_base32]`;
/// settled rows excluded from `claim_due`.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_facade_window_crash_reclaim_settles() {
    let pool = pool();
    let svc = service(pool.clone());
    let w = facade_window_fixture(&pool, &svc, "drill-facade-crash").await;

    let stub = StubSink::start().await.expect("start stub");
    stub.set_behavior(SinkBehavior {
        duplicate: DuplicateBehavior::ReplayOriginalReceipt,
        ..SinkBehavior::default()
    })
    .await;
    let (relay, repo, config) = relay_for(&pool, &stub, AUDIT_SOURCE_SYSTEM).await;

    // Crash window (suite-proven driver): claim with the 1s lease floor →
    // real 202 + receipt → claim dropped WITHOUT settle (the crash) → lease
    // forced past expiry. Dual-format spelling pinned inside.
    let key_b32 = crash_after_delivery(&pool, &repo, &config, w.key_audit_id).await;
    assert_eq!(stub.posts(), 1, "exactly one POST before the crash");

    // Settle-side fencing probe (no state change): the crashed epoch's
    // persisted token is dead — the fenced re-read fails.
    let (persisted_token,): (Uuid,) = sqlx::query_as(
        "SELECT claim_token FROM audit_governance_outbox WHERE event_id = $1",
    )
    .bind(w.key)
    .fetch_one(&pool)
    .await
    .expect("crashed claim token");
    assert!(
        !repo
            .settle(w.key_audit_id, persisted_token)
            .await
            .expect("fencing probe"),
        "the crashed epoch's token is fenced out (expired lease)"
    );

    // Reclaim + redeliver through the REAL relay: the conforming sink
    // replays the original receipt → receipt validation passes → settle.
    assert_eq!(
        relay.dispatch_batch().await.expect("reclaim"),
        1,
        "the reclaimed window row is claimed again"
    );
    let row = outbox_row(&pool, w.key_audit_id).await;
    assert_eq!(
        row.status, 2,
        "the replayed receipt settles the crash window"
    );
    assert!(row.delivered_at.is_some(), "delivered_at stamped");
    assert_eq!(
        row.attempts, 2,
        "claim budget: one crash claim + one reclaim"
    );
    assert!(row.claim_token.is_none(), "fencing token cleared on settle");
    assert!(row.lease_expires_at.is_none(), "lease cleared on settle");
    assert!(row.last_error.is_none(), "no error recorded on settle");
    assert_eq!(stub.posts(), 2, "one real POST per claim");
    // THE acceptance pin: the SAME base32 Idempotency-Key across crash and
    // replay (sink-side dedup contract).
    assert_eq!(
        stub.seen_idempotency_keys().await,
        vec![key_b32.clone(), key_b32],
        "same base32 key redelivered across the crash window"
    );

    // Status-2 exclusion (F6): a settled row never re-enters the due set —
    // `claim_due`'s `status IN (0,1)` filter excludes it. (No lease-forcing
    // here: the 0239 `audit_governance_claim_state` CHECK forbids writing
    // `lease_expires_at` on a status-2 row — token and lease are both NULL.)
    assert!(
        repo.claim_due(time::Duration::seconds(30), 1)
            .await
            .expect("claim after settle")
            .is_empty(),
        "settled rows are excluded from claim_due"
    );

    cleanup_facade_rows(&pool, w.ws, &[w.msg_id], &[w.key_audit_id], &[]).await;
}

// ----- FR-7 — permanent-rejection dead-path leg (qa_lead M2): 422 → requeue (attempt 1) → dead (attempt 2). -----

/// The true 19-key window shape passes `validate_delivery_payload` (D2 —
/// `PayloadGuard` is NOT the classifier here; the sink's 422 is) and rides the
/// shared permanent-class machine: round 1 requeues (`attempts == 1`);
/// `force_re_due` re-parks deterministically; round 2 reaches
/// `PERMANENT_DEAD_AT = 2` → dead status 3.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_facade_window_permanent_rejection_deads_after_one_retry() {
    let pool = pool();
    let svc = service(pool.clone());
    let w = facade_window_fixture(&pool, &svc, "drill-facade-dead").await;

    let stub = StubSink::start().await.expect("start stub");
    stub.set_behavior(SinkBehavior {
        events_status: 422,
        ..SinkBehavior::default()
    })
    .await;
    let (relay, repo, _config) = relay_for(&pool, &stub, AUDIT_SOURCE_SYSTEM).await;

    // Round 1: claim → 422 → permanent → requeue (attempt 1, never dead).
    assert_eq!(
        relay.dispatch_batch().await.expect("round 1"),
        1,
        "claims the window row"
    );
    let row = outbox_row(&pool, w.key_audit_id).await;
    assert_eq!(row.status, 0, "attempt 1 requeues — never dead (≤1 retry)");
    assert_eq!(row.attempts, 1);
    assert!(row.delivered_at.is_none(), "never delivered");
    assert!(row.claim_token.is_none(), "requeue clears the fencing token");
    assert!(row.lease_expires_at.is_none(), "requeue clears the lease");
    assert!(
        row.last_error
            .as_deref()
            .is_some_and(|e| e.contains("Unprocessable")),
        "last_error names the permanent class (got {:?})",
        row.last_error
    );
    assert_eq!(stub.posts(), 1, "exactly one real POST in round 1");
    assert_eq!(
        stub.seen_idempotency_keys().await,
        vec![w.key_audit_id.to_string()],
        "base32 key on attempt 1"
    );

    // Deterministic re-due (no 1.2s sleep): backdate `available_at` past
    // the requeue's backoff(1) = 1s park.
    force_re_due(&pool, w.key_audit_id).await;
    assert_eq!(
        relay.dispatch_batch().await.expect("round 2"),
        1,
        "reclaims after re-due"
    );
    let row = outbox_row(&pool, w.key_audit_id).await;
    assert_eq!(row.status, 3, "dead terminal at attempt 2 (≤1 retry)");
    assert_eq!(row.attempts, 2, "budget counts claims: exactly two");
    assert!(row.delivered_at.is_none(), "never delivered");
    assert!(row.claim_token.is_none(), "mark_dead clears the token");
    assert!(row.lease_expires_at.is_none(), "mark_dead clears the lease");
    assert!(
        row.last_error
            .as_deref()
            .is_some_and(|e| e.contains("classified permanent: Unprocessable")),
        "last_error pins the dead cause (got {:?})",
        row.last_error
    );
    assert_eq!(stub.posts(), 2, "exactly two real POSTs — never a third");
    assert_eq!(
        stub.seen_idempotency_keys().await,
        vec![w.key_audit_id.to_string(), w.key_audit_id.to_string()],
        "same base32 key on both attempts (dedup key stable across retries)"
    );

    // Terminal non-claimability: dead is a real terminal (status 3 is
    // excluded by the claim predicate), never retried a third time.
    assert!(
        !claim_predicate_holds(&pool, w.key).await,
        "dead is a real terminal, not a stuck row"
    );
    assert!(
        repo.claim_due(time::Duration::seconds(30), 1)
            .await
            .expect("no third claim")
            .is_empty(),
        "never retried a third time"
    );

    cleanup_facade_rows(&pool, w.ws, &[w.msg_id], &[w.key_audit_id], &[]).await;
}
