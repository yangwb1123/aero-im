//! End-to-end governance drill suite (campaign R3–R14): the REAL
//! aero-im-core write path (`svc.moderate_delete` → storage action
//! `message.moderated` → 0239 trigger enqueue) feeding the REAL audit
//! connector (`AuditRelay` over `PgOutboxRepo` + `StubSink`).
//!
//! Slot map (drill-spec §7; tests are named `drill_*`, never bare "R3"):
//!   R3    priority claim preempts FIFO on write-path rows (set-parity, D3)
//!   R4    payload contract: the 16-key envelope via the real write path
//!   R5    422 → dead terminal at attempt 2 (≤1 retry)
//!   R6    403 → immediate dead (T-11 fail-closed, zero retries)
//!   R7    closed endpoint → pending forever with attempts growth (T-11)
//!   R8    relay absent → rows stay status 0, never silently delivered
//!   R9    `moderate_delete(None)` refuses like R-D1 (production guard)
//!   R10b  payload source ≠ relay source → dead-lettered, documented
//!   R11   crash window → conforming sink replay → settle (status 2)
//!   R12   crash window → 422 at attempt 2 → dead (budget counts claims)
//!   R13   crash window → 403 → immediate dead
//!   R14   duplicate conflict signals (409 / conflict:true) → dead, documented
//!
//! Load-bearing invariants (drill-spec §2.3/§8 — do not weaken):
//!   * START-OF-TEST singleton re-assert is the invariant — there is NO Drop
//!     guard (`EnforcementGuard` rejected: async sqlx restore cannot run in
//!     sync Drop; harness kills skip Drop anyway). The runbook runs the suite
//!     with `--test-threads=1`; every later test self-heals via its own
//!     re-assert. The panic-leaves-ON failure mode is NOT just a comment:
//!     `drill_panic_leaves_enforcement_on_backstopped_by_start_reassert`
//!     (crash.rs, F1) fires an ACTUAL panic mid-drill, asserts the leaked-ON
//!     state raises P0001 on unbound INSERTs, and proves the re-assert
//!     closes it.
//!   * Every relay drill pins `RelayConfig.source_system` to the seeded
//!     binding's value (drill-spec item 2): the 0239 trigger stamps
//!     `binding.source_system` into the payload and
//!     `validate_delivery_payload` deads any mismatch (`PayloadGuard`) BEFORE
//!     any POST. Ops contract: one relay per `AERO_AUDIT_SOURCE_SYSTEM`.
//!   * Claimed-row assertions are SET-PARITY only: `UPDATE … RETURNING`
//!     emits target-table heap order (decision D3) — never Vec position.
//!   * Self-isolation: TRUNCATE the drill-owned outbox + delete orphaned
//!     `message.moderated` audit rows — the 0241 reconciler runs ahead of
//!     EVERY claim batch and would otherwise backfill foreign rows into the
//!     drill's claim set. Throwaway-DB only.
//!   * Reconciler-overrides-switch semantics (doc-pinned): the runtime
//!     switch (`snaplink_commercial_runtime.enabled`) is an ENQUEUE-time gate
//!     only; 0241 is runtime-gate-free and runs ahead of every claim batch,
//!     so rows accepted while the switch is off self-heal on the next tick.
//!   * Threat-model statement (doc-pinned): a DB role with write access
//!     defeats the local audit trail (0239 has no guard trigger; the
//!     retention sweep deletes by design) — the tamper-resistant record is
//!     the external sink.

use std::collections::HashSet;
use std::sync::Arc;

use aero_audit_connector::{
    client::AuditClient, config::RelayConfig, outbox::OutboxRepo, pg::PgOutboxRepo,
    relay::AuditRelay, stub::StubSink,
};
use aero_common::{
    AuditClaimPayload, AuditId, Block, MessageId, ParticipantId, RoomKind, WorkspaceId,
    MODERATION_OUTBOUND_ACTION,
};
use aero_storage::{db::PgPool, ParticipantRepo, WorkspaceRepo};
use reqwest::Url;
use uuid::Uuid;

use super::{new_participant, pool, service, unique_slug};

/// One drill's write-path rows (all IDs scoped to the drill, never global).
struct DrillRows {
    ws: WorkspaceId,
    /// The seeded binding's `source_system` (the fixture-level item-2 pin:
    /// every relay drill builds its `RelayConfig` from this value).
    source: String,
    event_ids: Vec<AuditId>,
    message_ids: Vec<MessageId>,
}

// ---------------------------------------------------------------------------
// Fixtures (SQL mirrors of aero-storage/src/audit_governance.rs seeding —
// the storage helpers are `#[cfg(test)]` and unreachable from a dependency
// crate; the singleton re-assert discipline is identical).
// ---------------------------------------------------------------------------

/// Self-isolating start (shared throwaway DB): (1) TRUNCATE the
/// drill-owned outbox (re-runs after a failed test must never corrupt
/// count/parity assertions — pg.rs precedent); (2) orphan guard — the 0241
/// reconciler runs ahead of EVERY claim batch (`relay.rs dispatch_batch`)
/// and would otherwise backfill foreign orphaned `message.moderated` audit
/// rows (other crates' tests, crashed drills) into the drill's claim set,
/// corrupting R3 set-parity and R7 global counts.
async fn self_isolate(pool: &PgPool) {
    sqlx::query("TRUNCATE audit_governance_outbox")
        .execute(pool)
        .await
        .expect("reset governance outbox");
    sqlx::query(
        "DELETE FROM audit_events
          WHERE action = 'message.moderated'
            AND NOT EXISTS (
                SELECT 1 FROM audit_governance_outbox o WHERE o.event_id = audit_events.id)",
    )
    .execute(pool)
    .await
    .expect("orphaned moderation audit rows removed");
}

/// Gate 1 + Gate 2 + entitlement prerequisites for the 0239 trigger,
/// mirroring `audit_governance.rs::enable_enforcement_with_binding`
/// (the entitlement projection is mandatory: 0235 metering RAISEs on message
/// INSERT with enforcement on). Returns the binding's `source_system` — the
/// fixture-level pin (drill-spec item 2): a drift between this seeding SQL
/// and the trigger stamp deads at seed time, before any relay round.
async fn seed_governance_enforcement(pool: &PgPool, ws: WorkspaceId) -> String {
    sqlx::query(
        "UPDATE snaplink_commercial_runtime
            SET enabled = TRUE, updated_at = clock_timestamp()
          WHERE singleton",
    )
    .execute(pool)
    .await
    .expect("enable commercial enforcement");
    sqlx::query(
        "INSERT INTO snaplink_commercial_bindings
               (workspace_id, tenant_id, client_id, audit_client_id, source_system,
                revision, enabled)
         VALUES ($1, $2, $3, $4, $5, 1, TRUE)
         ON CONFLICT (workspace_id) DO NOTHING",
    )
    .bind(ws.to_uuid())
    .bind(format!("tenant-{ws}"))
    .bind(format!("client-{ws}"))
    .bind(format!("audit-client-{ws}"))
    .bind(format!("source-{ws}"))
    .execute(pool)
    .await
    .expect("seed enabled binding");
    sqlx::query(
        "INSERT INTO snaplink_entitlement_projections
               (workspace_id, tenant_id, revision, active, im_enabled,
                notifications_enabled, messages_soft, messages_hard,
                messages_unlimited, notifications_soft, notifications_hard,
                notifications_unlimited, effective_at, generated_at)
         VALUES ($1, $2, 1, TRUE, TRUE, TRUE, 0, 0, TRUE, 0, 0, TRUE,
                 clock_timestamp(), clock_timestamp())
         ON CONFLICT (workspace_id) DO NOTHING",
    )
    .bind(ws.to_uuid())
    .bind(format!("tenant-{ws}"))
    .execute(pool)
    .await
    .expect("seed active entitlement projection");
    let source: String = sqlx::query_scalar(
        "SELECT source_system FROM snaplink_commercial_bindings WHERE workspace_id = $1",
    )
    .bind(ws.to_uuid())
    .fetch_one(pool)
    .await
    .expect("read binding source_system back");
    assert_eq!(source, format!("source-{ws}"), "binding source_system pin");
    source
}

/// Restore the fresh-DB default (`enabled = FALSE`, 0235) after a drill that
/// flipped the global singleton on — a drill that leaves it ON makes every
/// later message INSERT in the shared DB raise P0001 (0235 metering).
async fn restore_enforcement_disabled(pool: &PgPool) {
    sqlx::query(
        "UPDATE snaplink_commercial_runtime
            SET enabled = FALSE, updated_at = clock_timestamp()
          WHERE singleton",
    )
    .execute(pool)
    .await
    .expect("restore enforcement disabled (fresh-DB default)");
}

async fn workspace_fixture(pool: &PgPool, prefix: &str) -> (WorkspaceId, ParticipantId) {
    let participants = ParticipantRepo::new(pool.clone());
    let owner = new_participant(&participants, prefix).await;
    let ws = WorkspaceRepo::new(pool.clone())
        .create(format!("{prefix} WS"), unique_slug(prefix), owner.id)
        .await
        .expect("create workspace");
    (ws.id, owner.id)
}

/// One moderated row through the REAL service seam: `svc.moderate_delete` →
/// `soft_delete_outboxed_system` → `message.moderated` audit append → 0239
/// trigger enqueue, all in one tx. Asserts the write-path sanity gate
/// immediately (1 audit + 1 governance row, status 0 / class 'admin' /
/// priority 100), so a gate-not-seeded zero-row failure fails HERE, not
/// mid-drill. Returns the outbox `event_id` (1:1 with `audit_events.id`).
async fn moderated_row(
    pool: &PgPool,
    svc: &crate::service::ImService,
    ws: WorkspaceId,
    message_id: MessageId,
) -> AuditId {
    svc.moderate_delete(message_id, Some(ws), "drill-reason", "drill-digest")
        .await
        .expect("moderate_delete commits (enforcement ON + binding seeded)");
    let (audit_id,): (Uuid,) = sqlx::query_as(
        "SELECT id FROM audit_events
          WHERE target = $1::text AND action = 'message.moderated'",
    )
    .bind(message_id.to_string())
    .fetch_one(pool)
    .await
    .expect("exactly one audit row");
    let row: (Uuid, i32, String, i16) = sqlx::query_as(
        "SELECT event_id, status, class, priority
           FROM audit_governance_outbox WHERE event_id = $1",
    )
    .bind(audit_id)
    .fetch_one(pool)
    .await
    .expect("exactly one governance row");
    assert_eq!(row.0, audit_id, "event_id 1:1 with audit_events.id");
    assert_eq!(row.1, 0, "status 0 = enqueued (0239 normative)");
    assert_eq!(row.2, "admin", "class 'admin' (GOVERNANCE_CLASS_ADMIN)");
    assert_eq!(row.3, 100, "priority 100 (GOVERNANCE_PRIORITY_MODERATION)");
    AuditId::from_uuid(audit_id)
}

/// Produce `n` moderation rows through the real write path, with the
/// governance table self-isolated and enforcement ON + binding + entitlement
/// seeded (G2). Returns the drill's scoped rows + the binding source pin.
async fn write_path_rows(
    pool: &PgPool,
    svc: &crate::service::ImService,
    prefix: &str,
    n: usize,
) -> DrillRows {
    self_isolate(pool).await;
    let (ws, owner) = workspace_fixture(pool, prefix).await;
    let source = seed_governance_enforcement(pool, ws).await;
    let room = svc
        .create_room_in_workspace(owner, ws, RoomKind::Channel, Some(format!("{prefix}-room")))
        .await
        .expect("create room in workspace")
        .id;
    let mut event_ids = Vec::new();
    let mut message_ids = Vec::new();
    for i in 0..n {
        let msg = svc
            .send_message(
                owner,
                room,
                vec![Block::text(format!("{prefix} message {i}"))],
                None,
                None,
            )
            .await
            .expect("send message");
        event_ids.push(moderated_row(pool, svc, ws, msg.id).await);
        message_ids.push(msg.id);
    }
    DrillRows {
        ws,
        source,
        event_ids,
        message_ids,
    }
}

/// End-of-drill cleanup: delete drill-owned outbox/audit/event-outbox rows
/// and restore the singleton (drill-spec §2.3.4).
async fn cleanup_drill_rows(pool: &PgPool, rows: &DrillRows) {
    let ids: Vec<Uuid> = rows.event_ids.iter().map(AuditId::to_uuid).collect();
    if !ids.is_empty() {
        sqlx::query("DELETE FROM audit_governance_outbox WHERE event_id = ANY($1)")
            .bind(&ids)
            .execute(pool)
            .await
            .expect("clean governance rows");
        sqlx::query("DELETE FROM audit_events WHERE id = ANY($1)")
            .bind(&ids)
            .execute(pool)
            .await
            .expect("clean audit rows");
    }
    let msgs: Vec<Uuid> = rows.message_ids.iter().map(MessageId::to_uuid).collect();
    if !msgs.is_empty() {
        sqlx::query("DELETE FROM event_outbox WHERE message_id = ANY($1)")
            .bind(&msgs)
            .execute(pool)
            .await
            .expect("clean event-outbox rows");
    }
    restore_enforcement_disabled(pool).await;
}

/// The relay-test config shape (`relay.rs::test_config`) with the source
/// system pinned to the caller's value — the item-2 fixture contract: relay
/// drills ALWAYS pass the seeded binding's `source_system` (except R10b,
/// which deliberately passes a different one).
fn drill_config(stub: &StubSink, source_system: &str) -> RelayConfig {
    RelayConfig {
        token_endpoint: Url::parse(&stub.token_url()).expect("stub token URL"),
        events_url: Url::parse(&stub.events_url()).expect("stub events URL"),
        resource: "audit-governance".into(),
        client_id: "drill-client".into(),
        client_secret: "0123456789abcdef0123456789abcdef".into(),
        expected_iss: "https://idp.example.test".into(),
        expected_aud: "audit-governance".into(),
        expected_scope: "audit:event:write".into(),
        expected_sub: "aero-im.source".into(),
        source_system: source_system.into(),
        request_timeout: std::time::Duration::from_secs(5),
        delivery_lease: std::time::Duration::from_secs(30),
        poll_interval: std::time::Duration::from_secs(1),
        shutdown_drain: std::time::Duration::from_secs(2),
        batch_size: 100,
        concurrency: 4,
        jwks_uri: None,
    }
}

async fn relay_for(
    pool: &PgPool,
    stub: &StubSink,
    source_system: &str,
) -> (AuditRelay, Arc<PgOutboxRepo>, RelayConfig) {
    let repo = Arc::new(PgOutboxRepo::new(pool.clone()));
    let config = drill_config(stub, source_system);
    let client = AuditClient::new(config.clone()).expect("build audit client");
    (
        AuditRelay::new(repo.clone(), client, config.clone()),
        repo,
        config,
    )
}

/// Deterministic crash-window driver (drill-spec §5, no sleeps): claim with
/// the 1s lease floor (public API) → real 202 + receipt at the stub → DROP
/// the claim without `settle` (the crash) → force the lease past expiry. The
/// row re-enters the due set with attempts incremented and a rotated token.
/// Also pins the dual-format spelling (D-W2): the `Idempotency-Key` header
/// is `AuditId` Display (base32) while the payload `event_id` is `uuid::text`.
async fn crash_after_delivery(
    pool: &PgPool,
    repo: &PgOutboxRepo,
    config: &RelayConfig,
    event_id: AuditId,
) -> String {
    let claims = repo
        .claim_due(time::Duration::seconds(1), 10)
        .await
        .expect("crash-window claim");
    assert_eq!(claims.len(), 1, "exactly one due row");
    assert_eq!(claims[0].event_id, event_id);
    assert_eq!(claims[0].attempts, 1);
    let key = claims[0].event_id.to_string();
    let payload_event_id = claims[0].payload["event_id"]
        .as_str()
        .expect("payload event_id")
        .to_owned();
    assert_ne!(
        key, payload_event_id,
        "dual-format pin: header base32 vs payload uuid::text"
    );
    let client = AuditClient::new(config.clone()).expect("build crash client");
    client
        .deliver(&claims[0])
        .await
        .expect("delivery succeeds before the crash (202 + receipt)");
    drop(claims); // the crash: no settle, lease expiry reclaims
    force_lease_expiry(pool, event_id).await;
    key
}

async fn force_lease_expiry(pool: &PgPool, event_id: AuditId) {
    sqlx::query(
        "UPDATE audit_governance_outbox
            SET lease_expires_at = clock_timestamp() - interval '1 second'
          WHERE event_id = $1",
    )
    .bind(event_id.to_uuid())
    .execute(pool)
    .await
    .expect("force lease expiry");
}

struct RowState {
    status: i32,
    attempts: i64,
    delivered_at: Option<time::OffsetDateTime>,
    last_error: Option<String>,
    claim_token: Option<Uuid>,
    lease_expires_at: Option<time::OffsetDateTime>,
}

async fn outbox_row(pool: &PgPool, event_id: AuditId) -> RowState {
    let (status, attempts, delivered_at, last_error, claim_token, lease_expires_at) =
        sqlx::query_as::<
            _,
            (
                i32,
                i64,
                Option<time::OffsetDateTime>,
                Option<String>,
                Option<Uuid>,
                Option<time::OffsetDateTime>,
            ),
        >(
            "SELECT status, attempts, delivered_at, last_error, claim_token, lease_expires_at
               FROM audit_governance_outbox WHERE event_id = $1",
        )
        .bind(event_id.to_uuid())
        .fetch_one(pool)
        .await
        .expect("governance row");
    RowState {
        status,
        attempts,
        delivered_at,
        last_error,
        claim_token,
        lease_expires_at,
    }
}

async fn count_status(pool: &PgPool, ids: &[AuditId], status: i32) -> i64 {
    let ids: Vec<Uuid> = ids.iter().map(AuditId::to_uuid).collect();
    sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_governance_outbox
          WHERE event_id = ANY($1) AND status = $2",
    )
    .bind(&ids)
    .bind(status)
    .fetch_one(pool)
    .await
    .expect("count status")
}

async fn sum_attempts(pool: &PgPool, ids: &[AuditId]) -> i64 {
    let ids: Vec<Uuid> = ids.iter().map(AuditId::to_uuid).collect();
    sqlx::query_scalar(
        "SELECT COALESCE(SUM(attempts), 0)::bigint FROM audit_governance_outbox
          WHERE event_id = ANY($1)",
    )
    .bind(&ids)
    .fetch_one(pool)
    .await
    .expect("sum attempts")
}

async fn count_error_fragment(pool: &PgPool, ids: &[AuditId], fragment: &str) -> i64 {
    let ids: Vec<Uuid> = ids.iter().map(AuditId::to_uuid).collect();
    sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_governance_outbox
          WHERE event_id = ANY($1) AND last_error LIKE $2",
    )
    .bind(&ids)
    .bind(format!("%{fragment}%"))
    .fetch_one(pool)
    .await
    .expect("count last_error fragment")
}

async fn count_null_errors(pool: &PgPool, ids: &[AuditId]) -> i64 {
    let ids: Vec<Uuid> = ids.iter().map(AuditId::to_uuid).collect();
    sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_governance_outbox
          WHERE event_id = ANY($1) AND last_error IS NULL",
    )
    .bind(&ids)
    .fetch_one(pool)
    .await
    .expect("count null last_error")
}

// ---------------------------------------------------------------------------
// R3 — priority claim preempts FIFO on write-path rows (AC1).
// ---------------------------------------------------------------------------

/// 40 backlog rows (priority 10, earlier `available_at`, inverted
/// `created_at`) + 10 moderation rows via the REAL `svc.moderate_delete`
/// write path (priority 100, later `available_at`); `claim_due(30s, 10)`
/// with limit 10 < 50 must return SET-EQUAL {all 10 admin} — priority
/// preempts FIFO regardless of enqueue order. Set-parity only (D3: the
/// `UPDATE … RETURNING` emits heap order, never CTE order).
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_priority_claim_preempts_fifo_on_write_path_rows() {
    let pool = pool();
    let svc = service(pool.clone());
    let rows = write_path_rows(&pool, &svc, "drill-r3", 10).await;
    let mut backlog: Vec<Uuid> = Vec::new();
    for i in 0..40 {
        let event_id = Uuid::new_v4();
        sqlx::query(
            r"INSERT INTO audit_governance_outbox
                    (event_id, payload, status, attempts, priority,
                     available_at, created_at)
              VALUES ($1, $2, 0, 0, 10,
                      clock_timestamp() - make_interval(secs => 200)
                                     + make_interval(secs => $3),
                      clock_timestamp() - make_interval(secs => 200)
                                     + make_interval(secs => $4))",
        )
        .bind(event_id)
        .bind(serde_json::json!({
            "event_id": event_id.to_string(),
            "source_system": rows.source,
        }))
        .bind(i64::from(i)) // available_at: t0 + i seconds (strictly increasing)
        .bind(i64::from(40 - i)) // created_at: t0 + (40 − i) seconds (inverted)
        .execute(&pool)
        .await
        .expect("seed backlog row");
        backlog.push(event_id);
    }

    let repo = PgOutboxRepo::new(pool.clone());
    let claimed = repo
        .claim_due(time::Duration::seconds(30), 10)
        .await
        .expect("claim due rows");
    assert_eq!(
        claimed.len(),
        10,
        "limit 10 < 50 forces the claim to select, not drain"
    );
    let claimed_ids: HashSet<AuditId> = claimed.iter().map(|c| c.event_id).collect();
    let admin_set: HashSet<AuditId> = rows.event_ids.iter().copied().collect();
    assert_eq!(
        claimed_ids, admin_set,
        "priority DESC preempts FIFO on REAL write-path rows (set-parity, D3)"
    );
    for claim in &claimed {
        assert_eq!(claim.attempts, 1, "first claim records attempts == 1");
        assert_eq!(claim.priority, 100, "moderation lane priority");
        assert_eq!(claim.class, "admin", "moderation lane class");
    }

    // Second claim (limit 50) drains exactly the 40 backlog — the moderation
    // rows are leased (status 1, live lease), the backlog is all that is due.
    let rest = repo
        .claim_due(time::Duration::seconds(30), 50)
        .await
        .expect("claim the backlog");
    let rest_set: HashSet<AuditId> = rest.iter().map(|c| c.event_id).collect();
    let backlog_set: HashSet<AuditId> = backlog.iter().map(|id| AuditId::from_uuid(*id)).collect();
    assert_eq!(
        rest.len(),
        40,
        "the remaining due set is exactly the backlog"
    );
    assert_eq!(rest_set, backlog_set, "backlog claimed set-parity");

    // Cleanup: backlog rows are synthetic (no audit_events row) — delete by
    // outbox id; the write-path rows via the shared helper.
    let mut all_ids: Vec<Uuid> = backlog;
    all_ids.extend(rows.event_ids.iter().map(AuditId::to_uuid));
    sqlx::query("DELETE FROM audit_governance_outbox WHERE event_id = ANY($1)")
        .bind(&all_ids)
        .execute(&pool)
        .await
        .expect("clean outbox rows");
    let audit_ids: Vec<Uuid> = rows.event_ids.iter().map(AuditId::to_uuid).collect();
    sqlx::query("DELETE FROM audit_events WHERE id = ANY($1)")
        .bind(&audit_ids)
        .execute(&pool)
        .await
        .expect("clean audit rows");
    restore_enforcement_disabled(&pool).await;
}

// ---------------------------------------------------------------------------
// R4 — payload contract: the 16-key envelope via the real write path (AC2).
// ---------------------------------------------------------------------------

/// On a claimed moderation row produced by `svc.moderate_delete`: action ==
/// the leaf `MODERATION_OUTBOUND_ACTION`, `idempotency_key` == `event_id` ==
/// `audit_events.id::text` (join), `source_system` == the binding row value
/// (cross-checked from `snaplink_commercial_bindings`, not just the fixture
/// string), all 16 envelope keys present, system actor, message target,
/// reason/digest carried, and the dual-format pin (base32 header vs
/// `uuid::text` payload).
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_payload_contract_16_key_envelope_via_moderate_delete() {
    // All 16 envelope keys present — exactly the `jsonb_build_object` list
    // in `aero_enqueue_governance_audit` (0239).
    const ENVELOPE_KEYS: [&str; 16] = [
        "event_id",
        "source_system",
        "event_type",
        "schema_id",
        "schema_version",
        "occurred_at",
        "actor",
        "targets",
        "aggregate_type",
        "aggregate_id",
        "action",
        "outcome",
        "payload",
        "data_classification",
        "retention_class",
        "idempotency_key",
    ];
    let pool = pool();
    let svc = service(pool.clone());
    let rows = write_path_rows(&pool, &svc, "drill-r4", 1).await;
    let event_id = rows.event_ids[0];
    let message_id = rows.message_ids[0];

    let repo = PgOutboxRepo::new(pool.clone());
    let claims = repo
        .claim_due(time::Duration::seconds(30), 1)
        .await
        .expect("claim one row");
    assert_eq!(claims.len(), 1);
    let claim = &claims[0];
    assert_eq!(claim.event_id, event_id);
    let payload = &claim.payload;

    let obj = payload.as_object().expect("payload is an object");
    for key in ENVELOPE_KEYS {
        assert!(obj.contains_key(key), "missing envelope key {key}");
    }
    assert_eq!(obj.len(), 16, "exactly the 16-key envelope");

    // Fail-closed typed parse + Value-level re-serialize equality with the
    // stored JSONB (the storage-level parity test's half-A shape).
    let typed: AuditClaimPayload =
        serde_json::from_value(payload.clone()).expect("payload parses into the leaf twin");
    assert_eq!(
        serde_json::to_value(&typed).unwrap(),
        *payload,
        "re-serialized twin equals the stored JSONB (semantic parity)"
    );

    // Field pins.
    assert_eq!(
        payload["action"], MODERATION_OUTBOUND_ACTION,
        "action == leaf MODERATION_OUTBOUND_ACTION"
    );
    let audit_id_text = event_id.to_uuid().to_string();
    assert_eq!(
        payload["event_id"], audit_id_text,
        "payload event_id mirrors"
    );
    assert_eq!(
        payload["idempotency_key"], audit_id_text,
        "idempotency_key == event_id == audit_events.id::text"
    );
    assert_eq!(
        payload["aggregate_id"],
        rows.ws.to_uuid().to_string(),
        "aggregate_id == workspace id"
    );
    assert_eq!(payload["actor"]["id"], "system", "nil actor → system id");
    assert_eq!(
        payload["actor"]["type"], "system",
        "nil actor → system type"
    );
    assert_eq!(
        payload["targets"][0]["id"],
        message_id.to_string(),
        "targets[0].id == message id"
    );
    assert_eq!(payload["targets"][0]["type"], "resource");
    assert_eq!(payload["payload"]["reason"], "drill-reason");
    assert_eq!(payload["payload"]["digest"], "drill-digest");
    assert_eq!(payload["event_type"], "aero.im.security");
    assert_eq!(payload["outcome"], "success");

    // source_system cross-checked against the binding ROW (not the fixture
    // string): the 0239 trigger stamps `binding.source_system`.
    let binding_source: String = sqlx::query_scalar(
        "SELECT source_system FROM snaplink_commercial_bindings WHERE workspace_id = $1",
    )
    .bind(rows.ws.to_uuid())
    .fetch_one(&pool)
    .await
    .expect("binding source_system");
    assert_eq!(binding_source, rows.source, "fixture pin");
    assert_eq!(
        payload["source_system"], binding_source,
        "payload source pin"
    );

    // Dual-format pin (D-W2): the connector's Idempotency-Key header is the
    // base32 `AuditId` Display, the payload's event_id is uuid::text — the
    // receipt validator equates them value-level; a regression respelling
    // either side breaks here.
    assert_ne!(claim.event_id.to_string(), audit_id_text);

    cleanup_drill_rows(&pool, &rows).await;
}

mod crash;
mod terminal;
