//! Recurring scheduled-message repository (repeating "Send later").
//!
//! Backs `migrations/0041_recurring_messages.sql`. Complements the one-shot
//! [`ScheduledRepo`](crate::ScheduledRepo): a user stages a message that is
//! re-posted to a room on a repeating cadence (`hourly` / `daily` / `weekly`).
//! Each row carries its next firing time in `next_run`; the recurring dispatcher
//! leases one occurrence, replays its snapshotted payload through the normal
//! send path, and advances `next_run` only after a fenced success confirmation.
//! Failures retain the same occurrence key/payload and are re-parked with
//! backoff. Cancelling a series flips `active = false`.
//!
//! Purely additive: a NEW [`RecurringMessageRepo`]; no existing repo is touched.
//! The [`RecurringMessage`] model lives here (and is re-exported from the crate
//! root) rather than in `aero-common`, since it is a storage-layer projection.
//! The cadence arithmetic is the pure, db-free [`next_occurrence`].

use aero_common::{Error, ParticipantId, RecurringMessageId, RoomId};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};

use crate::message::authorization::PostPolicy;
use crate::scheduled::lock_deferred_room_access;

/// Advance `from` by one step of `cadence`, or `None` for an unknown cadence.
///
/// Pure and database-free, so it unit-tests without a Postgres. The recognised
/// cadences are `"hourly"` (`+1h`), `"daily"` (`+1d`), and `"weekly"` (`+7d`);
/// any other string yields `None`, which callers treat as a validation error.
#[must_use]
pub fn next_occurrence(cadence: &str, from: time::OffsetDateTime) -> Option<time::OffsetDateTime> {
    let step = match cadence {
        "hourly" => time::Duration::hours(1),
        "daily" => time::Duration::days(1),
        "weekly" => time::Duration::days(7),
        _ => return None,
    };
    Some(from + step)
}

/// One recurring scheduled message — a message re-posted on a repeating cadence.
///
/// A storage-layer projection of a `recurring_messages` row. `Serialize` so a
/// handler can hand the row straight back as JSON; the timestamps render as
/// RFC 3339, and `blocks` is the raw stored payload (a JSON array of blocks).
#[derive(Debug, Clone, Serialize)]
pub struct RecurringMessage {
    /// The recurring message's unique id.
    pub id: RecurringMessageId,
    /// The room the message is re-posted to.
    pub room_id: RoomId,
    /// The sender the message is posted as (and is owner-scoped to).
    pub sender_id: ParticipantId,
    /// The message payload (a JSON array of blocks), replayed on each firing.
    pub blocks: serde_json::Value,
    /// The cadence (`hourly` / `daily` / `weekly`).
    pub cadence: String,
    /// When the series next fires (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub next_run: time::OffsetDateTime,
    /// Whether the series is still live (cancelling flips this to `false`).
    pub active: bool,
    /// When the series was created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    /// Attempts made for the currently due occurrence.
    pub delivery_attempts: i32,
    /// Earliest retry time after a transient failure.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub retry_at: Option<time::OffsetDateTime>,
    /// Last delivery failure, retained when the series is disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Terminal delivery failure timestamp. A dead series is returned by list
    /// APIs with `active = false` until its owner cancels it.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub dead_at: Option<time::OffsetDateTime>,
}

/// One leased occurrence of a recurring message.
#[derive(Debug, Clone)]
pub struct RecurringDeliveryClaim {
    pub series: RecurringMessage,
    pub delivery_key: uuid::Uuid,
    pub payload: serde_json::Value,
    pub claim_token: uuid::Uuid,
    pub attempt: i32,
    pub lease_expires_at: time::OffsetDateTime,
}

/// Result of a token-fenced recurring occurrence failure settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecurringFailureDisposition {
    RetryScheduled,
    Dead,
    FenceLost,
}

#[derive(Debug, sqlx::FromRow)]
struct ClaimRow {
    id: uuid::Uuid,
    room_id: uuid::Uuid,
    sender_id: uuid::Uuid,
    blocks: serde_json::Value,
    cadence: String,
    next_run: time::OffsetDateTime,
    active: bool,
    created_at: time::OffsetDateTime,
    retry_at: Option<time::OffsetDateTime>,
    last_error: Option<String>,
    dead_at: Option<time::OffsetDateTime>,
    delivery_key: uuid::Uuid,
    delivery_payload: serde_json::Value,
    claim_token: uuid::Uuid,
    delivery_attempts: i32,
    lease_expires_at: time::OffsetDateTime,
}

fn claim_row_to_model(row: ClaimRow) -> RecurringDeliveryClaim {
    RecurringDeliveryClaim {
        series: RecurringMessage {
            id: RecurringMessageId::from_uuid(row.id),
            room_id: RoomId::from_uuid(row.room_id),
            sender_id: ParticipantId::from_uuid(row.sender_id),
            blocks: row.blocks,
            cadence: row.cadence,
            next_run: row.next_run,
            active: row.active,
            created_at: row.created_at,
            delivery_attempts: row.delivery_attempts,
            retry_at: row.retry_at,
            last_error: row.last_error,
            dead_at: row.dead_at,
        },
        delivery_key: row.delivery_key,
        payload: row.delivery_payload,
        claim_token: row.claim_token,
        attempt: row.delivery_attempts,
        lease_expires_at: row.lease_expires_at,
    }
}

/// The columns a [`RecurringMessage`] is built from, in select order. Shared by
/// every query so the row decoding stays in one place.
const COLUMNS: &str = "id, room_id, sender_id, blocks, cadence, next_run, active, created_at,
    delivery_attempts, retry_at, last_error, dead_at";

type Row = (
    uuid::Uuid,
    uuid::Uuid,
    uuid::Uuid,
    serde_json::Value,
    String,
    time::OffsetDateTime,
    bool,
    time::OffsetDateTime,
    i32,
    Option<time::OffsetDateTime>,
    Option<String>,
    Option<time::OffsetDateTime>,
);

fn row_to_model(r: Row) -> RecurringMessage {
    let (
        id,
        room_id,
        sender_id,
        blocks,
        cadence,
        next_run,
        active,
        created_at,
        delivery_attempts,
        retry_at,
        last_error,
        dead_at,
    ) = r;
    RecurringMessage {
        id: RecurringMessageId::from_uuid(id),
        room_id: RoomId::from_uuid(room_id),
        sender_id: ParticipantId::from_uuid(sender_id),
        blocks,
        cadence,
        next_run,
        active,
        created_at,
        delivery_attempts,
        retry_at,
        last_error,
        dead_at,
    }
}

#[derive(Debug)]
struct LockedRecurringTarget {
    room: RoomId,
    active: bool,
    claim_token: Option<uuid::Uuid>,
    dead_at: Option<time::OffsetDateTime>,
}

async fn lock_owned_recurring_target(
    tx: &mut Transaction<'_, Postgres>,
    requested_room: Option<RoomId>,
    id: RecurringMessageId,
    actor: ParticipantId,
    post_policy: PostPolicy,
) -> Result<LockedRecurringTarget, Error> {
    let room = if let Some(room) = requested_room {
        lock_deferred_room_access(tx, room, actor, post_policy).await?;
        room
    } else {
        let resolved = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid)>(
            "SELECT room_id, sender_id FROM recurring_messages WHERE id = $1",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut **tx)
        .await?;
        let Some((room, sender)) = resolved else {
            return Err(Error::NotFound(format!("recurring message {id}")));
        };
        if sender != actor.to_uuid() {
            return Err(Error::NotFound(format!("recurring message {id}")));
        }
        let room = RoomId::from_uuid(room);
        lock_deferred_room_access(tx, room, actor, post_policy).await?;
        room
    };

    let locked = sqlx::query_as::<
        _,
        (
            uuid::Uuid,
            uuid::Uuid,
            bool,
            Option<uuid::Uuid>,
            Option<time::OffsetDateTime>,
        ),
    >(
        r"SELECT room_id, sender_id, active, claim_token, dead_at
            FROM recurring_messages
           WHERE id = $1
             AND room_id = $2
           FOR UPDATE",
    )
    .bind(id.to_uuid())
    .bind(room.to_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    let Some((locked_room, sender, active, claim_token, dead_at)) = locked else {
        return Err(Error::NotFound(format!("recurring message {id}")));
    };
    if locked_room != room.to_uuid() {
        return Err(Error::Conflict(
            "recurring message room identity changed".into(),
        ));
    }
    if sender != actor.to_uuid() {
        return Err(Error::NotFound(format!("recurring message {id}")));
    }
    Ok(LockedRecurringTarget {
        room,
        active,
        claim_token,
        dead_at,
    })
}

async fn lock_accessible_recurring_rooms(
    tx: &mut Transaction<'_, Postgres>,
    sender: ParticipantId,
) -> Result<Vec<uuid::Uuid>, Error> {
    let rooms = sqlx::query_scalar::<_, uuid::Uuid>(
        r"SELECT DISTINCT room_id
            FROM recurring_messages
           WHERE sender_id = $1
           ORDER BY room_id",
    )
    .bind(sender.to_uuid())
    .fetch_all(&mut **tx)
    .await?;
    let mut accessible = Vec::with_capacity(rooms.len());
    for raw_room in rooms {
        let room = RoomId::from_uuid(raw_room);
        if crate::message::authorization::lock_effective_message_write_access(
            tx,
            room,
            sender,
            PostPolicy::Ignore,
        )
        .await?
        .is_some()
        {
            accessible.push(raw_room);
        }
    }
    Ok(accessible)
}

/// Repository over the `recurring_messages` table (repeating "Send later").
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`RecurringMessageRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct RecurringMessageRepo {
    pool: PgPool,
}

impl RecurringMessageRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persist a new recurring message while holding the sender's canonical
    /// effective room-access fence.
    ///
    /// # Errors
    /// Propagates database failures and stable authorization errors.
    pub async fn create_authorized(
        &self,
        room: RoomId,
        sender: ParticipantId,
        blocks: &serde_json::Value,
        cadence: &str,
        first_run: time::OffsetDateTime,
    ) -> Result<RecurringMessageId, Error> {
        let id = RecurringMessageId::new();
        let mut tx = self.pool.begin().await?;
        lock_deferred_room_access(&mut tx, room, sender, PostPolicy::Enforce).await?;
        sqlx::query(
            r"INSERT INTO recurring_messages
                  (id, room_id, sender_id, blocks, cadence, next_run)
               VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(sender.to_uuid())
        .bind(blocks)
        .bind(cadence)
        .bind(first_run)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// List active and terminal-failed recurring messages for `room`, newest
    /// first. Canceled series remain hidden; dead rows retain `last_error` so
    /// owners can see why delivery stopped.
    ///
    /// # Errors
    /// Propagates database failures and current-access errors.
    pub async fn list_for_room_authorized(
        &self,
        room: RoomId,
        actor: ParticipantId,
    ) -> Result<Vec<RecurringMessage>, Error> {
        let mut tx = self.pool.begin().await?;
        lock_deferred_room_access(&mut tx, room, actor, PostPolicy::Ignore).await?;
        let sql = format!(
            "SELECT {COLUMNS}
               FROM recurring_messages
              WHERE room_id = $1
                AND (active OR dead_at IS NOT NULL)
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(room.to_uuid())
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Owner-global inventory filtered to rooms the owner can still access.
    pub async fn list_for_sender_authorized(
        &self,
        sender: ParticipantId,
    ) -> Result<Vec<RecurringMessage>, Error> {
        let mut tx = self.pool.begin().await?;
        let accessible_rooms = lock_accessible_recurring_rooms(&mut tx, sender).await?;
        if accessible_rooms.is_empty() {
            tx.commit().await?;
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT {COLUMNS}
               FROM recurring_messages
              WHERE sender_id = $1
                AND room_id = ANY($2)
                AND (active OR dead_at IS NOT NULL)
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(sender.to_uuid())
            .bind(&accessible_rooms)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Return one visible recurring series after binding its opaque id to an
    /// optional path room and the immutable owner.
    pub async fn get_authorized(
        &self,
        requested_room: Option<RoomId>,
        id: RecurringMessageId,
        actor: ParticipantId,
    ) -> Result<RecurringMessage, Error> {
        let mut tx = self.pool.begin().await?;
        let target =
            lock_owned_recurring_target(&mut tx, requested_room, id, actor, PostPolicy::Ignore)
                .await?;
        if !target.active && target.dead_at.is_none() {
            return Err(Error::NotFound(format!("recurring message {id}")));
        }
        let sql = format!("SELECT {COLUMNS} FROM recurring_messages WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(row_to_model(row))
    }

    /// Lease active occurrences due at `now` using the default retry bound. A
    /// first claim snapshots `blocks`
    /// and mints the occurrence's stable delivery key; retries retain both.
    /// Expired leases are reclaimable with a new token and incremented attempt.
    pub async fn claim_due(
        &self,
        now: time::OffsetDateTime,
        lease: time::Duration,
        limit: i64,
    ) -> Result<Vec<RecurringDeliveryClaim>, sqlx::Error> {
        self.claim_due_with_policy(now, lease, 8, limit).await
    }

    /// Lease due occurrences and terminalize an expired claim once its bounded
    /// attempt budget is exhausted.
    pub async fn claim_due_with_policy(
        &self,
        now: time::OffsetDateTime,
        lease: time::Duration,
        max_attempts: i32,
        limit: i64,
    ) -> Result<Vec<RecurringDeliveryClaim>, sqlx::Error> {
        let lease_secs = lease.whole_seconds().clamp(1, 3600);
        let max_attempts = max_attempts.clamp(1, 100);
        let limit = limit.clamp(1, 500);
        let rows = sqlx::query_as::<_, ClaimRow>(
            "WITH exhausted_ids AS (
                 SELECT id
                   FROM recurring_messages
                  WHERE active
                    AND claim_token IS NOT NULL
                    AND lease_expires_at <= $1
                    AND delivery_attempts >= $4
                  ORDER BY lease_expires_at, id
                  FOR UPDATE SKIP LOCKED
                  LIMIT $2
             ),
             exhausted AS (
                 UPDATE recurring_messages AS recurring
                    SET active = false,
                        dead_at = $1,
                        last_error = COALESCE(
                            recurring.last_error,
                            'delivery lease expired after maximum attempts'
                        ),
                        claim_token = NULL,
                        claimed_at = NULL,
                        lease_expires_at = NULL,
                        delivery_key = NULL,
                        delivery_payload = NULL,
                        retry_at = NULL
                   FROM exhausted_ids
                  WHERE recurring.id = exhausted_ids.id
             ),
             due AS (
                 SELECT id
                   FROM recurring_messages
                  WHERE active
                    AND next_run <= $1
                    AND COALESCE(retry_at, next_run) <= $1
                    AND delivery_attempts < $4
                    AND (
                        claim_token IS NULL
                        OR lease_expires_at <= $1
                    )
                  ORDER BY COALESCE(retry_at, next_run), next_run, id
                  FOR UPDATE SKIP LOCKED
                  LIMIT $2
             )
             UPDATE recurring_messages AS recurring
                SET claim_token = gen_random_uuid(),
                    claimed_at = $1,
                    lease_expires_at =
                        $1 + make_interval(secs => $3::double precision),
                    delivery_key = COALESCE(recurring.delivery_key, gen_random_uuid()),
                    delivery_payload = COALESCE(recurring.delivery_payload, recurring.blocks),
                    delivery_attempts = recurring.delivery_attempts + 1
               FROM due
              WHERE recurring.id = due.id
          RETURNING recurring.id,
                    recurring.room_id,
                    recurring.sender_id,
                    recurring.blocks,
                    recurring.cadence,
                    recurring.next_run,
                    recurring.active,
                    recurring.created_at,
                    recurring.retry_at,
                    recurring.last_error,
                    recurring.dead_at,
                    recurring.delivery_key,
                    recurring.delivery_payload,
                    recurring.claim_token,
                    recurring.delivery_attempts,
                    recurring.lease_expires_at",
        )
        .bind(now)
        .bind(limit)
        .bind(lease_secs)
        .bind(max_attempts)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(claim_row_to_model).collect())
    }

    /// Advance the cadence only when this worker still owns the exact occurrence
    /// token/generation. Success clears the snapshot so the next occurrence gets
    /// a fresh idempotency key and attempt count.
    #[allow(clippy::too_many_arguments)]
    pub async fn confirm_sent(
        &self,
        id: RecurringMessageId,
        claim_token: uuid::Uuid,
        delivery_key: uuid::Uuid,
        attempt: i32,
        next_run: time::OffsetDateTime,
        sent_at: time::OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE recurring_messages
                SET next_run = $5,
                    last_sent_at = $6,
                    claim_token = NULL,
                    claimed_at = NULL,
                    lease_expires_at = NULL,
                    delivery_key = NULL,
                    delivery_payload = NULL,
                    delivery_attempts = 0,
                    retry_at = NULL,
                    last_error = NULL,
                    dead_at = NULL
              WHERE id = $1
                AND active
                AND claim_token = $2
                AND delivery_key = $3
                AND delivery_attempts = $4",
        )
        .bind(id.to_uuid())
        .bind(claim_token)
        .bind(delivery_key)
        .bind(attempt)
        .bind(next_run)
        .bind(sent_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Re-park a transient failure without moving `next_run`, or disable the
    /// series in visible dead state after a permanent/exhausted failure.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_failure(
        &self,
        id: RecurringMessageId,
        claim_token: uuid::Uuid,
        delivery_key: uuid::Uuid,
        attempt: i32,
        retry_at: time::OffsetDateTime,
        error: &str,
        retryable: bool,
        max_attempts: i32,
    ) -> Result<RecurringFailureDisposition, sqlx::Error> {
        let max_attempts = max_attempts.clamp(1, 100);
        let state: Option<(bool,)> = sqlx::query_as(
            "UPDATE recurring_messages
                SET active = CASE
                        WHEN $7 AND delivery_attempts < $8 THEN true
                        ELSE false
                    END,
                    claim_token = NULL,
                    claimed_at = NULL,
                    lease_expires_at = NULL,
                    retry_at = CASE
                        WHEN $7 AND delivery_attempts < $8 THEN $5
                        ELSE NULL
                    END,
                    last_error = left($6, 2048),
                    dead_at = CASE
                        WHEN $7 AND delivery_attempts < $8 THEN NULL
                        ELSE now()
                    END,
                    delivery_key = CASE
                        WHEN $7 AND delivery_attempts < $8 THEN delivery_key
                        ELSE NULL
                    END,
                    delivery_payload = CASE
                        WHEN $7 AND delivery_attempts < $8 THEN delivery_payload
                        ELSE NULL
                    END
              WHERE id = $1
                AND active
                AND claim_token = $2
                AND delivery_key = $3
                AND delivery_attempts = $4
          RETURNING active",
        )
        .bind(id.to_uuid())
        .bind(claim_token)
        .bind(delivery_key)
        .bind(attempt)
        .bind(retry_at)
        .bind(error)
        .bind(retryable)
        .bind(max_attempts)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match state {
            Some((true,)) => RecurringFailureDisposition::RetryScheduled,
            Some((false,)) => RecurringFailureDisposition::Dead,
            None => RecurringFailureDisposition::FenceLost,
        })
    }

    /// Cancel one owner series after current room access and immutable identity
    /// are locked in the write transaction. Active worker leases remain fenced.
    ///
    /// # Errors
    /// Returns stable NotFound/Forbidden/Conflict errors.
    pub async fn cancel_authorized(
        &self,
        requested_room: Option<RoomId>,
        id: RecurringMessageId,
        sender: ParticipantId,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        let target =
            lock_owned_recurring_target(&mut tx, requested_room, id, sender, PostPolicy::Ignore)
                .await?;
        if (!target.active && target.dead_at.is_none()) || target.claim_token.is_some() {
            return Err(Error::Conflict(
                "recurring message cannot be canceled in its current state".into(),
            ));
        }
        let result = sqlx::query(
            r"UPDATE recurring_messages
                 SET active = false,
                     claim_token = NULL,
                     claimed_at = NULL,
                     lease_expires_at = NULL,
                     delivery_key = NULL,
                     delivery_payload = NULL,
                     delivery_attempts = 0,
                     retry_at = NULL,
                     last_error = NULL,
                     dead_at = NULL
               WHERE id = $1
                 AND sender_id = $2
                 AND room_id = $3
                 AND (active OR dead_at IS NOT NULL)
                 AND claim_token IS NULL",
        )
        .bind(id.to_uuid())
        .bind(sender.to_uuid())
        .bind(target.room.to_uuid())
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Err(Error::Conflict(
                "recurring message state changed during cancellation".into(),
            ));
        }
        tx.commit().await?;
        Ok(())
    }

    // Historical repository tests keep their compact call shape, but these
    // shims do not exist in production builds.
    #[cfg(test)]
    pub async fn create(
        &self,
        room: RoomId,
        sender: ParticipantId,
        blocks: &serde_json::Value,
        cadence: &str,
        first_run: time::OffsetDateTime,
    ) -> Result<RecurringMessageId, Error> {
        self.create_authorized(room, sender, blocks, cadence, first_run)
            .await
    }

    #[cfg(test)]
    pub async fn list_for_room(&self, room: RoomId) -> Result<Vec<RecurringMessage>, Error> {
        let actor =
            sqlx::query_scalar::<_, uuid::Uuid>("SELECT created_by FROM rooms WHERE id = $1")
                .bind(room.to_uuid())
                .fetch_optional(&self.pool)
                .await?
                .ok_or_else(|| Error::NotFound(format!("room {room}")))?;
        self.list_for_room_authorized(room, ParticipantId::from_uuid(actor))
            .await
    }

    #[cfg(test)]
    pub async fn cancel(
        &self,
        id: RecurringMessageId,
        sender: ParticipantId,
    ) -> Result<bool, Error> {
        match self.cancel_authorized(None, id, sender).await {
            Ok(()) => Ok(true),
            Err(Error::NotFound(_) | Error::Forbidden(_) | Error::Conflict(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_occurrence_steps_by_cadence() {
        let from = time::OffsetDateTime::UNIX_EPOCH;
        assert_eq!(
            next_occurrence("hourly", from),
            Some(from + time::Duration::hours(1))
        );
        assert_eq!(
            next_occurrence("daily", from),
            Some(from + time::Duration::days(1))
        );
        assert_eq!(
            next_occurrence("weekly", from),
            Some(from + time::Duration::days(7))
        );
        assert_eq!(next_occurrence("yearly", from), None);
        assert_eq!(next_occurrence("", from), None);
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored recurring_message
/// ```
#[cfg(test)]
#[path = "recurring_message/db_tests.rs"]
mod db_tests;

#[cfg(test)]
#[path = "recurring_message/authorization_tests.rs"]
mod authorization_tests;
