//! B5-1 auth-slice v2 outbox write repository (H3 handoff — landed here per
//! the `audit_governance` module doc).
//!
//! This repo is the **enqueue side** of the governance outbox: it writes the
//! 1:1 pair (1 `audit_events` row + 1 `audit_governance_outbox` row,
//! `event_id = audit_events.id`) that the 0239 trigger writes for
//! `message.moderated`, but for the auth tokens that are **explicitly**
//! written (D1 — never by trigger-token extension). The consume side
//! (`aero-audit-connector` claim/settle/dead) is zero-change.
//!
//! # R1 + R7 single implementation point
//!
//! [`append_pair_in_tx_fail_open`] wraps the audit + outbox INSERTs in a RAW
//! SQL `SAVEPOINT aero_audit_pair`. Database-class failures (RAISE P0001,
//! CHECK 23514, FK 23503, unique 23505, transient 40P01/40001/55P03)
//! `ROLLBACK TO SAVEPOINT` — both rows gone, whole pair absent, never a half
//! pair — then enqueue the ORIGINAL INPUT into `audit_governance_failed_pairs`
//! (0244) **in the same tx** (replayable, not lost), classify per the §2.8
//! table, count `audit_auth_write_failures_total{category}` (storage's first
//! metric — deliberate exception), and return `Ok(None)` so the caller's
//! domain tx commits (fail-open R7). Connection-level errors roll back to the
//! savepoint and PROPAGATE (the caller's tx is unusable; the domain op fails
//! as today — never a silent drop, never a half pair).
//!
//! IMMEDIATE-constraint pin (D4): 0239/0236 CHECK/RAISE are all IMMEDIATE. A
//! future `DEFERRABLE` change would defer errors to COMMIT and break the
//! SAVEPOINT branch (whole-tx fail-closed, violates R7) — migration 0244's
//! DDL comment and this doc are the two pins.
//!
//! # Envelope
//!
//! [`governance_envelope`] is the Rust mirror of the 0239 trigger's
//! `jsonb_build_object` (16 keys). `source_system` is parameterized: auth
//! explicit writes pass [`tokens::AUTH_SOURCE_SYSTEM`]; the moderation
//! trigger uses `binding.source_system`. L1 aggregation rows additionally
//! carry top-level `"aggregated": true` (the cross-slice parity-exemption key
//! shared with 0242 rows — NEVER inside `detail`).

use aero_common::{
    audit_wire_occurred_at, AuditActor, AuditId, AuditTarget, ParticipantId, WorkspaceId,
    AUDIT_ACTOR_TYPE_PARTICIPANT, AUDIT_ACTOR_TYPE_SYSTEM, AUDIT_AGGREGATE_TYPE,
    AUDIT_DATA_CLASSIFICATION, AUDIT_EVENT_TYPE, AUDIT_OUTCOME_SUCCESS, AUDIT_RETENTION_CLASS,
    AUDIT_SCHEMA_ID, AUDIT_SCHEMA_VERSION, AUDIT_SOURCE_SYSTEM, AUDIT_TARGET_TYPE_RESOURCE,
    GOVERNANCE_CLASS_ADMIN, GOVERNANCE_CLASS_MESSAGE, L1_WINDOW_SECONDS,
    LOCAL_ACTION_MESSAGE_DELETED,
};
use sqlx::{PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use super::failed_pairs::FailedPairRepo;
use super::tokens::{
    AUTH_SOURCE_SYSTEM, L1_AUTH_LOGIN_FAILURE_ACTION, OUTBOUND_AUTH_LOGIN_FAILURE,
};
use crate::audit::AuditRepo;

/// Governance class for auth 1:1 pairs (class 'admin', priority 10 — the
/// §2.7 lane; moderation stays admin/100 and never preempts the audit lane).
const AUTH_PAIR_CLASS: &str = GOVERNANCE_CLASS_ADMIN;
const AUTH_PAIR_PRIORITY: i16 = 10;
/// L1 aggregation rows are class 'message', priority 10 (D2 outcome A).
const L1_CLASS: &str = GOVERNANCE_CLASS_MESSAGE;
const L1_PRIORITY: i16 = 10;

/// R-D2 message-delete lane: 1:1 class-'message' priority-10 rows written by
/// the Rust writer (never the 0242 window — the 1:1 requirement is
/// load-bearing, 0246 header). The priority is local (aero-storage must not
/// depend on aero-ai; `GOVERNANCE_PRIORITY_BACKLOG = 10` drift-guarded by the
/// `db_tests` pins — `AUTH_PAIR_PRIORITY` precedent).
const MESSAGE_DELETE_CLASS: &str = GOVERNANCE_CLASS_MESSAGE;
const MESSAGE_DELETE_PRIORITY: i16 = 10;
/// Reconciler batch upper bound (mirror of the connector's `MAX_CLAIM`=500
/// claim-batch cap — the scan must never outsize a claim batch).
const RECONCILE_BATCH_MAX: i64 = 500;

/// One orphaned `message.deleted` audit row awaiting the Rust-side backfill
/// (version-skew window: the old binary wrote the audit row, no outbox twin).
type DeleteLaneOrphan = (
    uuid::Uuid,
    uuid::Uuid,
    Option<uuid::Uuid>,
    Option<String>,
    serde_json::Value,
    OffsetDateTime,
);

/// Counter categories (bounded label vocabulary, §2.8).
const CATEGORY_CONTRACT: &str = "contract";
const CATEGORY_BINDING: &str = "binding";
const CATEGORY_DATABASE: &str = "database";

/// SQLSTATE for the connection-level standalone fallback DLQ enqueue
/// (08 class = connection exception; no `PgDatabaseError` code exists).
const SQLSTATE_CONNECTION: &str = "08006";

/// Cheap to clone (shares the underlying `PgPool`).
#[derive(Clone)]
#[must_use]
pub struct AuditGovernanceOutboxRepo {
    pool: PgPool,
    /// F-A watermark: max processed `login_failures.created_at` from the
    /// last aggregation tick (process-local; resets to `UNIX_EPOCH` on restart
    /// → a closed-window rescan, deduped by `ON CONFLICT DO NOTHING`).
    l1_watermark: time::OffsetDateTime,
}

impl AuditGovernanceOutboxRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            l1_watermark: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    /// Transaction-scoped outbox row insert (mirrors
    /// [`AuditRepo::append_in_tx`](crate::audit::AuditRepo::append_in_tx)
    /// shape). `status = 0` is built in; `event_id` MUST equal an
    /// `audit_events.id` (1:1 parity) except for L1 aggregation rows (D7
    /// exemption — synthetic v5 `event_id`, no audit row). Idempotent via
    /// `ON CONFLICT (event_id) DO NOTHING` — the same contract the 0239
    /// trigger's INSERT carries (replay path: a same-id re-insert is a
    /// no-op, never a 23505).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] (connection-level; the caller's tx is
    /// unusable).
    pub async fn append_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        event_id: AuditId,
        class: &str,
        priority: i16,
        payload: serde_json::Value,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO audit_governance_outbox (event_id, status, class, priority, payload)
               VALUES ($1, 0, $2, $3, $4)
               ON CONFLICT (event_id) DO NOTHING",
        )
        .bind(event_id.to_uuid())
        .bind(class)
        .bind(priority)
        .bind(payload)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// 1:1 `message.deleted` outbox row, written in the delete tx (R-D2
    /// writer — the 0245/0246 declared carve-out: `message.deleted` is NOT
    /// trigger-owned). The audit row MUST already exist in-tx (the caller
    /// appended it first); `occurred_at` is re-selected from it (`write_pair`
    /// precedent — single clock domain, never a Rust formatter).
    ///
    /// Fail-closed: propagates [`sqlx::Error`] — a lost outbox row aborts the
    /// delete (mirrors the 0239/0242/0245/0246 AFTER-trigger abort semantics;
    /// the deliberate opposite of the auth `append_pair_in_tx_fail_open`).
    /// Gate-free (0242 D2): no runtime gate, no binding lookup.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] (the caller's tx aborts with it).
    pub async fn append_message_delete_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        audit_id: AuditId,
        workspace: WorkspaceId,
        actor: Option<ParticipantId>,
        target: Option<&str>,
        detail: serde_json::Value,
    ) -> Result<(), sqlx::Error> {
        // Server-stamped created_at re-selected (write_pair precedent,
        // outbox.rs write_pair) — single clock domain, never a Rust formatter.
        let occurred_at: OffsetDateTime =
            sqlx::query_scalar("SELECT created_at FROM audit_events WHERE id = $1")
                .bind(audit_id.to_uuid())
                .fetch_one(&mut **tx)
                .await?;
        let payload = governance_envelope(
            audit_id,
            workspace,
            actor,
            target,
            detail,
            LOCAL_ACTION_MESSAGE_DELETED, // outbound action = local token verbatim
            AUDIT_OUTCOME_SUCCESS,
            occurred_at,
            AUDIT_SOURCE_SYSTEM, // "aero-im.source" — passes connector PayloadGuard
        );
        Self::append_in_tx(
            tx,
            audit_id,
            MESSAGE_DELETE_CLASS,
            MESSAGE_DELETE_PRIORITY,
            payload,
        )
        .await
    }

    /// R1 + R7 single implementation point: SAVEPOINT-wrapped 1:1 audit pair.
    ///
    /// Success → `RELEASE SAVEPOINT`; the pair commits with the caller's
    /// transaction. Database-class failure → `ROLLBACK TO SAVEPOINT` (both
    /// rows gone), original input enqueued into the DLQ **in the same tx**
    /// (nested savepoint — a DLQ-enqueue failure can never abort the domain
    /// tx, F-6), classified log + forced counter, `Ok(None)` (fail-open; the
    /// domain row commits). Connection-level failure → rollback to savepoint
    /// + propagate `Err` (the caller's tx is unusable).
    ///
    /// Contract pre-check (F-2): a non-object `detail` can never produce a DB
    /// error (`audit_events.detail` has no CHECK and the envelope is always
    /// an object), so it is rejected in Rust — warn + `{category="contract"}`
    /// counter + `Ok(None)` with zero DB touches.
    ///
    /// # Errors
    /// Propagates connection-level [`sqlx::Error`] only.
    pub async fn append_pair_in_tx_fail_open(
        tx: &mut Transaction<'_, Postgres>,
        workspace: WorkspaceId,
        actor: Option<ParticipantId>,
        action: &str,
        target: Option<&str>,
        detail: serde_json::Value,
        outbound_action: &str,
    ) -> Result<Option<AuditId>, sqlx::Error> {
        Self::append_pair_in_tx_fail_open_inner(
            tx,
            workspace,
            actor,
            action,
            target,
            detail,
            outbound_action,
            "in_tx",
            false,
        )
        .await
    }

    /// Replay-only variant: on a Database-class re-failure it returns
    /// `Ok(None)` WITHOUT enqueueing a fresh DLQ clone — the ORIGINAL DLQ
    /// row's `replay_attempts` increments and caps at dead (F-4/async-reviewer
    /// fix: `replay_all` must not double the population under a persistent
    /// bug). The producer path keeps the DLQ clone (at-least-once, D9).
    pub async fn append_pair_in_tx_fail_open_no_dlq(
        tx: &mut Transaction<'_, Postgres>,
        workspace: WorkspaceId,
        actor: Option<ParticipantId>,
        action: &str,
        target: Option<&str>,
        detail: serde_json::Value,
        outbound_action: &str,
    ) -> Result<Option<AuditId>, sqlx::Error> {
        Self::append_pair_in_tx_fail_open_inner(
            tx,
            workspace,
            actor,
            action,
            target,
            detail,
            outbound_action,
            "replay",
            true,
        )
        .await
    }

    /// Shared pair-write implementation; `path` feeds the observability log
    /// contract (`in_tx` | `standalone`). Signature mirrors the design-pinned
    /// [`append_pair_in_tx_fail_open`] (+ the `path` classifier).
    #[allow(clippy::too_many_arguments)]
    async fn append_pair_in_tx_fail_open_inner(
        tx: &mut Transaction<'_, Postgres>,
        workspace: WorkspaceId,
        actor: Option<ParticipantId>,
        action: &str,
        target: Option<&str>,
        detail: serde_json::Value,
        outbound_action: &str,
        path: &str,
        suppress_dlq: bool,
    ) -> Result<Option<AuditId>, sqlx::Error> {
        // F-2 contract pre-check: non-object detail → Ok(None), no DB touch.
        if !detail.is_object() {
            tracing::warn!(
                workspace_id = %workspace,
                action,
                path,
                error_sqlstate = "contract",
                "audit pair skipped: detail must be a JSON object (contract bug)"
            );
            Self::count_failure(CATEGORY_CONTRACT);
            return Ok(None);
        }

        sqlx::query("SAVEPOINT aero_audit_pair")
            .execute(&mut **tx)
            .await?;

        match Self::write_pair(
            tx,
            workspace,
            actor,
            action,
            target,
            detail.clone(),
            outbound_action,
        )
        .await
        {
            Ok(audit_id) => {
                sqlx::query("RELEASE SAVEPOINT aero_audit_pair")
                    .execute(&mut **tx)
                    .await?;
                Ok(Some(audit_id))
            }
            Err(e) if is_fail_open_error(&e) => {
                // Whole pair rolled back — zero rows escape (at-least-once
                // discipline, AGENTS.md §4.2). The tx remains usable. The
                // shared classifier ([`is_fail_open_error`]) is the G1-pinned
                // branch boundary — Database-class → fail-open, everything
                // else → propagate below.
                let db_err = e
                    .as_database_error()
                    .expect("is_fail_open_error checked the Database class");
                sqlx::query("ROLLBACK TO SAVEPOINT aero_audit_pair")
                    .execute(&mut **tx)
                    .await?;
                let sqlstate = db_err.code().unwrap_or_default().to_string();
                let (category, level) = Self::classify(&sqlstate);
                // D9: same-tx DLQ enqueue (nested savepoint — a failed DLQ
                // insert, e.g. 42P01 with a missing 0244, can never abort the
                // domain tx: F-6 last-resort guard). Suppressed on the replay
                // path (`suppress_dlq`) — a re-failed replay increments the
                // ORIGINAL row's attempts instead of cloning the population.
                let dlq_id = if suppress_dlq {
                    None
                } else {
                    // F-6 (code-architecture-reviewer blocker): the DLQ
                    // enqueue runs under its OWN NESTED SAVEPOINT — a failed
                    // DLQ INSERT (e.g. 42P01 with a missing 0244 in a bad
                    // deploy, or any Database-class error) would otherwise
                    // ABORT the whole caller's tx: PG marks the tx aborted and
                    // the subsequent `tx.commit()` fails with "current
                    // transaction is aborted", silently rolling back the
                    // domain op — fail-CLOSED, violating R7. The nested
                    // savepoint restores the tx to a usable state: the domain
                    // commits, the pair is lost-but-logged.
                    sqlx::query("SAVEPOINT aero_audit_dlq")
                        .execute(&mut **tx)
                        .await?;
                    match FailedPairRepo::enqueue_in_tx(
                        tx,
                        workspace,
                        actor,
                        action,
                        target,
                        detail,
                        outbound_action,
                        &sqlstate,
                        Some(db_err.message()),
                    )
                    .await
                    {
                        Ok(id) => {
                            sqlx::query("RELEASE SAVEPOINT aero_audit_dlq")
                                .execute(&mut **tx)
                                .await?;
                            Some(id)
                        }
                        Err(dlq_err) => {
                            // Restore the tx to usable state; the pair is
                            // lost-but-logged (R7 fail-open preserved).
                            sqlx::query("ROLLBACK TO SAVEPOINT aero_audit_dlq")
                                .execute(&mut **tx)
                                .await?;
                            tracing::error!(
                                workspace_id = %workspace,
                                action,
                                path,
                                error_sqlstate = %sqlstate,
                                dlq_error = %dlq_err,
                                "audit pair failed open AND its DLQ enqueue failed (pair lost-but-logged)"
                            );
                            None
                        }
                    }
                };
                Self::log_and_count(category, level, workspace, action, path, &sqlstate, dlq_id);
                Ok(None)
            }
            Err(other) => {
                // Connection-level: the caller's tx is unusable — propagate.
                sqlx::query("ROLLBACK TO SAVEPOINT aero_audit_pair")
                    .execute(&mut **tx)
                    .await?;
                Err(other)
            }
        }
    }

    /// The two INSERTs of the pair: audit row (via the shared
    /// [`AuditRepo::append_in_tx`] — ONE audit INSERT impl) then the outbox
    /// row with the 16-key envelope. `occurred_at` is re-selected from the
    /// audit row so the envelope mirrors the trigger's
    /// `jsonb_build_object('occurred_at', NEW.created_at)` (server-stamped,
    /// single clock domain).
    async fn write_pair(
        tx: &mut Transaction<'_, Postgres>,
        workspace: WorkspaceId,
        actor: Option<ParticipantId>,
        action: &str,
        target: Option<&str>,
        detail: serde_json::Value,
        outbound_action: &str,
    ) -> Result<AuditId, sqlx::Error> {
        let audit_id =
            AuditRepo::append_in_tx(tx, workspace, actor, action, target, detail.clone()).await?;
        // Server-stamped created_at re-selected so the envelope mirrors the
        // trigger's jsonb_build_object('occurred_at', NEW.created_at) exactly
        // (single clock domain).
        let occurred_at: OffsetDateTime =
            sqlx::query_scalar("SELECT created_at FROM audit_events WHERE id = $1")
                .bind(audit_id.to_uuid())
                .fetch_one(&mut **tx)
                .await?;
        let payload = governance_envelope(
            audit_id,
            workspace,
            actor,
            target,
            detail,
            outbound_action,
            AUDIT_OUTCOME_SUCCESS,
            occurred_at,
            AUTH_SOURCE_SYSTEM,
        );
        Self::append_in_tx(tx, audit_id, AUTH_PAIR_CLASS, AUTH_PAIR_PRIORITY, payload).await?;
        Ok(audit_id)
    }

    /// login/refresh path: own transaction pair (begin → pair → commit),
    /// **swallow-all** signature (D12) — no `Err` branch exists here.
    ///
    /// `Some(id)` = the pair committed. `None` = the pair was skipped
    /// (Database-class failure already enqueued the DLQ row in the pair's own
    /// tx, which commits with it; a connection-level failure warns and
    /// best-effort enqueues the DLQ row in a fresh tx). The auth result is
    /// NEVER flipped (R7).
    pub async fn record_pair_standalone(
        &self,
        workspace: WorkspaceId,
        actor: Option<ParticipantId>,
        action: &str,
        target: Option<&str>,
        detail: serde_json::Value,
        outbound_action: &str,
    ) -> Option<AuditId> {
        let mut tx = match self.pool.begin().await {
            Ok(tx) => tx,
            Err(e) => {
                Self::count_failure(CATEGORY_DATABASE);
                tracing::warn!(
                    workspace_id = %workspace,
                    action,
                    path = "standalone",
                    error = %e,
                    "audit pair skipped: begin failed (connection-level)"
                );
                return None;
            }
        };
        match Self::append_pair_in_tx_fail_open_inner(
            &mut tx,
            workspace,
            actor,
            action,
            target,
            detail.clone(),
            outbound_action,
            "standalone",
            false,
        )
        .await
        {
            Ok(Some(audit_id)) => {
                if let Err(e) = tx.commit().await {
                    Self::count_failure(CATEGORY_DATABASE);
                    tracing::warn!(
                        workspace_id = %workspace,
                        action,
                        path = "standalone",
                        error = %e,
                        "audit pair skipped: standalone commit failed"
                    );
                    return None;
                }
                Some(audit_id)
            }
            Ok(None) => {
                // The DLQ row (if any — a Database-class failure enqueued it
                // inside this tx) must PERSIST with it: the compensation
                // mirrors the dropped pair's fate (at-least-once, D9).
                // Committing an empty tx (contract pre-check / F-6 case) is
                // harmless.
                if let Err(e) = tx.commit().await {
                    Self::count_failure(CATEGORY_DATABASE);
                    tracing::warn!(
                        workspace_id = %workspace,
                        action,
                        path = "standalone",
                        error = %e,
                        "audit pair skipped: standalone commit failed"
                    );
                }
                None
            }
            Err(e) => {
                let _ = tx.rollback().await;
                Self::count_failure(CATEGORY_DATABASE);
                tracing::warn!(
                    workspace_id = %workspace,
                    action,
                    path = "standalone",
                    error = %e,
                    "audit pair skipped: connection-level failure"
                );
                // Best-effort compensation (landing design §2.2): a fresh tx
                // may still succeed after a transient connection error.
                if let Err(dlq_err) = FailedPairRepo::enqueue_standalone(
                    &self.pool,
                    workspace,
                    actor,
                    action,
                    target,
                    detail,
                    outbound_action,
                    SQLSTATE_CONNECTION,
                    Some(&e.to_string()),
                )
                .await
                {
                    Self::count_failure(CATEGORY_DATABASE);
                    tracing::warn!(
                        workspace_id = %workspace,
                        action,
                        path = "standalone",
                        dlq_error = %dlq_err,
                        "standalone DLQ enqueue failed (pair lost-but-logged)"
                    );
                }
                None
            }
        }
    }

    /// L1 `auth.login.failure` aggregation (D2 outcome A / DP-3): closed
    /// buckets of `login_failures` only (`bucket_end` ≤ now − window), exactly
    /// one outbox row per bucket, class 'message', priority 10, deterministic
    /// uuid-v5 `event_id` (idempotent rerun + concurrent multi-instance safe
    /// via `ON CONFLICT DO NOTHING`), no `audit_events` row ever (D7 — avoids
    /// the 0236 v1 trigger side effect and default-workspace audit-view
    /// pollution). Never deletes base rows (forensic retention; the retention
    /// timer owns deletion).
    ///
    /// # F-C clock-domain skew bound (documented residual)
    ///
    /// For the L1 auth path the app-vs-DB skew is **structurally absent**:
    /// `login_failures.created_at` is DB-stamped (`DEFAULT now()`, microsecond
    /// precision — migration 0140), and `LoginFailureRepo::record` never binds
    /// it. Producer and aggregator share one clock domain (the DB server
    /// clock), so a row written in bucket B is always scanned by the first
    /// tick whose boundary passes B's end. The freeze residual applies ONLY to:
    /// 1. a DB **clock retreat** ≥ the one-window margin (the boundary
    ///    expression `floor(now/W)*W − W` retreats with the clock; the
    ///    watermark assignment is unconditional, so a retreat triggers a
    ///    rescan but `ON CONFLICT DO NOTHING` cannot merge into an existing
    ///    row — the frozen undercount stands), or
    /// 2. an **out-of-band insert** (backfill/restore/manual) into a bucket
    ///    whose outbox row already exists: the row is scanned
    ///    (`created_at ≥ watermark`) but `ON CONFLICT DO NOTHING` freezes the
    ///    count — there
    ///    is no merge arm for the auth pull-scan (the 0242 trigger's
    ///    `DO UPDATE … WHERE status = 0` merge is message-lane only).
    ///
    /// The closure margin is **one window** beyond bucket-end
    /// (`current_bucket_start − W`), less conservative than the architect's
    /// recommended `bucket_end ≤ now − 2W` — acceptable for DB-stamped rows
    /// (a row is scanned only after its bucket end is ≥ one full window in
    /// the past), and it is the documented tolerance.
    ///
    /// Returns the number of NEWLY inserted outbox rows.
    ///
    /// # Errors
    /// Propagates connection-level [`sqlx::Error`] only.
    pub async fn aggregate_login_failure_buckets(
        &mut self,
        window_secs: i64,
    ) -> Result<usize, sqlx::Error> {
        let window_secs = window_secs.max(1);
        // F-A (design-gate blocker): the scan must be SARGABLE on
        // `created_at` so the 0243 `login_failures_created_at_idx` index is
        // usable. The closed-bucket boundary is fetched once (server clock),
        // `created_at < boundary` is the index-friendly closed-bucket
        // predicate (equivalent to the legacy `bucket_end <= now - W` — the
        // extra-window margin is preserved: boundary = current_bucket_start -
        // W), and `>= watermark` limits each tick to newly-closed rows.
        //
        // **Async-reviewer fixes (design gate, run 386d075c)**: the watermark
        // advances to the BOUNDARY (all rows below it were scanned + upserted
        // in this call), never to a global MAX — a MAX over open-bucket rows
        // skipped by the scan would permanently lose every bucket's first
        // `(boot mod W)` seconds (ON CONFLICT DO NOTHING freezes the
        // undercount). The advance happens AFTER the insert loop, so a
        // mid-loop failure leaves the watermark behind the scanned rows and
        // the next tick rescans them (deduped — never lost).
        //
        // **Q1 residual (distributed-engineer)**: the watermark predicate is
        // `>=` (not `>`) so a row whose `created_at` is EXACTLY a boundary
        // (a whole-second multiple of W) is picked up by the first scan whose
        // boundary passes its bucket end — with strict `>` it was excluded by
        // the scan that closed its bucket AND by every later scan (permanent
        // leak). The initial watermark is `UNIX_EPOCH`, so `>=` is safe from
        // the first tick (no row predates 1970).
        let boundary: i64 = sqlx::query_scalar(
            r"SELECT floor(extract(epoch FROM clock_timestamp()) / $1)::bigint * $1 - $1",
        )
        .bind(window_secs)
        .fetch_one(&self.pool)
        .await?;
        let buckets: Vec<(i64, i64)> = sqlx::query_as(
            r"SELECT floor(extract(epoch FROM created_at) / $1)::bigint * $1 AS bucket_start,
                    COUNT(*)::bigint AS n
               FROM login_failures
              WHERE created_at < to_timestamp($2)
                AND created_at >= $3
              GROUP BY bucket_start
              ORDER BY bucket_start",
        )
        .bind(window_secs)
        .bind(boundary)
        .bind(self.l1_watermark)
        .fetch_all(&self.pool)
        .await?;

        let nil_workspace = WorkspaceId::nil();
        let mut inserted = 0usize;
        for (bucket_start, count) in buckets {
            let event_id = l1_event_id(nil_workspace, bucket_start);
            let occurred_at = OffsetDateTime::from_unix_timestamp(bucket_start)
                .unwrap_or_else(|_| OffsetDateTime::now_utc());
            let mut envelope = governance_envelope(
                AuditId::from_uuid(event_id),
                nil_workspace,
                None,
                None,
                serde_json::json!({
                    "count": count,
                    "window_start_epoch": bucket_start,
                    "window_secs": window_secs,
                }),
                OUTBOUND_AUTH_LOGIN_FAILURE,
                "failure",
                occurred_at,
                AUTH_SOURCE_SYSTEM,
            );
            // Parity-exemption key at the ENVELOPE TOP LEVEL (0242-shared
            // expression — NEVER inside detail).
            envelope["aggregated"] = serde_json::Value::Bool(true);
            let row: Option<uuid::Uuid> = sqlx::query_scalar(
                r"INSERT INTO audit_governance_outbox (event_id, status, class, priority, payload)
                   VALUES ($1, 0, $2, $3, $4)
                   ON CONFLICT (event_id) DO NOTHING
                   RETURNING event_id",
            )
            .bind(event_id)
            .bind(L1_CLASS)
            .bind(L1_PRIORITY)
            .bind(envelope)
            .fetch_optional(&self.pool)
            .await?;
            if row.is_some() {
                inserted += 1;
            }
        }
        // Watermark advance AFTER the insert loop (async-reviewer fix): only
        // on success does the boundary become the new watermark — a mid-loop
        // failure (`?` above) leaves it behind so nothing is lost.
        self.l1_watermark =
            time::OffsetDateTime::from_unix_timestamp(boundary).unwrap_or(self.l1_watermark);
        Ok(inserted)
    }

    /// R-D2 §10.5: version-skew backfill — the Rust parallel-scan twin of
    /// the 0241 reconciler (which is `message.moderated`-only and immutable;
    /// no migration edit). During a rolling deploy an old binary commits the
    /// `message.deleted` audit row but no outbox row; this scan backfills
    /// orphaned rows (`action = message.deleted` with no outbox twin) within
    /// the audit-retention window (`cutoff`), enqueueing each via the SAME
    /// 16-key envelope as [`Self::append_message_delete_in_tx`]
    /// (`ON CONFLICT (event_id) DO NOTHING` — idempotent, concurrent-instance
    /// safe). Never fabricates a `message.moderated` claim and never touches
    /// trigger-owned tokens (exact-token scan). The scan is `SARGable` on the
    /// partition key (`action`, `created_at >= cutoff` prunes partitions),
    /// bounded by `batch`.
    ///
    /// Returns the number of NEWLY inserted outbox rows.
    ///
    /// # Errors
    /// Propagates connection-level [`sqlx::Error`] only.
    pub async fn reconcile_message_deleted(
        &self,
        cutoff: OffsetDateTime,
        batch: i64,
    ) -> Result<i64, sqlx::Error> {
        let batch = batch.clamp(1, RECONCILE_BATCH_MAX);
        // AuditId/WorkspaceId/ParticipantId are newtypes — decode the raw
        // uuid columns and wrap (no sqlx Decode impl for the newtypes).
        let orphans: Vec<DeleteLaneOrphan> = sqlx::query_as(
            r"SELECT a.id, a.workspace_id, a.actor_id, a.target, a.detail, a.created_at
                 FROM audit_events a
                WHERE a.action = $1
                  AND a.created_at >= $2
                  AND NOT EXISTS (
                        SELECT 1 FROM audit_governance_outbox o
                         WHERE o.event_id = a.id)
                ORDER BY a.created_at
                LIMIT $3",
        )
        .bind(LOCAL_ACTION_MESSAGE_DELETED)
        .bind(cutoff)
        .bind(batch)
        .fetch_all(&self.pool)
        .await?;
        let mut inserted = 0i64;
        for (audit_id, workspace, actor, target, detail, created_at) in orphans {
            let payload = governance_envelope(
                AuditId::from_uuid(audit_id),
                WorkspaceId::from_uuid(workspace),
                actor.map(ParticipantId::from_uuid),
                target.as_deref(),
                detail,
                LOCAL_ACTION_MESSAGE_DELETED,
                AUDIT_OUTCOME_SUCCESS,
                created_at,
                AUDIT_SOURCE_SYSTEM,
            );
            let row: Option<uuid::Uuid> = sqlx::query_scalar(
                r"INSERT INTO audit_governance_outbox (event_id, status, class, priority, payload)
                   VALUES ($1, 0, $2, $3, $4)
                   ON CONFLICT (event_id) DO NOTHING
                   RETURNING event_id",
            )
            .bind(audit_id)
            .bind(MESSAGE_DELETE_CLASS)
            .bind(MESSAGE_DELETE_PRIORITY)
            .bind(payload)
            .fetch_optional(&self.pool)
            .await?;
            if row.is_some() {
                inserted += 1;
            }
        }
        Ok(inserted)
    }

    /// R-D2 §10.2: retention sweep for the governance outbox — DELETE only
    /// TERMINAL rows (`status IN (2, 3)`: delivered / dead; the sink holds
    /// the ledger once delivered) older than `cutoff`. Live rows (status
    /// 0/1) are NEVER touched while the relay runs (0246 header: "never
    /// delete while the relay runs" — a live row is still due for claim).
    /// Returns the number of deleted rows.
    ///
    /// # Errors
    /// Propagates connection-level [`sqlx::Error`] only.
    pub async fn sweep_terminal_before(&self, cutoff: OffsetDateTime) -> Result<u64, sqlx::Error> {
        let deleted = sqlx::query(
            r"DELETE FROM audit_governance_outbox
               WHERE status IN (2, 3)
                 AND created_at < $1",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(deleted.rows_affected())
    }

    /// SQLSTATE classification (§2.8): P0001 → binding (ERROR); contract-bug
    /// classes 23514/23503/23505/other → contract (ERROR) / database (ERROR);
    /// transient 40P01/40001/55P03 → database (warn).
    fn classify(sqlstate: &str) -> (&'static str, tracing::Level) {
        match sqlstate {
            "P0001" => (CATEGORY_BINDING, tracing::Level::ERROR),
            "23514" | "23503" | "23505" => (CATEGORY_CONTRACT, tracing::Level::ERROR),
            "40P01" | "40001" | "55P03" => (CATEGORY_DATABASE, tracing::Level::WARN),
            _ => (CATEGORY_DATABASE, tracing::Level::ERROR),
        }
    }

    fn count_failure(category: &str) {
        aero_common::metrics::inc_counter_labeled(
            aero_common::metrics::names::AUDIT_AUTH_WRITE_FAILURES_TOTAL,
            1,
            &[("category", category)],
        );
    }

    /// Log-contract (D10): every failure line carries `workspace_id`,
    /// `action`, `path`, `error_sqlstate`, DLQ row id (post-enqueue). The
    /// `detail` NEVER enters a log — it only ever reaches the DLQ table.
    fn log_and_count(
        category: &'static str,
        level: tracing::Level,
        workspace: WorkspaceId,
        action: &str,
        path: &str,
        sqlstate: &str,
        dlq_id: Option<i64>,
    ) {
        Self::count_failure(category);
        let message = "audit pair failed open (domain commits; pair enqueued to DLQ)";
        match level {
            tracing::Level::ERROR => {
                tracing::error!(
                    workspace_id = %workspace,
                    action,
                    path,
                    error_sqlstate = sqlstate,
                    dlq_id,
                    "{message}"
                );
            }
            _ => {
                tracing::warn!(
                    workspace_id = %workspace,
                    action,
                    path,
                    error_sqlstate = sqlstate,
                    dlq_id,
                    "{message}"
                );
            }
        }
    }
}

/// Deterministic v5 `event_id` for an L1 login-failure bucket
/// (connector design §2.6): `aero.im.audit.l1:{nil-workspace}:auth.login.failure:{bucket_start_epoch}`.
fn l1_event_id(nil_workspace: WorkspaceId, bucket_start_epoch: i64) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!(
            "aero.im.audit.l1:{}:{}:{}",
            nil_workspace.to_uuid(),
            L1_AUTH_LOGIN_FAILURE_ACTION,
            bucket_start_epoch
        )
        .as_bytes(),
    )
}

/// 0239-trigger-envelope Rust mirror (16 keys): `event_id`, `source_system`,
/// `event_type`, `schema_id`, `schema_version`, `occurred_at`, `actor`,
/// `targets`, `aggregate_type`, `aggregate_id`, `action`, `outcome`,
/// `payload`, `data_classification`, `retention_class`, `idempotency_key`.
///
/// `source_system` is parameterized (auth explicit writes =
/// [`tokens::AUTH_SOURCE_SYSTEM`]; the moderation trigger uses
/// `binding.source_system`). `payload` is the caller's `detail` (object
/// pass-through mirroring `aero_snaplink_audit_payload`). L1 rows add
/// top-level `"aggregated": true` at the call site — never inside `detail`.
///
/// Signature is pinned verbatim by the governing designs (16-key envelope
/// mirror of the 0239 trigger's `jsonb_build_object`). `detail` is moved into
/// the envelope via `json!` (consumption invisible to clippy's analysis).
#[must_use]
#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
pub fn governance_envelope(
    event_id: AuditId,
    workspace: WorkspaceId,
    actor: Option<ParticipantId>,
    target: Option<&str>,
    detail: serde_json::Value,
    outbound_action: &str,
    outcome: &str,
    occurred_at: OffsetDateTime,
    source_system: &str,
) -> serde_json::Value {
    // Canonical PG timestamptz→jsonb spelling (§10.4): `+00:00` offset,
    // trailing-zero-trimmed fraction, omitted when zero — byte-identical to
    // the 0239 trigger's `jsonb_build_object('occurred_at', NEW.created_at)`
    // (the `Rfc3339` well-known format would emit `Z` and drift the wire).
    let occurred_at = audit_wire_occurred_at(occurred_at);
    let actor = match actor {
        Some(actor) => AuditActor {
            // actor_id::text spelling (UUID-hyphenated — the trigger's
            // `COALESCE(NEW.actor_id::text, 'system')`; `ActorId` Display is
            // ULID base32 and would drift the wire contract).
            id: actor.to_uuid().to_string(),
            kind: AUDIT_ACTOR_TYPE_PARTICIPANT.to_string(),
        },
        None => AuditActor {
            id: AUDIT_ACTOR_TYPE_SYSTEM.to_string(),
            kind: AUDIT_ACTOR_TYPE_SYSTEM.to_string(),
        },
    };
    let targets = match target {
        Some(target) => vec![AuditTarget {
            id: target.to_string(),
            kind: AUDIT_TARGET_TYPE_RESOURCE.to_string(),
        }],
        None => Vec::new(),
    };
    serde_json::json!({
        // event_id / idempotency_key spell the UUID-hyphenated text
        // (`to_uuid().to_string()`), mirroring the trigger's `NEW.id::text` —
        // `AuditId` Display formats ULID base32, which would drift the wire
        // contract vs moderation rows (pinned by the db_tests drift guard).
        "event_id": event_id.to_uuid().to_string(),
        "source_system": source_system,
        "event_type": AUDIT_EVENT_TYPE,
        "schema_id": AUDIT_SCHEMA_ID,
        "schema_version": AUDIT_SCHEMA_VERSION,
        "occurred_at": occurred_at,
        "actor": actor,
        "targets": targets,
        "aggregate_type": AUDIT_AGGREGATE_TYPE,
        "aggregate_id": workspace.to_uuid().to_string(),
        "action": outbound_action,
        "outcome": outcome,
        "payload": detail,
        "data_classification": AUDIT_DATA_CLASSIFICATION,
        "retention_class": AUDIT_RETENTION_CLASS,
        "idempotency_key": event_id.to_uuid().to_string(),
    })
}

/// Shared fail-open classifier (D4/D9): `sqlx::Error::Database(_)` → fail-open
/// (SAVEPOINT rollback + DLQ + classified log/count); everything else
/// (IO / `Protocol` / `PoolTimedOut` / connection-level) → propagate (in-tx) /
/// warn + `None` (standalone). Both write paths share it — it is the match
/// guard on the pair writer's failure branch (the single shared branch
/// boundary), so a future error class can never silently fall into the
/// wrong side.
///
/// G1 unit test pins the propagation branch; `PgDatabaseError` is
/// `pub(crate)` in sqlx-postgres, so `Database(_)` cannot be constructed in
/// tests — the Database side is pinned behaviorally by the live-PG `db_tests`
/// (`auth_pair_enforcement_on_without_nil_binding_fails_open_drops_pair`).
#[must_use]
pub fn is_fail_open_error(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(_))
}

/// Compile-time reference to leaf constants the `db_tests` recompute from
/// (G2-closure pattern — never inline literals in assertions).
#[allow(dead_code)]
const _L1_WINDOW: i64 = L1_WINDOW_SECONDS;

#[cfg(test)]
mod tests {
    use super::*;

    /// G1: the classifier's propagation branch — non-Database errors must
    /// never be treated as fail-open. (The Database branch is unpinnable in
    /// unit tests: `PgDatabaseError` is `pub(crate)` in sqlx-postgres 0.8.6;
    /// it is pinned behaviorally by the live-PG F1 regression pair.)
    #[test]
    fn audit_pair_error_classification_io_protocol_propagate() {
        assert!(!is_fail_open_error(&sqlx::Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "conn reset"
        ))));
        assert!(!is_fail_open_error(&sqlx::Error::Protocol(
            "connection gone".into()
        )));
        assert!(!is_fail_open_error(&sqlx::Error::PoolTimedOut));
    }
}
