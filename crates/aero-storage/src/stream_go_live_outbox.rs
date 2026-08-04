//! Durable producer outbox for `stream.live` lifecycle transitions.
//!
//! [`crate::StreamRepo::mark_live`] creates these immutable rows atomically with
//! the `idle|ended -> live` state transition.  Relays use a random claim token,
//! not the attempt counter, as the generation fence; a stale worker therefore
//! cannot settle a row after a successor has reclaimed it.

use aero_common::{ParticipantId, RoomId};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use ulid::Ulid;
use uuid::Uuid;

const MAX_CLAIM: i64 = 500;
const MAX_LEASE_SECONDS: i64 = 86_400;
const MAX_BACKOFF_SECONDS: i64 = 300;
const MAX_ERROR_CHARS: usize = 2_048;

const COLUMNS: &str = "id, event_id, stream_id, room_id, owner_id, title, subject, \
                       traceparent, seq, attempts, claim_token, available_at, claimed_at, \
                       nats_published_at, webhooks_materialized_at, completed_at, last_error, \
                       created_at";

/// Identifiers returned when this caller won the atomic publisher transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoLiveTransition {
    pub outbox_id: Uuid,
    pub event_id: Uuid,
}

/// Result of trying to reserve a stream publisher in `PostgreSQL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkLiveOutcome {
    /// This caller changed the stream to live and committed an outbox row.
    Started(GoLiveTransition),
    /// Another protocol/node already owns the live state.
    AlreadyLive,
    /// The stream disappeared between credential resolution and reservation.
    NotFound,
}

/// Immutable lifecycle snapshot plus its relay state.
#[derive(Debug, Clone)]
pub struct StreamGoLiveOutboxRow {
    pub id: Uuid,
    pub event_id: Uuid,
    pub stream_id: Ulid,
    pub room_id: Option<RoomId>,
    pub owner_id: ParticipantId,
    pub title: String,
    pub subject: String,
    pub traceparent: Option<String>,
    pub seq: Option<u64>,
    pub attempts: i32,
    pub claim_token: Uuid,
    pub available_at: OffsetDateTime,
    pub claimed_at: Option<OffsetDateTime>,
    pub nats_published_at: Option<OffsetDateTime>,
    pub webhooks_materialized_at: Option<OffsetDateTime>,
    pub completed_at: Option<OffsetDateTime>,
    pub last_error: Option<String>,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, sqlx::FromRow)]
struct DbRow {
    id: Uuid,
    event_id: Uuid,
    stream_id: Uuid,
    room_id: Option<Uuid>,
    owner_id: Uuid,
    title: String,
    subject: String,
    traceparent: Option<String>,
    seq: Option<i64>,
    attempts: i32,
    claim_token: Uuid,
    available_at: OffsetDateTime,
    claimed_at: Option<OffsetDateTime>,
    nats_published_at: Option<OffsetDateTime>,
    webhooks_materialized_at: Option<OffsetDateTime>,
    completed_at: Option<OffsetDateTime>,
    last_error: Option<String>,
    created_at: OffsetDateTime,
}

impl TryFrom<DbRow> for StreamGoLiveOutboxRow {
    type Error = sqlx::Error;

    fn try_from(row: DbRow) -> Result<Self, Self::Error> {
        let seq = row
            .seq
            .map(u64::try_from)
            .transpose()
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
        Ok(Self {
            id: row.id,
            event_id: row.event_id,
            stream_id: Ulid(row.stream_id.as_u128()),
            room_id: row.room_id.map(RoomId::from_uuid),
            owner_id: ParticipantId::from_uuid(row.owner_id),
            title: row.title,
            subject: row.subject,
            traceparent: row.traceparent,
            seq,
            attempts: row.attempts,
            claim_token: row.claim_token,
            available_at: row.available_at,
            claimed_at: row.claimed_at,
            nats_published_at: row.nats_published_at,
            webhooks_materialized_at: row.webhooks_materialized_at,
            completed_at: row.completed_at,
            last_error: row.last_error,
            created_at: row.created_at,
        })
    }
}

/// `PostgreSQL` queue used by every server instance.
#[derive(Clone)]
pub struct StreamGoLiveOutboxRepo {
    pool: PgPool,
}

impl StreamGoLiveOutboxRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Claim committed, due rows with `SKIP LOCKED`.
    ///
    /// `claim_token` is regenerated on every claim, including stale recovery.
    /// `attempts` counts relay claims (not webhook HTTP calls).
    pub async fn claim_due(
        &self,
        now: OffsetDateTime,
        lease: Duration,
        limit: i64,
    ) -> Result<Vec<StreamGoLiveOutboxRow>, sqlx::Error> {
        let stale_before = now - clamp_lease(lease);
        let limit = limit.clamp(1, MAX_CLAIM);
        let sql = format!(
            r"WITH claimable AS (
                  SELECT candidate.id AS claimed_id
                    FROM stream_go_live_outbox AS candidate
                   WHERE candidate.completed_at IS NULL
                     AND candidate.available_at <= $1
                     AND (candidate.claimed_at IS NULL OR candidate.claimed_at <= $2)
                   ORDER BY candidate.available_at, candidate.created_at, candidate.id
                   FOR UPDATE SKIP LOCKED
                   LIMIT $3
              )
              UPDATE stream_go_live_outbox AS outbox
                 SET claimed_at = $1,
                     claim_token = gen_random_uuid(),
                     attempts = outbox.attempts + 1
                FROM claimable
               WHERE outbox.id = claimable.claimed_id
           RETURNING {COLUMNS}"
        );
        let rows = sqlx::query_as::<_, DbRow>(&sql)
            .bind(now)
            .bind(stale_before)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// Claim one known row (used by bounded post-commit nudges and deterministic
    /// tests). Returns `None` when another live lease owns it or it is complete.
    pub async fn claim_by_id(
        &self,
        id: Uuid,
        now: OffsetDateTime,
        lease: Duration,
    ) -> Result<Option<StreamGoLiveOutboxRow>, sqlx::Error> {
        let stale_before = now - clamp_lease(lease);
        let sql = format!(
            r"WITH claimable AS (
                  SELECT candidate.id AS claimed_id
                    FROM stream_go_live_outbox AS candidate
                   WHERE candidate.id = $1
                     AND candidate.completed_at IS NULL
                     AND candidate.available_at <= $2
                     AND (candidate.claimed_at IS NULL OR candidate.claimed_at <= $3)
                   FOR UPDATE SKIP LOCKED
              )
              UPDATE stream_go_live_outbox AS outbox
                 SET claimed_at = $2,
                     claim_token = gen_random_uuid(),
                     attempts = outbox.attempts + 1
                FROM claimable
               WHERE outbox.id = claimable.claimed_id
           RETURNING {COLUMNS}"
        );
        let row = sqlx::query_as::<_, DbRow>(&sql)
            .bind(id)
            .bind(now)
            .bind(stale_before)
            .fetch_optional(&self.pool)
            .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// Persist a per-subject sequence exactly once, fenced to the current claim.
    pub async fn assign_seq_if_absent(
        &self,
        id: Uuid,
        claim_token: Uuid,
        seq: u64,
    ) -> Result<Option<u64>, sqlx::Error> {
        let seq = i64::try_from(seq).map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        let stored = sqlx::query_scalar::<_, i64>(
            r"UPDATE stream_go_live_outbox
                  SET seq = COALESCE(seq, $3)
                WHERE id = $1
                  AND claim_token = $2
                  AND claimed_at IS NOT NULL
                  AND completed_at IS NULL
            RETURNING seq",
        )
        .bind(id)
        .bind(claim_token)
        .bind(seq)
        .fetch_optional(&self.pool)
        .await?;
        stored
            .map(u64::try_from)
            .transpose()
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))
    }

    /// Record that the stable-id NATS publish was accepted.
    pub async fn mark_nats_published(
        &self,
        id: Uuid,
        claim_token: Uuid,
        at: OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        mark_stage(&self.pool, id, claim_token, at, "nats_published_at").await
    }

    /// Record that every matching target has a durable, deduplicated delivery row.
    pub async fn mark_webhooks_materialized(
        &self,
        id: Uuid,
        claim_token: Uuid,
        at: OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        mark_stage(&self.pool, id, claim_token, at, "webhooks_materialized_at").await
    }

    /// Complete only after both independently retryable stages are durable.
    pub async fn mark_completed(
        &self,
        id: Uuid,
        claim_token: Uuid,
        at: OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE stream_go_live_outbox
                  SET completed_at = $3,
                      claimed_at = NULL,
                      last_error = NULL
                WHERE id = $1
                  AND claim_token = $2
                  AND claimed_at IS NOT NULL
                  AND completed_at IS NULL
                  AND nats_published_at IS NOT NULL
                  AND webhooks_materialized_at IS NOT NULL",
        )
        .bind(id)
        .bind(claim_token)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Release a failed claim with bounded exponential backoff.
    ///
    /// Successful stage timestamps are intentionally retained, so a retry only
    /// repeats work whose durable acknowledgement is still missing.
    pub async fn mark_failed(
        &self,
        id: Uuid,
        claim_token: Uuid,
        attempts: i32,
        now: OffsetDateTime,
        error: &str,
    ) -> Result<bool, sqlx::Error> {
        let available_at = now + retry_delay(attempts);
        let error = truncate_error(error);
        let result = sqlx::query(
            r"UPDATE stream_go_live_outbox
                  SET claimed_at = NULL,
                      available_at = $4,
                      last_error = $5
                WHERE id = $1
                  AND claim_token = $2
                  AND attempts = $3
                  AND completed_at IS NULL",
        )
        .bind(id)
        .bind(claim_token)
        .bind(attempts)
        .bind(available_at)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Test/operability lookup by queue id.
    pub async fn get(&self, id: Uuid) -> Result<Option<StreamGoLiveOutboxRow>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM stream_go_live_outbox WHERE id = $1");
        sqlx::query_as::<_, DbRow>(&sql)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .map(TryInto::try_into)
            .transpose()
    }
}

async fn mark_stage(
    pool: &PgPool,
    id: Uuid,
    claim_token: Uuid,
    at: OffsetDateTime,
    column: &'static str,
) -> Result<bool, sqlx::Error> {
    debug_assert!(matches!(
        column,
        "nats_published_at" | "webhooks_materialized_at"
    ));
    let sql = format!(
        "UPDATE stream_go_live_outbox
            SET {column} = COALESCE({column}, $3)
          WHERE id = $1
            AND claim_token = $2
            AND claimed_at IS NOT NULL
            AND completed_at IS NULL"
    );
    let result = sqlx::query(&sql)
        .bind(id)
        .bind(claim_token)
        .bind(at)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() == 1)
}

fn clamp_lease(lease: Duration) -> Duration {
    Duration::seconds(lease.whole_seconds().clamp(1, MAX_LEASE_SECONDS))
}

fn retry_delay(attempts: i32) -> Duration {
    let exponent = u32::try_from(attempts.saturating_sub(1).clamp(0, 30)).unwrap_or(0);
    let seconds = 1_i64
        .checked_shl(exponent)
        .unwrap_or(MAX_BACKOFF_SECONDS)
        .min(MAX_BACKOFF_SECONDS);
    Duration::seconds(seconds)
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
mod tests {
    use super::*;

    #[test]
    fn retry_is_bounded_and_error_truncation_preserves_utf8() {
        assert_eq!(retry_delay(1), Duration::seconds(1));
        assert_eq!(retry_delay(2), Duration::seconds(2));
        assert_eq!(retry_delay(99), Duration::seconds(MAX_BACKOFF_SECONDS));
        let long = "界".repeat(1_000);
        let truncated = truncate_error(&long);
        assert!(truncated.len() <= MAX_ERROR_CHARS);
        assert!(truncated.is_char_boundary(truncated.len()));
    }
}
