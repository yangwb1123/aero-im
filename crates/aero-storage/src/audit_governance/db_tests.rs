use aero_common::{
    AuditActor, AuditAggregatePayload, AuditClaimPayload, AuditId, AuditTarget, MessageId,
    ParticipantId, RoomId, WorkspaceId, AGGREGATED_MESSAGE_ACTION, AUDIT_ACTOR_TYPE_PARTICIPANT,
    AUDIT_AGGREGATE_TYPE, AUDIT_DATA_CLASSIFICATION, AUDIT_EVENT_TYPE, AUDIT_OUTCOME_SUCCESS,
    AUDIT_RETENTION_CLASS, AUDIT_SCHEMA_ID, AUDIT_SCHEMA_VERSION, AUDIT_SOURCE_SYSTEM,
    GOVERNANCE_CLASS_MESSAGE, GOVERNANCE_CLASS_ROOM, L1_WINDOW_SECONDS,
    LOCAL_ACTION_MESSAGE_CREATE, LOCAL_ACTION_MESSAGE_DELETED, LOCAL_ACTION_MESSAGE_EDIT,
    LOCAL_ACTION_MESSAGE_RECALLED, LOCAL_ACTION_ROOM_ARCHIVED, LOCAL_ACTION_ROOM_CREATE,
    MODERATION_OUTBOUND_ACTION,
};
use sqlx::PgPool;
use uuid::Uuid;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

// Throwaway workspace + actor, self-contained (mirrors audit.rs db_tests).
async fn fixture(repo_pool: &PgPool) -> (WorkspaceId, ParticipantId) {
    let actor = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(actor.to_uuid())
        .bind(format!("gov-actor-{actor}"))
        .execute(repo_pool)
        .await
        .expect("insert participant");
    let ws = WorkspaceId::new();
    let mut tx = repo_pool.begin().await.expect("begin workspace fixture");
    sqlx::query(
        "INSERT INTO workspaces (id, name, slug, created_by, created_at)
             VALUES ($1, $2, $3, $4, now())",
    )
    .bind(ws.to_uuid())
    .bind("Governance Test WS")
    .bind(format!("gov-{ws}"))
    .bind(actor.to_uuid())
    .execute(&mut *tx)
    .await
    .expect("insert workspace");
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1, $2, 'owner')",
    )
    .bind(ws.to_uuid())
    .bind(actor.to_uuid())
    .execute(&mut *tx)
    .await
    .expect("insert workspace owner");
    tx.commit().await.expect("commit workspace fixture");
    (ws, actor)
}

// Room + message fixture in `ws`, so the transactional delete+audit tests
// have something real to soft-delete (mirrors audit.rs db_tests).
async fn message_in_workspace(p: &PgPool, ws: WorkspaceId, sender: ParticipantId) -> MessageId {
    let room = RoomId::new();
    sqlx::query(
        "INSERT INTO rooms (id, kind, name, created_by, workspace_id)
             VALUES ($1, 'group', $2, $3, $4)",
    )
    .bind(room.to_uuid())
    .bind(format!("gov-tx-room-{room}"))
    .bind(sender.to_uuid())
    .bind(ws.to_uuid())
    .execute(p)
    .await
    .expect("insert room");
    let repo = crate::message::MessageRepo::new(p.clone());
    repo.insert(crate::message::NewMessage {
        room_id: room,
        sender_id: sender,
        blocks: vec![aero_common::Block::text(
            "to be deleted, audited, governance-enqueued atomically",
        )],
        reply_to: None,
        metadata: serde_json::json!({}),
        expires_at: None,
    })
    .await
    .expect("insert message")
    .id
}

// Gate 1 + Gate 2 prerequisites for the 0239 trigger (and the 0236 v1
// trigger): commercial enforcement enabled + an enabled binding for the
// workspace. Same setup the A1 spec and harness leg D use. Tenant/client/
// source are globally UNIQUE (0235) — suffix with the ws uuid so parallel
// tests on one DB cannot collide. Also seeds an active entitlement
// projection (0235 `messages_snaplink_metering` fires on message INSERT
// when enforcement is on and would otherwise raise
// 'commercial entitlement is unavailable').
async fn enable_enforcement_with_binding(p: &PgPool, ws: WorkspaceId) {
    sqlx::query(
        "UPDATE snaplink_commercial_runtime
                SET enabled = TRUE, updated_at = clock_timestamp()
              WHERE singleton",
    )
    .execute(p)
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
    .execute(p)
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
    .execute(p)
    .await
    .expect("seed active entitlement projection");
}

/// Restore the fresh-DB default (`snaplink_commercial_runtime.enabled =
/// FALSE`, 0235) after a test that flipped it on. The singleton is
/// GLOBAL: the harness's main integration leg runs the whole ignored
/// suite on ONE shared DB, so a test that leaves the switch on makes
/// every later message INSERT (through the 0235 metering trigger) raise
/// P0001 in workspaces without bindings — restore-at-end keeps the
/// module order-independent (mirror of the defensive re-asserts at
/// start in the disabled-window tests).
async fn restore_enforcement_disabled(p: &PgPool) {
    sqlx::query(
        "UPDATE snaplink_commercial_runtime
                SET enabled = FALSE, updated_at = clock_timestamp()
              WHERE singleton",
    )
    .execute(p)
    .await
    .expect("restore enforcement disabled (fresh-DB default)");
}

// The exact production seam `AiWorker::handle_moderate` calls
// (worker/mod.rs): system soft-delete + `message.moderated` audit append
// in ONE transaction (aero-storage events.rs `soft_delete_outboxed_system`).
async fn moderate_finalize(
    p: &PgPool,
    id: MessageId,
    ws: WorkspaceId,
    detail: serde_json::Value,
) -> Result<Option<crate::message::events::OutboxedMessageDelete>, sqlx::Error> {
    crate::message::MessageRepo::new(p.clone())
        .soft_delete_outboxed_system(
            id,
            Some(ws),
            None,
            Some("message.moderated"),
            detail,
            ParticipantId::nil(),
            None,
        )
        .await
}

async fn count_governance_rows(p: &PgPool) -> i64 {
    sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM audit_governance_outbox")
        .fetch_one(p)
        .await
        .expect("count governance rows")
        .0
}

// Workspace-scoped outbox count: the harness entry shares ONE throwaway
// DB across all `audit_governance::` tests, and an earlier test (parity)
// may leave an enabled-binding workspace with its own moderation audit
// row — the reconciler correctly backfills that too. Tests whose
// assertions are about their own fixtures filter on the envelope's
// `aggregate_id` (== workspace id), never on the global table.
async fn governance_rows_for(p: &PgPool, ws: WorkspaceId) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_governance_outbox
              WHERE payload->>'aggregate_id' = $1",
    )
    .bind(ws.to_uuid().to_string())
    .fetch_one(p)
    .await
    .expect("count workspace-scoped governance rows")
}

// Order-independence within one harness entry run (tests share the DB;
// the harness grants each entry its own throwaway DB, but not each test).
// Full-table counts/selects below assume a pristine table, so each test
// resets it at start — same isolation assumption the drills document.
async fn reset_governance_table(p: &PgPool) {
    // The commercial runtime switch is a database-global singleton.  If a
    // preceding test panicked after enabling it, the next test must restore
    // the fresh-DB fail-open default before it inserts fixture rows; otherwise
    // the 0236 audit trigger can reject an unrelated fixture for lacking an
    // entitlement projection.  Keeping this reset self-contained makes the
    // ignored suite order-independent even after a failed test run.
    restore_enforcement_disabled(p).await;
    // The B5-1 auth slice added full-table DLQ / login-failure counters; the
    // module's isolation assumption ("full-table counts assume a pristine
    // table, so each test resets it at start") extends to the new tables.
    // `audit_events` is deliberately NOT cleared — tests scope by
    // workspace/actor/target and rows accumulate across a run.
    sqlx::query("DELETE FROM audit_governance_failed_pairs")
        .execute(p)
        .await
        .expect("reset failed-pairs DLQ");
    sqlx::query("DELETE FROM login_failures")
        .execute(p)
        .await
        .expect("reset login failures");
    sqlx::query("DELETE FROM audit_governance_outbox")
        .execute(p)
        .await
        .expect("reset governance table");
}

// --- Cross-domain helpers (shared by the submodule test bodies via
// `use super::*`; a lane test may probe the 0242 function, an L1 test
// may not probe the 0245 function, etc. — so every helper lives here
// at the db_tests level, keeping the submodule bodies byte-identical
// to the pre-split file). ---

async fn l1_aggregate_migrated(p: &PgPool) -> bool {
    let probe: Option<String> =
        sqlx::query_scalar("SELECT to_regprocedure('aero_enqueue_l1_aggregate_audit()')::text")
            .fetch_one(p)
            .await
            .expect("probe for the 0242 function");
    if probe.is_none() {
        eprintln!("SKIP: 0242 not migrated (aero_enqueue_l1_aggregate_audit missing)");
        return false;
    }
    true
}

/// Direct `audit_events` INSERT through the 0242 trigger (fixture shape:
/// fresh id, workspace, actor, action, detail, fixed `created_at`). The
/// 0236 v1 trigger also fires per row — enforcement must stay off
/// (Gate 1 fail-open) or the binding lookup runs; the L1 tests never
/// need a binding.
async fn insert_audit_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ws: WorkspaceId,
    actor: ParticipantId,
    action: &str,
    created_at: time::OffsetDateTime,
) {
    sqlx::query(
        "INSERT INTO audit_events (id, workspace_id, actor_id, action, detail, created_at)
             VALUES ($1, $2, $3, $4, '{}'::jsonb, $5)",
    )
    .bind(Uuid::new_v4())
    .bind(ws.to_uuid())
    .bind(actor.to_uuid())
    .bind(action)
    .bind(created_at)
    .execute(&mut **tx)
    .await
    .expect("insert audit row");
}

/// Window key recomputation from the leaf consts — the G2 pin. `md5` is
/// computed by PG (same function the 0242 trigger uses), so the spelling
/// is byte-identical to the SQL side.
async fn recompute_window_key(
    p: &PgPool,
    ws: WorkspaceId,
    created_at: time::OffsetDateTime,
) -> Uuid {
    let window_epoch: i64 =
        sqlx::query_scalar("SELECT floor(extract(epoch FROM $1::timestamptz) / $2)::bigint")
            .bind(created_at)
            .bind(L1_WINDOW_SECONDS)
            .fetch_one(p)
            .await
            .expect("window epoch");
    let key: Uuid = sqlx::query_scalar("SELECT md5($1)::uuid")
        .bind(format!(
            "{}|{}|{}",
            ws.to_uuid(),
            GOVERNANCE_CLASS_MESSAGE,
            window_epoch
        ))
        .fetch_one(p)
        .await
        .expect("recompute window key");
    key
}

/// 0245 presence probe (0242 `l1_aggregate_migrated` precedent): on a
/// partially-migrated shared DB the trigger is absent → explicit SKIP,
/// never a silent green (the harness leg is 0245-file-gated anyway, so
/// this only fires on a shared-DB run).
async fn room_trigger_migrated(p: &PgPool) -> bool {
    let probe: Option<String> =
        sqlx::query_scalar("SELECT to_regprocedure('aero_enqueue_room_audit()')::text")
            .fetch_one(p)
            .await
            .expect("probe for the 0245 function");
    if probe.is_none() {
        eprintln!("SKIP: 0245 not migrated (aero_enqueue_room_audit missing)");
        return false;
    }
    true
}

/// Direct `audit_events` INSERT through the 0245 trigger, RETURNING the id
/// (the 1:1 assertion needs the audit id; the 0236 v1 trigger also fires
/// per row — enforcement state decides whether a v1 row is produced).
async fn insert_audit_row_returning_id(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ws: WorkspaceId,
    actor: ParticipantId,
    action: &str,
    created_at: time::OffsetDateTime,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO audit_events (id, workspace_id, actor_id, action, detail, created_at)
             VALUES ($1, $2, $3, $4, '{}'::jsonb, $5)
             RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(ws.to_uuid())
    .bind(actor.to_uuid())
    .bind(action)
    .bind(created_at)
    .fetch_one(&mut **tx)
    .await
    .expect("insert audit row")
}

/// DB, so this only fires on a shared-DB run or a stale `aero-cli`
/// binary — the AC6 manual-path closure greps for this exact SKIP line
/// with `--nocapture`).
async fn recall_trigger_migrated(p: &PgPool) -> bool {
    let probe: Option<String> =
        sqlx::query_scalar("SELECT to_regprocedure('aero_enqueue_message_recall_audit()')::text")
            .fetch_one(p)
            .await
            .expect("probe for the 0246 function");
    if probe.is_none() {
        eprintln!("SKIP: 0246 not migrated (aero_enqueue_message_recall_audit missing)");
        return false;
    }
    true
}

mod auth;
mod ddl;
mod dedup;
mod failed_pairs;
mod gates;
mod l1;
mod l1_auth;
mod lanes;
mod parity;
mod producer;
mod rd2;
