//! Transactional producer outbox for durable room events.
//!
//! Message creation writes the business row and one outbox row in the same
//! `PostgreSQL` transaction (see [`crate::MessageRepo::insert_outboxed`]);
//! collaborative canvas operations reuse the queue with their immutable op UUID
//! as the compatibility aggregate id. Relay workers claim committed rows with a
//! renewable lease, stamp a per-subject sequence exactly once, then publish
//! using the stable [`EventOutboxRow::event_id`] as the NATS message-id.

use aero_common::{MessageId, RoomEvent, RoomId};
use sqlx::{PgPool, Postgres, Transaction};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

const MAX_CLAIM: i64 = 500;
const MAX_LEASE_SECONDS: i64 = 86_400;
const BASE_BACKOFF_SECONDS: i64 = 1;
const MAX_BACKOFF_SECONDS: i64 = 300;
const MAX_ERROR_CHARS: usize = 2_048;

const COLUMNS: &str = "id, event_id, message_id, event_kind, aggregate_version, delivery_ordinal, \
                       subject, payload, traceparent, seq, attempts, available_at, claimed_at, \
                       published_at, last_error, created_at";

/// Durable room-event type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventOutboxKind {
    Message,
    Edited,
    Deleted,
    Notify,
    Reaction,
    CanvasOp,
}

impl EventOutboxKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Edited => "edited",
            Self::Deleted => "deleted",
            Self::Notify => "notify",
            Self::Reaction => "reaction",
            Self::CanvasOp => "canvas_op",
        }
    }
}

impl TryFrom<&str> for EventOutboxKind {
    type Error = sqlx::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "message" => Ok(Self::Message),
            "edited" => Ok(Self::Edited),
            "deleted" => Ok(Self::Deleted),
            "notify" => Ok(Self::Notify),
            "reaction" => Ok(Self::Reaction),
            "canvas_op" => Ok(Self::CanvasOp),
            other => Err(sqlx::Error::Decode(
                format!("unknown event_outbox event_kind {other:?}").into(),
            )),
        }
    }
}

/// One durable event waiting for (or recording) publication to NATS.
#[derive(Debug, Clone)]
pub struct EventOutboxRow {
    /// Queue-record identifier used by claim/complete operations.
    pub id: Uuid,
    /// Stable event identifier, suitable for the NATS `Nats-Msg-Id` header.
    pub event_id: Uuid,
    /// Aggregate identity. Message events store their message UUID; a canvas-op
    /// event stores its immutable operation UUID.
    pub message_id: MessageId,
    /// Mutation represented by this row.
    pub event_kind: EventOutboxKind,
    /// Strictly increasing version within this aggregate.
    pub aggregate_version: i64,
    /// Durable creation order within the room. Creation events are published in
    /// this order so delivery ACKs represent a cumulative prefix even when
    /// time-sortable message ids race across writers.
    pub delivery_ordinal: Option<i64>,
    pub subject: String,
    /// Unstamped serialized `RoomEvent`; `seq` and `traceparent` are separate
    /// columns and are applied only by the relay.
    pub payload: serde_json::Value,
    pub traceparent: Option<String>,
    /// Per-subject sequence assigned once by the relay and reused on retries.
    pub seq: Option<u64>,
    /// Number of successful lease claims, including the current claim.
    pub attempts: i32,
    pub available_at: OffsetDateTime,
    pub claimed_at: Option<OffsetDateTime>,
    pub published_at: Option<OffsetDateTime>,
    pub last_error: Option<String>,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, sqlx::FromRow)]
struct DbRow {
    id: Uuid,
    event_id: Uuid,
    message_id: Uuid,
    event_kind: String,
    aggregate_version: i64,
    delivery_ordinal: Option<i64>,
    subject: String,
    payload: serde_json::Value,
    traceparent: Option<String>,
    seq: Option<i64>,
    attempts: i32,
    available_at: OffsetDateTime,
    claimed_at: Option<OffsetDateTime>,
    published_at: Option<OffsetDateTime>,
    last_error: Option<String>,
    created_at: OffsetDateTime,
}

impl TryFrom<DbRow> for EventOutboxRow {
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
            message_id: MessageId::from_uuid(row.message_id),
            event_kind: EventOutboxKind::try_from(row.event_kind.as_str())?,
            aggregate_version: row.aggregate_version,
            delivery_ordinal: row.delivery_ordinal,
            subject: row.subject,
            payload: row.payload,
            traceparent: row.traceparent,
            seq,
            attempts: row.attempts,
            available_at: row.available_at,
            claimed_at: row.claimed_at,
            published_at: row.published_at,
            last_error: row.last_error,
            created_at: row.created_at,
        })
    }
}

/// Transaction-scoped insert data built by the message repository.
pub(crate) struct NewEventOutbox {
    pub message_id: MessageId,
    pub event_kind: EventOutboxKind,
    pub subject: String,
    pub payload: serde_json::Value,
    pub traceparent: Option<String>,
}

/// PostgreSQL-backed outbox queue.
#[derive(Clone)]
pub struct EventOutboxRepo {
    pool: PgPool,
}

impl EventOutboxRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert an event on the caller's transaction and return its queue id.
    ///
    /// This is crate-private so business callers cannot accidentally break the
    /// message + outbox atomicity contract by enqueuing after commit.
    pub(crate) async fn insert_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        new: NewEventOutbox,
    ) -> Result<Uuid, sqlx::Error> {
        Self::insert_idempotent_in_tx(tx, new, Uuid::new_v4()).await
    }

    /// Append an event with a caller-stable event id.
    ///
    /// The caller must hold the relevant aggregate serialization lock (or be
    /// creating that aggregate) for the transaction. That makes `MAX + 1` a
    /// safe, gap-free aggregate-version allocator. Reusing `event_id` returns
    /// the original row and does not append another aggregate version.
    pub(crate) async fn insert_idempotent_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        new: NewEventOutbox,
        event_id: Uuid,
    ) -> Result<Uuid, sqlx::Error> {
        let id = Uuid::new_v4();
        let row_id = sqlx::query_scalar::<_, Uuid>(
            r"INSERT INTO event_outbox
                  (id, event_id, message_id, event_kind, aggregate_version,
                   delivery_ordinal, subject, payload, traceparent)
               SELECT $1, $2, $3, $4,
                      COALESCE(MAX(aggregate_version), 0) + 1,
                      (SELECT delivery_ordinal FROM messages WHERE id = $3),
                      $5, $6, $7
                 FROM event_outbox
                WHERE message_id = $3
               ON CONFLICT (event_id) DO UPDATE
                   SET event_id = EXCLUDED.event_id
            RETURNING id",
        )
        .bind(id)
        .bind(event_id)
        .bind(new.message_id.to_uuid())
        .bind(new.event_kind.as_str())
        .bind(new.subject)
        .bind(new.payload)
        .bind(new.traceparent)
        .fetch_one(&mut **tx)
        .await?;
        Ok(row_id)
    }

    /// Serialize and append one room event on the caller's message transaction.
    pub(crate) async fn insert_room_event_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        message_id: MessageId,
        room_id: RoomId,
        event_kind: EventOutboxKind,
        event: &RoomEvent,
        traceparent: Option<String>,
        event_id: Option<Uuid>,
    ) -> Result<Uuid, sqlx::Error> {
        let payload =
            serde_json::to_value(event).map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        let new = NewEventOutbox {
            message_id,
            event_kind,
            subject: format!("im.room.{room_id}"),
            payload,
            traceparent,
        };
        match event_id {
            Some(event_id) => Self::insert_idempotent_in_tx(tx, new, event_id).await,
            None => Self::insert_in_tx(tx, new).await,
        }
    }

    /// Return the retained outbox row for a canonical message, regardless of
    /// whether it has already been published.
    pub async fn for_message(
        &self,
        message_id: MessageId,
    ) -> Result<Option<EventOutboxRow>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM event_outbox
              WHERE message_id = $1
              ORDER BY aggregate_version
              LIMIT 1"
        );
        let row = sqlx::query_as::<_, DbRow>(&sql)
            .bind(message_id.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// Return the not-yet-published outbox row for a canonical message.
    pub async fn pending_for_message(
        &self,
        message_id: MessageId,
    ) -> Result<Option<EventOutboxRow>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM event_outbox
              WHERE message_id = $1 AND published_at IS NULL
              ORDER BY aggregate_version
              LIMIT 1"
        );
        let row = sqlx::query_as::<_, DbRow>(&sql)
            .bind(message_id.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// Claim up to `limit` due rows.
    ///
    /// A claim increments `attempts` and owns the row until `lease` expires.
    /// Crashed workers therefore leave no permanent `processing` state:
    /// another relay can reclaim the row after the lease. `SKIP LOCKED` keeps
    /// concurrent relays from waiting on or returning the same row.
    pub async fn claim_due(
        &self,
        now: OffsetDateTime,
        lease: Duration,
        limit: i64,
    ) -> Result<Vec<EventOutboxRow>, sqlx::Error> {
        let stale_before = lease_stale_before(now, lease);
        let limit = limit.clamp(1, MAX_CLAIM);
        let sql = format!(
            r"WITH claimable AS (
                  SELECT candidate.id AS claimed_id
                    FROM event_outbox AS candidate
                   WHERE candidate.published_at IS NULL
                     AND candidate.available_at <= $1
                     AND (candidate.claimed_at IS NULL OR candidate.claimed_at <= $2)
                     AND NOT EXISTS (
                           SELECT 1
                             FROM event_outbox AS earlier
                            WHERE earlier.message_id = candidate.message_id
                              AND earlier.aggregate_version < candidate.aggregate_version
                              AND earlier.published_at IS NULL
                     )
                     AND (
                           candidate.event_kind <> 'message'
                           OR candidate.delivery_ordinal IS NULL
                           OR NOT EXISTS (
                               SELECT 1
                                 FROM event_outbox AS earlier_message
                                WHERE earlier_message.subject = candidate.subject
                                  AND earlier_message.event_kind = 'message'
                                  AND earlier_message.delivery_ordinal <
                                      candidate.delivery_ordinal
                                  AND earlier_message.published_at IS NULL
                           )
                     )
                   ORDER BY candidate.available_at ASC,
                            candidate.created_at ASC,
                            candidate.id ASC
                   FOR UPDATE SKIP LOCKED
                   LIMIT $3
              )
              UPDATE event_outbox AS outbox
                SET claimed_at = $1,
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

    /// Claim one known row for the post-commit fast path.
    ///
    /// Returns `None` when the row is not due, its live lease belongs to
    /// another worker, it was already published, or it does not exist.
    pub async fn claim_by_id(
        &self,
        id: Uuid,
        now: OffsetDateTime,
        lease: Duration,
    ) -> Result<Option<EventOutboxRow>, sqlx::Error> {
        let stale_before = lease_stale_before(now, lease);
        let sql = format!(
            r"WITH claimable AS (
                  SELECT candidate.id AS claimed_id
                    FROM event_outbox AS candidate
                   WHERE candidate.id = $1
                     AND candidate.published_at IS NULL
                     AND candidate.available_at <= $2
                     AND (candidate.claimed_at IS NULL OR candidate.claimed_at <= $3)
                     AND NOT EXISTS (
                           SELECT 1
                             FROM event_outbox AS earlier
                            WHERE earlier.message_id = candidate.message_id
                              AND earlier.aggregate_version < candidate.aggregate_version
                              AND earlier.published_at IS NULL
                     )
                     AND (
                           candidate.event_kind <> 'message'
                           OR candidate.delivery_ordinal IS NULL
                           OR NOT EXISTS (
                               SELECT 1
                                 FROM event_outbox AS earlier_message
                                WHERE earlier_message.subject = candidate.subject
                                  AND earlier_message.event_kind = 'message'
                                  AND earlier_message.delivery_ordinal <
                                      candidate.delivery_ordinal
                                  AND earlier_message.published_at IS NULL
                           )
                     )
                   FOR UPDATE SKIP LOCKED
              )
              UPDATE event_outbox AS outbox
                SET claimed_at = $2,
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

    /// Highest appended aggregate version for one message.
    pub async fn latest_aggregate_version(
        &self,
        message_id: MessageId,
    ) -> Result<Option<i64>, sqlx::Error> {
        sqlx::query_scalar("SELECT MAX(aggregate_version) FROM event_outbox WHERE message_id = $1")
            .bind(message_id.to_uuid())
            .fetch_one(&self.pool)
            .await
    }

    /// Store a per-subject sequence only when the row does not already have one.
    ///
    /// Concurrent callers all receive the same persisted value. A published or
    /// missing row returns `None`.
    pub async fn assign_seq_if_absent(
        &self,
        id: Uuid,
        seq: u64,
    ) -> Result<Option<u64>, sqlx::Error> {
        let seq = i64::try_from(seq).map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        let stored = sqlx::query_scalar::<_, i64>(
            r"UPDATE event_outbox
                  SET seq = COALESCE(seq, $2)
                WHERE id = $1 AND published_at IS NULL
            RETURNING seq",
        )
        .bind(id)
        .bind(seq)
        .fetch_optional(&self.pool)
        .await?;
        stored
            .map(u64::try_from)
            .transpose()
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))
    }

    /// Mark a row published. Repeating completion is a harmless no-op.
    pub async fn mark_published(
        &self,
        id: Uuid,
        attempts: i32,
        published_at: OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE event_outbox
                  SET published_at = $3,
                      claimed_at = NULL,
                      last_error = NULL
                WHERE id = $1
                  AND attempts = $2
                  AND claimed_at IS NOT NULL
                  AND published_at IS NULL",
        )
        .bind(id)
        .bind(attempts)
        .bind(published_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Release a failed claim and schedule it with bounded exponential backoff.
    ///
    /// `attempts` is a fencing value from the claimed row. If its lease expired
    /// and another worker reclaimed it in the meantime, this stale failure
    /// cannot release or overwrite the newer claim.
    pub async fn mark_failed(
        &self,
        id: Uuid,
        attempts: i32,
        now: OffsetDateTime,
        error: &str,
    ) -> Result<bool, sqlx::Error> {
        let available_at = now + outbox_backoff_delay(attempts);
        let error = truncate_error(error);
        let result = sqlx::query(
            r"UPDATE event_outbox
                  SET available_at = $3,
                      claimed_at = NULL,
                      last_error = $4
                WHERE id = $1
                  AND attempts = $2
                  AND claimed_at IS NOT NULL
                  AND published_at IS NULL",
        )
        .bind(id)
        .bind(attempts)
        .bind(available_at)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Delete published audit rows older than `cutoff`; pending rows are never
    /// touched.
    pub async fn sweep_published_before(&self, cutoff: OffsetDateTime) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM event_outbox WHERE published_at IS NOT NULL AND published_at < $1",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

/// Retry delay for a claimed row. Attempt one waits one second, then doubles to
/// a five-minute ceiling.
#[must_use]
pub fn outbox_backoff_delay(attempts: i32) -> Duration {
    let shift = u32::try_from(attempts.saturating_sub(1).clamp(0, 62)).unwrap_or(0);
    let raw = BASE_BACKOFF_SECONDS.saturating_mul(1_i64.checked_shl(shift).unwrap_or(i64::MAX));
    Duration::seconds(raw.min(MAX_BACKOFF_SECONDS))
}

fn lease_stale_before(now: OffsetDateTime, lease: Duration) -> OffsetDateTime {
    let seconds = lease.whole_seconds().clamp(1, MAX_LEASE_SECONDS);
    now - Duration::seconds(seconds)
}

fn truncate_error(error: &str) -> String {
    error.chars().take(MAX_ERROR_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_backoff_is_exponential_and_bounded() {
        assert_eq!(outbox_backoff_delay(-5), Duration::seconds(1));
        assert_eq!(outbox_backoff_delay(0), Duration::seconds(1));
        assert_eq!(outbox_backoff_delay(1), Duration::seconds(1));
        assert_eq!(outbox_backoff_delay(2), Duration::seconds(2));
        assert_eq!(outbox_backoff_delay(9), Duration::seconds(256));
        assert_eq!(outbox_backoff_delay(10), Duration::seconds(300));
        assert_eq!(outbox_backoff_delay(i32::MAX), Duration::seconds(300));
    }

    #[test]
    fn error_truncation_is_utf8_safe_and_schema_bounded() {
        let error = "故".repeat(MAX_ERROR_CHARS + 10);
        let truncated = truncate_error(&error);
        assert_eq!(truncated.chars().count(), MAX_ERROR_CHARS);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn lease_is_positive_and_bounded() {
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(2);
        assert_eq!(
            lease_stale_before(now, Duration::ZERO),
            now - Duration::seconds(1)
        );
        assert_eq!(
            lease_stale_before(now, Duration::days(30)),
            now - Duration::seconds(MAX_LEASE_SECONDS)
        );
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::message::{MessageRepo, NewMessage};
    use crate::participant::NewHuman;
    use crate::{ParticipantRepo, RoomRepo, WorkspaceRepo};
    use aero_common::{Block, RoomKind, WorkspaceId, WorkspaceRole};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(8)
            .connect_lazy(&url)
            .expect("connect_lazy never fails for a well-formed URL")
    }

    async fn enroll_default(pool: &PgPool, participant: aero_common::ParticipantId) {
        WorkspaceRepo::new(pool.clone())
            .add_member(
                WorkspaceId::from_uuid(uuid::Uuid::nil()),
                participant,
                WorkspaceRole::Member,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migrations applied"]
    async fn lease_retry_sequence_publish_and_retention_lifecycle() {
        let pool = pool();
        let unique = Uuid::new_v4();
        let participant = ParticipantRepo::new(pool.clone())
            .create_human(NewHuman {
                email: format!("outbox-lifecycle-{unique}@example.test"),
                display_name: format!("outbox-lifecycle-{unique}"),
                password_hash: "test".into(),
            })
            .await
            .unwrap();
        enroll_default(&pool, participant.id).await;
        let room = RoomRepo::new(pool.clone())
            .create(
                RoomKind::Group,
                Some("outbox-lifecycle".into()),
                participant.id,
            )
            .await
            .unwrap();
        let inserted = MessageRepo::new(pool.clone())
            .insert_outboxed(
                NewMessage {
                    room_id: room.id,
                    sender_id: participant.id,
                    blocks: vec![Block::text("lease-safe")],
                    reply_to: None,
                    metadata: serde_json::Value::Null,
                    expires_at: None,
                },
                None,
                vec![participant.id],
                None,
            )
            .await
            .unwrap();
        let repo = EventOutboxRepo::new(pool.clone());
        let now = OffsetDateTime::now_utc();
        let lease = Duration::seconds(30);

        let first = repo
            .claim_by_id(inserted.outbox_id, now, lease)
            .await
            .unwrap()
            .expect("first claim");
        assert_eq!(first.attempts, 1);
        assert!(repo
            .claim_by_id(inserted.outbox_id, now + Duration::seconds(1), lease)
            .await
            .unwrap()
            .is_none());

        let reclaimed = repo
            .claim_by_id(inserted.outbox_id, now + Duration::seconds(31), lease)
            .await
            .unwrap()
            .expect("expired lease is reclaimable");
        assert_eq!(reclaimed.attempts, 2);
        assert_eq!(
            repo.assign_seq_if_absent(inserted.outbox_id, 41)
                .await
                .unwrap(),
            Some(41)
        );
        assert_eq!(
            repo.assign_seq_if_absent(inserted.outbox_id, 99)
                .await
                .unwrap(),
            Some(41)
        );

        assert!(!repo
            .mark_failed(
                inserted.outbox_id,
                first.attempts,
                now + Duration::seconds(31),
                "stale worker",
            )
            .await
            .unwrap());
        let failed_at = now + Duration::seconds(31);
        assert!(repo
            .mark_failed(
                inserted.outbox_id,
                reclaimed.attempts,
                failed_at,
                &"失".repeat(MAX_ERROR_CHARS + 10),
            )
            .await
            .unwrap());
        assert!(repo
            .claim_by_id(inserted.outbox_id, failed_at, lease)
            .await
            .unwrap()
            .is_none());

        let retry_at = failed_at + outbox_backoff_delay(reclaimed.attempts);
        let retried = repo
            .claim_by_id(inserted.outbox_id, retry_at, lease)
            .await
            .unwrap()
            .expect("backoff elapsed");
        assert_eq!(retried.id, inserted.outbox_id);
        assert_eq!(retried.attempts, 3);
        assert_eq!(retried.seq, Some(41));
        assert_eq!(
            retried
                .last_error
                .as_deref()
                .expect("failure retained")
                .chars()
                .count(),
            MAX_ERROR_CHARS
        );

        let published_at = retry_at + Duration::seconds(1);
        assert!(repo
            .mark_published(inserted.outbox_id, retried.attempts, published_at)
            .await
            .unwrap());
        assert!(repo
            .pending_for_message(inserted.message().id)
            .await
            .unwrap()
            .is_none());
        assert!(repo
            .claim_by_id(
                inserted.outbox_id,
                published_at + Duration::seconds(60),
                lease,
            )
            .await
            .unwrap()
            .is_none());
        assert!(!repo
            .mark_published(inserted.outbox_id, retried.attempts, published_at)
            .await
            .unwrap());
        assert!(
            repo.sweep_published_before(published_at + Duration::seconds(1))
                .await
                .unwrap()
                >= 1
        );
        assert!(repo
            .for_message(inserted.message().id)
            .await
            .unwrap()
            .is_none());

        sqlx::query("DELETE FROM messages WHERE room_id = $1")
            .bind(room.id.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM room_members WHERE room_id = $1")
            .bind(room.id.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.id.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(participant.id.to_uuid())
            .execute(&pool)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migrations applied"]
    async fn later_room_message_waits_for_retrying_earlier_ordinal() {
        let pool = pool();
        let unique = Uuid::new_v4();
        let participant = ParticipantRepo::new(pool.clone())
            .create_human(NewHuman {
                email: format!("outbox-order-{unique}@example.test"),
                display_name: format!("outbox-order-{unique}"),
                password_hash: "test".into(),
            })
            .await
            .unwrap();
        enroll_default(&pool, participant.id).await;
        let room = RoomRepo::new(pool.clone())
            .create(RoomKind::Group, Some("outbox-order".into()), participant.id)
            .await
            .unwrap();
        let messages = MessageRepo::new(pool.clone());
        let first = messages
            .insert_outboxed(
                NewMessage {
                    room_id: room.id,
                    sender_id: participant.id,
                    blocks: vec![Block::text("first")],
                    reply_to: None,
                    metadata: serde_json::Value::Null,
                    expires_at: None,
                },
                None,
                vec![participant.id],
                None,
            )
            .await
            .unwrap();
        let second = messages
            .insert_outboxed(
                NewMessage {
                    room_id: room.id,
                    sender_id: participant.id,
                    blocks: vec![Block::text("second")],
                    reply_to: None,
                    metadata: serde_json::Value::Null,
                    expires_at: None,
                },
                None,
                vec![participant.id],
                None,
            )
            .await
            .unwrap();

        let repo = EventOutboxRepo::new(pool);
        let now = OffsetDateTime::now_utc();
        let lease = Duration::seconds(30);
        assert!(
            repo.claim_by_id(second.outbox_id, now, lease)
                .await
                .unwrap()
                .is_none(),
            "a later ordinal cannot publish around an earlier pending message"
        );
        let first_claim = repo
            .claim_by_id(first.outbox_id, now, lease)
            .await
            .unwrap()
            .expect("earliest room ordinal is claimable");
        let first_ordinal = first_claim.delivery_ordinal.expect("creation ordinal");
        let second_row = repo
            .for_message(second.message().id)
            .await
            .unwrap()
            .expect("second outbox row");
        assert!(first_ordinal < second_row.delivery_ordinal.expect("second ordinal"));

        assert!(repo
            .mark_failed(first_claim.id, first_claim.attempts, now, "transient")
            .await
            .unwrap());
        let retry_at = now + outbox_backoff_delay(first_claim.attempts);
        assert!(
            repo.claim_by_id(second.outbox_id, retry_at, lease)
                .await
                .unwrap()
                .is_none(),
            "bounded retry has no terminal zombie state and keeps the prefix closed"
        );
        let retry = repo
            .claim_by_id(first.outbox_id, retry_at, lease)
            .await
            .unwrap()
            .expect("failed earliest row remains retryable");
        assert!(repo
            .mark_published(retry.id, retry.attempts, retry_at)
            .await
            .unwrap());
        assert!(
            repo.claim_by_id(second.outbox_id, retry_at + Duration::seconds(1), lease,)
                .await
                .unwrap()
                .is_some(),
            "publishing the earlier ordinal unblocks the next message"
        );
    }
}
