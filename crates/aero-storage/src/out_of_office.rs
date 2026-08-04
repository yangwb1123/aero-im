//! Out-of-office / auto-responder repository (per-user).
//!
//! Backs `migrations/0052_out_of_office.sql`. A user sets an out-of-office (OOO)
//! status — a free-text message plus an optional active window — then clears it
//! when back. While active, an out-of-band bus-listener bot
//! ([`crate`]-external `ooo_bot`) posts the message ONCE per sender into a 1:1
//! DM room when someone messages the absent user.
//!
//! Two tables, both keyed by participant id:
//! - `out_of_office` holds the OOO row (one per user); [`set`](OutOfOfficeRepo::set)
//!   upserts it, [`get`](OutOfOfficeRepo::get) reads it, [`clear`](OutOfOfficeRepo::clear)
//!   removes it, and [`is_active`](OutOfOfficeRepo::is_active) tests the window.
//! - `ooo_auto_replies` dedupes the bot's replies: one row per (OOO-user, sender)
//!   pair, so a sender is auto-replied at most once per OOO period.
//!   [`should_autoreply`](OutOfOfficeRepo::should_autoreply) checks for the row and
//!   [`record_autoreply`](OutOfOfficeRepo::record_autoreply) inserts it.
//!
//! Purely additive: a NEW [`OutOfOfficeRepo`]; no existing repo is touched. The
//! [`OutOfOffice`] model lives here (and is re-exported from the crate root) since
//! it is a storage-layer projection. The window predicate
//! [`within_window`] is pure and unit-tested without a database.

use aero_common::ParticipantId;
use serde::Serialize;
use sqlx::PgPool;

/// One out-of-office status — a per-user message with an optional active window.
///
/// A storage-layer projection of an `out_of_office` row. `Serialize` so a handler
/// can hand the row straight back as JSON; the timestamps render as RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct OutOfOffice {
    /// The participant the OOO status belongs to (and is keyed by).
    pub participant_id: ParticipantId,
    /// The auto-reply message shown to anyone who DMs the absent user.
    pub message: String,
    /// When the OOO becomes active, or `None` for "active immediately".
    #[serde(with = "time::serde::rfc3339::option")]
    pub starts_at: Option<time::OffsetDateTime>,
    /// When the OOO stops being active, or `None` for "no end".
    #[serde(with = "time::serde::rfc3339::option")]
    pub ends_at: Option<time::OffsetDateTime>,
    /// When the OOO status was first set (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// Whether `now` falls inside the half-open-ish `[starts_at?, ends_at?]` window.
///
/// Pure helper shared by [`OutOfOfficeRepo::is_active`]: a `None` bound is open
/// (no lower / no upper limit), so `(None, None)` is always active. Unit-tested
/// without a database.
#[must_use]
pub fn within_window(
    now: time::OffsetDateTime,
    starts_at: Option<time::OffsetDateTime>,
    ends_at: Option<time::OffsetDateTime>,
) -> bool {
    if let Some(s) = starts_at {
        if now < s {
            return false;
        }
    }
    if let Some(e) = ends_at {
        if now > e {
            return false;
        }
    }
    true
}

/// The columns an [`OutOfOffice`] is built from, in select order. Shared by every
/// query so the row decoding stays in one place.
const COLUMNS: &str = "participant_id, message, starts_at, ends_at, created_at";

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    participant_id: uuid::Uuid,
    message: String,
    starts_at: Option<time::OffsetDateTime>,
    ends_at: Option<time::OffsetDateTime>,
    created_at: time::OffsetDateTime,
}

fn row_to_model(r: Row) -> OutOfOffice {
    OutOfOffice {
        participant_id: ParticipantId::from_uuid(r.participant_id),
        message: r.message,
        starts_at: r.starts_at,
        ends_at: r.ends_at,
        created_at: r.created_at,
    }
}

/// Repository over the `out_of_office` and `ooo_auto_replies` tables.
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules and the bot build one inline via [`OutOfOfficeRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct OutOfOfficeRepo {
    pool: PgPool,
}

impl OutOfOfficeRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Set (upsert) `participant`'s out-of-office status. An existing row for the
    /// same participant is overwritten (message + window), leaving `created_at` at
    /// its original value. The caller validates the message is non-empty.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert.
    pub async fn set(
        &self,
        participant: ParticipantId,
        message: &str,
        starts_at: Option<time::OffsetDateTime>,
        ends_at: Option<time::OffsetDateTime>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO out_of_office (participant_id, message, starts_at, ends_at)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (participant_id) DO UPDATE
                 SET message = EXCLUDED.message,
                     starts_at = EXCLUDED.starts_at,
                     ends_at = EXCLUDED.ends_at",
        )
        .bind(participant.to_uuid())
        .bind(message)
        .bind(starts_at)
        .bind(ends_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch `participant`'s out-of-office status, or `None` if they have not set
    /// one. This returns the raw row regardless of its window — use
    /// [`is_active`](Self::is_active) to test whether it is currently in effect.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(
        &self,
        participant: ParticipantId,
    ) -> Result<Option<OutOfOffice>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM out_of_office WHERE participant_id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(participant.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// Clear `participant`'s out-of-office status. Returns `true` iff a row was
    /// removed — a second clear (or one for a user with no OOO) is a no-op
    /// returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn clear(&self, participant: ParticipantId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM out_of_office WHERE participant_id = $1")
            .bind(participant.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Whether `participant` is currently out-of-office at instant `now`: a row
    /// exists AND `now` falls within its `[starts_at?, ends_at?]` window (see
    /// [`within_window`]). `false` when no OOO is set.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn is_active(
        &self,
        participant: ParticipantId,
        now: time::OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        match self.get(participant).await? {
            Some(ooo) => Ok(within_window(now, ooo.starts_at, ooo.ends_at)),
            None => Ok(false),
        }
    }

    /// Whether the bot should auto-reply on behalf of `ooo_user` to `sender`:
    /// `true` iff no `ooo_auto_replies` row yet exists for that pair. The caller
    /// is expected to call [`record_autoreply`](Self::record_autoreply) right
    /// after a successful send so the next message from the same sender is
    /// suppressed.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn should_autoreply(
        &self,
        ooo_user: ParticipantId,
        sender: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (bool,)>(
            r"SELECT NOT EXISTS (
                SELECT 1 FROM ooo_auto_replies
                 WHERE ooo_participant_id = $1 AND sender_id = $2
              )",
        )
        .bind(ooo_user.to_uuid())
        .bind(sender.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Record that the bot auto-replied on behalf of `ooo_user` to `sender` in
    /// `room`, so [`should_autoreply`](Self::should_autoreply) returns `false`
    /// thereafter. Idempotent: a duplicate (already-recorded) pair is a no-op.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn record_autoreply(
        &self,
        ooo_user: ParticipantId,
        sender: ParticipantId,
        room: aero_common::RoomId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO ooo_auto_replies (ooo_participant_id, sender_id, room_id)
               VALUES ($1, $2, $3)
               ON CONFLICT (ooo_participant_id, sender_id) DO NOTHING",
        )
        .bind(ooo_user.to_uuid())
        .bind(sender.to_uuid())
        .bind(room.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    fn t(offset_secs: i64) -> time::OffsetDateTime {
        time::OffsetDateTime::UNIX_EPOCH + Duration::seconds(offset_secs)
    }

    #[test]
    fn within_window_open_bounds_always_active() {
        assert!(within_window(t(0), None, None));
        assert!(within_window(t(1_000_000), None, None));
    }

    #[test]
    fn within_window_respects_start() {
        let now = t(100);
        assert!(
            !within_window(now, Some(t(200)), None),
            "before start is inactive"
        );
        assert!(
            within_window(now, Some(t(100)), None),
            "exactly at start is active"
        );
        assert!(
            within_window(now, Some(t(50)), None),
            "after start is active"
        );
    }

    #[test]
    fn within_window_respects_end() {
        let now = t(100);
        assert!(
            within_window(now, None, Some(t(200))),
            "before end is active"
        );
        assert!(
            within_window(now, None, Some(t(100))),
            "exactly at end is active"
        );
        assert!(
            !within_window(now, None, Some(t(50))),
            "after end is inactive"
        );
    }

    #[test]
    fn within_window_closed_range() {
        let start = Some(t(100));
        let end = Some(t(200));
        assert!(!within_window(t(99), start, end));
        assert!(within_window(t(150), start, end));
        assert!(!within_window(t(201), start, end));
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored out_of_office
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::RoomId;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway participant so the test is self-contained.
    async fn mk_participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("ooo-participant-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn set_get_clear_and_window() {
        let p = pool();
        let repo = OutOfOfficeRepo::new(p.clone());
        let user = mk_participant(&p).await;
        let now = time::OffsetDateTime::now_utc();

        // No OOO yet.
        assert!(repo.get(user).await.unwrap().is_none());
        assert!(!repo.is_active(user, now).await.unwrap());

        // Set with an open window -> active.
        repo.set(user, "away fishing", None, None).await.unwrap();
        let got = repo.get(user).await.unwrap().expect("present");
        assert_eq!(got.message, "away fishing");
        assert!(repo.is_active(user, now).await.unwrap());

        // Upsert with a future window -> not yet active.
        let future = now + time::Duration::days(1);
        repo.set(user, "back soon", Some(future), None)
            .await
            .unwrap();
        let got = repo.get(user).await.unwrap().expect("present");
        assert_eq!(got.message, "back soon");
        assert!(
            !repo.is_active(user, now).await.unwrap(),
            "future window inactive now"
        );
        assert!(
            repo.is_active(user, future).await.unwrap(),
            "active at start"
        );

        // Clear -> gone; second clear is a no-op.
        assert!(repo.clear(user).await.unwrap());
        assert!(!repo.clear(user).await.unwrap());
        assert!(repo.get(user).await.unwrap().is_none());

        // Cleanup.
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(user.to_uuid())
            .execute(&p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn autoreply_dedupe_per_pair() {
        let p = pool();
        let repo = OutOfOfficeRepo::new(p.clone());
        let ooo_user = mk_participant(&p).await;
        let sender = mk_participant(&p).await;
        let room = RoomId::new();

        // First time: should reply.
        assert!(repo.should_autoreply(ooo_user, sender).await.unwrap());
        repo.record_autoreply(ooo_user, sender, room).await.unwrap();
        // After recording: suppressed for the same pair.
        assert!(!repo.should_autoreply(ooo_user, sender).await.unwrap());
        // Idempotent re-record is a no-op (no error / no duplicate-key).
        repo.record_autoreply(ooo_user, sender, room).await.unwrap();
        // A different sender is still eligible.
        let sender2 = mk_participant(&p).await;
        assert!(repo.should_autoreply(ooo_user, sender2).await.unwrap());

        // Cleanup.
        sqlx::query("DELETE FROM ooo_auto_replies WHERE ooo_participant_id = $1")
            .bind(ooo_user.to_uuid())
            .execute(&p)
            .await
            .ok();
        for who in [ooo_user, sender, sender2] {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(who.to_uuid())
                .execute(&p)
                .await
                .ok();
        }
    }
}
