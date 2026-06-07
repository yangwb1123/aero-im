//! Recurring scheduled-message repository (repeating "Send later").
//!
//! Backs `migrations/0041_recurring_messages.sql`. Complements the one-shot
//! [`ScheduledRepo`](crate::ScheduledRepo): a user stages a message that is
//! re-posted to a room on a repeating cadence (`hourly` / `daily` / `weekly`).
//! Each row carries its next firing time in `next_run`; the recurring dispatcher
//! polls due rows ([`list_due`](RecurringMessageRepo::list_due)), replays each
//! through the normal send path, then advances `next_run` via
//! [`reschedule`](RecurringMessageRepo::reschedule). Cancelling a series flips
//! `active = false` ([`cancel`](RecurringMessageRepo::cancel)).
//!
//! Purely additive: a NEW [`RecurringMessageRepo`]; no existing repo is touched.
//! The [`RecurringMessage`] model lives here (and is re-exported from the crate
//! root) rather than in `aero-common`, since it is a storage-layer projection.
//! The cadence arithmetic is the pure, db-free [`next_occurrence`].

use aero_common::{ParticipantId, RecurringMessageId, RoomId};
use serde::Serialize;
use sqlx::PgPool;

/// Advance `from` by one step of `cadence`, or `None` for an unknown cadence.
///
/// Pure and database-free, so it unit-tests without a Postgres. The recognised
/// cadences are `"hourly"` (`+1h`), `"daily"` (`+1d`), and `"weekly"` (`+7d`);
/// any other string yields `None`, which callers treat as a validation error.
#[must_use]
pub fn next_occurrence(
    cadence: &str,
    from: time::OffsetDateTime,
) -> Option<time::OffsetDateTime> {
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
}

/// The columns a [`RecurringMessage`] is built from, in select order. Shared by
/// every query so the row decoding stays in one place.
const COLUMNS: &str = "id, room_id, sender_id, blocks, cadence, next_run, active, created_at";

type Row = (
    uuid::Uuid,
    uuid::Uuid,
    uuid::Uuid,
    serde_json::Value,
    String,
    time::OffsetDateTime,
    bool,
    time::OffsetDateTime,
);

fn row_to_model(r: Row) -> RecurringMessage {
    let (id, room_id, sender_id, blocks, cadence, next_run, active, created_at) = r;
    RecurringMessage {
        id: RecurringMessageId::from_uuid(id),
        room_id: RoomId::from_uuid(room_id),
        sender_id: ParticipantId::from_uuid(sender_id),
        blocks,
        cadence,
        next_run,
        active,
        created_at,
    }
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

    /// Persist a new recurring message, returning its generated id. The caller is
    /// responsible for room-access and cadence/blocks validation; `first_run` is
    /// the first firing time (typically [`next_occurrence`] of `now`).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn create(
        &self,
        room: RoomId,
        sender: ParticipantId,
        blocks: &serde_json::Value,
        cadence: &str,
        first_run: time::OffsetDateTime,
    ) -> Result<RecurringMessageId, sqlx::Error> {
        let id = RecurringMessageId::new();
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
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List the still-active recurring messages for `room`, newest first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_room(
        &self,
        room: RoomId,
    ) -> Result<Vec<RecurringMessage>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM recurring_messages
              WHERE room_id = $1 AND active
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(room.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// List the active recurring messages now due (`next_run <= now`), soonest
    /// first. The recurring dispatcher fires each, then reschedules it.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_due(
        &self,
        now: time::OffsetDateTime,
    ) -> Result<Vec<RecurringMessage>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM recurring_messages
              WHERE active AND next_run <= $1
              ORDER BY next_run ASC, id ASC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(now)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Move a series' next firing time to `next_run`. Called by the dispatcher
    /// after a successful (or attempted) firing to advance the series.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn reschedule(
        &self,
        id: RecurringMessageId,
        next_run: time::OffsetDateTime,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE recurring_messages SET next_run = $2 WHERE id = $1")
            .bind(id.to_uuid())
            .bind(next_run)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Cancel one of the caller's own recurring series (set `active = false`).
    /// Returns `true` iff a row flipped — owner-scoped (`sender_id` in the
    /// `WHERE`) and only when still active, so a caller can never cancel another
    /// user's series, and a second cancel (or a stranger's) is a no-op returning
    /// `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn cancel(
        &self,
        id: RecurringMessageId,
        sender: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE recurring_messages
                 SET active = false
               WHERE id = $1 AND sender_id = $2 AND active",
        )
        .bind(id.to_uuid())
        .bind(sender.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
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
mod db_tests {
    use super::*;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway sender + room so the test is self-contained.
    async fn fixture(p: &PgPool) -> (RoomId, ParticipantId) {
        let sender = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(sender.to_uuid())
            .bind(format!("recurring-sender-{sender}"))
            .execute(p)
            .await
            .expect("insert participant");
        let room = RoomId::new();
        // `rooms.workspace_id` is NOT NULL (migration 0006); reuse the reserved
        // all-zero default workspace, guaranteed to exist by that migration's
        // backfill, so the fixture row satisfies the FK + NOT NULL constraint.
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id)
             VALUES ($1, 'channel', $2, $3, '00000000-0000-0000-0000-000000000000')",
        )
        .bind(room.to_uuid())
        .bind(format!("recurring-room-{room}"))
        .bind(sender.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        (room, sender)
    }

    fn blocks() -> serde_json::Value {
        serde_json::json!([{ "type": "text", "text": "standup time" }])
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn recurring_create_list_for_room() {
        let p = pool();
        let repo = RecurringMessageRepo::new(p.clone());
        let (room, sender) = fixture(&p).await;

        let first = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
        let id = repo
            .create(room, sender, &blocks(), "daily", first)
            .await
            .unwrap();

        let listed = repo.list_for_room(room).await.unwrap();
        assert!(listed.iter().any(|m| m.id == id), "list_for_room shows it");
        let found = listed.iter().find(|m| m.id == id).expect("present");
        assert_eq!(found.cadence, "daily");
        assert!(found.active, "newly created series is active");

        sqlx::query("DELETE FROM recurring_messages WHERE room_id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn recurring_list_due_respects_next_run() {
        let p = pool();
        let repo = RecurringMessageRepo::new(p.clone());
        let (room, sender) = fixture(&p).await;

        let past = time::OffsetDateTime::now_utc() - time::Duration::minutes(5);
        let future = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
        let due_id = repo
            .create(room, sender, &blocks(), "hourly", past)
            .await
            .unwrap();
        let pending_id = repo
            .create(room, sender, &blocks(), "hourly", future)
            .await
            .unwrap();

        let now = time::OffsetDateTime::now_utc();
        let due = repo.list_due(now).await.unwrap();
        assert!(due.iter().any(|m| m.id == due_id), "past next_run is due");
        assert!(
            !due.iter().any(|m| m.id == pending_id),
            "future next_run is not due"
        );

        sqlx::query("DELETE FROM recurring_messages WHERE room_id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn recurring_reschedule_moves_next_run() {
        let p = pool();
        let repo = RecurringMessageRepo::new(p.clone());
        let (room, sender) = fixture(&p).await;

        let past = time::OffsetDateTime::now_utc() - time::Duration::minutes(5);
        let id = repo
            .create(room, sender, &blocks(), "hourly", past)
            .await
            .unwrap();

        // Due now…
        let now = time::OffsetDateTime::now_utc();
        assert!(
            repo.list_due(now).await.unwrap().iter().any(|m| m.id == id),
            "due before reschedule"
        );

        // …reschedule into the future ⇒ no longer due.
        let next = next_occurrence("hourly", now).expect("known cadence");
        repo.reschedule(id, next).await.unwrap();
        assert!(
            !repo
                .list_due(time::OffsetDateTime::now_utc())
                .await
                .unwrap()
                .iter()
                .any(|m| m.id == id),
            "not due after reschedule into the future"
        );

        sqlx::query("DELETE FROM recurring_messages WHERE room_id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn recurring_cancel_owner_scoped() {
        let p = pool();
        let repo = RecurringMessageRepo::new(p.clone());
        let (room, sender) = fixture(&p).await;
        let stranger = ParticipantId::new();

        let first = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
        let id = repo
            .create(room, sender, &blocks(), "weekly", first)
            .await
            .unwrap();

        // A stranger cannot cancel; the owner can, once.
        assert!(
            !repo.cancel(id, stranger).await.unwrap(),
            "stranger cannot cancel another user's series"
        );
        assert!(repo.cancel(id, sender).await.unwrap(), "owner cancels");
        assert!(
            !repo.cancel(id, sender).await.unwrap(),
            "second cancel is a no-op"
        );

        // Cancelled series leaves the room list.
        assert!(
            !repo.list_for_room(room).await.unwrap().iter().any(|m| m.id == id),
            "cancelled series leaves list_for_room"
        );

        sqlx::query("DELETE FROM recurring_messages WHERE room_id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
