//! Creator goal / bounty-bar repository.
//!
//! Backs `migrations/0090_goals.sql`. A "goal" is a streamer goal bar: a titled
//! target on a metric (`gifts` / `viewers` / `points`) the broadcast fills toward as
//! progress accrues. `current_tally` climbs toward `target`; the goal flips
//! `active` -> `reached` the instant the tally first meets the target.
//!
//! The CRITICAL operation is [`GoalRepo::add_progress`]: an atomic
//! `UPDATE ... SET current_tally = current_tally + delta,
//!   status = CASE WHEN current_tally + delta >= target THEN 'reached' ELSE status END`
//! with a `WHERE status = 'active' RETURNING`, so concurrent progress bumps serialize
//! correctly and the threshold crossing flips the status exactly once (only the bump
//! that first reaches the target observes `just_reached = true`). A `goal_events`
//! audit row is appended in the same transaction.
//!
//! Purely additive: a NEW [`GoalRepo`]; no existing repo is touched. `stream_id` /
//! `creator_id` are plain UUID columns (not cascading FKs), mirroring
//! [`RaidRepo`](crate::RaidRepo).

use aero_common::{GoalId, ParticipantId};
use serde::Serialize;
use sqlx::PgPool;
use ulid::Ulid;
use uuid::Uuid;

/// Whether `metric` is one of the accepted goal metric types.
/// Pure, so the rule is unit-tested without a database.
#[must_use]
pub fn is_valid_metric(metric: &str) -> bool {
    matches!(metric, "gifts" | "viewers" | "points")
}

/// One creator goal — a `goals` row.
#[derive(Debug, Clone, Serialize)]
pub struct Goal {
    pub id: GoalId,
    pub stream_id: Ulid,
    pub creator_id: ParticipantId,
    pub title: String,
    pub description: Option<String>,
    /// `gifts` / `viewers` / `points`.
    pub metric_type: String,
    pub target: i64,
    pub current_tally: i64,
    /// `active` / `reached` / `cancelled`.
    pub status: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<time::OffsetDateTime>,
}

const COLUMNS: &str = "id, stream_id, creator_id, title, description, metric_type, \
                       target, current_tally, status, created_at, expires_at";

type Row = (
    Uuid,
    Uuid,
    Uuid,
    String,
    Option<String>,
    String,
    i64,
    i64,
    String,
    time::OffsetDateTime,
    Option<time::OffsetDateTime>,
);

fn row_to_model(r: Row) -> Goal {
    let (id, stream_id, creator_id, title, description, metric_type, target, current_tally, status, created_at, expires_at) =
        r;
    Goal {
        id: GoalId::from_uuid(id),
        stream_id: Ulid(stream_id.as_u128()),
        creator_id: ParticipantId::from_uuid(creator_id),
        title,
        description,
        metric_type,
        target,
        current_tally,
        status,
        created_at,
        expires_at,
    }
}

/// Repository over the goal tables.
///
/// Cheap to clone — wraps a [`PgPool`]; feature modules build one inline via
/// [`GoalRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct GoalRepo {
    pool: PgPool,
}

impl GoalRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a goal on a stream, returning its generated id. The caller validates
    /// `metric_type` and owner-gating.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_goal(
        &self,
        stream: Ulid,
        creator: ParticipantId,
        title: &str,
        description: Option<&str>,
        metric_type: &str,
        target: i64,
        expires_at: Option<time::OffsetDateTime>,
    ) -> Result<GoalId, sqlx::Error> {
        let id = GoalId::new();
        sqlx::query(
            r"INSERT INTO goals
                  (id, stream_id, creator_id, title, description, metric_type, target, expires_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id.to_uuid())
        .bind(Uuid::from_u128(stream.0))
        .bind(creator.to_uuid())
        .bind(title)
        .bind(description)
        .bind(metric_type)
        .bind(target.max(1))
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Fetch one goal by id, or `None`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get_goal(&self, goal: GoalId) -> Result<Option<Goal>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM goals WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(goal.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// A stream's `active` goals, newest first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_active(&self, stream: Ulid) -> Result<Vec<Goal>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM goals
              WHERE stream_id = $1 AND status = 'active'
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(Uuid::from_u128(stream.0))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// All of a stream's goals (active / reached / cancelled), newest first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_all(&self, stream: Ulid) -> Result<Vec<Goal>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM goals
              WHERE stream_id = $1
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(Uuid::from_u128(stream.0))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Advance an `active` goal's tally by `delta`, flipping it to `reached` on the
    /// threshold crossing. Returns `Some((new_tally, just_reached))` if the goal was
    /// active and updated, or `None` if the goal is unknown / already reached /
    /// cancelled (so the bump is a no-op).
    ///
    /// Atomic: the `UPDATE ... WHERE status = 'active' RETURNING` both bumps the
    /// tally and conditionally flips the status in one statement, so concurrent
    /// bumps serialize and exactly one bump observes `just_reached = true` (the first
    /// to reach `target`). A `goal_events` audit row is appended in the same tx.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the transaction.
    pub async fn add_progress(
        &self,
        goal: GoalId,
        delta: i64,
    ) -> Result<Option<(i64, bool)>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        // Bump + conditional flip, returning the new tally and whether THIS bump
        // crossed the threshold (was below target before, at/above after). Only acts
        // on still-active goals.
        let updated = sqlx::query_as::<_, (i64, bool)>(
            r"UPDATE goals
                 SET current_tally = current_tally + $2,
                     status = CASE
                        WHEN current_tally + $2 >= target THEN 'reached'
                        ELSE status
                     END
               WHERE id = $1 AND status = 'active'
               RETURNING current_tally,
                         (current_tally - $2 < target AND current_tally >= target) AS just_reached",
        )
        .bind(goal.to_uuid())
        .bind(delta)
        .fetch_optional(&mut *tx)
        .await?;

        let Some((new_tally, just_reached)) = updated else {
            // Goal unknown / not active — nothing to do; let the tx drop (rollback).
            return Ok(None);
        };

        // Audit the contribution.
        sqlx::query(
            r"INSERT INTO goal_events (id, goal_id, delta) VALUES ($1, $2, $3)",
        )
        .bind(Uuid::from_u128(ulid::Ulid::new().0))
        .bind(goal.to_uuid())
        .bind(delta)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(Some((new_tally, just_reached)))
    }

    /// Cancel a goal — owner only (the caller passes the resolved owner). Returns
    /// `true` if THIS call cancelled an active/reached goal (idempotent: `false` when
    /// the goal is gone, not owned by `actor`, or already cancelled). Verifying
    /// `creator_id` in the `WHERE` keeps the check atomic and leaks nothing.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn cancel_goal(
        &self,
        goal: GoalId,
        actor: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let res = sqlx::query(
            r"UPDATE goals SET status = 'cancelled'
               WHERE id = $1 AND creator_id = $2 AND status <> 'cancelled'",
        )
        .bind(goal.to_uuid())
        .bind(actor.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_validation() {
        assert!(is_valid_metric("gifts"));
        assert!(is_valid_metric("viewers"));
        assert!(is_valid_metric("points"));
        assert!(!is_valid_metric("subs"));
        assert!(!is_valid_metric(""));
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored goals_
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

    async fn creator(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(id.to_uuid())
            .bind(format!("goal-creator-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn goals_tally_consistent_under_repeated_progress() {
        let p = pool();
        let repo = GoalRepo::new(p.clone());
        let stream = Ulid::new();
        let owner = creator(&p).await;

        let goal = repo
            .create_goal(stream, owner, "100 gifts", None, "gifts", 100, None)
            .await
            .unwrap();

        // Repeated small bumps accumulate exactly.
        let mut last = 0;
        for _ in 0..9 {
            let (tally, reached) = repo.add_progress(goal, 10).await.unwrap().unwrap();
            assert!(!reached, "not reached until tally >= 100");
            assert_eq!(tally, last + 10);
            last = tally;
        }
        assert_eq!(last, 90);
        assert_eq!(repo.get_goal(goal).await.unwrap().unwrap().current_tally, 90);
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn goals_threshold_crossing_flips_status_once() {
        let p = pool();
        let repo = GoalRepo::new(p.clone());
        let stream = Ulid::new();
        let owner = creator(&p).await;

        let goal = repo
            .create_goal(stream, owner, "Reach 30", None, "points", 30, None)
            .await
            .unwrap();

        // Below target: active, not yet reached.
        let (t1, r1) = repo.add_progress(goal, 20).await.unwrap().unwrap();
        assert_eq!(t1, 20);
        assert!(!r1);
        assert_eq!(repo.get_goal(goal).await.unwrap().unwrap().status, "active");

        // This bump crosses the threshold: just_reached true, status flips to reached.
        let (t2, r2) = repo.add_progress(goal, 15).await.unwrap().unwrap();
        assert_eq!(t2, 35);
        assert!(r2, "the crossing bump reports just_reached");
        assert_eq!(repo.get_goal(goal).await.unwrap().unwrap().status, "reached");

        // Once reached, further bumps are no-ops (status no longer active): None.
        assert!(repo.add_progress(goal, 100).await.unwrap().is_none());
        // Tally unchanged by the no-op bump.
        assert_eq!(repo.get_goal(goal).await.unwrap().unwrap().current_tally, 35);

        // The reached goal drops out of the active listing.
        assert!(repo.list_active(stream).await.unwrap().iter().all(|g| g.id != goal));
        assert!(repo.list_all(stream).await.unwrap().iter().any(|g| g.id == goal));
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn goals_cancel_is_owner_gated_and_idempotent() {
        let p = pool();
        let repo = GoalRepo::new(p.clone());
        let stream = Ulid::new();
        let owner = creator(&p).await;
        let stranger = creator(&p).await;

        let goal = repo
            .create_goal(stream, owner, "Cancelable", None, "viewers", 50, None)
            .await
            .unwrap();

        // A non-owner cannot cancel.
        assert!(!repo.cancel_goal(goal, stranger).await.unwrap());
        assert_eq!(repo.get_goal(goal).await.unwrap().unwrap().status, "active");

        // Owner cancels; second cancel is a no-op.
        assert!(repo.cancel_goal(goal, owner).await.unwrap());
        assert!(!repo.cancel_goal(goal, owner).await.unwrap());
        assert_eq!(repo.get_goal(goal).await.unwrap().unwrap().status, "cancelled");

        // A cancelled goal no longer accepts progress.
        assert!(repo.add_progress(goal, 10).await.unwrap().is_none());
    }
}
