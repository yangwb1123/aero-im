//! Durable, per-tenant AI usage accounting.
//!
//! A paid call first creates a fenced, stable-id reservation with a conservative
//! estimate. Success finalizes its actual charge, ordinary failure cancels it,
//! and an expired ambiguous reservation is conservatively finalized. A leased
//! relay then inserts ready charges into `ai_usage_ledger` and acknowledges them
//! in one transaction.

use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

const MAX_CLAIM: i64 = 500;
const MAX_LEASE_SECONDS: i64 = 86_400;
const MAX_BACKOFF_SECONDS: i64 = 300;
const MAX_ERROR_CHARS: usize = 2_048;
const MAX_OUTCOME_BYTES: usize = 256 * 1_024;

/// Legacy/bulk ledger row. New paid calls use [`UsageCharge`] with
/// [`AiUsageRepo::reserve`] before contacting a provider.
#[derive(Debug, Clone)]
pub struct UsageRow {
    pub workspace_id: Option<Uuid>,
    pub kind: String,
    pub cost_micros: i64,
}

/// One paid provider operation and its conservative or finalized cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageCharge {
    pub usage_id: Uuid,
    pub workspace_id: Option<Uuid>,
    pub kind: String,
    pub cost_micros: i64,
    /// Versioned schema for a replayable provider result. `None` is reserved for
    /// legacy/bulk cost-only operations.
    pub outcome_kind: Option<String>,
}

/// Minimal provider result stored with a finalized charge so a stable retry can
/// resume the business write without making another paid call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageOutcome {
    pub kind: String,
    pub payload: serde_json::Value,
}

/// Provider-call fence returned only after a reservation is durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageReservation {
    pub usage_id: Uuid,
    pub reservation_token: Uuid,
}

/// Result of trying to reserve a stable logical provider operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageReserveOutcome {
    Reserved(UsageReservation),
    /// Another caller currently owns the same stable operation.
    InFlight,
    /// Its charge was already finalized or relayed. A successful provider
    /// operation carries the minimal replay result; ambiguous recovery and
    /// legacy cost-only rows carry `None`.
    Finalized(Option<UsageOutcome>),
    /// An expired ambiguous reservation was conservatively finalized now.
    Recovered,
}

/// Whether this caller finalized the reservation or observed an already-final
/// stable operation after an ambiguous database response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageFinalizeOutcome {
    Finalized,
    Duplicate,
}

/// Current generation of a leased outbox row.
#[derive(Debug, Clone)]
pub struct AiUsageOutboxClaim {
    pub usage_id: Uuid,
    pub workspace_id: Option<Uuid>,
    pub kind: String,
    pub cost_micros: i64,
    pub attempts: i32,
    pub claim_token: Uuid,
    pub lease_expires_at: OffsetDateTime,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, sqlx::FromRow)]
struct StoredCharge {
    workspace_id: Option<Uuid>,
    kind: String,
    cost_micros: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct StoredReservation {
    workspace_id: Option<Uuid>,
    kind: String,
    outcome_kind: Option<String>,
    outcome_json: Option<serde_json::Value>,
    status: String,
    reservation_token: Option<Uuid>,
    reservation_expires_at: Option<OffsetDateTime>,
}

#[derive(Debug, sqlx::FromRow)]
struct ClaimRow {
    usage_id: Uuid,
    workspace_id: Option<Uuid>,
    kind: String,
    cost_micros: i64,
    attempts: i32,
    claim_token: Uuid,
    lease_expires_at: OffsetDateTime,
    created_at: OffsetDateTime,
}

impl From<ClaimRow> for AiUsageOutboxClaim {
    fn from(row: ClaimRow) -> Self {
        Self {
            usage_id: row.usage_id,
            workspace_id: row.workspace_id,
            kind: row.kind,
            cost_micros: row.cost_micros,
            attempts: row.attempts,
            claim_token: row.claim_token,
            lease_expires_at: row.lease_expires_at,
            created_at: row.created_at,
        }
    }
}

/// A per-kind usage rollup for the summary endpoint.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UsageByKind {
    pub kind: String,
    pub calls: i64,
    pub cost_micros: i64,
}

#[derive(Clone)]
pub struct AiUsageRepo {
    pool: PgPool,
}

impl AiUsageRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Reserve a stable paid operation before any provider request is sent.
    ///
    /// A cancelled operation can be retried with a fresh fence. An active,
    /// finalized, or ambiguous operation never grants a second provider call.
    /// Expired ambiguity is converted to a ready estimated charge while holding
    /// the row lock. Reusing an id across workspace/kind domains is rejected.
    pub async fn reserve(
        &self,
        charge: &UsageCharge,
        now: OffsetDateTime,
        lease: Duration,
    ) -> Result<UsageReserveOutcome, sqlx::Error> {
        validate_charge(charge)?;
        let reservation_expires_at = now + clamped_lease(lease);
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query_scalar::<_, Uuid>(
            r"INSERT INTO ai_usage_outbox
                    (usage_id, workspace_id, kind, cost_micros, outcome_kind, status,
                     reservation_token, reservation_expires_at)
              VALUES ($1, $2, $3, $4, $5, 'reserved', gen_random_uuid(), $6)
              ON CONFLICT (usage_id) DO NOTHING
           RETURNING reservation_token",
        )
        .bind(charge.usage_id)
        .bind(charge.workspace_id)
        .bind(&charge.kind)
        .bind(charge.cost_micros)
        .bind(&charge.outcome_kind)
        .bind(reservation_expires_at)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(reservation_token) = inserted {
            tx.commit().await?;
            return Ok(UsageReserveOutcome::Reserved(UsageReservation {
                usage_id: charge.usage_id,
                reservation_token,
            }));
        }

        let stored = sqlx::query_as::<_, StoredReservation>(
            r"SELECT workspace_id, kind, outcome_kind, outcome_json, status,
                      reservation_token, reservation_expires_at
                FROM ai_usage_outbox
               WHERE usage_id = $1
               FOR UPDATE",
        )
        .bind(charge.usage_id)
        .fetch_one(&mut *tx)
        .await?;
        if stored.workspace_id != charge.workspace_id
            || stored.kind != charge.kind
            || stored.outcome_kind != charge.outcome_kind
        {
            return Err(sqlx::Error::Protocol(format!(
                "ai usage id {} was reused with a different payload",
                charge.usage_id
            )));
        }

        let outcome = match stored.status.as_str() {
            "cancelled" => {
                let reservation_token = Uuid::new_v4();
                sqlx::query(
                    r"UPDATE ai_usage_outbox
                          SET cost_micros = $2,
                              status = 'reserved',
                              reservation_token = $3,
                              reservation_expires_at = $4,
                              finalized_at = NULL,
                              cancelled_at = NULL,
                              completed_at = NULL,
                              available_at = $5,
                              claim_token = NULL,
                              lease_expires_at = NULL,
                              attempts = 0,
                              last_error = NULL,
                              outcome_json = NULL
                        WHERE usage_id = $1",
                )
                .bind(charge.usage_id)
                .bind(charge.cost_micros)
                .bind(reservation_token)
                .bind(reservation_expires_at)
                .bind(now)
                .execute(&mut *tx)
                .await?;
                UsageReserveOutcome::Reserved(UsageReservation {
                    usage_id: charge.usage_id,
                    reservation_token,
                })
            }
            "reserved" => {
                let expires_at = stored.reservation_expires_at.ok_or_else(|| {
                    sqlx::Error::Protocol(format!(
                        "reserved ai usage {} has no expiry",
                        charge.usage_id
                    ))
                })?;
                if expires_at > now {
                    UsageReserveOutcome::InFlight
                } else {
                    sqlx::query(
                        r"UPDATE ai_usage_outbox
                              SET status = 'ready',
                                  reservation_token = NULL,
                                  reservation_expires_at = NULL,
                                  finalized_at = $2,
                                  available_at = $2,
                                  last_error =
                                      'provider reservation expired; outcome ambiguous; conservative estimate charged'
                            WHERE usage_id = $1
                              AND status = 'reserved'",
                    )
                    .bind(charge.usage_id)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                    UsageReserveOutcome::Recovered
                }
            }
            "ready" | "completed" => UsageReserveOutcome::Finalized(stored_outcome(&stored)?),
            other => {
                return Err(sqlx::Error::Protocol(format!(
                    "ai usage {} has unknown status {other}",
                    charge.usage_id
                )));
            }
        };
        tx.commit().await?;
        Ok(outcome)
    }

    /// Finalize the actual charge after provider success. The random reservation
    /// token fences stale/concurrent callers. A prior successful finalize is
    /// idempotent, including when its database response was lost.
    pub async fn finalize(
        &self,
        reservation: UsageReservation,
        cost_micros: i64,
        outcome: Option<&UsageOutcome>,
        now: OffsetDateTime,
    ) -> Result<UsageFinalizeOutcome, sqlx::Error> {
        validate_cost(cost_micros)?;
        validate_outcome(outcome)?;
        let mut tx = self.pool.begin().await?;
        let stored = sqlx::query_as::<_, StoredReservation>(
            r"SELECT workspace_id, kind, outcome_kind, outcome_json, status,
                      reservation_token, reservation_expires_at
                FROM ai_usage_outbox
               WHERE usage_id = $1
               FOR UPDATE",
        )
        .bind(reservation.usage_id)
        .fetch_one(&mut *tx)
        .await?;
        ensure_expected_outcome(&stored, outcome)?;
        let outcome = match stored.status.as_str() {
            "reserved" if stored.reservation_token == Some(reservation.reservation_token) => {
                sqlx::query(
                    r"UPDATE ai_usage_outbox
                          SET cost_micros = $2,
                              status = 'ready',
                              reservation_token = NULL,
                              reservation_expires_at = NULL,
                              finalized_at = $3,
                              available_at = $3,
                              last_error = NULL,
                              outcome_json = $5
                        WHERE usage_id = $1
                          AND status = 'reserved'
                          AND reservation_token = $4",
                )
                .bind(reservation.usage_id)
                .bind(cost_micros)
                .bind(now)
                .bind(reservation.reservation_token)
                .bind(outcome.map(|value| &value.payload))
                .execute(&mut *tx)
                .await?;
                UsageFinalizeOutcome::Finalized
            }
            "reserved" => {
                return Err(sqlx::Error::Protocol(format!(
                    "stale ai usage reservation fence for {}",
                    reservation.usage_id
                )));
            }
            "ready" | "completed" => UsageFinalizeOutcome::Duplicate,
            "cancelled" => {
                return Err(sqlx::Error::Protocol(format!(
                    "cannot finalize cancelled ai usage {}",
                    reservation.usage_id
                )));
            }
            other => {
                return Err(sqlx::Error::Protocol(format!(
                    "ai usage {} has unknown status {other}",
                    reservation.usage_id
                )));
            }
        };
        tx.commit().await?;
        Ok(outcome)
    }

    /// Cancel a reservation after an ordinary provider error. Idempotent when a
    /// previous cancellation committed but its response was lost. `false` means
    /// the reservation was already finalized or its fence was superseded.
    pub async fn cancel(
        &self,
        reservation: UsageReservation,
        now: OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query_scalar::<_, Uuid>(
            r"UPDATE ai_usage_outbox
                  SET status = 'cancelled',
                      reservation_token = NULL,
                      reservation_expires_at = NULL,
                      cancelled_at = $3,
                      last_error = NULL
                WHERE usage_id = $1
                  AND status = 'reserved'
                  AND reservation_token = $2
            RETURNING usage_id",
        )
        .bind(reservation.usage_id)
        .bind(reservation.reservation_token)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;
        if changed.is_some() {
            tx.commit().await?;
            return Ok(true);
        }
        let status = sqlx::query_scalar::<_, String>(
            "SELECT status FROM ai_usage_outbox WHERE usage_id = $1",
        )
        .bind(reservation.usage_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.rollback().await?;
        Ok(status == "cancelled")
    }

    /// Conservatively finalize expired provider reservations whose outcome is
    /// unknowable after cancellation/process loss.
    pub async fn recover_expired_reservations(
        &self,
        now: OffsetDateTime,
        limit: i64,
    ) -> Result<Vec<UsageCharge>, sqlx::Error> {
        let limit = limit.clamp(1, MAX_CLAIM);
        sqlx::query_as::<_, (Uuid, Option<Uuid>, String, i64, Option<String>)>(
            r"WITH expired AS (
                  SELECT candidate.usage_id
                    FROM ai_usage_outbox AS candidate
                   WHERE candidate.status = 'reserved'
                     AND candidate.reservation_expires_at <= $1
                   ORDER BY candidate.reservation_expires_at, candidate.usage_id
                   FOR UPDATE SKIP LOCKED
                   LIMIT $2
              )
              UPDATE ai_usage_outbox AS outbox
                 SET status = 'ready',
                     reservation_token = NULL,
                     reservation_expires_at = NULL,
                     finalized_at = $1,
                     available_at = $1,
                     last_error =
                         'provider reservation expired; outcome ambiguous; conservative estimate charged'
                FROM expired
               WHERE outbox.usage_id = expired.usage_id
           RETURNING outbox.usage_id, outbox.workspace_id, outbox.kind,
                     outbox.cost_micros, outbox.outcome_kind",
        )
        .bind(now)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(
                    |(usage_id, workspace_id, kind, cost_micros, outcome_kind)| UsageCharge {
                        usage_id,
                        workspace_id,
                        kind,
                        cost_micros,
                        outcome_kind,
                    },
                )
                .collect()
        })
    }

    /// Claim due work using `SKIP LOCKED`. Every claim rotates a random token,
    /// increments its attempt number, and has a database-enforced expiry.
    pub async fn claim_due(
        &self,
        now: OffsetDateTime,
        lease: Duration,
        limit: i64,
    ) -> Result<Vec<AiUsageOutboxClaim>, sqlx::Error> {
        let lease_expires_at = now + clamped_lease(lease);
        let limit = limit.clamp(1, MAX_CLAIM);
        let rows = sqlx::query_as::<_, ClaimRow>(
            r"WITH claimable AS (
                  SELECT candidate.usage_id
                    FROM ai_usage_outbox AS candidate
                   WHERE candidate.status = 'ready'
                     AND candidate.available_at <= $1
                     AND (
                           candidate.lease_expires_at IS NULL
                           OR candidate.lease_expires_at <= $1
                     )
                   ORDER BY candidate.available_at, candidate.created_at,
                            candidate.usage_id
                   FOR UPDATE SKIP LOCKED
                   LIMIT $2
              )
              UPDATE ai_usage_outbox AS outbox
                 SET claim_token = gen_random_uuid(),
                     lease_expires_at = $3,
                     attempts = outbox.attempts + 1
                FROM claimable
               WHERE outbox.usage_id = claimable.usage_id
           RETURNING outbox.usage_id, outbox.workspace_id, outbox.kind,
                     outbox.cost_micros, outbox.attempts, outbox.claim_token,
                     outbox.lease_expires_at, outbox.created_at",
        )
        .bind(now)
        .bind(limit)
        .bind(lease_expires_at)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Insert the ledger row and acknowledge the matching claim atomically.
    ///
    /// Both the random token and the unexpired database lease fence a stale
    /// worker. `ON CONFLICT usage_id` closes the crash window where a predecessor
    /// inserted the ledger row but its outcome was ambiguous to the caller.
    pub async fn settle(&self, usage_id: Uuid, claim_token: Uuid) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query_as::<_, (Option<Uuid>, String, i64, OffsetDateTime)>(
            r"SELECT workspace_id, kind, cost_micros, created_at
                FROM ai_usage_outbox
               WHERE usage_id = $1
                 AND claim_token = $2
                 AND status = 'ready'
                 AND lease_expires_at > clock_timestamp()
               FOR UPDATE",
        )
        .bind(usage_id)
        .bind(claim_token)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((workspace_id, kind, cost_micros, created_at)) = row else {
            tx.rollback().await?;
            return Ok(false);
        };

        let inserted = sqlx::query(
            r"INSERT INTO ai_usage_ledger
                    (usage_id, workspace_id, kind, cost_micros, created_at)
              VALUES ($1, $2, $3, $4, $5)
              ON CONFLICT (usage_id) WHERE usage_id IS NOT NULL DO NOTHING",
        )
        .bind(usage_id)
        .bind(workspace_id)
        .bind(&kind)
        .bind(cost_micros)
        .bind(created_at)
        .execute(&mut *tx)
        .await?;

        if inserted.rows_affected() == 0 {
            let stored = sqlx::query_as::<_, StoredCharge>(
                r"SELECT workspace_id, kind, cost_micros
                    FROM ai_usage_ledger
                   WHERE usage_id = $1",
            )
            .bind(usage_id)
            .fetch_one(&mut *tx)
            .await?;
            if stored.workspace_id != workspace_id
                || stored.kind != kind
                || stored.cost_micros != cost_micros
            {
                return Err(sqlx::Error::Protocol(format!(
                    "ai usage ledger id {usage_id} conflicts with its outbox payload"
                )));
            }
        }

        let acknowledged = sqlx::query(
            r"UPDATE ai_usage_outbox
                  SET status = 'completed',
                      completed_at = clock_timestamp(),
                      claim_token = NULL,
                      lease_expires_at = NULL,
                      last_error = NULL
                WHERE usage_id = $1
                  AND claim_token = $2
                  AND status = 'ready'
                  AND lease_expires_at > clock_timestamp()",
        )
        .bind(usage_id)
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

    /// Re-park a failed ledger attempt. There is deliberately no dead-letter:
    /// accounting rows remain retryable until they are durably recorded.
    pub async fn mark_failed(
        &self,
        usage_id: Uuid,
        claim_token: Uuid,
        attempts: i32,
        now: OffsetDateTime,
        error: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE ai_usage_outbox
                  SET available_at = $4,
                      claim_token = NULL,
                      lease_expires_at = NULL,
                      last_error = $5
                WHERE usage_id = $1
                  AND claim_token = $2
                  AND attempts = $3
                  AND status = 'ready'
                  AND lease_expires_at > clock_timestamp()",
        )
        .bind(usage_id)
        .bind(claim_token)
        .bind(attempts)
        .bind(now + ai_usage_backoff(attempts))
        .bind(truncate_error(error))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn pending_count(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM ai_usage_outbox WHERE status IN ('reserved', 'ready')",
        )
        .fetch_one(&self.pool)
        .await
    }

    /// Compatibility helper for legacy/import callers. Runtime paid calls use
    /// [`Self::reserve`] instead.
    pub async fn insert_batch(&self, rows: &[UsageRow]) -> Result<u64, sqlx::Error> {
        if rows.is_empty() {
            return Ok(0);
        }
        let ws: Vec<Option<Uuid>> = rows.iter().map(|r| r.workspace_id).collect();
        let kinds: Vec<String> = rows.iter().map(|r| r.kind.clone()).collect();
        let micros: Vec<i64> = rows.iter().map(|r| r.cost_micros).collect();
        let result = sqlx::query(
            r"INSERT INTO ai_usage_ledger (workspace_id, kind, cost_micros)
              SELECT * FROM UNNEST($1::uuid[], $2::text[], $3::bigint[])",
        )
        .bind(&ws)
        .bind(&kinds)
        .bind(&micros)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn summary_since(
        &self,
        workspace: Uuid,
        since: OffsetDateTime,
    ) -> Result<Vec<UsageByKind>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (String, i64, i64)>(
            r"SELECT kind, COUNT(*)::bigint, COALESCE(SUM(cost_micros), 0)::bigint
                FROM ai_usage_ledger
               WHERE workspace_id = $1 AND created_at >= $2
               GROUP BY kind
               ORDER BY 3 DESC, kind ASC",
        )
        .bind(workspace)
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(kind, calls, cost_micros)| UsageByKind {
                kind,
                calls,
                cost_micros,
            })
            .collect())
    }

    /// Purge historical ledger rows and terminal dedup records together. Active
    /// reservations and ready accounting work are never removed.
    pub async fn sweep_older_than(&self, cutoff: OffsetDateTime) -> Result<u64, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r"DELETE FROM ai_usage_outbox
               WHERE (status = 'completed' AND completed_at < $1)
                  OR (status = 'cancelled' AND cancelled_at < $1)",
        )
        .bind(cutoff)
        .execute(&mut *tx)
        .await?;
        let ledger = sqlx::query("DELETE FROM ai_usage_ledger WHERE created_at < $1")
            .bind(cutoff)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(ledger.rows_affected())
    }
}

#[must_use]
pub fn ai_usage_backoff(attempts: i32) -> Duration {
    let shift = u32::try_from(attempts.saturating_sub(1).clamp(0, 30)).unwrap_or(0);
    let seconds = 1_i64
        .checked_shl(shift)
        .unwrap_or(MAX_BACKOFF_SECONDS)
        .min(MAX_BACKOFF_SECONDS);
    Duration::seconds(seconds)
}

fn clamped_lease(lease: Duration) -> Duration {
    Duration::seconds(lease.whole_seconds().clamp(1, MAX_LEASE_SECONDS))
}

fn validate_charge(charge: &UsageCharge) -> Result<(), sqlx::Error> {
    if charge.kind.is_empty() || charge.kind.len() > 64 {
        return Err(sqlx::Error::Protocol(
            "ai usage kind must contain 1..=64 bytes".into(),
        ));
    }
    if charge
        .outcome_kind
        .as_ref()
        .is_some_and(|kind| kind.is_empty() || kind.len() > 64)
    {
        return Err(sqlx::Error::Protocol(
            "ai usage outcome kind must contain 1..=64 bytes".into(),
        ));
    }
    validate_cost(charge.cost_micros)
}

fn validate_cost(cost_micros: i64) -> Result<(), sqlx::Error> {
    if cost_micros > 0 {
        Ok(())
    } else {
        Err(sqlx::Error::Protocol(
            "durable ai usage cost must be positive".into(),
        ))
    }
}

fn validate_outcome(outcome: Option<&UsageOutcome>) -> Result<(), sqlx::Error> {
    let Some(outcome) = outcome else {
        return Ok(());
    };
    if outcome.kind.is_empty() || outcome.kind.len() > 64 {
        return Err(sqlx::Error::Protocol(
            "ai usage outcome kind must contain 1..=64 bytes".into(),
        ));
    }
    let bytes = serde_json::to_vec(&outcome.payload)
        .map_err(|error| sqlx::Error::Protocol(format!("serialize ai usage outcome: {error}")))?;
    if bytes.len() > MAX_OUTCOME_BYTES {
        return Err(sqlx::Error::Protocol(format!(
            "ai usage outcome exceeds {MAX_OUTCOME_BYTES} bytes"
        )));
    }
    Ok(())
}

fn stored_outcome(stored: &StoredReservation) -> Result<Option<UsageOutcome>, sqlx::Error> {
    match (&stored.outcome_kind, &stored.outcome_json) {
        (Some(kind), Some(payload)) => Ok(Some(UsageOutcome {
            kind: kind.clone(),
            payload: payload.clone(),
        })),
        (Some(_) | None, None) => Ok(None),
        (None, Some(_)) => Err(sqlx::Error::Protocol(
            "ai usage outcome payload has no schema kind".into(),
        )),
    }
}

fn ensure_expected_outcome(
    stored: &StoredReservation,
    supplied: Option<&UsageOutcome>,
) -> Result<(), sqlx::Error> {
    match (&stored.outcome_kind, supplied) {
        (None, None) => {}
        (Some(expected), Some(actual)) if expected == &actual.kind => {}
        (expected, actual) => {
            return Err(sqlx::Error::Protocol(format!(
                "ai usage outcome schema mismatch: expected {expected:?}, got {:?}",
                actual.map(|value| &value.kind)
            )));
        }
    }
    if matches!(stored.status.as_str(), "ready" | "completed") {
        let existing = stored_outcome(stored)?;
        if existing.as_ref() != supplied {
            return Err(sqlx::Error::Protocol(
                "finalized ai usage outcome conflicts with retry payload".into(),
            ));
        }
    }
    Ok(())
}

fn truncate_error(error: &str) -> &str {
    if error.len() <= MAX_ERROR_CHARS {
        return error;
    }
    let mut end = MAX_ERROR_CHARS;
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    &error[..end]
}

#[cfg(test)]
#[path = "ai_usage/tests.rs"]
mod tests;
