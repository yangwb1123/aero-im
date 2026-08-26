//! DLQ compensation for fail-open audit pairs (D9, migration 0244).
//!
//! When a pair write fails open (Database-class error → SAVEPOINT rollback),
//! the ORIGINAL INPUT is enqueued here in the same transaction so the event
//! is replayable, not lost. The table has **no triggers and no FKs**
//! (anti-recursion: writes happen outside the 0236/0239 trigger scope;
//! anti-blocking: a bad row can never block replay). The connector never
//! claims this table — the producer side only writes; replay is an ops loop
//! (no CLI/route consumer in this slice).

use aero_common::{AuditId, ParticipantId, WorkspaceId};
use sqlx::{PgPool, Postgres, Transaction};

/// Cheap to clone (shares the underlying `PgPool`).
#[derive(Clone)]
#[must_use]
pub struct FailedPairRepo {
    pool: PgPool,
}

/// (workspace, actor, action, target, detail, `outbound_action`) of one
/// F-4 replay cap: a row is DEAD after this many failed replay attempts
/// (a persistent bug cannot keep doubling the table; ops triages dead rows).
pub const MAX_REPLAY_ATTEMPTS: i64 = 5;

/// unreplayed DLQ row (factored out of [`replay`](Self::replay)'s query row).
type ReplayCandidate = (
    uuid::Uuid,
    Option<uuid::Uuid>,
    String,
    Option<String>,
    serde_json::Value,
    String,
);

impl FailedPairRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Same-tx enqueue (in-tx write path, called AFTER `ROLLBACK TO
    /// SAVEPOINT`). The DLQ row commits with the caller's transaction — the
    /// domain commits ⇒ the DLQ row is in; the domain rolls back ⇒ the DLQ
    /// row is gone (same fate as the dropped pair). Returns the row id.
    ///
    /// Signature is pinned verbatim by the landing design (D9) — 8 params +
    /// the tx handle.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] (the caller's nested-savepoint guard
    /// bounds the damage — F-6).
    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        workspace: WorkspaceId,
        actor: Option<ParticipantId>,
        action: &str,
        target: Option<&str>,
        detail: serde_json::Value,
        outbound_action: &str,
        error_sqlstate: &str,
        error_message: Option<&str>,
    ) -> Result<i64, sqlx::Error> {
        let id: i64 = sqlx::query_scalar(
            r"INSERT INTO audit_governance_failed_pairs
                   (workspace_id, actor_id, action, target, detail, outbound_action,
                    error_sqlstate, error_message)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             RETURNING id",
        )
        .bind(workspace.to_uuid())
        .bind(actor.map(|p| p.to_uuid()))
        .bind(action)
        .bind(target)
        .bind(detail)
        .bind(outbound_action)
        .bind(error_sqlstate)
        .bind(error_message)
        .fetch_one(&mut **tx)
        .await?;
        Ok(id)
    }

    /// Best-effort fresh-tx enqueue (standalone write path's connection-level
    /// branch; a fresh connection may still succeed after a transient error).
    /// A failure only warns — the caller already swallowed the pair loss.
    ///
    /// Signature is pinned verbatim by the landing design (D9).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] (the caller warns and moves on).
    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_standalone(
        pool: &PgPool,
        workspace: WorkspaceId,
        actor: Option<ParticipantId>,
        action: &str,
        target: Option<&str>,
        detail: serde_json::Value,
        outbound_action: &str,
        error_sqlstate: &str,
        error_message: Option<&str>,
    ) -> Result<i64, sqlx::Error> {
        let mut tx = pool.begin().await?;
        let id = Self::enqueue_in_tx(
            &mut tx,
            workspace,
            actor,
            action,
            target,
            detail,
            outbound_action,
            error_sqlstate,
            error_message,
        )
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// DLQ depth (gauge sampler + tests).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn count(&self) -> Result<i64, sqlx::Error> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_governance_failed_pairs")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    /// Data-lifecycle retention sweep (design §9 F-4, third leg): hard-delete
    /// TERMINAL DLQ rows older than `cutoff` — `status = 'dead'` (replay
    /// attempts exhausted; the compensation is unreachable) or
    /// `replayed_at IS NOT NULL` (compensation already delivered; the row is
    /// only a delivery receipt). Never-replayed `pending` rows are the alert
    /// surface (`aero_audit_governance_failed_pairs` gauge) and are NEVER
    /// swept. Returns the number of rows deleted.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn sweep_terminal_before(
        &self,
        cutoff: time::OffsetDateTime,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(
            r"DELETE FROM audit_governance_failed_pairs
               WHERE (status = 'dead' OR replayed_at IS NOT NULL)
                 AND created_at < $1",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Replay one DLQ row: rebuild the pair with the stored original input in
    /// a fresh transaction (NEW `AuditId` — the original pair never committed,
    /// so there is no `idempotency` `conflict`). On success stamp `replayed_at`
    /// and increment `replay_attempts` once.
    ///
    /// **Q2 claim fence (async-reviewer)**: the claim SELECT runs INSIDE the
    /// replay tx, so the `FOR UPDATE` row lock spans the pair rebuild + status
    /// update — a concurrent replay of the same row SKIPs (`SKIP LOCKED`) or
    /// blocks and re-checks `replayed_at` after the winner commits → `Ok(None)`.
    /// The terminal UPDATE additionally re-checks the claim predicate and
    /// asserts `rows_affected() == 1` (belt-and-braces: a zero-row UPDATE
    /// rolls back our pair write — no duplicate pair can ever commit).
    ///
    /// A re-failed replay does NOT clone the row — it increments the ORIGINAL
    /// row's `replay_attempts` (async-reviewer no-clone fix); at
    /// [`MAX_REPLAY_ATTEMPTS`] the row is marked `dead` so `replay_all` cannot
    /// keep hammering a persistent bug (F-4).
    ///
    /// Returns `Some(audit_id)` when the pair was rebuilt, `None` when the
    /// row does not exist, was already replayed, or is locked by a concurrent
    /// replay (which will finish it).
    ///
    /// # Errors
    /// Propagates connection-level [`sqlx::Error`] only (Database-class
    /// failures fail open inside the pair writer and return `Ok(None)`).
    pub async fn replay(&self, id: i64) -> Result<Option<AuditId>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let row: Option<ReplayCandidate> = sqlx::query_as(
            r"SELECT workspace_id, actor_id, action, target, detail, outbound_action
                   FROM audit_governance_failed_pairs
                  WHERE id = $1 AND status = 'pending' AND replayed_at IS NULL
                  FOR UPDATE SKIP LOCKED",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((workspace, actor, action, target, detail, outbound_action)) = row else {
            // Not claimable (absent, already replayed, or locked by a
            // concurrent replay that will finish it) — commit the empty tx
            // (harmless) and report None.
            tx.commit().await?;
            return Ok(None);
        };
        // No-DLQ variant (async-reviewer fix): a Database-class re-failure
        // returns Ok(None) WITHOUT enqueueing a fresh clone — the original
        // row's attempts increment below and the population cannot double.
        let pair = crate::audit_governance::outbox::AuditGovernanceOutboxRepo::append_pair_in_tx_fail_open_no_dlq(
            &mut tx,
            WorkspaceId::from_uuid(workspace),
            actor.map(ParticipantId::from_uuid),
            &action,
            target.as_deref(),
            detail,
            &outbound_action,
        )
        .await?;
        if let Some(audit_id) = pair {
            let updated = sqlx::query(
                "UPDATE audit_governance_failed_pairs
                    SET replayed_at = now(),
                        replay_attempts = replay_attempts + 1
                  WHERE id = $1 AND status = 'pending' AND replayed_at IS NULL",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() != 1 {
                // Belt-and-braces (Q2): the row was replayed/claimed by a
                // concurrent run between our claim and this UPDATE — drop our
                // pair (rollback) and report None; the winner's rebuild stands.
                tx.rollback().await?;
                return Ok(None);
            }
            tx.commit().await?;
            Ok(Some(audit_id))
        } else {
            // Re-failed (Database class, no clone — no_dlq writer): increment
            // the ORIGINAL row's attempts; MAX_ATTEMPTS reached marks it DEAD
            // so `replay_all` cannot keep hammering a persistent bug (F-4).
            // Guarded like the success arm (Q2): a zero-row UPDATE means a
            // concurrent run already claimed it — drop our tx and report None.
            let updated = sqlx::query(
                "UPDATE audit_governance_failed_pairs
                    SET replay_attempts = replay_attempts + 1,
                        status = CASE WHEN replay_attempts + 1 >= $2 THEN 'dead' ELSE status END
                  WHERE id = $1 AND status = 'pending' AND replayed_at IS NULL",
            )
            .bind(id)
            .bind(MAX_REPLAY_ATTEMPTS)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() != 1 {
                tx.rollback().await?;
                return Ok(None);
            }
            tx.commit().await?;
            Ok(None)
        }
    }

    /// Ops loop: replay up to `limit` unreplayed rows. Returns the number of
    /// successfully replayed pairs.
    ///
    /// The batch SELECT is only a candidate-id pre-filter (plain SELECT — no
    /// `FOR UPDATE`: the per-row claim inside [`Self::replay`] is the Q2
    /// fence; an autocommit lock here would release at statement end anyway).
    /// A row locked by a concurrent run is simply retried by the next loop.
    ///
    /// # Errors
    /// Propagates connection-level [`sqlx::Error`] only.
    pub async fn replay_all(&self, limit: i64) -> Result<usize, sqlx::Error> {
        let ids: Vec<i64> = sqlx::query_scalar(
            r"SELECT id FROM audit_governance_failed_pairs
               WHERE status = 'pending' AND replayed_at IS NULL
               ORDER BY id
               LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut replayed = 0usize;
        for id in ids {
            if self.replay(id).await?.is_some() {
                replayed += 1;
            }
        }
        Ok(replayed)
    }
}
