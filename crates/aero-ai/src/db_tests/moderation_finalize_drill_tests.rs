//! DB-level drills for the [`AiWorker::handle_moderate`] producer chain:
//! AI verdict → in-tx audit append → 0239 admin-lane row →
//! priority-preempts-backlog claim. Closes the coverage gap where the R3–R14
//! im-core suite stops at `svc.moderate_delete` (the interactive path): the AI
//! producer — the highest-volume admin-class source — had no end-to-end proof
//! that its finalize commits atomically, that the relay claims its lane ahead
//! of backlog, and that the fail-closed binding abort leaves zero artifacts.
//!
//! Slot map (consolidated drill spec §4):
//!   Drill 1  happy-path atomic finalize + v1 parity + replay idempotency
//!   Drill 2  admin lane (priority 100) preempts backlog (priority 10) claim
//!   Drill 3  Gate-2 P0001 abort mid-finalize → zero durable artifacts
//!   Drill 4  ALLOW verdict → zero side effects through the real chain
//!   Drill 5  BLOCK without `target_id` → warn + Ok, no DB touch (hermetic)
//!
//! Load-bearing invariants (do not weaken):
//!   * Run with `--test-threads=1`; START-OF-TEST singleton re-assert is the
//!     invariant — there is NO Drop guard (async sqlx restore cannot run in
//!     sync Drop). Every drill self-heals via its own re-assert.
//!   * Claimed-row assertions are SET-PARITY only (`RETURNING` emits
//!     target-table heap order) — never Vec position.
//!   * Every `worker.process` goes through the 30s timeout wrapper — never
//!     blind-retry.
//!   * Drill 2's exclusion fence is a pinned 3600s lease, never a timing
//!     assumption.
//!   * The governance lane asserts `priority DESC` (higher = claimed first);
//!     `ai_jobs.priority_for` is the OPPOSITE direction (ASC lower-first) —
//!     never "align" the two models.

use std::collections::HashSet;

use aero_audit_connector::{outbox::OutboxRepo, pg::PgOutboxRepo};
use aero_common::{AuditId, LOCAL_ACTION_MODERATED, MODERATION_OUTBOUND_ACTION, WorkspaceId};
use aero_storage::{AiJob, AiJobKind, AiJobRepo, AiJobStatus};
use sqlx::postgres::PgPoolOptions;

use crate::{AiError, AiWorker, WorkerConfig};

use super::*;

// ---------------------------------------------------------------------------
// Drill 1 — happy path: the finalize commits atomically (A1 + A1b)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_moderation_finalize_commits_atomically() {
    let pool = pool();
    self_isolate(&pool).await;
    restore_enforcement_disabled(&pool).await;
    let fx = fixture(&pool, "drill1").await;
    let source = seed_governance_enforcement(&pool, fx.ws).await;
    let stub = StubAnthropicServer::start("BLOCK: drill-reason").await;
    let (svc, sink) = drill_service(&pool, &stub.base_url);
    let worker = AiWorker::with_config(svc, WorkerConfig::default());

    // 1. Enqueue through the real repo; pin the ASC lane value (direction
    //    trap: this is the ai_jobs model — lower = first; the governance
    //    outbox model is DESC — higher = first).
    let jobs = AiJobRepo::new(pool.clone());
    let job_id = jobs
        .enqueue(
            AiJobKind::Moderate,
            Some(fx.msg.to_uuid()),
            Some(fx.ws.to_uuid()),
            serde_json::json!({"text": "drill content"}),
        )
        .await
        .expect("enqueue moderation job");
    let (priority,): (i16,) = sqlx::query_as("SELECT priority FROM ai_jobs WHERE id = $1")
        .bind(uuid::Uuid::from_u128(job_id.0))
        .fetch_one(&pool)
        .await
        .expect("job priority row");
    assert_eq!(priority, 10, "priority_for(Moderate)=10 (ASC lower-first lane)");

    // 2. Claim — identity assert: a foreign queued job (crashed prior run)
    //    must fail loudly here, not skew the drill.
    let claimed = jobs.claim(1).await.expect("claim moderation job");
    assert_eq!(claimed.len(), 1, "exactly one job claimed");
    assert_eq!(claimed[0].id, job_id, "claimed job identity == enqueued job");
    assert_eq!(claimed[0].attempts, 1, "attempts incremented to 1");
    assert_eq!(claimed[0].status, AiJobStatus::Running, "claimed → running");

    // 3. Drive the production finalize chain (timeout-wrapped).
    let result = run_processed(&worker, claimed[0].clone())
        .await
        .expect("process must commit Ok");
    assert_eq!(result["verdict"], "block");
    assert_eq!(result["reason"], "drill-reason");
    assert_eq!(
        sink.0.lock().expect("sink lock").len(),
        1,
        "one reserve recorded (Capture pattern: finalize is not recorded)"
    );

    // 4. Replay idempotency (at-least-once): a second process on the same job
    //    must return Ok WITHOUT double-appending anything (the audit leg has
    //    no ON CONFLICT — its idempotency rests on the already-deleted
    //    early-return + guarded UPDATE; the outbox leg has the 0239
    //    `ON CONFLICT (event_id) DO NOTHING`).
    run_processed(&worker, claimed[0].clone())
        .await
        .expect("replay must also return Ok");
    assert_eq!(
        sink.0.lock().expect("sink lock").len(),
        2,
        "replay re-ran the provider round-trip (2nd reserve) — idempotency is in the DB layer"
    );

    // 5. Single read set (one session) — every count is the post-replay state.
    let mut tx = pool.begin().await.expect("begin read-set tx");

    let (deleted_at, blocks, version, embedding_null, searchable_text): (
        Option<time::OffsetDateTime>,
        serde_json::Value,
        i32,
        bool,
        String,
    ) = sqlx::query_as(
        "SELECT deleted_at, blocks, version, embedding IS NULL, searchable_text
           FROM messages WHERE id = $1",
    )
    .bind(fx.msg.to_uuid())
    .fetch_one(&mut *tx)
    .await
    .expect("message row");
    assert!(deleted_at.is_some(), "deleted_at set");
    assert_eq!(blocks, serde_json::json!([]), "blocks emptied");
    assert_eq!(version, 2, "version bumped 1 → 2 and stays 2 after replay");
    assert!(embedding_null, "embedding nulled from the pre-seeded vector");
    assert_eq!(searchable_text, "", "searchable_text emptied from pre-seeded text");

    let (audit_id, actor_id, detail): (uuid::Uuid, Option<uuid::Uuid>, serde_json::Value) =
        sqlx::query_as(
            "SELECT id, actor_id, detail
               FROM audit_events
              WHERE action = $1 AND target = $2::text AND workspace_id = $3",
        )
        .bind(LOCAL_ACTION_MODERATED)
        .bind(fx.msg.to_string())
        .bind(fx.ws.to_uuid())
        .fetch_one(&mut *tx)
        .await
        .expect("exactly one audit row (replay did not double-append)");
    assert!(actor_id.is_none(), "system actor (worker passes audit_actor = None)");
    assert_eq!(detail["reason"], "drill-reason");
    assert_eq!(detail["source"], "ai_worker");

    let (status, class, outbox_priority, delivery_mode, payload): (
        i32,
        String,
        i16,
        String,
        serde_json::Value,
    ) = sqlx::query_as(
        "SELECT status, class, priority, delivery_mode, payload
           FROM audit_governance_outbox WHERE event_id = $1",
    )
    .bind(audit_id)
    .fetch_one(&mut *tx)
    .await
    .expect("exactly one governance outbox row");
    assert_eq!(status, 0, "status 0 = enqueued (0239 normative)");
    assert_eq!(class, "admin", "GOVERNANCE_CLASS_ADMIN");
    assert_eq!(outbox_priority, 100, "GOVERNANCE_PRIORITY_MODERATION");
    assert_eq!(delivery_mode, "push", "0239 delivery_mode default");
    assert_eq!(
        payload["action"],
        MODERATION_OUTBOUND_ACTION,
        "outbound action asserted via the leaf const, never a literal"
    );
    assert_eq!(payload["event_id"], audit_id.to_string());
    assert_eq!(payload["source_system"], source);
    assert_eq!(payload["aggregate_id"], fx.ws.to_uuid().to_string());
    assert_eq!(payload["idempotency_key"], audit_id.to_string());

    let (event_count,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM event_outbox
          WHERE message_id = $1 AND event_kind = 'deleted'",
    )
    .bind(fx.msg.to_uuid())
    .fetch_one(&mut *tx)
    .await
    .expect("event_outbox count");
    assert_eq!(
        event_count, 1,
        "exactly one Deleted event_outbox row (replay did not double-broadcast)"
    );

    // v1 parity: the 0236 trigger wrote its same-tx twin — pins the insert the
    // cleanup also deletes (a broken 0236 would pass the v2 asserts silently).
    let (dest, v1_source, idem_key, v1_ws): (String, String, String, String) = sqlx::query_as(
        "SELECT destination, source_system, idempotency_key, workspace_id::text
           FROM snaplink_delivery_outbox WHERE delivery_id = $1",
    )
    .bind(format!("audit:{audit_id}"))
    .fetch_one(&mut *tx)
    .await
    .expect("v1 snaplink_delivery_outbox twin (0236 same-tx insert)");
    assert_eq!(dest, "audit");
    assert_eq!(v1_source, source);
    assert_eq!(idem_key, audit_id.to_string());
    assert_eq!(v1_ws, fx.ws.to_uuid().to_string());

    tx.commit().await.expect("commit read-set tx");

    // 6. Cleanup.
    sqlx::query("DELETE FROM ai_jobs WHERE id = $1")
        .bind(uuid::Uuid::from_u128(job_id.0))
        .execute(&pool)
        .await
        .expect("clean ai_jobs");
    sqlx::query("DELETE FROM event_outbox WHERE message_id = $1 AND event_kind = 'deleted'")
        .bind(fx.msg.to_uuid())
        .execute(&pool)
        .await
        .expect("clean event_outbox");
    sqlx::query("DELETE FROM audit_governance_outbox WHERE event_id = $1")
        .bind(audit_id)
        .execute(&pool)
        .await
        .expect("clean governance outbox");
    sqlx::query("DELETE FROM snaplink_delivery_outbox WHERE delivery_id = $1")
        .bind(format!("audit:{audit_id}"))
        .execute(&pool)
        .await
        .expect("clean v1 outbox");
    sqlx::query("DELETE FROM audit_events WHERE id = $1")
        .bind(audit_id)
        .execute(&pool)
        .await
        .expect("clean audit_events");
    restore_enforcement_disabled(&pool).await;
}

// ---------------------------------------------------------------------------
// Drill 2 — the admin lane preempts backlog under priority DESC (A2)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_moderation_lane_preempts_backlog_claim() {
    const BACKLOG_SEED_COUNT: usize = 3;

    let pool = pool();
    self_isolate(&pool).await;
    restore_enforcement_disabled(&pool).await;
    let fx = fixture(&pool, "drill2").await;
    let source = seed_governance_enforcement(&pool, fx.ws).await;

    // 1. Synthetic backlog FIRST: strictly earlier (200s), lower priority (10),
    //    no audit twin (reconciler direction is audit→outbox; no interference).
    let backlog_ids: Vec<uuid::Uuid> = (0..BACKLOG_SEED_COUNT)
        .map(|_| uuid::Uuid::new_v4())
        .collect();
    for id in &backlog_ids {
        sqlx::query(
            "INSERT INTO audit_governance_outbox
                   (event_id, payload, status, attempts, priority, available_at, created_at)
             VALUES ($1, $2, 0, 0, 10,
                     clock_timestamp() - interval '200 seconds',
                     clock_timestamp() - interval '200 seconds')",
        )
        .bind(id)
        .bind(serde_json::json!({
            "event_id": id.to_string(),
            "source_system": source,
        }))
        .execute(&pool)
        .await
        .expect("seed backlog row");
    }

    // 2. Produce the admin row LAST through the real worker path.
    let jobs = AiJobRepo::new(pool.clone());
    let job_id = jobs
        .enqueue(
            AiJobKind::Moderate,
            Some(fx.msg.to_uuid()),
            Some(fx.ws.to_uuid()),
            serde_json::json!({"text": "drill content"}),
        )
        .await
        .expect("enqueue moderation job");
    let claimed = jobs.claim(1).await.expect("claim moderation job");
    assert_eq!(claimed[0].id, job_id, "claimed job identity");
    let stub = StubAnthropicServer::start("BLOCK: drill-reason").await;
    let (svc, _sink) = drill_service(&pool, &stub.base_url);
    let worker = AiWorker::with_config(svc, WorkerConfig::default());
    let result = run_processed(&worker, claimed[0].clone())
        .await
        .expect("process must commit Ok");
    assert_eq!(result["verdict"], "block");
    let admin_id: uuid::Uuid = sqlx::query_scalar(
        "SELECT event_id FROM audit_governance_outbox WHERE payload->>'action' = $1",
    )
    .bind(MODERATION_OUTBOUND_ACTION)
    .fetch_one(&pool)
    .await
    .expect("admin row from the real finalize chain");

    // 3. First claim — lease-pinned (3600s): the fence is deterministic, no
    //    wall-clock timing assumption. Limit 1 forces selection: precedence
    //    can only come from `priority DESC` (the admin row is 200s YOUNGER
    //    than the backlog yet must be claimed first).
    let repo = PgOutboxRepo::new(pool.clone());
    let first = repo
        .claim_due(time::Duration::seconds(3600), 1)
        .await
        .expect("first claim");
    let first_set: HashSet<AuditId> = first.iter().map(|c| c.event_id).collect();
    assert_eq!(
        first_set,
        HashSet::from([AuditId::from_uuid(admin_id)]),
        "set-parity: limit-1 claim must return exactly the admin row"
    );
    assert_eq!(first[0].priority, 100, "admin lane priority");
    assert_eq!(first[0].class, "admin", "admin lane class");
    assert_eq!(first[0].attempts, 1, "first claim increments attempts to 1");
    let (admin_at,): (time::OffsetDateTime,) =
        sqlx::query_as("SELECT available_at FROM audit_governance_outbox WHERE event_id = $1")
            .bind(admin_id)
            .fetch_one(&pool)
            .await
            .expect("admin available_at");
    let (backlog_at,): (time::OffsetDateTime,) =
        sqlx::query_as("SELECT available_at FROM audit_governance_outbox WHERE event_id = $1")
            .bind(backlog_ids[0])
            .fetch_one(&pool)
            .await
            .expect("backlog available_at");
    assert!(
        admin_at > backlog_at,
        "precedence can only come from priority DESC (admin is 200s younger)"
    );

    // 4. Second claim drains exactly the backlog set; the admin row's 3600s
    //    lease excludes it regardless of wall-clock stall between the awaits.
    let second = repo
        .claim_due(time::Duration::seconds(30), 10)
        .await
        .expect("second claim");
    let second_set: HashSet<AuditId> = second.iter().map(|c| c.event_id).collect();
    let expected: HashSet<AuditId> =
        backlog_ids.iter().map(|&id| AuditId::from_uuid(id)).collect();
    assert_eq!(
        second.len(),
        BACKLOG_SEED_COUNT,
        "second claim drains exactly the {BACKLOG_SEED_COUNT} backlog rows"
    );
    assert_eq!(second_set, expected, "set-parity with the seeded backlog set");
    assert!(
        !second_set.contains(&AuditId::from_uuid(admin_id)),
        "admin row excluded by the pinned 3600s lease"
    );
    assert!(
        second.iter().all(|c| c.attempts == 1),
        "backlog rows first-claimed"
    );

    // 5. Cleanup.
    sqlx::query("DELETE FROM ai_jobs WHERE id = $1")
        .bind(uuid::Uuid::from_u128(job_id.0))
        .execute(&pool)
        .await
        .expect("clean ai_jobs");
    let mut outbox_ids: Vec<uuid::Uuid> = backlog_ids;
    outbox_ids.push(admin_id);
    sqlx::query("DELETE FROM audit_governance_outbox WHERE event_id = ANY($1)")
        .bind(&outbox_ids)
        .execute(&pool)
        .await
        .expect("clean governance outbox");
    sqlx::query("DELETE FROM audit_events WHERE id = $1")
        .bind(admin_id)
        .execute(&pool)
        .await
        .expect("clean audit_events");
    sqlx::query("DELETE FROM snaplink_delivery_outbox WHERE delivery_id = $1")
        .bind(format!("audit:{admin_id}"))
        .execute(&pool)
        .await
        .expect("clean v1 outbox");
    sqlx::query("DELETE FROM event_outbox WHERE message_id = $1 AND event_kind = 'deleted'")
        .bind(fx.msg.to_uuid())
        .execute(&pool)
        .await
        .expect("clean event_outbox");
    restore_enforcement_disabled(&pool).await;
}

// ---------------------------------------------------------------------------
// Drill 3 — Gate-2 binding abort: zero durable artifacts (A3)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_moderation_finalize_abort_leaves_zero_rows() {
    let pool = pool();
    self_isolate(&pool).await;
    restore_enforcement_disabled(&pool).await;
    // Fixture BEFORE enforcement ON — mandatory here: with no binding, the
    // 0235 metering trigger RAISEs P0001 on any message INSERT (canonical
    // body-order qualification).
    let fx = fixture(&pool, "drill3").await;
    let (binding_count,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM snaplink_commercial_bindings
          WHERE workspace_id = $1 AND enabled",
    )
    .bind(fx.ws.to_uuid())
    .fetch_one(&pool)
    .await
    .expect("binding count");
    assert_eq!(binding_count, 0, "pre-state: workspace has no enabled binding");
    // Gate 1 only: runtime singleton ON, binding absent → Gate 2 fail-closed.
    sqlx::query(
        "UPDATE snaplink_commercial_runtime
            SET enabled = TRUE, updated_at = clock_timestamp()
          WHERE singleton",
    )
    .execute(&pool)
    .await
    .expect("enable runtime singleton (Gate 1)");

    let (before_outbox,): (i64,) = sqlx::query_as("SELECT count(*) FROM audit_governance_outbox")
        .fetch_one(&pool)
        .await
        .expect("outbox count before");
    let (before_v1,): (i64,) = sqlx::query_as("SELECT count(*) FROM snaplink_delivery_outbox")
        .fetch_one(&pool)
        .await
        .expect("v1 count before");

    let jobs = AiJobRepo::new(pool.clone());
    let job_id = jobs
        .enqueue(
            AiJobKind::Moderate,
            Some(fx.msg.to_uuid()),
            Some(fx.ws.to_uuid()),
            serde_json::json!({"text": "drill content"}),
        )
        .await
        .expect("enqueue moderation job");
    let claimed = jobs.claim(1).await.expect("claim moderation job");
    assert_eq!(claimed[0].id, job_id, "claimed job identity");
    let stub = StubAnthropicServer::start("BLOCK: drill-reason").await;
    let (svc, _sink) = drill_service(&pool, &stub.base_url);
    let worker = AiWorker::with_config(svc, WorkerConfig::default());
    let err = run_processed(&worker, claimed[0].clone())
        .await
        .expect_err("Gate-2 P0001 must abort the finalize");
    assert!(
        matches!(err, AiError::Storage(_)),
        "binding RAISE surfaces as AiError::Storage, got {err:?}"
    );

    // Zero durable artifacts — the whole single tx rolled back.
    let (deleted_at, version, searchable_text): (Option<time::OffsetDateTime>, i32, String) =
        sqlx::query_as(
            "SELECT deleted_at, version, searchable_text FROM messages WHERE id = $1",
        )
        .bind(fx.msg.to_uuid())
        .fetch_one(&pool)
        .await
        .expect("message row");
    assert!(deleted_at.is_none(), "message still visible");
    assert_eq!(version, 1, "version untouched");
    assert_eq!(searchable_text, "pre-drill visible text", "searchable_text untouched");
    let (audit_count,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM audit_events
          WHERE action = $1 AND target = $2::text",
    )
    .bind(LOCAL_ACTION_MODERATED)
    .bind(fx.msg.to_string())
    .fetch_one(&pool)
    .await
    .expect("audit count");
    assert_eq!(audit_count, 0, "zero audit rows");
    let (after_outbox,): (i64,) = sqlx::query_as("SELECT count(*) FROM audit_governance_outbox")
        .fetch_one(&pool)
        .await
        .expect("outbox count after");
    assert_eq!(after_outbox, before_outbox, "governance outbox delta 0");
    let (after_v1,): (i64,) = sqlx::query_as("SELECT count(*) FROM snaplink_delivery_outbox")
        .fetch_one(&pool)
        .await
        .expect("v1 count after");
    assert_eq!(after_v1, before_v1, "v1 outbox delta 0 (same-tx rollback)");
    let (event_count,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM event_outbox
          WHERE message_id = $1 AND event_kind = 'deleted'",
    )
    .bind(fx.msg.to_uuid())
    .fetch_one(&pool)
    .await
    .expect("event_outbox count");
    assert_eq!(event_count, 0, "no Deleted event_outbox row");

    // Cleanup: the job row is left `running` by the bypassed run loop — the
    // drill owns it.
    sqlx::query("DELETE FROM ai_jobs WHERE id = $1")
        .bind(uuid::Uuid::from_u128(job_id.0))
        .execute(&pool)
        .await
        .expect("clean ai_jobs (left running by the aborted process)");
    restore_enforcement_disabled(&pool).await;
}

// ---------------------------------------------------------------------------
// Drill 4 — ALLOW verdict: zero side effects through the real chain (A4)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_moderation_allow_verdict_zero_side_effects() {
    let pool = pool();
    self_isolate(&pool).await;
    restore_enforcement_disabled(&pool).await;
    let fx = fixture(&pool, "drill4").await;
    let _source = seed_governance_enforcement(&pool, fx.ws).await;
    let (before_outbox,): (i64,) = sqlx::query_as("SELECT count(*) FROM audit_governance_outbox")
        .fetch_one(&pool)
        .await
        .expect("outbox count before");
    let (before_v1,): (i64,) = sqlx::query_as("SELECT count(*) FROM snaplink_delivery_outbox")
        .fetch_one(&pool)
        .await
        .expect("v1 count before");

    let stub = StubAnthropicServer::start("SAFE: drill-ok").await;
    let (svc, sink) = drill_service(&pool, &stub.base_url);
    let worker = AiWorker::with_config(svc, WorkerConfig::default());
    let jobs = AiJobRepo::new(pool.clone());
    let job_id = jobs
        .enqueue(
            AiJobKind::Moderate,
            Some(fx.msg.to_uuid()),
            Some(fx.ws.to_uuid()),
            serde_json::json!({"text": "drill content"}),
        )
        .await
        .expect("enqueue moderation job");
    let claimed = jobs.claim(1).await.expect("claim moderation job");
    let result = run_processed(&worker, claimed[0].clone())
        .await
        .expect("ALLOW path returns Ok");
    assert_eq!(
        result["verdict"], "safe",
        "production result key is \"safe\", not \"allow\""
    );

    // Zero side effects — nothing may touch the message.
    let (deleted_at, version, searchable_text, embedding_null): (
        Option<time::OffsetDateTime>,
        i32,
        String,
        bool,
    ) = sqlx::query_as(
        "SELECT deleted_at, version, searchable_text, embedding IS NULL
           FROM messages WHERE id = $1",
    )
    .bind(fx.msg.to_uuid())
    .fetch_one(&pool)
    .await
    .expect("message row");
    assert!(deleted_at.is_none(), "message untouched");
    assert_eq!(version, 1, "version untouched");
    assert_eq!(searchable_text, "pre-drill visible text");
    assert!(!embedding_null, "embedding still pre-seeded");
    let (audit_count,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM audit_events
          WHERE action = $1 AND target = $2::text",
    )
    .bind(LOCAL_ACTION_MODERATED)
    .bind(fx.msg.to_string())
    .fetch_one(&pool)
    .await
    .expect("audit count");
    assert_eq!(audit_count, 0, "zero audit rows");
    let (after_outbox,): (i64,) = sqlx::query_as("SELECT count(*) FROM audit_governance_outbox")
        .fetch_one(&pool)
        .await
        .expect("outbox count after");
    assert_eq!(after_outbox, before_outbox, "governance outbox delta 0");
    let (after_v1,): (i64,) = sqlx::query_as("SELECT count(*) FROM snaplink_delivery_outbox")
        .fetch_one(&pool)
        .await
        .expect("v1 count after");
    assert_eq!(after_v1, before_v1, "v1 outbox delta 0");
    let (event_count,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM event_outbox
          WHERE message_id = $1 AND event_kind = 'deleted'",
    )
    .bind(fx.msg.to_uuid())
    .fetch_one(&pool)
    .await
    .expect("event_outbox count");
    assert_eq!(event_count, 0, "no Deleted event_outbox row");
    assert_eq!(
        sink.0.lock().expect("sink lock").len(),
        1,
        "the paid call still reserved even though nothing was deleted"
    );

    sqlx::query("DELETE FROM ai_jobs WHERE id = $1")
        .bind(uuid::Uuid::from_u128(job_id.0))
        .execute(&pool)
        .await
        .expect("clean ai_jobs");
    restore_enforcement_disabled(&pool).await;
}

// ---------------------------------------------------------------------------
// Drill 5 — BLOCK verdict with no target_id: warn + Ok, no DB touch (A5)
// ---------------------------------------------------------------------------

/// Hermetic — NOT `#[ignore]`d: runs in the regular `cargo test` suite with no
/// Postgres and no external network. The repos sit on a never-connected lazy
/// pool (127.0.0.1:1 refuses connections): if this branch ever gains a DB
/// touch, the drill fails loudly instead of silently connecting.
#[tokio::test]
async fn drill_moderation_block_without_target_id_noop() {
    let lazy = PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgresql://unused:unused@127.0.0.1:1/unused")
        .expect("lazy pool never connects");
    let stub = StubAnthropicServer::start("BLOCK: drill-reason").await;
    let (svc, _sink) = drill_service(&lazy, &stub.base_url);
    let worker = AiWorker::with_config(svc, WorkerConfig::default());
    let job = AiJob {
        id: ulid::Ulid::new(),
        kind: AiJobKind::Moderate,
        target_id: None,
        workspace_id: Some(WorkspaceId::new().to_uuid()),
        status: AiJobStatus::Queued,
        attempts: 0,
        payload: serde_json::json!({"text": "drill content"}),
        result: None,
        error: None,
        scheduled_at: time::OffsetDateTime::now_utc(),
        started_at: None,
        finished_at: None,
    };
    // The warn! branch fires and falls through to the same result builder —
    // no delete, no audit, no outbox; pinned by the absence of any DB
    // interaction (lazy pool never connects).
    let result = run_processed(&worker, job).await.expect("warn + Ok, no delete");
    assert_eq!(result["verdict"], "block");
    assert!(result["reason"].is_string());
}
