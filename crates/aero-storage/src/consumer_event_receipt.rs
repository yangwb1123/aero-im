//! Durable idempotency receipts for NATS side-effect consumers.
//!
//! Broker message-id de-duplication is necessarily bounded by stream retention,
//! but a producer outbox may retry indefinitely.  A completed receipt prevents
//! such a late retry from repeating an AI call, push, or webhook delivery.

use sqlx::{PgPool, Postgres, Transaction};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

const MAX_CONSUMER_BYTES: usize = 128;
const MAX_ERROR_CHARS: usize = 2_048;
const MAX_LEASE_SECONDS: i64 = 86_400;

/// Result of attempting to own one consumer/event pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumerEventClaim {
    /// This caller owns the current processing lease.
    Claimed { attempts: i32 },
    /// A previous invocation completed the event; the broker delivery is safe
    /// to ACK without invoking the handler.
    Completed,
    /// Another invocation still holds an unexpired processing lease.
    Busy,
}

/// PostgreSQL-backed durable consumer receipt store.
#[derive(Clone)]
pub struct ConsumerEventReceiptRepo {
    pool: PgPool,
}

impl ConsumerEventReceiptRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Claim a new event or reclaim an expired processing lease.
    ///
    /// `attempts` is a fencing token: completion/release only succeeds for the
    /// exact claim that owns the latest lease.
    pub async fn claim(
        &self,
        consumer: &str,
        event_id: Uuid,
        now: OffsetDateTime,
        lease: Duration,
    ) -> Result<ConsumerEventClaim, sqlx::Error> {
        validate_consumer(consumer)?;
        let lease_expires_at = now + clamped_lease(lease);
        let attempts = sqlx::query_scalar::<_, i32>(
            r"INSERT INTO consumer_event_receipts
                  (consumer, event_id, state, attempts, lease_expires_at,
                   completed_at, last_error, created_at, updated_at)
               VALUES ($1, $2, 'processing', 1, $3, NULL, NULL, $4, $4)
               ON CONFLICT (consumer, event_id) DO UPDATE
                   SET attempts = consumer_event_receipts.attempts + 1,
                       lease_expires_at = EXCLUDED.lease_expires_at,
                       completed_at = NULL,
                       last_error = NULL,
                       updated_at = EXCLUDED.updated_at
                 WHERE consumer_event_receipts.state = 'processing'
                   AND consumer_event_receipts.lease_expires_at <= $4
            RETURNING attempts",
        )
        .bind(consumer)
        .bind(event_id)
        .bind(lease_expires_at)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(attempts) = attempts {
            return Ok(ConsumerEventClaim::Claimed { attempts });
        }

        let state = sqlx::query_scalar::<_, String>(
            r"SELECT state
                FROM consumer_event_receipts
               WHERE consumer = $1 AND event_id = $2",
        )
        .bind(consumer)
        .bind(event_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match state.as_deref() {
            Some("completed") => ConsumerEventClaim::Completed,
            // Includes a concurrent processing owner. A row can disappear
            // between the statements only if a completion sweep races us; in
            // that rare case, declining to ACK causes a safe later retry.
            _ => ConsumerEventClaim::Busy,
        })
    }

    /// Extend the currently owned processing lease.
    ///
    /// Both `attempts` and the unexpired lease are load-bearing fences. A caller
    /// that wakes after its lease expired cannot resurrect stale ownership, even
    /// if no replacement claim has incremented `attempts` yet.
    pub async fn renew(
        &self,
        consumer: &str,
        event_id: Uuid,
        attempts: i32,
        now: OffsetDateTime,
        lease: Duration,
    ) -> Result<bool, sqlx::Error> {
        validate_consumer(consumer)?;
        let lease_expires_at = now + clamped_lease(lease);
        let result = sqlx::query(
            r"UPDATE consumer_event_receipts
                  SET lease_expires_at = $5,
                      updated_at = $4
                WHERE consumer = $1
                  AND event_id = $2
                  AND state = 'processing'
                  AND attempts = $3
                  AND lease_expires_at > $4
                  AND lease_expires_at > clock_timestamp()
                  AND $5 > clock_timestamp()",
        )
        .bind(consumer)
        .bind(event_id)
        .bind(attempts)
        .bind(now)
        .bind(lease_expires_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Mark the currently fenced processing lease complete.
    pub async fn complete(
        &self,
        consumer: &str,
        event_id: Uuid,
        attempts: i32,
        now: OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        validate_consumer(consumer)?;
        let result = sqlx::query(
            r"UPDATE consumer_event_receipts
                  SET state = 'completed',
                      lease_expires_at = NULL,
                      completed_at = $4,
                      last_error = NULL,
                      updated_at = $4
                WHERE consumer = $1
                  AND event_id = $2
                  AND state = 'processing'
                  AND attempts = $3
                  AND lease_expires_at > $4
                  AND lease_expires_at > clock_timestamp()",
        )
        .bind(consumer)
        .bind(event_id)
        .bind(attempts)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Complete a fenced receipt inside a caller-owned transaction.
    ///
    /// Side-effect consumers that materialize their own durable outbox use this
    /// method so the full fan-out and completion receipt become visible
    /// atomically.
    pub async fn complete_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        consumer: &str,
        event_id: Uuid,
        attempts: i32,
        now: OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        validate_consumer(consumer)?;
        let result = sqlx::query(
            r"UPDATE consumer_event_receipts
                  SET state = 'completed',
                      lease_expires_at = NULL,
                      completed_at = $4,
                      last_error = NULL,
                      updated_at = $4
                WHERE consumer = $1
                  AND event_id = $2
                  AND state = 'processing'
                  AND attempts = $3
                  AND lease_expires_at > $4
                  AND lease_expires_at > clock_timestamp()",
        )
        .bind(consumer)
        .bind(event_id)
        .bind(attempts)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Release a failed claim for immediate retry, fenced by `attempts`.
    pub async fn release(
        &self,
        consumer: &str,
        event_id: Uuid,
        attempts: i32,
        now: OffsetDateTime,
        error: &str,
    ) -> Result<bool, sqlx::Error> {
        validate_consumer(consumer)?;
        let error = truncate_error(error);
        let result = sqlx::query(
            r"UPDATE consumer_event_receipts
                  SET lease_expires_at = $4,
                      last_error = $5,
                      updated_at = $4
                WHERE consumer = $1
                  AND event_id = $2
                  AND state = 'processing'
                  AND attempts = $3
                  AND lease_expires_at > $4
                  AND lease_expires_at > clock_timestamp()",
        )
        .bind(consumer)
        .bind(event_id)
        .bind(attempts)
        .bind(now)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Delete old completed receipts only when no producer outbox row with the
    /// same `event_id` is still pending.
    ///
    /// That anti-join is load-bearing: pending outbox retries have no deadline
    /// and may outlive both NATS stream retention and its duplicate window.
    pub async fn sweep_completed_before(&self, cutoff: OffsetDateTime) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r"DELETE FROM consumer_event_receipts AS receipt
                WHERE receipt.state = 'completed'
                  AND receipt.completed_at < $1
                  AND NOT EXISTS (
                        SELECT 1
                          FROM event_outbox AS outbox
                         WHERE outbox.event_id = receipt.event_id
                           AND outbox.published_at IS NULL
                  )",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Delete processing rows whose lease expired before `cutoff`.
    ///
    /// `JetStream` eventually parks poison events after its bounded delivery
    /// count. Without this sweep, the final abandoned processing lease would
    /// otherwise remain forever. Removing an incomplete receipt is safe: a
    /// future producer replay must execute the handler again.
    pub async fn sweep_abandoned_processing_before(
        &self,
        cutoff: OffsetDateTime,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r"DELETE FROM consumer_event_receipts
                WHERE state = 'processing'
                  AND lease_expires_at < $1",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

fn validate_consumer(consumer: &str) -> Result<(), sqlx::Error> {
    if consumer.is_empty() || consumer.len() > MAX_CONSUMER_BYTES {
        return Err(sqlx::Error::Protocol(
            "consumer receipt name must contain 1..=128 bytes".into(),
        ));
    }
    Ok(())
}

fn clamped_lease(lease: Duration) -> Duration {
    Duration::seconds(lease.whole_seconds().clamp(1, MAX_LEASE_SECONDS))
}

fn truncate_error(error: &str) -> String {
    error.chars().take(MAX_ERROR_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_is_clamped_to_safe_bounds() {
        assert_eq!(clamped_lease(Duration::ZERO), Duration::seconds(1));
        assert_eq!(
            clamped_lease(Duration::days(7)),
            Duration::seconds(MAX_LEASE_SECONDS)
        );
    }

    #[test]
    fn error_is_char_safe_and_bounded() {
        let value = "界".repeat(MAX_ERROR_CHARS + 10);
        let truncated = truncate_error(&value);
        assert_eq!(truncated.chars().count(), MAX_ERROR_CHARS);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn invalid_consumer_names_are_rejected_before_io() {
        assert!(validate_consumer("").is_err());
        assert!(validate_consumer(&"x".repeat(MAX_CONSUMER_BYTES + 1)).is_err());
        assert!(validate_consumer("aero-push").is_ok());
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL with migrations applied"]
    async fn claim_fencing_completion_and_pending_outbox_retention() {
        let Some(url) = std::env::var("DATABASE_URL").ok() else {
            return;
        };
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let repo = ConsumerEventReceiptRepo::new(pool.clone());
        let consumer = format!("receipt-test-{}", Uuid::new_v4());
        let event_id = Uuid::new_v4();
        let now = OffsetDateTime::now_utc();
        let lease = Duration::minutes(5);

        assert_eq!(
            repo.claim(&consumer, event_id, now, lease).await.unwrap(),
            ConsumerEventClaim::Claimed { attempts: 1 }
        );
        assert_eq!(
            repo.claim(&consumer, event_id, now + Duration::minutes(1), lease)
                .await
                .unwrap(),
            ConsumerEventClaim::Busy
        );
        assert!(repo
            .renew(&consumer, event_id, 1, now + Duration::minutes(4), lease,)
            .await
            .unwrap());
        assert_eq!(
            repo.claim(&consumer, event_id, now + Duration::minutes(6), lease)
                .await
                .unwrap(),
            ConsumerEventClaim::Busy
        );
        assert_eq!(
            repo.claim(&consumer, event_id, now + Duration::minutes(10), lease)
                .await
                .unwrap(),
            ConsumerEventClaim::Claimed { attempts: 2 }
        );
        assert!(
            !repo
                .renew(&consumer, event_id, 1, now + Duration::minutes(10), lease,)
                .await
                .unwrap(),
            "stale attempt must not renew a newer lease"
        );
        assert!(
            !repo
                .complete(&consumer, event_id, 1, now + Duration::minutes(10))
                .await
                .unwrap(),
            "stale attempt must not complete a newer lease"
        );
        assert!(repo
            .complete(&consumer, event_id, 2, now + Duration::minutes(10))
            .await
            .unwrap());
        assert_eq!(
            repo.claim(&consumer, event_id, now + Duration::days(30), lease)
                .await
                .unwrap(),
            ConsumerEventClaim::Completed
        );

        sqlx::query(
            r#"INSERT INTO event_outbox
                  (id, event_id, message_id, event_kind, aggregate_version,
                   subject, payload)
               VALUES ($1, $2, $3, 'message', 1, 'im.room.receipt-test',
                       '{"kind":"message"}'::jsonb)"#,
        )
        .bind(Uuid::new_v4())
        .bind(event_id)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();

        let cutoff = now + Duration::days(31);
        repo.sweep_completed_before(cutoff).await.unwrap();
        let retained: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM consumer_event_receipts
                  WHERE consumer = $1 AND event_id = $2
             )",
        )
        .bind(&consumer)
        .bind(event_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            retained,
            "a pending producer outbox must retain its receipt"
        );
        sqlx::query("UPDATE event_outbox SET published_at = $2 WHERE event_id = $1")
            .bind(event_id)
            .bind(now)
            .execute(&pool)
            .await
            .unwrap();
        repo.sweep_completed_before(cutoff).await.unwrap();
        let retained: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM consumer_event_receipts
                  WHERE consumer = $1 AND event_id = $2
             )",
        )
        .bind(&consumer)
        .bind(event_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            !retained,
            "published producer outbox permits receipt cleanup"
        );

        let expired_id = Uuid::new_v4();
        assert_eq!(
            repo.claim(&consumer, expired_id, now, lease).await.unwrap(),
            ConsumerEventClaim::Claimed { attempts: 1 }
        );
        assert!(!repo
            .renew(&consumer, expired_id, 1, now + Duration::minutes(6), lease,)
            .await
            .unwrap());
        assert!(!repo
            .complete(&consumer, expired_id, 1, now + Duration::minutes(6))
            .await
            .unwrap());
        assert_eq!(
            repo.claim(&consumer, expired_id, now + Duration::minutes(6), lease,)
                .await
                .unwrap(),
            ConsumerEventClaim::Claimed { attempts: 2 }
        );
        assert!(repo
            .complete(&consumer, expired_id, 2, now + Duration::minutes(6))
            .await
            .unwrap());

        let abandoned_id = Uuid::new_v4();
        assert_eq!(
            repo.claim(&consumer, abandoned_id, now - Duration::days(60), lease,)
                .await
                .unwrap(),
            ConsumerEventClaim::Claimed { attempts: 1 }
        );
        repo.sweep_abandoned_processing_before(now - Duration::days(30))
            .await
            .unwrap();
        let abandoned_retained: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM consumer_event_receipts
                  WHERE consumer = $1 AND event_id = $2
             )",
        )
        .bind(&consumer)
        .bind(abandoned_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!abandoned_retained, "this test's abandoned lease is swept");

        sqlx::query("DELETE FROM event_outbox WHERE event_id = $1")
            .bind(event_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM consumer_event_receipts WHERE consumer = $1")
            .bind(&consumer)
            .execute(&pool)
            .await
            .unwrap();
    }
}
