//! Scheduled-message repository ("Send later" + reminders).
//!
//! Backs `migrations/0016_scheduled.sql` and the leased delivery state added by
//! `0184_scheduled_delivery_state.sql`. A user composes a message now; the row
//! sits here until its `scheduled_at` passes, at which point a delivery worker
//! claims a lease and replays it through the normal send path. A reminder is
//! just a self-targeted scheduled note.
//!
//! Purely additive: a NEW [`ScheduledRepo`]; no existing repo is touched. The
//! [`ScheduledMessage`] model lives here (and is re-exported from the crate root)
//! rather than in `aero-common`, since it is a storage-layer projection.
//!
//! The due-poll is concurrency-safe: it selects + leases rows in a single
//! `UPDATE ... RETURNING` over a `FOR UPDATE SKIP LOCKED` subquery. Claiming is
//! deliberately **not** delivery. Success must be fenced with
//! [`confirm_delivered`](ScheduledRepo::confirm_delivered); failures are
//! re-parked with backoff, and expired leases can be reclaimed.

use aero_common::{Block, Error, MessageId, ParticipantId, RoomId, ScheduledMessageId};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};

use crate::message::authorization::{lock_effective_message_write_access, PostPolicy};

/// One scheduled (not-yet-delivered, or already delivered/canceled) message.
#[derive(Debug, Clone, Serialize)]
pub struct ScheduledMessage {
    pub id: ScheduledMessageId,
    pub room_id: RoomId,
    pub sender_id: ParticipantId,
    pub blocks: Vec<Block>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<MessageId>,
    #[serde(with = "time::serde::rfc3339")]
    pub scheduled_at: time::OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub delivered_at: Option<time::OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub canceled_at: Option<time::OffsetDateTime>,
    /// Durable delivery state: `pending`, `claimed`, `delivered`, `dead`, or
    /// `canceled`.
    pub delivery_status: String,
    /// Number of attempts in the current explicit-retry generation.
    pub attempts: i32,
    /// Explicit owner retries increment this generation while retaining the
    /// stable sender idempotency key.
    pub delivery_generation: i32,
    /// Earliest retry eligibility (equal to `scheduled_at` before first claim).
    #[serde(with = "time::serde::rfc3339")]
    pub available_at: time::OffsetDateTime,
    /// Last delivery error, truncated by the repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// When the row entered terminal failed state.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub dead_at: Option<time::OffsetDateTime>,
}

/// A leased scheduled-message delivery. Both the random token and monotonically
/// increasing attempt must match when settling it, preventing an expired worker
/// from confirming or re-parking a newer owner's claim (including ABA cycles).
#[derive(Debug, Clone)]
pub struct ScheduledDeliveryClaim {
    pub message: ScheduledMessage,
    pub claim_token: uuid::Uuid,
    pub attempt: i32,
    pub generation: i32,
    pub lease_expires_at: time::OffsetDateTime,
}

/// Result of a fenced failure settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduledFailureDisposition {
    RetryScheduled,
    Dead,
    FenceLost,
}

/// Largest batch [`ScheduledRepo::claim_due`] will return regardless of `limit`,
/// bounding the work one dispatcher tick does. Clamping is pure so it unit-tests
/// without a database.
const MAX_CLAIM: i64 = 500;
const DEFAULT_LEASE_SECS: i64 = 300;
const DEFAULT_MAX_ATTEMPTS: i32 = 8;

/// Clamp a requested claim batch into `1..=MAX_CLAIM` (defaulting non-positive to
/// `1`, since a non-positive `LIMIT` would claim nothing useful).
fn clamp_claim(requested: i64) -> i64 {
    requested.clamp(1, MAX_CLAIM)
}

/// The columns a `ScheduledMessage` is built from, in select order. Shared by
/// every query so the row decoding stays in one place.
const COLUMNS: &str = "id, room_id, sender_id, blocks, reply_to, scheduled_at, created_at,
    delivered_at, canceled_at, delivery_status, attempts, delivery_generation,
    available_at, last_error, dead_at";
const CLAIM_COLUMNS: &str = "scheduled.id AS id,
    scheduled.room_id AS room_id,
    scheduled.sender_id AS sender_id,
    scheduled.blocks AS blocks,
    scheduled.reply_to AS reply_to,
    scheduled.scheduled_at AS scheduled_at,
    scheduled.created_at AS created_at,
    scheduled.delivered_at AS delivered_at,
    scheduled.canceled_at AS canceled_at,
    scheduled.delivery_status AS delivery_status,
    scheduled.attempts AS attempts,
    scheduled.delivery_generation AS delivery_generation,
    scheduled.available_at AS available_at,
    scheduled.last_error AS last_error,
    scheduled.dead_at AS dead_at";

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: uuid::Uuid,
    room_id: uuid::Uuid,
    sender_id: uuid::Uuid,
    blocks: serde_json::Value,
    reply_to: Option<uuid::Uuid>,
    scheduled_at: time::OffsetDateTime,
    created_at: time::OffsetDateTime,
    delivered_at: Option<time::OffsetDateTime>,
    canceled_at: Option<time::OffsetDateTime>,
    delivery_status: String,
    attempts: i32,
    delivery_generation: i32,
    available_at: time::OffsetDateTime,
    last_error: Option<String>,
    dead_at: Option<time::OffsetDateTime>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct ClaimRow {
    id: uuid::Uuid,
    room_id: uuid::Uuid,
    sender_id: uuid::Uuid,
    blocks: serde_json::Value,
    reply_to: Option<uuid::Uuid>,
    scheduled_at: time::OffsetDateTime,
    created_at: time::OffsetDateTime,
    delivered_at: Option<time::OffsetDateTime>,
    canceled_at: Option<time::OffsetDateTime>,
    delivery_status: String,
    attempts: i32,
    delivery_generation: i32,
    available_at: time::OffsetDateTime,
    last_error: Option<String>,
    dead_at: Option<time::OffsetDateTime>,
    claim_token: uuid::Uuid,
    lease_expires_at: time::OffsetDateTime,
}

fn row_to_model(r: Row) -> ScheduledMessage {
    ScheduledMessage {
        id: ScheduledMessageId::from_uuid(r.id),
        room_id: RoomId::from_uuid(r.room_id),
        sender_id: ParticipantId::from_uuid(r.sender_id),
        blocks: serde_json::from_value(r.blocks).unwrap_or_default(),
        reply_to: r.reply_to.map(MessageId::from_uuid),
        scheduled_at: r.scheduled_at,
        created_at: r.created_at,
        delivered_at: r.delivered_at,
        canceled_at: r.canceled_at,
        delivery_status: r.delivery_status,
        attempts: r.attempts,
        delivery_generation: r.delivery_generation,
        available_at: r.available_at,
        last_error: r.last_error,
        dead_at: r.dead_at,
    }
}

fn claim_row_to_model(r: ClaimRow) -> ScheduledDeliveryClaim {
    let attempt = r.attempts;
    let generation = r.delivery_generation;
    ScheduledDeliveryClaim {
        message: ScheduledMessage {
            id: ScheduledMessageId::from_uuid(r.id),
            room_id: RoomId::from_uuid(r.room_id),
            sender_id: ParticipantId::from_uuid(r.sender_id),
            blocks: serde_json::from_value(r.blocks).unwrap_or_default(),
            reply_to: r.reply_to.map(MessageId::from_uuid),
            scheduled_at: r.scheduled_at,
            created_at: r.created_at,
            delivered_at: r.delivered_at,
            canceled_at: r.canceled_at,
            delivery_status: r.delivery_status,
            attempts: r.attempts,
            delivery_generation: r.delivery_generation,
            available_at: r.available_at,
            last_error: r.last_error,
            dead_at: r.dead_at,
        },
        claim_token: r.claim_token,
        attempt,
        generation,
        lease_expires_at: r.lease_expires_at,
    }
}

/// Acquire the canonical workspace -> room -> membership fence used by every
/// deferred-message user operation. A missing room remains a stable 404 while
/// a live room that the actor can no longer enter is a stable 403.
pub(crate) async fn lock_deferred_room_access(
    tx: &mut Transaction<'_, Postgres>,
    room: RoomId,
    actor: ParticipantId,
    post_policy: PostPolicy,
) -> Result<(), Error> {
    if lock_effective_message_write_access(tx, room, actor, post_policy)
        .await?
        .is_some()
    {
        return Ok(());
    }

    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM rooms WHERE id = $1)")
        .bind(room.to_uuid())
        .fetch_one(&mut **tx)
        .await?;
    if exists {
        Err(Error::Forbidden(format!(
            "participant {actor} cannot access room {room}"
        )))
    } else {
        Err(Error::NotFound(format!("room {room}")))
    }
}

async fn lock_live_reply_parent(
    tx: &mut Transaction<'_, Postgres>,
    reply_to: Option<MessageId>,
    room: RoomId,
) -> Result<(), Error> {
    let Some(reply_to) = reply_to else {
        return Ok(());
    };
    let exists = sqlx::query_scalar::<_, bool>(
        r"SELECT true
            FROM messages
           WHERE id = $1
             AND room_id = $2
             AND deleted_at IS NULL
           FOR SHARE",
    )
    .bind(reply_to.to_uuid())
    .bind(room.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(false);
    if exists {
        Ok(())
    } else {
        Err(Error::NotFound(format!("reply message {reply_to}")))
    }
}

#[derive(Debug)]
struct LockedScheduledTarget {
    room: RoomId,
    delivery_status: String,
    attempts: i32,
    delivery_generation: i32,
    canceled_at: Option<time::OffsetDateTime>,
    claim_token: Option<uuid::Uuid>,
}

async fn lock_owned_scheduled_target(
    tx: &mut Transaction<'_, Postgres>,
    requested_room: Option<RoomId>,
    id: ScheduledMessageId,
    actor: ParticipantId,
    post_policy: PostPolicy,
) -> Result<LockedScheduledTarget, Error> {
    let room = if let Some(room) = requested_room {
        lock_deferred_room_access(tx, room, actor, post_policy).await?;
        room
    } else {
        // Legacy opaque-id routes have no path room. Resolve immutable identity
        // without locking, reject non-owners opaquely, then take the canonical
        // room fence before locking and revalidating the target row.
        let resolved = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid)>(
            "SELECT room_id, sender_id FROM scheduled_messages WHERE id = $1",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut **tx)
        .await?;
        let Some((room, sender)) = resolved else {
            return Err(Error::NotFound(format!("scheduled message {id}")));
        };
        if sender != actor.to_uuid() {
            return Err(Error::NotFound(format!("scheduled message {id}")));
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
            String,
            i32,
            i32,
            Option<time::OffsetDateTime>,
            Option<uuid::Uuid>,
        ),
    >(
        r"SELECT room_id, sender_id, delivery_status, attempts,
                  delivery_generation, canceled_at, claim_token
            FROM scheduled_messages
           WHERE id = $1
             AND room_id = $2
           FOR UPDATE",
    )
    .bind(id.to_uuid())
    .bind(room.to_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    let Some((locked_room, sender, status, attempts, generation, canceled_at, claim_token)) =
        locked
    else {
        return Err(Error::NotFound(format!("scheduled message {id}")));
    };
    if locked_room != room.to_uuid() {
        return Err(Error::Conflict(
            "scheduled message room identity changed".into(),
        ));
    }
    if sender != actor.to_uuid() {
        return Err(Error::NotFound(format!("scheduled message {id}")));
    }
    Ok(LockedScheduledTarget {
        room,
        delivery_status: status,
        attempts,
        delivery_generation: generation,
        canceled_at,
        claim_token,
    })
}

async fn lock_accessible_scheduled_rooms(
    tx: &mut Transaction<'_, Postgres>,
    sender: ParticipantId,
) -> Result<Vec<uuid::Uuid>, Error> {
    let rooms = sqlx::query_scalar::<_, uuid::Uuid>(
        r"SELECT DISTINCT room_id
            FROM scheduled_messages
           WHERE sender_id = $1
           ORDER BY room_id",
    )
    .bind(sender.to_uuid())
    .fetch_all(&mut **tx)
    .await?;
    let mut accessible = Vec::with_capacity(rooms.len());
    for raw_room in rooms {
        let room = RoomId::from_uuid(raw_room);
        if lock_effective_message_write_access(tx, room, sender, PostPolicy::Ignore)
            .await?
            .is_some()
        {
            accessible.push(raw_room);
        }
    }
    Ok(accessible)
}

#[derive(Clone)]
pub struct ScheduledRepo {
    pool: PgPool,
}

impl ScheduledRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persist a new scheduled message under the same transaction that fences
    /// the sender's current room access and an optional live same-room reply.
    pub async fn create_authorized(
        &self,
        room: RoomId,
        sender: ParticipantId,
        blocks: &[Block],
        reply_to: Option<MessageId>,
        scheduled_at: time::OffsetDateTime,
    ) -> Result<ScheduledMessageId, Error> {
        let id = ScheduledMessageId::new();
        let blocks_json = serde_json::to_value(blocks)?;
        let mut tx = self.pool.begin().await?;
        lock_deferred_room_access(&mut tx, room, sender, PostPolicy::Enforce).await?;
        lock_live_reply_parent(&mut tx, reply_to, room).await?;
        sqlx::query(
            r"INSERT INTO scheduled_messages
                  (id, room_id, sender_id, blocks, reply_to, scheduled_at, available_at)
               VALUES ($1, $2, $3, $4, $5, $6, $6)",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(sender.to_uuid())
        .bind(sqlx::types::Json(blocks_json))
        .bind(reply_to.map(|m| m.to_uuid()))
        .bind(scheduled_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Create a reminder anchored to a live message. The message's canonical
    /// room is resolved and revalidated after the room-access fence is held.
    pub async fn create_reminder_authorized(
        &self,
        message: MessageId,
        sender: ParticipantId,
        blocks: &[Block],
        scheduled_at: time::OffsetDateTime,
    ) -> Result<(ScheduledMessageId, RoomId), Error> {
        let anchor = message.to_string();
        let valid_card = blocks.iter().any(|block| {
            matches!(
                block,
                Block::Card { schema, payload }
                    if schema == "message_reminder"
                        && payload.get("message_id").and_then(serde_json::Value::as_str)
                            == Some(anchor.as_str())
            )
        });
        if !valid_card {
            return Err(Error::Invalid(
                "message reminder payload must bind its live message id".into(),
            ));
        }
        let blocks_json = serde_json::to_value(blocks)?;
        let id = ScheduledMessageId::new();
        let mut tx = self.pool.begin().await?;
        let room = sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT room_id
               FROM messages
              WHERE id = $1
                AND deleted_at IS NULL",
        )
        .bind(message.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(room) = room.map(RoomId::from_uuid) else {
            return Err(Error::NotFound(format!("message {message}")));
        };
        lock_deferred_room_access(&mut tx, room, sender, PostPolicy::Enforce).await?;
        let live = sqlx::query_scalar::<_, bool>(
            r"SELECT true
                FROM messages
               WHERE id = $1
                 AND room_id = $2
                 AND deleted_at IS NULL
               FOR SHARE",
        )
        .bind(message.to_uuid())
        .bind(room.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(false);
        if !live {
            return Err(Error::NotFound(format!("message {message}")));
        }
        sqlx::query(
            r"INSERT INTO scheduled_messages
                  (id, room_id, sender_id, blocks, reply_to, scheduled_at, available_at)
               VALUES ($1, $2, $3, $4, NULL, $5, $5)",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(sender.to_uuid())
        .bind(sqlx::types::Json(blocks_json))
        .bind(scheduled_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((id, room))
    }

    /// List a sender's editable, unattempted scheduled messages, soonest first.
    /// A scoped read locks current room access; a global owner inventory filters
    /// every room whose access has since been revoked.
    pub async fn list_pending_authorized(
        &self,
        sender: ParticipantId,
        room: Option<RoomId>,
    ) -> Result<Vec<ScheduledMessage>, Error> {
        let mut tx = self.pool.begin().await?;
        let accessible_rooms = if let Some(room) = room {
            lock_deferred_room_access(&mut tx, room, sender, PostPolicy::Ignore).await?;
            vec![room.to_uuid()]
        } else {
            lock_accessible_scheduled_rooms(&mut tx, sender).await?
        };
        if accessible_rooms.is_empty() {
            tx.commit().await?;
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT {COLUMNS}
               FROM scheduled_messages
              WHERE sender_id = $1
                AND delivery_status = 'pending'
                AND attempts = 0
                AND delivery_generation = 0
                AND canceled_at IS NULL
                AND room_id = ANY($2)
              ORDER BY scheduled_at ASC, id ASC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(sender.to_uuid())
            .bind(&accessible_rooms)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// List the caller's actionable scheduled deliveries for one room. Pending
    /// drafts/retries, active claims, and terminal failures remain visible;
    /// delivered/canceled rows are intentionally omitted.
    pub async fn list_actionable_authorized(
        &self,
        sender: ParticipantId,
        room: Option<RoomId>,
    ) -> Result<Vec<ScheduledMessage>, Error> {
        let mut tx = self.pool.begin().await?;
        let accessible_rooms = if let Some(room) = room {
            lock_deferred_room_access(&mut tx, room, sender, PostPolicy::Ignore).await?;
            vec![room.to_uuid()]
        } else {
            lock_accessible_scheduled_rooms(&mut tx, sender).await?
        };
        if accessible_rooms.is_empty() {
            tx.commit().await?;
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT {COLUMNS}
               FROM scheduled_messages
              WHERE sender_id = $1
                AND delivery_status IN ('pending', 'claimed', 'dead')
                AND canceled_at IS NULL
                AND room_id = ANY($2)
              ORDER BY
                    CASE delivery_status
                        WHEN 'dead' THEN 0
                        WHEN 'claimed' THEN 1
                        ELSE 2
                    END,
                    scheduled_at ASC,
                    id ASC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(sender.to_uuid())
            .bind(&accessible_rooms)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Resolve a message anchor and return the caller's room-wide pending
    /// reminder inventory only while that anchor is still live and accessible.
    pub async fn list_reminders_authorized(
        &self,
        message: MessageId,
        sender: ParticipantId,
    ) -> Result<Vec<ScheduledMessage>, Error> {
        let mut tx = self.pool.begin().await?;
        let room = sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT room_id
               FROM messages
              WHERE id = $1
                AND deleted_at IS NULL",
        )
        .bind(message.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(room) = room.map(RoomId::from_uuid) else {
            return Err(Error::NotFound(format!("message {message}")));
        };
        lock_deferred_room_access(&mut tx, room, sender, PostPolicy::Ignore).await?;
        let live = sqlx::query_scalar::<_, bool>(
            r"SELECT true
                FROM messages
               WHERE id = $1
                 AND room_id = $2
                 AND deleted_at IS NULL
               FOR SHARE",
        )
        .bind(message.to_uuid())
        .bind(room.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(false);
        if !live {
            return Err(Error::NotFound(format!("message {message}")));
        }
        let sql = format!(
            "SELECT {COLUMNS}
               FROM scheduled_messages
              WHERE sender_id = $1
                AND room_id = $2
                AND delivery_status = 'pending'
                AND attempts = 0
                AND delivery_generation = 0
                AND canceled_at IS NULL
              ORDER BY scheduled_at ASC, id ASC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(sender.to_uuid())
            .bind(room.to_uuid())
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Return one actionable owner row after canonical room authorization.
    pub async fn get_authorized(
        &self,
        requested_room: Option<RoomId>,
        id: ScheduledMessageId,
        sender: ParticipantId,
    ) -> Result<ScheduledMessage, Error> {
        let mut tx = self.pool.begin().await?;
        let target =
            lock_owned_scheduled_target(&mut tx, requested_room, id, sender, PostPolicy::Ignore)
                .await?;
        if !matches!(
            target.delivery_status.as_str(),
            "pending" | "claimed" | "dead"
        ) || target.canceled_at.is_some()
        {
            return Err(Error::NotFound(format!("scheduled message {id}")));
        }
        let sql = format!("SELECT {COLUMNS} FROM scheduled_messages WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(row_to_model(row))
    }

    /// Edit one of the caller's own still-pending scheduled messages: replace its
    /// `blocks`, `reply_to`, and `scheduled_at`. Sender-scoped and gated on the
    /// row being unattempted and pending, so a user can never mutate the payload
    /// behind the stable delivery idempotency key after delivery has started.
    /// Claimed, retrying, delivered, dead, or canceled rows are a no-op. Returns
    /// `true` iff a row was updated. The caller is responsible for rejecting
    /// empty blocks / a past `scheduled_at` (mirroring [`create`](Self::create)).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update (including a blocks
    /// JSON-encode failure).
    pub async fn update_authorized(
        &self,
        requested_room: Option<RoomId>,
        id: ScheduledMessageId,
        sender: ParticipantId,
        scheduled_at: time::OffsetDateTime,
        blocks: &[Block],
        reply_to: Option<MessageId>,
    ) -> Result<(), Error> {
        let blocks_json = serde_json::to_value(blocks)?;
        let mut tx = self.pool.begin().await?;
        let target =
            lock_owned_scheduled_target(&mut tx, requested_room, id, sender, PostPolicy::Enforce)
                .await?;
        if target.delivery_status != "pending"
            || target.attempts != 0
            || target.delivery_generation != 0
            || target.canceled_at.is_some()
            || target.claim_token.is_some()
        {
            return Err(Error::Conflict(
                "scheduled message is no longer editable".into(),
            ));
        }
        lock_live_reply_parent(&mut tx, reply_to, target.room).await?;
        let result = sqlx::query(
            r"UPDATE scheduled_messages
                 SET blocks = $3,
                     reply_to = $4,
                     scheduled_at = $5,
                     available_at = $5,
                     last_error = NULL
               WHERE id = $1
                 AND sender_id = $2
                 AND room_id = $6
                 AND delivery_status = 'pending'
                 AND attempts = 0
                 AND delivery_generation = 0
                 AND canceled_at IS NULL",
        )
        .bind(id.to_uuid())
        .bind(sender.to_uuid())
        .bind(sqlx::types::Json(blocks_json))
        .bind(reply_to.map(|m| m.to_uuid()))
        .bind(scheduled_at)
        .bind(target.room.to_uuid())
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Err(Error::Conflict(
                "scheduled message state changed during update".into(),
            ));
        }
        tx.commit().await?;
        Ok(())
    }

    /// Cancel one of the caller's own unclaimed scheduled deliveries. Pending
    /// drafts/retries and dead rows can be terminated; an active lease remains
    /// fenced so cancellation cannot race a known worker.
    pub async fn cancel_authorized(
        &self,
        requested_room: Option<RoomId>,
        id: ScheduledMessageId,
        sender: ParticipantId,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        let target =
            lock_owned_scheduled_target(&mut tx, requested_room, id, sender, PostPolicy::Ignore)
                .await?;
        if !matches!(target.delivery_status.as_str(), "pending" | "dead")
            || target.canceled_at.is_some()
            || target.claim_token.is_some()
        {
            return Err(Error::Conflict(
                "scheduled message cannot be canceled in its current state".into(),
            ));
        }
        let result = sqlx::query(
            r"UPDATE scheduled_messages
                 SET delivery_status = 'canceled',
                     canceled_at = now(),
                     dead_at = NULL,
                     last_error = NULL
               WHERE id = $1
                 AND sender_id = $2
                 AND room_id = $3
                 AND delivery_status IN ('pending', 'dead')
                 AND claim_token IS NULL
                 AND canceled_at IS NULL",
        )
        .bind(id.to_uuid())
        .bind(sender.to_uuid())
        .bind(target.room.to_uuid())
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Err(Error::Conflict(
                "scheduled message state changed during cancellation".into(),
            ));
        }
        tx.commit().await?;
        Ok(())
    }

    /// Explicitly requeue one of the caller's dead deliveries. Payload fields
    /// remain immutable and the stable sender idempotency key is retained; a new
    /// generation resets only the bounded-attempt counter.
    pub async fn retry_dead_authorized(
        &self,
        requested_room: Option<RoomId>,
        id: ScheduledMessageId,
        sender: ParticipantId,
        retry_at: time::OffsetDateTime,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        let target =
            lock_owned_scheduled_target(&mut tx, requested_room, id, sender, PostPolicy::Enforce)
                .await?;
        if target.delivery_status != "dead"
            || target.canceled_at.is_some()
            || target.claim_token.is_some()
        {
            return Err(Error::Conflict(
                "scheduled message is not a retryable dead delivery".into(),
            ));
        }
        let result = sqlx::query(
            r"UPDATE scheduled_messages
                 SET delivery_status = 'pending',
                     available_at = $3,
                     attempts = 0,
                     delivery_generation = delivery_generation + 1,
                     dead_at = NULL,
                     last_error = NULL
               WHERE id = $1
                 AND sender_id = $2
                 AND room_id = $4
                 AND delivery_status = 'dead'
                 AND claim_token IS NULL
                 AND canceled_at IS NULL",
        )
        .bind(id.to_uuid())
        .bind(sender.to_uuid())
        .bind(retry_at)
        .bind(target.room.to_uuid())
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Err(Error::Conflict(
                "scheduled message state changed during retry".into(),
            ));
        }
        tx.commit().await?;
        Ok(())
    }

    // Compatibility shims are compiled only for the crate's historical
    // integration tests. Production callers cannot bypass the explicit
    // transaction-authorized API surface above.
    #[cfg(test)]
    pub async fn create(
        &self,
        room: RoomId,
        sender: ParticipantId,
        blocks: &[Block],
        reply_to: Option<MessageId>,
        scheduled_at: time::OffsetDateTime,
    ) -> Result<ScheduledMessageId, Error> {
        match self
            .create_authorized(room, sender, blocks, reply_to, scheduled_at)
            .await
        {
            Err(Error::NotFound(message)) if message.starts_with("reply message ") => {
                Err(Error::Database(sqlx::Error::Protocol(
                    "reply_to must reference an existing live message in the same room".into(),
                )))
            }
            result => result,
        }
    }

    #[cfg(test)]
    pub async fn list_pending_for_sender(
        &self,
        sender: ParticipantId,
        room: Option<RoomId>,
    ) -> Result<Vec<ScheduledMessage>, Error> {
        self.list_pending_authorized(sender, room).await
    }

    #[cfg(test)]
    pub async fn list_actionable_for_sender(
        &self,
        sender: ParticipantId,
        room: Option<RoomId>,
    ) -> Result<Vec<ScheduledMessage>, Error> {
        self.list_actionable_authorized(sender, room).await
    }

    #[cfg(test)]
    pub async fn update(
        &self,
        id: ScheduledMessageId,
        sender: ParticipantId,
        scheduled_at: time::OffsetDateTime,
        blocks: &[Block],
        reply_to: Option<MessageId>,
    ) -> Result<bool, Error> {
        match self
            .update_authorized(None, id, sender, scheduled_at, blocks, reply_to)
            .await
        {
            Ok(()) => Ok(true),
            Err(Error::NotFound(message)) if !message.starts_with("reply message ") => Ok(false),
            Err(Error::Forbidden(_) | Error::Conflict(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    #[cfg(test)]
    pub async fn cancel(
        &self,
        id: ScheduledMessageId,
        sender: ParticipantId,
    ) -> Result<bool, Error> {
        match self.cancel_authorized(None, id, sender).await {
            Ok(()) => Ok(true),
            Err(Error::NotFound(_) | Error::Forbidden(_) | Error::Conflict(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    #[cfg(test)]
    pub async fn retry_dead(
        &self,
        id: ScheduledMessageId,
        sender: ParticipantId,
        retry_at: time::OffsetDateTime,
    ) -> Result<bool, Error> {
        match self.retry_dead_authorized(None, id, sender, retry_at).await {
            Ok(()) => Ok(true),
            Err(Error::NotFound(_) | Error::Forbidden(_) | Error::Conflict(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Claim due messages using the production lease/retry defaults.
    pub async fn claim_due(
        &self,
        now: time::OffsetDateTime,
        limit: i64,
    ) -> Result<Vec<ScheduledDeliveryClaim>, sqlx::Error> {
        self.claim_due_with_policy(now, limit, DEFAULT_LEASE_SECS, DEFAULT_MAX_ATTEMPTS)
            .await
    }

    /// Atomically lease up to `limit` due messages. Pending rows and expired
    /// claims are eligible; exhausted expired claims are moved to `dead`.
    ///
    /// `FOR UPDATE SKIP LOCKED` divides work between replicas. A fresh random
    /// token plus incremented attempt fences the previous owner after reclaim.
    pub async fn claim_due_with_policy(
        &self,
        now: time::OffsetDateTime,
        limit: i64,
        lease_secs: i64,
        max_attempts: i32,
    ) -> Result<Vec<ScheduledDeliveryClaim>, sqlx::Error> {
        let limit = clamp_claim(limit);
        let lease_secs = lease_secs.clamp(1, 3600);
        let max_attempts = max_attempts.clamp(1, 100);
        let sql = format!(
            "WITH exhausted_ids AS (
                 SELECT id
                   FROM scheduled_messages
                  WHERE delivery_status = 'claimed'
                    AND lease_expires_at <= $1
                    AND attempts >= $4
                    AND canceled_at IS NULL
                  ORDER BY lease_expires_at ASC, id ASC
                  FOR UPDATE SKIP LOCKED
                  LIMIT $2
             ),
             exhausted AS (
                 UPDATE scheduled_messages AS scheduled
                    SET delivery_status = 'dead',
                        dead_at = $1,
                        last_error = COALESCE(
                            scheduled.last_error,
                            'delivery lease expired after maximum attempts'
                        ),
                        claim_token = NULL,
                        claimed_at = NULL,
                        lease_expires_at = NULL
                   FROM exhausted_ids
                  WHERE scheduled.id = exhausted_ids.id
             ),
             due AS (
                 SELECT id
                  FROM scheduled_messages
                  WHERE scheduled_at <= $1
                    -- Pre-0184 binaries only update scheduled_at when editing.
                    -- For original never-attempted rows it remains the source
                    -- of truth; retries and explicit requeue generations use
                    -- available_at.
                    AND (
                        (attempts = 0 AND delivery_generation = 0)
                        OR available_at <= $1
                    )
                    AND delivered_at IS NULL
                    AND canceled_at IS NULL
                    AND attempts < $4
                    AND (
                        delivery_status = 'pending'
                        OR (
                            delivery_status = 'claimed'
                            AND lease_expires_at <= $1
                        )
                    )
                  ORDER BY
                        CASE
                            WHEN attempts = 0 AND delivery_generation = 0
                                THEN scheduled_at
                            ELSE available_at
                        END ASC,
                        scheduled_at ASC,
                        id ASC
                  FOR UPDATE SKIP LOCKED
                  LIMIT $2
             )
             UPDATE scheduled_messages AS scheduled
                SET delivery_status = 'claimed',
                    claim_token = gen_random_uuid(),
                    claimed_at = $1,
                    lease_expires_at =
                        $1 + make_interval(secs => $3::double precision),
                    attempts = scheduled.attempts + 1
               FROM due
              WHERE scheduled.id = due.id
          RETURNING {CLAIM_COLUMNS},
                    scheduled.claim_token,
                    scheduled.lease_expires_at"
        );
        let rows = sqlx::query_as::<_, ClaimRow>(&sql)
            .bind(now)
            .bind(limit)
            .bind(lease_secs)
            .bind(max_attempts)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(claim_row_to_model).collect())
    }

    /// Mark a claimed delivery successful iff this worker still owns the exact
    /// token/attempt generation.
    pub async fn confirm_delivered(
        &self,
        id: ScheduledMessageId,
        claim_token: uuid::Uuid,
        attempt: i32,
        generation: i32,
        delivered_at: time::OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE scheduled_messages
                SET delivery_status = 'delivered',
                    delivered_at = $5,
                    claim_token = NULL,
                    claimed_at = NULL,
                    lease_expires_at = NULL,
                    last_error = NULL
              WHERE id = $1
                AND delivery_status = 'claimed'
                AND claim_token = $2
                AND attempts = $3
                AND delivery_generation = $4
                AND canceled_at IS NULL",
        )
        .bind(id.to_uuid())
        .bind(claim_token)
        .bind(attempt)
        .bind(generation)
        .bind(delivered_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Settle a failed attempt. Retryable failures are re-parked until
    /// `retry_at`; permanent or exhausted failures enter the dead-letter state.
    /// A stale worker receives [`ScheduledFailureDisposition::FenceLost`].
    #[allow(clippy::too_many_arguments)]
    pub async fn record_failure(
        &self,
        id: ScheduledMessageId,
        claim_token: uuid::Uuid,
        attempt: i32,
        generation: i32,
        error: &str,
        retry_at: time::OffsetDateTime,
        retryable: bool,
        max_attempts: i32,
    ) -> Result<ScheduledFailureDisposition, sqlx::Error> {
        let max_attempts = max_attempts.clamp(1, 100);
        let status: Option<(String,)> = sqlx::query_as(
            "UPDATE scheduled_messages
                SET delivery_status =
                        CASE WHEN $7 AND attempts < $8 THEN 'pending' ELSE 'dead' END,
                    available_at =
                        CASE WHEN $7 AND attempts < $8 THEN $6 ELSE available_at END,
                    dead_at =
                        CASE WHEN $7 AND attempts < $8 THEN NULL ELSE now() END,
                    last_error = left($5, 2048),
                    claim_token = NULL,
                    claimed_at = NULL,
                    lease_expires_at = NULL
              WHERE id = $1
                AND delivery_status = 'claimed'
                AND claim_token = $2
                AND attempts = $3
                AND delivery_generation = $4
                AND canceled_at IS NULL
          RETURNING delivery_status",
        )
        .bind(id.to_uuid())
        .bind(claim_token)
        .bind(attempt)
        .bind(generation)
        .bind(error)
        .bind(retry_at)
        .bind(retryable)
        .bind(max_attempts)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match status.as_ref().map(|row| row.0.as_str()) {
            Some("pending") => ScheduledFailureDisposition::RetryScheduled,
            Some("dead") => ScheduledFailureDisposition::Dead,
            _ => ScheduledFailureDisposition::FenceLost,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_claim_bounds() {
        assert_eq!(clamp_claim(0), 1);
        assert_eq!(clamp_claim(-7), 1);
        assert_eq!(clamp_claim(1), 1);
        assert_eq!(clamp_claim(100), 100);
        assert_eq!(clamp_claim(10_000), MAX_CLAIM);
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored scheduled_
/// ```
#[cfg(test)]
#[path = "scheduled/db_tests.rs"]
mod db_tests;

#[cfg(test)]
#[path = "scheduled/authorization_tests.rs"]
mod authorization_tests;
