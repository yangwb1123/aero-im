//! Scheduled-message repository ("Send later" + reminders).
//!
//! Backs `migrations/0016_scheduled.sql`. A user composes a message now; the row
//! sits here until its `scheduled_at` passes, at which point the delivery worker
//! [`claim_due`](ScheduledRepo::claim_due)s it and replays it through the normal
//! send path. A reminder is just a self-targeted scheduled note.
//!
//! Purely additive: a NEW [`ScheduledRepo`]; no existing repo is touched. The
//! [`ScheduledMessage`] model lives here (and is re-exported from the crate root)
//! rather than in `aero-common`, since it is a storage-layer projection.
//!
//! The due-poll ([`claim_due`](ScheduledRepo::claim_due)) is concurrency-safe:
//! it selects + marks rows in a single `UPDATE ... RETURNING` over a
//! `FOR UPDATE SKIP LOCKED` subquery, so multiple dispatchers (or replicas)
//! never double-deliver the same message.

use aero_common::{Block, MessageId, ParticipantId, RoomId, ScheduledMessageId};
use serde::Serialize;
use sqlx::PgPool;

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
}

/// Largest batch [`ScheduledRepo::claim_due`] will return regardless of `limit`,
/// bounding the work one dispatcher tick does. Clamping is pure so it unit-tests
/// without a database.
const MAX_CLAIM: i64 = 500;

/// Clamp a requested claim batch into `1..=MAX_CLAIM` (defaulting non-positive to
/// `1`, since a non-positive `LIMIT` would claim nothing useful).
fn clamp_claim(requested: i64) -> i64 {
    requested.clamp(1, MAX_CLAIM)
}

/// The columns a `ScheduledMessage` is built from, in select order. Shared by
/// every query so the row decoding stays in one place.
const COLUMNS: &str =
    "id, room_id, sender_id, blocks, reply_to, scheduled_at, created_at, delivered_at, canceled_at";

type Row = (
    uuid::Uuid,
    uuid::Uuid,
    uuid::Uuid,
    serde_json::Value,
    Option<uuid::Uuid>,
    time::OffsetDateTime,
    time::OffsetDateTime,
    Option<time::OffsetDateTime>,
    Option<time::OffsetDateTime>,
);

fn row_to_model(r: Row) -> ScheduledMessage {
    let (id, room_id, sender_id, blocks, reply_to, scheduled_at, created_at, delivered_at, canceled_at) =
        r;
    ScheduledMessage {
        id: ScheduledMessageId::from_uuid(id),
        room_id: RoomId::from_uuid(room_id),
        sender_id: ParticipantId::from_uuid(sender_id),
        blocks: serde_json::from_value(blocks).unwrap_or_default(),
        reply_to: reply_to.map(MessageId::from_uuid),
        scheduled_at,
        created_at,
        delivered_at,
        canceled_at,
    }
}

#[derive(Clone)]
pub struct ScheduledRepo {
    pool: PgPool,
}

impl ScheduledRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persist a new scheduled message, returning its generated id. The caller
    /// is responsible for room-access + not-in-the-past validation.
    pub async fn create(
        &self,
        room: RoomId,
        sender: ParticipantId,
        blocks: &[Block],
        reply_to: Option<MessageId>,
        scheduled_at: time::OffsetDateTime,
    ) -> Result<ScheduledMessageId, sqlx::Error> {
        let id = ScheduledMessageId::new();
        let blocks_json =
            serde_json::to_value(blocks).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        sqlx::query(
            r"INSERT INTO scheduled_messages
                  (id, room_id, sender_id, blocks, reply_to, scheduled_at)
               VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(sender.to_uuid())
        .bind(sqlx::types::Json(blocks_json))
        .bind(reply_to.map(|m| m.to_uuid()))
        .bind(scheduled_at)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List a sender's still-pending (not delivered, not canceled) scheduled
    /// messages, soonest first. Optionally restricted to one room. Always scoped
    /// to `sender`, so one user can never see another's drafts.
    pub async fn list_pending_for_sender(
        &self,
        sender: ParticipantId,
        room: Option<RoomId>,
    ) -> Result<Vec<ScheduledMessage>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM scheduled_messages
              WHERE sender_id = $1
                AND delivered_at IS NULL
                AND canceled_at IS NULL
                AND ($2::uuid IS NULL OR room_id = $2)
              ORDER BY scheduled_at ASC, id ASC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(sender.to_uuid())
            .bind(room.map(|r| r.to_uuid()))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Edit one of the caller's own still-pending scheduled messages: replace its
    /// `blocks`, `reply_to`, and `scheduled_at`. Sender-scoped and gated on the
    /// row being neither delivered nor canceled, so a user can never edit
    /// another's message and an already-claimed (delivered) or canceled row is a
    /// no-op. Returns `true` iff a row was updated. The caller is responsible for
    /// rejecting a non-empty-blocks / not-in-the-past `scheduled_at` (mirroring
    /// [`create`](Self::create)).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update (including a blocks
    /// JSON-encode failure).
    pub async fn update(
        &self,
        id: ScheduledMessageId,
        sender: ParticipantId,
        scheduled_at: time::OffsetDateTime,
        blocks: &[Block],
        reply_to: Option<MessageId>,
    ) -> Result<bool, sqlx::Error> {
        let blocks_json =
            serde_json::to_value(blocks).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let result = sqlx::query(
            r"UPDATE scheduled_messages
                 SET blocks = $3,
                     reply_to = $4,
                     scheduled_at = $5
               WHERE id = $1
                 AND sender_id = $2
                 AND delivered_at IS NULL
                 AND canceled_at IS NULL",
        )
        .bind(id.to_uuid())
        .bind(sender.to_uuid())
        .bind(sqlx::types::Json(blocks_json))
        .bind(reply_to.map(|m| m.to_uuid()))
        .bind(scheduled_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Cancel one of the caller's own still-pending scheduled messages. Returns
    /// `true` iff a row was canceled — sender-scoped and only when not already
    /// delivered/canceled, so a user cannot cancel another's message or one that
    /// already went out.
    pub async fn cancel(
        &self,
        id: ScheduledMessageId,
        sender: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE scheduled_messages
                 SET canceled_at = now()
               WHERE id = $1
                 AND sender_id = $2
                 AND delivered_at IS NULL
                 AND canceled_at IS NULL",
        )
        .bind(id.to_uuid())
        .bind(sender.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Atomically claim up to `limit` messages due at/before `now` (not yet
    /// delivered or canceled), marking each `delivered_at = now()` and returning
    /// them. `FOR UPDATE SKIP LOCKED` lets concurrent dispatchers each grab a
    /// disjoint batch, so a message is delivered exactly once even with multiple
    /// workers. The returned rows carry the freshly-set `delivered_at`.
    pub async fn claim_due(
        &self,
        now: time::OffsetDateTime,
        limit: i64,
    ) -> Result<Vec<ScheduledMessage>, sqlx::Error> {
        let limit = clamp_claim(limit);
        let sql = format!(
            "UPDATE scheduled_messages
                SET delivered_at = now()
              WHERE id IN (
                    SELECT id FROM scheduled_messages
                     WHERE scheduled_at <= $1
                       AND delivered_at IS NULL
                       AND canceled_at IS NULL
                     ORDER BY scheduled_at ASC
                     FOR UPDATE SKIP LOCKED
                     LIMIT $2
                )
            RETURNING {COLUMNS}"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(now)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
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
mod db_tests {
    use super::*;
    use aero_common::{Block, RoomKind};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    // Create a throwaway sender + room so the test is self-contained.
    async fn fixture(p: &PgPool) -> (RoomId, ParticipantId) {
        let sender = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(sender.to_uuid())
            .bind(format!("sched-sender-{sender}"))
            .execute(p)
            .await
            .expect("insert participant");
        let room = RoomId::new();
        // `rooms.workspace_id` is NOT NULL (migration 0006); reuse the reserved
        // all-zero default workspace, guaranteed to exist by that migration's
        // backfill, so the fixture row satisfies the FK + NOT NULL constraint.
        sqlx::query(
            "INSERT INTO rooms (id, kind, created_by, workspace_id)
             VALUES ($1, $2, $3, '00000000-0000-0000-0000-000000000000'::uuid)",
        )
        .bind(room.to_uuid())
        .bind(format!("{:?}", RoomKind::Group).to_lowercase())
        .bind(sender.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        (room, sender)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn scheduled_create_list_cancel() {
        let p = pool();
        let repo = ScheduledRepo::new(p.clone());
        let (room, sender) = fixture(&p).await;

        let future = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
        let blocks = vec![Block::text("send later")];
        let id = repo
            .create(room, sender, &blocks, None, future)
            .await
            .unwrap();

        // Appears in the sender's pending list (both unfiltered and room-scoped).
        let all = repo.list_pending_for_sender(sender, None).await.unwrap();
        assert!(all.iter().any(|m| m.id == id), "pending list includes the new message");
        let scoped = repo.list_pending_for_sender(sender, Some(room)).await.unwrap();
        assert!(scoped.iter().any(|m| m.id == id), "room-scoped pending list includes it");

        // A different sender sees none of it.
        let other = ParticipantId::new();
        let theirs = repo.list_pending_for_sender(other, None).await.unwrap();
        assert!(!theirs.iter().any(|m| m.id == id), "pending list is sender-scoped");

        // Cancel is sender-scoped: a stranger can't cancel it; the owner can, once.
        assert!(!repo.cancel(id, other).await.unwrap(), "stranger cannot cancel");
        assert!(repo.cancel(id, sender).await.unwrap(), "owner cancels");
        assert!(!repo.cancel(id, sender).await.unwrap(), "second cancel is a no-op");

        let after = repo.list_pending_for_sender(sender, None).await.unwrap();
        assert!(!after.iter().any(|m| m.id == id), "canceled message leaves the pending list");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn scheduled_claim_due_delivers_once() {
        let p = pool();
        let repo = ScheduledRepo::new(p.clone());
        let (room, sender) = fixture(&p).await;

        // Due in the past ⇒ immediately claimable.
        let past = time::OffsetDateTime::now_utc() - time::Duration::minutes(1);
        let id = repo
            .create(room, sender, &[Block::text("due now")], None, past)
            .await
            .unwrap();

        let now = time::OffsetDateTime::now_utc();
        let first = repo.claim_due(now, 100).await.unwrap();
        assert!(first.iter().any(|m| m.id == id), "claim_due returns the due message");
        assert!(
            first.iter().find(|m| m.id == id).unwrap().delivered_at.is_some(),
            "claimed row carries delivered_at"
        );

        // A second claim must NOT return it again (no double-delivery).
        let second = repo.claim_due(time::OffsetDateTime::now_utc(), 100).await.unwrap();
        assert!(
            !second.iter().any(|m| m.id == id),
            "a delivered message is not claimed twice"
        );

        // And it is no longer pending for the sender.
        let pending = repo.list_pending_for_sender(sender, None).await.unwrap();
        assert!(!pending.iter().any(|m| m.id == id), "delivered message leaves the pending list");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn scheduled_update_edits_pending_and_noops_after_claim() {
        let p = pool();
        let repo = ScheduledRepo::new(p.clone());
        let (room, sender) = fixture(&p).await;

        let t1 = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
        let id = repo
            .create(room, sender, &[Block::text("v1")], None, t1)
            .await
            .unwrap();

        // Edit blocks + time; the listing reflects the new values.
        let t2 = time::OffsetDateTime::now_utc() + time::Duration::hours(3);
        let new_blocks = vec![Block::text("v2-edited")];
        assert!(
            repo.update(id, sender, t2, &new_blocks, None).await.unwrap(),
            "owner edits a pending message"
        );
        let pending = repo.list_pending_for_sender(sender, None).await.unwrap();
        let edited = pending.iter().find(|m| m.id == id).expect("still pending");
        assert!(
            matches!(edited.blocks.first(), Some(Block::Text { content, .. }) if content == "v2-edited"),
            "blocks reflect the edit"
        );
        assert!(edited.scheduled_at > t1, "scheduled_at moved later");

        // A stranger cannot edit it.
        let stranger = ParticipantId::new();
        assert!(
            !repo.update(id, stranger, t2, &new_blocks, None).await.unwrap(),
            "update is sender-scoped"
        );

        // Once claimed (delivered), update is a no-op.
        let claimed = repo
            .claim_due(time::OffsetDateTime::now_utc() + time::Duration::hours(4), 10)
            .await
            .unwrap();
        assert!(claimed.iter().any(|m| m.id == id), "claimed the now-due message");
        assert!(
            !repo.update(id, sender, t2, &new_blocks, None).await.unwrap(),
            "update of an already-claimed row is a no-op"
        );
    }
}
