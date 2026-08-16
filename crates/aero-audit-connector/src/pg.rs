//! `PostgreSQL` implementation of [`OutboxRepo`] over B5-1's governance outbox.
//!
//! The SQL statements are cloned from the proven `AiUsageRepo`
//! (`crates/aero-storage/src/ai_usage.rs`: claim CTE with `SKIP LOCKED`,
//! one-transaction fenced settle, fenced re-park with capped backoff) with
//! the dead terminal added and **one clock-domain correction**: `ai_usage`
//! mints `lease_expires_at` and `available_at` from an app-supplied `now`
//! while fences read `clock_timestamp()`; an app clock trailing the DB clock
//! by >= lease livelocks the row (claim→POST→fence-fail forever). Here every
//! filter, lease mint, and backoff arithmetic runs on `clock_timestamp()` —
//! a single clock domain — and the trait takes no caller `now` at all. The
//! table itself (`audit_governance_outbox`, status 0/1/2/3, `event_id` 1:1
//! with `audit_events.id`) is owned by B5-1's 0239 migration — this crate
//! owns only these statements and never writes the v1
//! `snaplink_delivery_outbox` table. Until 0239 lands, booting the relay
//! degrades to logged claim errors (F13), not a crash.

use async_trait::async_trait;
use serde_json::Value;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use aero_common::{AuditId, OutboxStatus};

use crate::outbox::{Claim, Error, OutboxRepo, StatusBuckets, VerdictProbe};
use crate::relay::{audit_backoff, clamped_lease, min_service_floor, truncate_error, MAX_CLAIM};

/// Governance outbox status enum — B5-1 0239 DDL normative values, derived
/// from the leaf vocabulary (`aero_common::model::audit::OutboxStatus`). The
/// constant names are kept (0239's comment pins "connector status machine
/// (pg.rs:27-30)" and external references may name them); the values are
/// single-sourced at the leaf.
pub const STATUS_ENQUEUED: i32 = OutboxStatus::Enqueued.as_i32();
pub const STATUS_CLAIMED: i32 = OutboxStatus::Claimed.as_i32();
pub const STATUS_DELIVERED: i32 = OutboxStatus::Delivered.as_i32();
pub const STATUS_DEAD: i32 = OutboxStatus::Dead.as_i32();

/// PG-backed outbox over `audit_governance_outbox`.
#[derive(Debug, Clone)]
pub struct PgOutboxRepo {
    pool: PgPool,
}

impl PgOutboxRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(Debug, sqlx::FromRow)]
struct ClaimRow {
    event_id: Uuid,
    attempts: i64,
    claim_token: Uuid,
    lease_expires_at: OffsetDateTime,
    payload: Value,
    // B5-3 R1: 0239 columns — priority SMALLINT, class TEXT. The RETURNING
    // below must stay in sync with this struct (decode is by name).
    priority: i16,
    class: String,
}

impl From<ClaimRow> for Claim {
    fn from(row: ClaimRow) -> Self {
        Self {
            event_id: AuditId::from_uuid(row.event_id),
            attempts: row.attempts,
            claim_token: row.claim_token,
            lease_expires_at: row.lease_expires_at,
            payload: row.payload,
            priority: row.priority,
            class: row.class,
        }
    }
}

#[async_trait]
impl OutboxRepo for PgOutboxRepo {
    async fn reconcile(&self, limit: i64) -> Result<i64, Error> {
        // Mirror of v1 `SnaplinkCommercialRepo::reconcile_audit`
        // (aero-storage/src/snaplink_commercial.rs): the relay runs this
        // ahead of every claim batch, so a moderation row accepted while the
        // commercial runtime switch was disabled self-heals on the first
        // tick after re-enablement (0241). The function is token-keyed
        // (only `message.moderated`), runtime-gate-free (the recovery path
        // exists BECAUSE the gate skipped enqueue), and idempotent
        // (`NOT EXISTS` + `ON CONFLICT (event_id) DO NOTHING`).
        let inserted =
            sqlx::query_scalar("SELECT aero_reconcile_governance_audit($1::integer)::bigint")
                .bind(limit.clamp(1, MAX_CLAIM))
                .fetch_one(&self.pool)
                .await?;
        Ok(inserted)
    }

    async fn claim_due(&self, lease: Duration, limit: i64) -> Result<Vec<Claim>, Error> {
        // Single clock domain: availability, lease expiry, and the lease mint
        // all read `clock_timestamp()` (DB wall clock). Binding an app-side
        // `now` here would let |app−DB skew| >= lease mint an already-expired
        // lease whose fence always fails while the claim filter never
        // re-exposes the row — the claim→POST→fence-fail livelock.
        //
        // B5-3 D-CAP (anti-starvation cap): the claimed set is two arms in
        // one statement. Arm A claims the top `limit − K` rows of the total
        // order `(priority DESC, available_at, created_at, event_id)`; arm B
        // claims the `K` earliest-due rows of the lowest-priority lane
        // (`priority = MIN(priority)`, today 10) NOT already selected by arm
        // A — the `NOT EXISTS` exclusion is load-bearing (`SKIP LOCKED` does
        // not dedupe arms within one statement); `MATERIALIZED` pins arm A's
        // single evaluation. Flood → low lane served ≥ K rows/round; high
        // lane underfull → set identical to today's uncapped top-`limit`.
        // Snapshot-relative: under concurrent claimers one statement's arm B
        // can be diluted, but the aggregate floor holds by conservation.
        let lease_secs = clamped_lease(lease).whole_seconds();
        let limit = limit.clamp(1, MAX_CLAIM);
        let floor = min_service_floor(limit);
        let rows = sqlx::query_as::<_, ClaimRow>(
            r"WITH arm_a AS MATERIALIZED (
                  SELECT candidate.event_id
                    FROM audit_governance_outbox AS candidate
                   WHERE candidate.status IN (0, 1)
                     AND candidate.available_at <= clock_timestamp()
                     AND (
                           candidate.lease_expires_at IS NULL
                           OR candidate.lease_expires_at <= clock_timestamp()
                     )
                   ORDER BY candidate.priority DESC, candidate.available_at,
                            candidate.created_at, candidate.event_id
                   FOR UPDATE SKIP LOCKED
                   LIMIT $1
              ),
              arm_b AS (
                  SELECT candidate.event_id
                    FROM audit_governance_outbox AS candidate
                   WHERE candidate.status IN (0, 1)
                     AND candidate.available_at <= clock_timestamp()
                     AND (
                           candidate.lease_expires_at IS NULL
                           OR candidate.lease_expires_at <= clock_timestamp()
                     )
                     AND candidate.priority = (
                           SELECT MIN(priority)
                             FROM audit_governance_outbox
                            WHERE status IN (0, 1)
                              AND available_at <= clock_timestamp()
                              AND (
                                    lease_expires_at IS NULL
                                    OR lease_expires_at <= clock_timestamp()
                              )
                     )
                     AND NOT EXISTS (
                           SELECT 1 FROM arm_a
                            WHERE arm_a.event_id = candidate.event_id
                     )
                   ORDER BY candidate.available_at, candidate.created_at,
                            candidate.event_id
                   FOR UPDATE SKIP LOCKED
                   LIMIT $2
              ),
              claimable AS (
                  SELECT event_id FROM arm_a
                  UNION ALL
                  SELECT event_id FROM arm_b
              )
              UPDATE audit_governance_outbox AS outbox
                 SET status = 1,
                     claim_token = gen_random_uuid(),
                     lease_expires_at = clock_timestamp()
                                        + make_interval(secs => $3),
                     attempts = outbox.attempts + 1
                FROM claimable
               WHERE outbox.event_id = claimable.event_id
           RETURNING outbox.event_id, outbox.attempts, outbox.claim_token,
                     outbox.lease_expires_at, outbox.payload,
                     outbox.priority, outbox.class",
        )
        .bind(limit - floor) // $1: arm A = top (limit − K); K ≤ limit − 1 so this is ≥ 1
        .bind(floor)         // $2: arm B = K (0 is valid: LIMIT 0 → empty arm B)
        .bind(lease_secs)    // $3: lease minted on the DB clock
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn settle(&self, event_id: AuditId, claim_token: Uuid) -> Result<bool, Error> {
        // One transaction: a fenced re-read proves the token and unexpired
        // lease still match before the acknowledgement flips status to 2.
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query_scalar::<_, Uuid>(
            r"SELECT event_id
                FROM audit_governance_outbox
               WHERE event_id = $1
                 AND claim_token = $2
                 AND status IN (0, 1)
                 AND lease_expires_at > clock_timestamp()
               FOR UPDATE",
        )
        .bind(event_id.to_uuid())
        .bind(claim_token)
        .fetch_optional(&mut *tx)
        .await?;
        if row.is_none() {
            tx.rollback().await?;
            return Ok(false);
        }
        let acknowledged = sqlx::query(
            r"UPDATE audit_governance_outbox
                  SET status = 2,
                      delivered_at = clock_timestamp(),
                      claim_token = NULL,
                      lease_expires_at = NULL,
                      last_error = NULL
                WHERE event_id = $1
                  AND claim_token = $2
                  AND status IN (0, 1)
                  AND lease_expires_at > clock_timestamp()",
        )
        .bind(event_id.to_uuid())
        .bind(claim_token)
        .execute(&mut *tx)
        .await?;
        if acknowledged.rows_affected() != 1 {
            tx.rollback().await?;
            return Ok(false);
        }
        tx.commit().await?;
        Ok(true)
    }

    async fn requeue(
        &self,
        event_id: AuditId,
        claim_token: Uuid,
        attempts: i64,
        error: &str,
    ) -> Result<bool, Error> {
        // `available_at` is computed on the DB clock so the backoff re-park
        // shares the claim filter's clock domain (single clock domain).
        let backoff_secs = audit_backoff(attempts).whole_seconds();
        let result = sqlx::query(
            r"UPDATE audit_governance_outbox
                  SET status = 0,
                      available_at = clock_timestamp()
                                     + make_interval(secs => $4),
                      claim_token = NULL,
                      lease_expires_at = NULL,
                      last_error = $5
                WHERE event_id = $1
                  AND claim_token = $2
                  AND attempts = $3
                  AND status IN (0, 1)
                  AND lease_expires_at > clock_timestamp()",
        )
        .bind(event_id.to_uuid())
        .bind(claim_token)
        .bind(attempts)
        .bind(backoff_secs)
        .bind(truncate_error(error))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn mark_dead(
        &self,
        event_id: AuditId,
        claim_token: Uuid,
        attempts: i64,
        error: &str,
    ) -> Result<bool, Error> {
        let result = sqlx::query(
            r"UPDATE audit_governance_outbox
                  SET status = 3,
                      claim_token = NULL,
                      lease_expires_at = NULL,
                      last_error = $4
                WHERE event_id = $1
                  AND claim_token = $2
                  AND attempts = $3
                  AND status IN (0, 1)
                  AND lease_expires_at > clock_timestamp()",
        )
        .bind(event_id.to_uuid())
        .bind(claim_token)
        .bind(attempts)
        .bind(truncate_error(error))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn verdict_probe(&self) -> Result<VerdictProbe, Error> {
        // QP1: status IN (0,1) counts, served by the due partial indexes
        // (`audit_governance_due_idx` / `audit_governance_due_prio_idx` —
        // both `WHERE status IN (0,1)`) — cost ∝ due set, not table size.
        let rows = sqlx::query_as::<_, (i32, i64)>(
            "SELECT status, count(*)::bigint FROM audit_governance_outbox
              WHERE status IN (0, 1) GROUP BY status ORDER BY status",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut probe = VerdictProbe::default();
        for (status, count) in rows {
            match status {
                STATUS_ENQUEUED => probe.enqueued = count,
                STATUS_CLAIMED => probe.claimed = count,
                _ => {}
            }
        }
        // QP2: EXISTS — the healthy-state "no dead rows" case is an O(1)
        // index lookup via the 0248 `audit_governance_status3_idx` partial
        // index (`WHERE status = 3`); a seq scan would cost the whole table.
        probe.has_dead = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM audit_governance_outbox WHERE status = 3)",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(probe)
    }

    async fn status_buckets(&self) -> Result<StatusBuckets, Error> {
        // Exact text mirror of `aero_eng::audit_provision::Q3_SQL`
        // (`GROUP BY status ORDER BY status`) for CLI/sampler oracle parity.
        let rows = sqlx::query_as::<_, (i32, i64)>(
            "SELECT status, count(*)::bigint FROM audit_governance_outbox
              GROUP BY status ORDER BY status",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut buckets = StatusBuckets::default();
        for (status, count) in rows {
            match status {
                STATUS_ENQUEUED => buckets.enqueued = count,
                STATUS_CLAIMED => buckets.claimed = count,
                STATUS_DELIVERED => buckets.delivered = count,
                STATUS_DEAD => buckets.dead = count,
                _ => {}
            }
        }
        Ok(buckets)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::Barrier;

    use super::*;

    /// Due rows seeded per concurrent-claim run.
    const ROWS: i64 = 50;
    /// Per-session claim limit: below [`ROWS`], so both sessions must win rows.
    const LIMIT: i64 = 25;

    fn pool(url: &str) -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(url)
            .expect("well-formed DATABASE_URL")
    }

    /// Ensure `audit_governance_outbox` exists. Reuses B5-1's 0239 table when
    /// it is migrated; otherwise creates the minimal equivalent shape (the
    /// repo SQL touches exactly these columns — `priority` is included per
    /// B5-3 R2: the claim ORDER BY's leading term). Throwaway-DB test only —
    /// the table is never dropped here.
    async fn ensure_outbox_table(pool: &PgPool) {
        let table: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('audit_governance_outbox')::text")
                .fetch_one(pool)
                .await
                .expect("probe for the governance outbox");
        if table.is_some() {
            return;
        }
        sqlx::query(
            r"CREATE TABLE audit_governance_outbox (
                 event_id uuid PRIMARY KEY,
                 payload jsonb NOT NULL,
                 status integer NOT NULL DEFAULT 0,
                 priority smallint NOT NULL DEFAULT 10, -- B5-3 R2: claim ORDER BY leading term (0239: DEFAULT 10 = GOVERNANCE_PRIORITY_BACKLOG)
                 class TEXT NOT NULL DEFAULT 'message', -- B5-3 R2: 0239: DEFAULT 'message' = GOVERNANCE_CLASS_MESSAGE (crates/aero-ai/src/governance.rs:39)
                 available_at timestamptz NOT NULL DEFAULT clock_timestamp(),
                 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
                 attempts bigint NOT NULL DEFAULT 0,
                 claim_token uuid,
                 lease_expires_at timestamptz,
                 delivered_at timestamptz,
                 last_error text
             )",
        )
        .execute(pool)
        .await
        .expect("create the minimal 0239-equivalent outbox");
    }

    /// A3 PG — true concurrent double-claim across two independent sessions.
    ///
    /// 50 due rows, per-session `LIMIT 25`: two sessions run the claim CTE
    /// simultaneously (barrier + multi-thread runtime, separate pools).
    /// `FOR UPDATE SKIP LOCKED` must hand each session its own disjoint 25
    /// with `attempts == 1` everywhere — zero double-claims, regardless of
    /// interleaving (a fully serialized run still yields 25/25 because
    /// `LIMIT < ROWS` forces both sessions to claim).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires live Postgres (DATABASE_URL)"]
    async fn concurrent_double_claim_across_two_sessions_is_impossible() {
        let url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must point at a throwaway Postgres");
        let pool_a = pool(&url);
        let pool_b = pool(&url);
        ensure_outbox_table(&pool_a).await;
        // Self-isolating start (db-reviewer finding 3, drill precedent):
        // shared-DB runs (re-runs after a failed test, or the harness main
        // DB) must never corrupt the count/parity assertions.
        sqlx::query("TRUNCATE audit_governance_outbox")
            .execute(&pool_a)
            .await
            .expect("reset the governance outbox");

        let mut seeded = Vec::new();
        for _ in 0..ROWS {
            let event_id = Uuid::new_v4();
            sqlx::query(
                r"INSERT INTO audit_governance_outbox
                        (event_id, payload, available_at, attempts, status)
                  VALUES ($1, $2, clock_timestamp(), 0, 0)",
            )
            .bind(event_id)
            .bind(json!({
                "event_id": event_id.to_string(),
                "source_system": "aero-im.source",
            }))
            .execute(&pool_a)
            .await
            .expect("seed governance row");
            seeded.push(event_id);
        }

        let session_a = PgOutboxRepo::new(pool_a.clone());
        let session_b = PgOutboxRepo::new(pool_b.clone());
        let barrier = Arc::new(Barrier::new(2));
        let claimed_a = {
            let barrier = Arc::clone(&barrier);
            let session = session_a.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                session
                    .claim_due(Duration::seconds(30), LIMIT)
                    .await
                    .expect("session A claim")
            })
        };
        let claimed_b = {
            let barrier = Arc::clone(&barrier);
            let session = session_b.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                session
                    .claim_due(Duration::seconds(30), LIMIT)
                    .await
                    .expect("session B claim")
            })
        };
        let (claimed_a, claimed_b) = tokio::join!(claimed_a, claimed_b);
        let claimed_a = claimed_a.expect("session A task");
        let claimed_b = claimed_b.expect("session B task");

        assert_eq!(claimed_a.len(), 25, "session A must claim its own 25 rows");
        assert_eq!(
            claimed_b.len(),
            25,
            "session B must claim the other 25 rows"
        );
        let ids_a: HashSet<AuditId> = claimed_a.iter().map(|claim| claim.event_id).collect();
        let ids_b: HashSet<AuditId> = claimed_b.iter().map(|claim| claim.event_id).collect();
        assert_eq!(ids_a.len(), 25);
        assert_eq!(ids_b.len(), 25);
        assert!(
            ids_a.is_disjoint(&ids_b),
            "zero double-claims: no row claimed by both sessions"
        );
        let seeded_set: HashSet<AuditId> =
            seeded.iter().map(|id| AuditId::from_uuid(*id)).collect();
        let union: HashSet<AuditId> = ids_a.union(&ids_b).copied().collect();
        assert_eq!(union, seeded_set, "every seeded row claimed exactly once");

        for claim in claimed_a.iter().chain(&claimed_b) {
            assert_eq!(claim.attempts, 1, "first claim must record attempts == 1");
        }
        let tokens: HashSet<Uuid> = claimed_a
            .iter()
            .chain(&claimed_b)
            .map(|claim| claim.claim_token)
            .collect();
        assert_eq!(
            tokens.len(),
            50,
            "every claim rotates a distinct fencing token"
        );

        // DB-level confirmation: 50 rows claimed exactly once, none twice.
        let claimed_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM audit_governance_outbox WHERE status = 1",
        )
        .fetch_one(&pool_a)
        .await
        .expect("count claimed rows");
        assert_eq!(claimed_rows, ROWS);
        let double_claimed: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM audit_governance_outbox WHERE attempts > 1",
        )
        .fetch_one(&pool_a)
        .await
        .expect("count double-claimed rows");
        assert_eq!(
            double_claimed, 0,
            "zero double-claims: attempts stays 1 everywhere"
        );

        for event_id in &seeded {
            sqlx::query("DELETE FROM audit_governance_outbox WHERE event_id = $1")
                .bind(event_id)
                .execute(&pool_a)
                .await
                .expect("clean up seeded row");
        }
    }

    /// F10 (deployment rec 4) — lease-expiry reclaim: a row claimed with a
    /// lease that expires is re-exposed by `claim_due` and reclaimed with a
    /// FRESH fencing token and attempts incremented. Closes the gap the
    /// double-claim test leaves open (it never lets a lease expire): this is
    /// the crash-after-claim recovery path (claim → crash → lease expiry →
    /// reclaim → redeliver with the same `Idempotency-Key = event_id`, so
    /// the sink dedups the replay — at-least-once, never loss).
    #[tokio::test]
    #[ignore = "requires live Postgres (DATABASE_URL)"]
    async fn expired_lease_is_reclaimed_with_fresh_token() {
        let url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must point at a throwaway Postgres");
        let pool = pool(&url);
        ensure_outbox_table(&pool).await;
        // Self-isolating start (db-reviewer finding 3, drill precedent).
        sqlx::query("TRUNCATE audit_governance_outbox")
            .execute(&pool)
            .await
            .expect("reset the governance outbox");

        let event_id = Uuid::new_v4();
        sqlx::query(
            r"INSERT INTO audit_governance_outbox
                    (event_id, payload, available_at, attempts, status)
              VALUES ($1, $2, clock_timestamp(), 0, 0)",
        )
        .bind(event_id)
        .bind(json!({
            "event_id": event_id.to_string(),
            "source_system": "aero-im.source",
        }))
        .execute(&pool)
        .await
        .expect("seed governance row");

        let repo = PgOutboxRepo::new(pool.clone());

        // Claim with the 1s lease floor (clamped_lease) — status flips to 1,
        // token A minted, attempts 1.
        let first = repo
            .claim_due(Duration::seconds(1), 10)
            .await
            .expect("first claim");
        assert_eq!(first.len(), 1, "seed row must be claimable");
        assert_eq!(first[0].event_id, AuditId::from_uuid(event_id));
        assert_eq!(first[0].attempts, 1);
        let first_token = first[0].claim_token;

        // While the lease is live the fence holds: the row is NOT re-claimable
        // (fresh claim_due against the same repo — the lease_expires_at
        // filter excludes it, not just SKIP LOCKED).
        let during = repo
            .claim_due(Duration::seconds(30), 10)
            .await
            .expect("claim while the lease is live");
        assert!(during.is_empty(), "live lease must fence the claimed row");

        // Sleep past the 1s lease (same-machine PG clock; the t11 drill uses
        // the same 1.2s margin over its 1s backoff).
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

        // Expired lease → the row re-enters the due set: reclaimed with a
        // FRESH token and attempts incremented (the claim budget counts
        // claims, not POSTs — a genuine 422 on the first real POST after a
        // reclaimed crash window goes straight to dead, same terminal as two
        // real attempts, since 422 is deterministic per payload).
        let second = repo
            .claim_due(Duration::seconds(30), 10)
            .await
            .expect("reclaim after lease expiry");
        assert_eq!(second.len(), 1, "expired lease must re-expose the row");
        assert_eq!(
            second[0].event_id,
            AuditId::from_uuid(event_id),
            "same row reclaimed"
        );
        assert_eq!(
            second[0].attempts, 2,
            "reclaim increments attempts (claim budget, not POST count)"
        );
        assert_ne!(
            second[0].claim_token, first_token,
            "reclaim rotates a fresh fencing token (the old token is dead)"
        );

        sqlx::query("DELETE FROM audit_governance_outbox WHERE event_id = $1")
            .bind(event_id)
            .execute(&pool)
            .await
            .expect("clean up seeded row");
    }

    /// B5-3 R3 — mixed-priority claim: `priority DESC` preempts FIFO, FIFO
    /// within a lane. Set-based contract (decision D3): `UPDATE … FROM
    /// (CTE ORDER BY …) … RETURNING` emits target-table heap order, not CTE
    /// order, so the claimed SET is the contract of an ORDER BY+LIMIT claim —
    /// never Vec position.
    ///
    /// 40 backlog rows (priority 10 = `GOVERNANCE_PRIORITY_BACKLOG`) seeded
    /// with EARLIER `available_at` and deliberately INVERTED `created_at`
    /// (earlier seeds carry later `created_at` — a swapped ORDER BY term is
    /// detectable); 10 admin rows (priority 100 =
    /// `GOVERNANCE_PRIORITY_MODERATION`) seeded with LATER `available_at` —
    /// FIFO can never explain their preemption. Claim `limit 25 < 50`; the
    /// claimed set must equal {all 10 admin} ∪ {the 15 backlog rows with the
    /// earliest `(available_at, created_at, event_id)`}, plan-independently.
    #[tokio::test]
    #[ignore = "requires live Postgres (DATABASE_URL)"]
    async fn mixed_priority_claim_orders_moderation_first_then_fifo() {
        let url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must point at a throwaway Postgres");
        let pool = pool(&url);
        ensure_outbox_table(&pool).await;
        // Self-isolating start (db-reviewer finding 3, drill precedent).
        sqlx::query("TRUNCATE audit_governance_outbox")
            .execute(&pool)
            .await
            .expect("reset the governance outbox");

        // t0 = now − 200s keeps every seeded available_at in the past (due).
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
            .bind(json!({
                "event_id": event_id.to_string(),
                "source_system": "aero-im.source",
            }))
            .bind(i64::from(i)) // available_at: t0 + i seconds (strictly increasing)
            .bind(i64::from(40 - i)) // created_at: t0 + (40 − i) seconds (inverted)
            .execute(&pool)
            .await
            .expect("seed backlog row");
            backlog.push(event_id);
        }
        let mut admin: Vec<Uuid> = Vec::new();
        for j in 0..10 {
            let event_id = Uuid::new_v4();
            sqlx::query(
                r"INSERT INTO audit_governance_outbox
                        (event_id, payload, status, attempts, priority,
                         available_at, created_at)
                  VALUES ($1, $2, 0, 0, 100,
                          clock_timestamp() - make_interval(secs => 200)
                                         + make_interval(secs => $3),
                          clock_timestamp() - make_interval(secs => 200))",
            )
            .bind(event_id)
            .bind(json!({
                "event_id": event_id.to_string(),
                "source_system": "aero-im.source",
            }))
            .bind(i64::from(100 + j)) // available_at: t0 + 100..110s — later than every backlog row
            .execute(&pool)
            .await
            .expect("seed admin row");
            admin.push(event_id);
        }

        let repo = PgOutboxRepo::new(pool.clone());
        let claimed = repo
            .claim_due(Duration::seconds(30), 25)
            .await
            .expect("claim due rows");
        assert_eq!(
            claimed.len(),
            25,
            "limit 25 < 50 forces the claim to select, not drain"
        );
        let claimed_ids: HashSet<AuditId> = claimed.iter().map(|claim| claim.event_id).collect();
        let mut expected: HashSet<AuditId> =
            admin.iter().map(|id| AuditId::from_uuid(*id)).collect();
        expected.extend(backlog.iter().take(15).map(|id| AuditId::from_uuid(*id)));
        assert_eq!(
            claimed_ids, expected,
            "claimed set must be {{all 10 admin}} ∪ {{15 earliest backlog}} — \
             priority DESC preempts FIFO regardless of enqueue order"
        );

        for event_id in backlog.iter().chain(&admin) {
            sqlx::query("DELETE FROM audit_governance_outbox WHERE event_id = $1")
                .bind(event_id)
                .execute(&pool)
                .await
                .expect("clean up seeded row");
        }
    }

    /// Seed one due row at `available_at = t0 + offset` (t0 = now − 200s).
    async fn seed(pool: &PgPool, priority: i16, offset: i64) -> Uuid {
        let event_id = Uuid::new_v4();
        sqlx::query(
            r"INSERT INTO audit_governance_outbox (event_id, payload, status, attempts, priority, available_at, created_at)
              VALUES ($1, $2, 0, 0, $3, clock_timestamp() - make_interval(secs => 200) + make_interval(secs => $4),
                      clock_timestamp() - make_interval(secs => 200))",
        )
        .bind(event_id)
        .bind(json!({"event_id": event_id.to_string(), "source_system": "aero-im.source"}))
        .bind(priority)
        .bind(offset)
        .execute(pool)
        .await
        .expect("seed governance row");
        event_id
    }

    /// B5-3 AC3.1 — sustained mixed lanes at batch 100: each round reserves
    /// K = `min_service_floor(100)` = 5 low-lane slots (190 admin + 40 backlog
    /// — corrected seed; two claims: 95 admin / 5 backlog, disjoint).
    #[tokio::test]
    #[ignore = "requires live Postgres (DATABASE_URL)"]
    async fn sustained_mixed_lanes_reserve_min_service_floor_each_round() {
        let url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must point at a throwaway Postgres");
        let pool = pool(&url);
        ensure_outbox_table(&pool).await;
        sqlx::query("TRUNCATE audit_governance_outbox").execute(&pool).await.expect("reset");
        let mut backlog = Vec::new();
        for i in 0..40 { backlog.push(seed(&pool, 10, i).await); }
        let mut admin = Vec::new();
        for j in 0..190 { admin.push(seed(&pool, 100, j).await); }
        let repo = PgOutboxRepo::new(pool.clone());
        let round1 = repo.claim_due(Duration::seconds(30), 100).await.expect("round 1");
        let round2 = repo.claim_due(Duration::seconds(30), 100).await.expect("round 2");
        let split = |claims: &[Claim]| {
            (
                claims.iter().filter(|c| c.priority == 100).count(),
                claims.iter().filter(|c| c.priority == 10).count(),
            )
        };
        assert_eq!(split(&round1), (95, 5), "round 1 must split 95 admin / 5 backlog");
        assert_eq!(split(&round2), (95, 5), "round 2 must split 95 admin / 5 backlog");
        let set1: HashSet<AuditId> = round1.iter().map(|c| c.event_id).collect();
        let set2: HashSet<AuditId> = round2.iter().map(|c| c.event_id).collect();
        assert!(set1.is_disjoint(&set2), "round 2 disjoint from round 1 (no arm overlap)");
        let admin_ids: HashSet<AuditId> = admin.iter().map(|id| AuditId::from_uuid(*id)).collect();
        let backlog_ids: HashSet<AuditId> = backlog.iter().map(|id| AuditId::from_uuid(*id)).collect();
        let union: HashSet<AuditId> = set1.union(&set2).copied().collect();
        assert_eq!(union.intersection(&admin_ids).count(), 190, "all 190 admin claimed");
        assert_eq!(
            union.intersection(&backlog_ids).count(),
            10,
            "10 backlog rows claimed (5 per round)"
        );
    }

    /// QA F3 — mixed-lane arm-B contention: two sessions × limit 25 vs 60
    /// admin + 2 backlog; the 2-row low lane is served only by arm B — SKIP
    /// LOCKED keeps claims disjoint (attempts == 1) with 2 low rows in 50.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires live Postgres (DATABASE_URL)"]
    async fn mixed_lane_arm_b_contention_stays_disjoint() {
        let url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must point at a throwaway Postgres");
        let pool_a = pool(&url);
        let pool_b = pool(&url);
        ensure_outbox_table(&pool_a).await;
        sqlx::query("TRUNCATE audit_governance_outbox").execute(&pool_a).await.expect("reset");
        for _ in 0..60 { seed(&pool_a, 100, 0).await; }
        let mut backlog = Vec::new();
        for i in 0..2 { backlog.push(seed(&pool_a, 10, i).await); }
        let session_a = PgOutboxRepo::new(pool_a.clone());
        let session_b = PgOutboxRepo::new(pool_b.clone());
        let barrier = Arc::new(Barrier::new(2));
        let claim = |session: PgOutboxRepo, barrier: Arc<Barrier>| async move {
            barrier.wait().await;
            session.claim_due(Duration::seconds(30), LIMIT).await.expect("concurrent claim")
        };
        let claimed_a = tokio::spawn(claim(session_a.clone(), Arc::clone(&barrier)));
        let claimed_b = tokio::spawn(claim(session_b.clone(), Arc::clone(&barrier)));
        let (claimed_a, claimed_b) = tokio::join!(claimed_a, claimed_b);
        let (claimed_a, claimed_b) = (claimed_a.expect("A"), claimed_b.expect("B"));
        assert_eq!(claimed_a.len(), 25, "session A claims 25");
        assert_eq!(claimed_b.len(), 25, "session B claims 25");
        let ids_a: HashSet<AuditId> = claimed_a.iter().map(|c| c.event_id).collect();
        let ids_b: HashSet<AuditId> = claimed_b.iter().map(|c| c.event_id).collect();
        assert!(ids_a.is_disjoint(&ids_b), "zero double-claims");
        let union: HashSet<AuditId> = ids_a.union(&ids_b).copied().collect();
        let backlog_ids: HashSet<AuditId> = backlog.iter().map(|id| AuditId::from_uuid(*id)).collect();
        assert_eq!(union.intersection(&backlog_ids).count(), 2, "both low rows claimed once");
        assert!(claimed_a.iter().chain(&claimed_b).all(|c| c.attempts == 1), "attempts == 1");
    }

    /// QA F4 — clamp parity: `claim_due(0)` clamps to batch 1 (K = 0, arm
    /// B `LIMIT 0`), claiming exactly one row (fake parity pin).
    #[tokio::test]
    #[ignore = "requires live Postgres (DATABASE_URL)"]
    async fn claim_due_zero_limit_clamps_to_one() {
        let url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must point at a throwaway Postgres");
        let pool = pool(&url);
        ensure_outbox_table(&pool).await;
        sqlx::query("TRUNCATE audit_governance_outbox").execute(&pool).await.expect("reset");
        let event_id = seed(&pool, 10, 0).await;
        let repo = PgOutboxRepo::new(pool.clone());
        let claimed = repo.claim_due(Duration::seconds(30), 0).await.expect("zero-limit claim");
        assert_eq!(claimed.len(), 1, "limit 0 clamps to batch 1 (K = 0)");
        assert_eq!(claimed[0].event_id, AuditId::from_uuid(event_id));
    }

    /// R1.4 — read-only sampling data plane (B5-4): `verdict_probe` +
    /// `status_buckets` over a seeded {0×2, 1×1, 2×1, 3×1} set →
    /// `{2, 1, true}` / `{2, 1, 1, 1}`; an all-delivered set → `{0, 0,
    /// false}` / `{0, 0, N, 0}`. **Read-only pin**: a full-row snapshot of
    /// every mutable column is byte-identical before and after both probes —
    /// the sampler must never mutate state (FM8).
    #[tokio::test]
    #[ignore = "requires live Postgres (DATABASE_URL)"]
    async fn sampling_data_plane_reads_never_mutate() {
        let url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must point at a throwaway Postgres");
        let pool = pool(&url);
        ensure_outbox_table(&pool).await;
        sqlx::query("TRUNCATE audit_governance_outbox").execute(&pool).await.expect("reset");
        let repo = PgOutboxRepo::new(pool.clone());

        // {0×2, 1×1, 2×1, 3×1} — one row in every status bucket.
        for (n, status) in [(2, 0), (1, 1), (1, 2), (1, 3)] {
            for _ in 0..n {
                sqlx::query(
                    r"INSERT INTO audit_governance_outbox
                            (event_id, payload, status, attempts, priority,
                             available_at, created_at)
                      VALUES ($1, $2, $3, 0, 10,
                              clock_timestamp() - make_interval(secs => 200),
                              clock_timestamp() - make_interval(secs => 200))",
                )
                .bind(Uuid::new_v4())
                .bind(json!({"event_id": Uuid::new_v4().to_string(), "source_system": "aero-im.source"}))
                .bind(status)
                .execute(&pool)
                .await
                .expect("seed probe row");
            }
        }

        let before = row_snapshot(&pool).await;
        let probe = repo.verdict_probe().await.expect("verdict probe");
        assert_eq!(
            probe,
            VerdictProbe { enqueued: 2, claimed: 1, has_dead: true },
            "Tier-1 probe over statuses {{0×2, 1×1, 2×1, 3×1}}"
        );
        let buckets = repo.status_buckets().await.expect("status buckets");
        assert_eq!(
            buckets,
            StatusBuckets { enqueued: 2, claimed: 1, delivered: 1, dead: 1 },
            "Tier-2 buckets mirror Q3_SQL exactly"
        );
        let after = row_snapshot(&pool).await;
        assert_eq!(
            before, after,
            "both probes are pure reads: no mutable column may change (FM8)"
        );

        // All-delivered subset → zero pending/claimed/dead.
        sqlx::query("TRUNCATE audit_governance_outbox").execute(&pool).await.expect("reset");
        for _ in 0..3 {
            sqlx::query(
                r"INSERT INTO audit_governance_outbox
                        (event_id, payload, status, attempts)
                  VALUES ($1, $2, 2, 0)",
            )
            .bind(Uuid::new_v4())
            .bind(json!({"event_id": Uuid::new_v4().to_string(), "source_system": "aero-im.source"}))
            .execute(&pool)
            .await
            .expect("seed delivered row");
        }
        assert_eq!(
            repo.verdict_probe().await.expect("verdict probe"),
            VerdictProbe { enqueued: 0, claimed: 0, has_dead: false },
            "no pending/claimed/dead rows → zero probe"
        );
        assert_eq!(
            repo.status_buckets().await.expect("status buckets"),
            StatusBuckets { enqueued: 0, claimed: 0, delivered: 3, dead: 0 },
            "delivered rows land only in the delivered bucket"
        );
    }

    /// Full-row mutable-column snapshot (read-only pin oracle).
    async fn row_snapshot(pool: &PgPool) -> Vec<(Uuid, i32, i64, Option<Uuid>, Option<OffsetDateTime>, Option<String>)> {
        sqlx::query_as::<_, (Uuid, i32, i64, Option<Uuid>, Option<OffsetDateTime>, Option<String>)>(
            "SELECT event_id, status, attempts, claim_token, lease_expires_at, last_error
               FROM audit_governance_outbox ORDER BY event_id",
        )
        .fetch_all(pool)
        .await
        .expect("snapshot the governance outbox")
    }
}
