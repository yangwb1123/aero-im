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

    /// Replay one DLQ row: rebuild the pair with the stored original input in
    /// a fresh transaction (NEW `AuditId` — the original pair never committed,
    /// so there is no `idempotency` `conflict`). On success stamp `replayed_at`.
    /// A re-failed replay re-enqueues a new DLQ row (at-least-once,
    /// acceptable) and leaves the original row for the next ops loop.
    ///
    /// Returns `Some(audit_id)` when the pair was rebuilt, `None` when the
    /// row does not exist or was already replayed.
    ///
    /// # Errors
    /// Propagates connection-level [`sqlx::Error`] only (Database-class
    /// failures fail open inside the pair writer and return `Ok(None)`).
    pub async fn replay(&self, id: i64) -> Result<Option<AuditId>, sqlx::Error> {
        let row: Option<ReplayCandidate> = sqlx::query_as(
                r"SELECT workspace_id, actor_id, action, target, detail, outbound_action
                   FROM audit_governance_failed_pairs
                  WHERE id = $1 AND status = 'pending' AND replayed_at IS NULL",
            )
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        let Some((workspace, actor, action, target, detail, outbound_action)) = row else {
            return Ok(None);
        };
        let mut tx = self.pool.begin().await?;
        let pair = crate::audit_governance::outbox::AuditGovernanceOutboxRepo::append_pair_in_tx_fail_open(
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
            sqlx::query(
                "UPDATE audit_governance_failed_pairs
                    SET replayed_at = now(),
                        replay_attempts = replay_attempts + 1
                  WHERE id = $1",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(Some(audit_id))
        } else {
            // Re-failed (Database class): the new DLQ row committed with this
            // tx; the original row stays for the next ops loop — but bounded:
            // MAX_ATTEMPTS reached marks it DEAD so `replay_all` cannot keep
            // doubling the table under a persistent bug (F-4). The new DLQ
            // row is inside this tx — commit keeps it.
            sqlx::query(
                "UPDATE audit_governance_failed_pairs
                    SET replay_attempts = replay_attempts + 1,
                        status = CASE WHEN replay_attempts + 1 >= $2 THEN 'dead' ELSE status END
                  WHERE id = $1",
            )
            .bind(id)
            .bind(MAX_REPLAY_ATTEMPTS)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(None)
        }
    }

    /// Ops loop: replay up to `limit` unreplayed rows. Returns the number of
    /// successfully replayed pairs.
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
