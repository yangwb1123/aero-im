//! Hype-train repository + pure escalation state machine (combo gifts).
//!
//! Backs `migrations/0079_hype_train.sql`. A "hype train" is an escalating
//! momentum session on a live stream: rapid successive gifts accumulate
//! `contribution` (units) within a sliding time window; every
//! [`UNITS_PER_LEVEL`] units advances the `level` (up to [`MAX_LEVEL`]). The
//! window slides forward on each contribution ([`WINDOW_SECS`]); when it lapses
//! the session is `expired`.
//!
//! The escalation arithmetic is a PURE function ([`apply_contribution`]) so it
//! unit-tests without a DB; the repository only persists the running totals and
//! the per-participant contribution breakdown. Purely additive: a NEW
//! [`HypeTrainRepo`]; no existing repo is touched. `stream_id` is a plain column
//! (not a cascading FK), mirroring [`ClipRepo`](crate::ClipRepo).

use aero_common::{HypeTrainSessionId, ParticipantId};
use serde::Serialize;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use ulid::Ulid;
use uuid::Uuid;

/// Units of contribution that advance the train by one level.
pub const UNITS_PER_LEVEL: u32 = 10;
/// Highest level a hype train can reach (the cap).
pub const MAX_LEVEL: u32 = 5;
/// Seconds of inactivity after which an active train lapses (the sliding window).
pub const WINDOW_SECS: i64 = 60;

/// One hype-train session — a storage-layer projection of a
/// `hype_train_sessions` row. `Serialize` so a handler can return it as JSON.
#[derive(Debug, Clone, Serialize)]
pub struct HypeTrainSession {
    /// The session's unique id.
    pub id: HypeTrainSessionId,
    /// The stream the train is running on (a plain reference, not a hard FK).
    pub stream_id: Ulid,
    /// Current escalation level (`1..=MAX_LEVEL`).
    pub level: i32,
    /// Running total of units fed into the train.
    pub contribution: i32,
    /// `"active"` | `"completed"` | `"expired"`.
    pub state: String,
    /// When the train started (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    /// When the train lapses without further contributions (RFC 3339).
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// The result of applying one contribution to a train's running state — pure, so
/// the escalation rule is unit-tested offline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Escalation {
    /// The new level after the contribution (clamped to [`MAX_LEVEL`]).
    pub level: u32,
    /// The new running contribution total.
    pub contribution: u32,
}

/// Apply `units` of contribution to a train currently at `contribution` total.
///
/// The new level is `1 + contribution / UNITS_PER_LEVEL`, clamped to
/// [`MAX_LEVEL`]. Pure (no DB, no clock) so the escalation/cap rule is
/// unit-tested directly. `units`/`contribution` saturate rather than overflow.
#[must_use]
pub fn apply_contribution(contribution: u32, units: u32) -> Escalation {
    let total = contribution.saturating_add(units);
    let level = (1 + total / UNITS_PER_LEVEL).min(MAX_LEVEL);
    Escalation {
        level,
        contribution: total,
    }
}

/// Whether a train with the given `expires_at` has lapsed at `now` (the window
/// elapsed without a fresh contribution). Pure boundary helper (`expires_at <=
/// now` ⇒ expired), so it unit-tests without a clock.
#[must_use]
pub fn is_expired(expires_at: OffsetDateTime, now: OffsetDateTime) -> bool {
    expires_at <= now
}

/// Repository over the `hype_train_sessions` + `hype_train_contributions` tables.
///
/// Cheap to clone — wraps a [`PgPool`]; feature modules build one inline via
/// [`HypeTrainRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct HypeTrainRepo {
    pool: PgPool,
}

type SessionRow = (Uuid, Uuid, i32, i32, String, OffsetDateTime, OffsetDateTime);

fn row_to_session(r: SessionRow) -> HypeTrainSession {
    let (id, stream_id, level, contribution, state, started_at, expires_at) = r;
    HypeTrainSession {
        id: HypeTrainSessionId::from_uuid(id),
        stream_id: Ulid(stream_id.as_u128()),
        level,
        contribution,
        state,
        started_at,
        expires_at,
    }
}

const SESSION_COLUMNS: &str = "id, stream_id, level, contribution, state, started_at, expires_at";

impl HypeTrainRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The current `active`, not-yet-expired session for a stream, if any. Marks a
    /// lapsed session `expired` in passing (lazy sweep) and returns `None` for it,
    /// so callers never see a stale train.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query/update.
    pub async fn current(
        &self,
        stream: Ulid,
        now: OffsetDateTime,
    ) -> Result<Option<HypeTrainSession>, sqlx::Error> {
        let sql = format!(
            "SELECT {SESSION_COLUMNS}
               FROM hype_train_sessions
              WHERE stream_id = $1 AND state = 'active'
              ORDER BY started_at DESC
              LIMIT 1"
        );
        let row = sqlx::query_as::<_, SessionRow>(&sql)
            .bind(Uuid::from_u128(stream.0))
            .fetch_optional(&self.pool)
            .await?;
        let Some(session) = row.map(row_to_session) else {
            return Ok(None);
        };
        if is_expired(session.expires_at, now) {
            // Lazily flip the lapsed train to `expired` and report no active train.
            sqlx::query("UPDATE hype_train_sessions SET state = 'expired' WHERE id = $1")
                .bind(session.id.to_uuid())
                .execute(&self.pool)
                .await?;
            return Ok(None);
        }
        Ok(Some(session))
    }

    /// Record a contribution of `units` from `participant` to `stream`'s hype
    /// train, advancing the escalation state machine, and return the resulting
    /// session. If there is no active (un-lapsed) train, a fresh one is started at
    /// level computed from `units`; otherwise the running total advances and the
    /// window slides forward by [`WINDOW_SECS`]. The per-participant contribution
    /// row is upserted in step.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the inserts/updates.
    pub async fn add_contribution(
        &self,
        stream: Ulid,
        participant: ParticipantId,
        units: u32,
        now: OffsetDateTime,
    ) -> Result<HypeTrainSession, sqlx::Error> {
        let expires = now + Duration::seconds(WINDOW_SECS);
        let mut tx = self.pool.begin().await?;

        // Serialize all contributions for this stream within the transaction. The
        // read (current level/contribution) → escalate → write sequence below is
        // otherwise a classic read-modify-write race: two concurrent gifts read the
        // same base, both write the same absolute total, and one is silently lost
        // (and two simultaneous first-gifts could even start two active trains). A
        // per-stream advisory xact lock makes the whole sequence atomic; it
        // releases on commit/rollback.
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let lock_key = stream.0 as u64 as i64; // low 64 bits of the stream ULID
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(lock_key)
            .execute(&mut *tx)
            .await?;

        // Read the active train inside the lock (a lapsed one is flipped to
        // `expired` and treated as none, so a fresh train starts) — the same lazy
        // expiry `current()` does, but transactional.
        let active_sql = format!(
            "SELECT {SESSION_COLUMNS}
               FROM hype_train_sessions
              WHERE stream_id = $1 AND state = 'active'
              ORDER BY started_at DESC
              LIMIT 1"
        );
        let existing = sqlx::query_as::<_, SessionRow>(&active_sql)
            .bind(Uuid::from_u128(stream.0))
            .fetch_optional(&mut *tx)
            .await?
            .map(row_to_session);
        let active = match existing {
            Some(s) if is_expired(s.expires_at, now) => {
                sqlx::query("UPDATE hype_train_sessions SET state = 'expired' WHERE id = $1")
                    .bind(s.id.to_uuid())
                    .execute(&mut *tx)
                    .await?;
                None
            }
            other => other,
        };

        let session = if let Some(existing) = active {
            // Advance the running active train: bump level/contribution and slide
            // the window forward.
            let base = u32::try_from(existing.contribution.max(0)).unwrap_or(0);
            let esc = apply_contribution(base, units);
            let sql = format!(
                "UPDATE hype_train_sessions
                    SET level = $2, contribution = $3, expires_at = $4
                  WHERE id = $1
              RETURNING {SESSION_COLUMNS}"
            );
            let row = sqlx::query_as::<_, SessionRow>(&sql)
                .bind(existing.id.to_uuid())
                .bind(i32::try_from(esc.level).unwrap_or(i32::MAX))
                .bind(i32::try_from(esc.contribution).unwrap_or(i32::MAX))
                .bind(expires)
                .fetch_one(&mut *tx)
                .await?;
            row_to_session(row)
        } else {
            // No active train: start a fresh one at the level the units imply.
            let esc = apply_contribution(0, units);
            let id = HypeTrainSessionId::new();
            let sql = format!(
                "INSERT INTO hype_train_sessions
                     (id, stream_id, level, contribution, state, started_at, expires_at)
                  VALUES ($1, $2, $3, $4, 'active', $5, $6)
               RETURNING {SESSION_COLUMNS}"
            );
            let row = sqlx::query_as::<_, SessionRow>(&sql)
                .bind(id.to_uuid())
                .bind(Uuid::from_u128(stream.0))
                .bind(i32::try_from(esc.level).unwrap_or(i32::MAX))
                .bind(i32::try_from(esc.contribution).unwrap_or(i32::MAX))
                .bind(now)
                .bind(expires)
                .fetch_one(&mut *tx)
                .await?;
            row_to_session(row)
        };

        // Upsert the participant's contribution within this session (same tx).
        sqlx::query(
            r"INSERT INTO hype_train_contributions (session_id, participant_id, units, updated_at)
               VALUES ($1, $2, $3, now())
               ON CONFLICT (session_id, participant_id)
               DO UPDATE SET units = hype_train_contributions.units + EXCLUDED.units,
                             updated_at = now()",
        )
        .bind(session.id.to_uuid())
        .bind(participant.to_uuid())
        .bind(i32::try_from(units).unwrap_or(i32::MAX))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(session)
    }
}

/// One row in a session's contributor leaderboard — participant + units + rank.
///
/// `Serialize` so the handler can return it directly as JSON.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct HypeTrainLeaderRow {
    pub participant_id: uuid::Uuid,
    pub total_units: i64,
    pub rank: i64,
}

impl HypeTrainRepo {
    /// Completed/expired sessions for a stream, newest first, paginated.
    ///
    /// Filters to the terminal states (`completed` / `expired`) so callers get
    /// the historical list and not the currently-active session.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_stream(
        &self,
        stream: Ulid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<HypeTrainSession>, sqlx::Error> {
        let sql = format!(
            "SELECT {SESSION_COLUMNS}
               FROM hype_train_sessions
              WHERE stream_id = $1 AND state IN ('completed', 'expired')
              ORDER BY started_at DESC
              LIMIT $2 OFFSET $3"
        );
        let rows = sqlx::query_as::<_, SessionRow>(&sql)
            .bind(Uuid::from_u128(stream.0))
            .bind(limit.max(1).min(100))
            .bind(offset.max(0))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_session).collect())
    }

    /// Top contributors across ALL sessions for a stream (stream-wide leaderboard),
    /// ranked by total units across every session for that stream, limited to `limit`
    /// rows (clamped `[1, 100]`).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn stream_leaderboard(
        &self,
        stream_id: Ulid,
        limit: i64,
    ) -> Result<Vec<HypeTrainLeaderRow>, sqlx::Error> {
        sqlx::query_as::<_, HypeTrainLeaderRow>(
            r"SELECT htc.participant_id,
                     SUM(htc.units)::bigint AS total_units,
                     RANK() OVER (ORDER BY SUM(htc.units) DESC)::bigint AS rank
               FROM hype_train_contributions htc
               JOIN hype_train_sessions hts ON htc.session_id = hts.id
              WHERE hts.stream_id = $1
              GROUP BY htc.participant_id
              ORDER BY total_units DESC
              LIMIT $2",
        )
        .bind(Uuid::from_u128(stream_id.0))
        .bind(limit.clamp(1, 100))
        .fetch_all(&self.pool)
        .await
    }

    /// Top contributors for a single session, ranked by total units
    /// (RANK OVER ORDER BY SUM(units) DESC), limited to the top `limit` rows.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn session_leaderboard(
        &self,
        session_id: uuid::Uuid,
        limit: i64,
    ) -> Result<Vec<HypeTrainLeaderRow>, sqlx::Error> {
        sqlx::query_as::<_, HypeTrainLeaderRow>(
            r"SELECT participant_id,
                     SUM(units)::bigint AS total_units,
                     RANK() OVER (ORDER BY SUM(units) DESC)::bigint AS rank
               FROM hype_train_contributions
              WHERE session_id = $1
              GROUP BY participant_id
              ORDER BY total_units DESC
              LIMIT $2",
        )
        .bind(session_id)
        .bind(limit.max(1).min(100))
        .fetch_all(&self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_contribution_starts_at_level_one() {
        // Below a full level's worth of units stays at level 1.
        let e = apply_contribution(0, 3);
        assert_eq!(e.level, 1);
        assert_eq!(e.contribution, 3);
    }

    #[test]
    fn each_full_bucket_advances_one_level() {
        assert_eq!(apply_contribution(0, UNITS_PER_LEVEL).level, 2);
        assert_eq!(apply_contribution(0, 2 * UNITS_PER_LEVEL).level, 3);
        // Accumulating across calls advances too.
        let e = apply_contribution(UNITS_PER_LEVEL, UNITS_PER_LEVEL);
        assert_eq!(e.contribution, 2 * UNITS_PER_LEVEL);
        assert_eq!(e.level, 3);
    }

    #[test]
    fn level_is_capped() {
        // A huge contribution does not exceed MAX_LEVEL.
        let e = apply_contribution(0, UNITS_PER_LEVEL * 100);
        assert_eq!(e.level, MAX_LEVEL);
        // Saturating add never overflows.
        let e = apply_contribution(u32::MAX, u32::MAX);
        assert_eq!(e.contribution, u32::MAX);
        assert_eq!(e.level, MAX_LEVEL);
    }

    #[test]
    fn expiry_boundary_is_inclusive() {
        let now = OffsetDateTime::now_utc();
        // expires_at strictly in the future ⇒ not expired.
        assert!(!is_expired(now + Duration::seconds(1), now));
        // expires_at == now ⇒ expired (the window has elapsed).
        assert!(is_expired(now, now));
        assert!(is_expired(now - Duration::seconds(1), now));
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored hype_train_
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

    async fn participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(id.to_uuid())
            .bind(format!("hype-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn hype_train_starts_advances_and_expires() {
        let p = pool();
        let repo = HypeTrainRepo::new(p.clone());
        let stream = Ulid::new();
        let alice = participant(&p).await;
        let bob = participant(&p).await;
        let now = OffsetDateTime::now_utc();

        // No train yet.
        assert!(repo.current(stream, now).await.unwrap().is_none());

        // First contribution starts a fresh active train at level 1.
        let s = repo.add_contribution(stream, alice, 3, now).await.unwrap();
        assert_eq!(s.level, 1);
        assert_eq!(s.contribution, 3);
        assert_eq!(s.state, "active");
        let cur = repo.current(stream, now).await.unwrap().expect("active");
        assert_eq!(cur.id, s.id);

        // A second contribution advances the same train past a level boundary.
        let s2 = repo
            .add_contribution(stream, bob, UNITS_PER_LEVEL, now)
            .await
            .unwrap();
        assert_eq!(s2.id, s.id, "same active train");
        assert_eq!(s2.contribution, 3 + i32::try_from(UNITS_PER_LEVEL).unwrap());
        assert_eq!(s2.level, 2);

        // After the window lapses, `current` sweeps it to expired and a new
        // contribution starts a brand-new train.
        let later = now + Duration::seconds(WINDOW_SECS + 1);
        assert!(
            repo.current(stream, later).await.unwrap().is_none(),
            "lapsed"
        );
        let s3 = repo
            .add_contribution(stream, alice, 1, later)
            .await
            .unwrap();
        assert_ne!(s3.id, s.id, "a fresh train started after expiry");
        assert_eq!(s3.level, 1);

        // Cleanup (contributions cascade with their sessions).
        sqlx::query("DELETE FROM hype_train_sessions WHERE stream_id = $1")
            .bind(Uuid::from_u128(stream.0))
            .execute(&p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn concurrent_contributions_do_not_lose_updates() {
        // A pool with enough connections that the concurrent calls genuinely
        // contend (a 2-conn cap would mask the race by serializing at the pool).
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        let p = sqlx::postgres::PgPoolOptions::new()
            .max_connections(12)
            .connect_lazy(&url)
            .expect("pool");
        let stream = Ulid::new();
        let now = OffsetDateTime::now_utc();

        // Seed an active train (contribution = 1) so every concurrent call takes
        // the read-modify-write UPDATE path — the one with the lost-update race.
        HypeTrainRepo::new(p.clone())
            .add_contribution(stream, participant(&p).await, 1, now)
            .await
            .unwrap();

        const N: usize = 10;
        const UNITS: u32 = 2;
        let mut parts = Vec::new();
        for _ in 0..N {
            parts.push(participant(&p).await);
        }
        let mut handles = Vec::new();
        for pid in parts {
            let pool = p.clone();
            handles.push(tokio::spawn(async move {
                HypeTrainRepo::new(pool)
                    .add_contribution(stream, pid, UNITS, now)
                    .await
                    .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        // Every concurrent contribution must be counted: 1 (seed) + N*UNITS. Under
        // the old non-serialized read-modify-write this would be < expected (lost
        // updates); the per-stream advisory lock makes it exact.
        let cur = HypeTrainRepo::new(p.clone())
            .current(stream, now)
            .await
            .unwrap()
            .expect("active");
        let expected = 1 + (i32::try_from(N).unwrap()) * (i32::try_from(UNITS).unwrap());
        assert_eq!(
            cur.contribution, expected,
            "all {N} concurrent contributions counted — no lost update"
        );

        sqlx::query("DELETE FROM hype_train_sessions WHERE stream_id = $1")
            .bind(Uuid::from_u128(stream.0))
            .execute(&p)
            .await
            .ok();
    }
}
