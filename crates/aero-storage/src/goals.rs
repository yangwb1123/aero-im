//! Creator goal / bounty-bar repository.
//!
//! Backs `migrations/0090_goals.sql`. A "goal" is a streamer goal bar: a titled
//! target on a metric (`gifts` / `viewers` / `points`) the broadcast fills toward as
//! progress accrues. `current_tally` climbs toward `target`; the goal flips
//! `active` -> `reached` the instant the tally first meets the target.
//!
//! The CRITICAL operation is [`GoalRepo::add_progress`]: a locked CTE computes the
//! representable contribution with `numeric`, applies it to an active goal, and
//! conditionally flips the status. Concurrent bumps serialize, the threshold
//! crossing flips exactly once, and BIGINT overflow saturates at the ceiling. The
//! applied delta is appended to `goal_events` in the same transaction.
//!
//! Purely additive: a NEW [`GoalRepo`]; no existing repo is touched. `stream_id` /
//! `creator_id` are plain UUID columns (not cascading FKs), mirroring
//! [`RaidRepo`](crate::RaidRepo).

use aero_common::{GoalId, ParticipantId};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};
use ulid::Ulid;
use uuid::Uuid;

/// Whether `metric` is one of the accepted goal metric types.
/// Pure, so the rule is unit-tested without a database.
#[must_use]
pub fn is_valid_metric(metric: &str) -> bool {
    matches!(metric, "gifts" | "viewers" | "points")
}

pub const MAX_ACTIVE_GOALS_PER_STREAM: i64 = 10;
pub const MAX_GOAL_TITLE_CHARS: usize = 120;
pub const MAX_GOAL_DESCRIPTION_CHARS: usize = 1_000;

#[derive(Debug, thiserror::Error)]
pub enum GoalCreateError {
    #[error("stream not found")]
    StreamNotFound,
    #[error("only the stream owner may create goals")]
    NotOwner,
    #[error("stream owner lacks effective access to create goals")]
    NotAuthorized,
    #[error("active goal limit reached")]
    LimitReached,
    #[error("invalid goal: {0}")]
    InvalidInput(String),
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

/// Owner-scoped cancellation result. Unknown and foreign-owned ids intentionally
/// collapse to [`Self::NotFound`], while the real owner retains idempotent retry
/// semantics for an already-cancelled goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalCancelOutcome {
    Cancelled,
    AlreadyCancelled,
    NotFound,
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
                       target, current_tally, \
                       CASE WHEN status = 'active' AND expires_at <= CURRENT_TIMESTAMP \
                            THEN 'expired' ELSE status END AS status, \
                       created_at, expires_at";

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
    let (
        id,
        stream_id,
        creator_id,
        title,
        description,
        metric_type,
        target,
        current_tally,
        status,
        created_at,
        expires_at,
    ) = r;
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

async fn has_effective_stream_access(
    tx: &mut Transaction<'_, Postgres>,
    room_id: Option<Uuid>,
    actor: ParticipantId,
) -> Result<bool, sqlx::Error> {
    if let Some(room_id) = room_id {
        sqlx::query_scalar::<_, bool>("SELECT aero_effective_room_access($1, $2, NULL::uuid)")
            .bind(room_id)
            .bind(actor.to_uuid())
            .fetch_one(&mut **tx)
            .await
    } else {
        Ok(sqlx::query_scalar::<_, bool>(
            "SELECT true
               FROM participants
              WHERE id = $1
                AND deleted_at IS NULL
              FOR SHARE",
        )
        .bind(actor.to_uuid())
        .fetch_optional(&mut **tx)
        .await?
        .is_some())
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

    /// Create a bounded active goal after locking the owner's effective access,
    /// then the canonical stream row. Ownership, access, expiration, input
    /// bounds, and the per-stream active quota are rechecked in the same
    /// transaction as the insert.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_goal_authorized(
        &self,
        stream: Ulid,
        creator: ParticipantId,
        title: &str,
        description: Option<&str>,
        metric_type: &str,
        target: i64,
        expires_at: Option<time::OffsetDateTime>,
    ) -> Result<GoalId, GoalCreateError> {
        if title.trim().is_empty() || title.chars().count() > MAX_GOAL_TITLE_CHARS {
            return Err(GoalCreateError::InvalidInput(format!(
                "title must contain 1..={MAX_GOAL_TITLE_CHARS} characters"
            )));
        }
        if description.is_some_and(|value| value.chars().count() > MAX_GOAL_DESCRIPTION_CHARS) {
            return Err(GoalCreateError::InvalidInput(format!(
                "description must be at most {MAX_GOAL_DESCRIPTION_CHARS} characters"
            )));
        }
        if !is_valid_metric(metric_type) || target < 1 {
            return Err(GoalCreateError::InvalidInput(
                "metric_type or target is invalid".into(),
            ));
        }
        if expires_at.is_some_and(|expiry| expiry <= time::OffsetDateTime::now_utc()) {
            return Err(GoalCreateError::InvalidInput(
                "expires_at must be in the future".into(),
            ));
        }

        let mut tx = self.pool.begin().await?;
        let stream_id = Uuid::from_u128(stream.0);
        // Resolve without locking the aggregate. Room-linked authorization must
        // take workspace/room/membership locks before the stream row to avoid
        // inverting the canonical live-governance order.
        let resolved = sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
            "SELECT owner_id, room_id FROM streams WHERE id = $1",
        )
        .bind(stream_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(GoalCreateError::StreamNotFound)?;
        if resolved.0 != creator.to_uuid() {
            return Err(GoalCreateError::NotOwner);
        }
        if !has_effective_stream_access(&mut tx, resolved.1, creator).await? {
            return Err(GoalCreateError::NotAuthorized);
        }

        let locked = sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
            "SELECT owner_id, room_id FROM streams WHERE id = $1 FOR UPDATE",
        )
        .bind(stream_id)
        .fetch_optional(&mut *tx)
        .await?;
        if locked != Some(resolved) {
            return match locked {
                None => Err(GoalCreateError::StreamNotFound),
                Some((owner, _)) if owner != creator.to_uuid() => Err(GoalCreateError::NotOwner),
                Some(_) => Err(GoalCreateError::NotAuthorized),
            };
        }
        if let Some(expiry) = expires_at {
            let still_future =
                sqlx::query_scalar::<_, bool>("SELECT $1::timestamptz > clock_timestamp()")
                    .bind(expiry)
                    .fetch_one(&mut *tx)
                    .await?;
            if !still_future {
                return Err(GoalCreateError::InvalidInput(
                    "expires_at must be in the future".into(),
                ));
            }
        }
        let active = sqlx::query_scalar::<_, i64>(
            r"SELECT count(*)
                FROM goals
               WHERE stream_id = $1
                 AND status = 'active'
                 AND (expires_at IS NULL OR expires_at > clock_timestamp())",
        )
        .bind(stream_id)
        .fetch_one(&mut *tx)
        .await?;
        if active >= MAX_ACTIVE_GOALS_PER_STREAM {
            return Err(GoalCreateError::LimitReached);
        }

        let id = GoalId::new();
        sqlx::query(
            r"INSERT INTO goals
                  (id, stream_id, creator_id, title, description, metric_type, target, expires_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id.to_uuid())
        .bind(stream_id)
        .bind(creator.to_uuid())
        .bind(title)
        .bind(description)
        .bind(metric_type)
        .bind(target)
        .bind(expires_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
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
              WHERE stream_id = $1
                AND status = 'active'
                AND (expires_at IS NULL OR expires_at > CURRENT_TIMESTAMP)
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
              ORDER BY created_at DESC, id DESC
              LIMIT 100"
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
    /// Atomic: the locked CTE both bumps the tally and conditionally flips the
    /// status, so concurrent bumps serialize and exactly one bump observes
    /// `just_reached = true`. At the `BIGINT` ceiling the applied contribution is
    /// saturated to the remaining representable capacity. That applied delta is
    /// appended to `goal_events` in the same transaction.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the transaction.
    pub async fn add_progress(
        &self,
        goal: GoalId,
        delta: i64,
    ) -> Result<Option<(i64, bool)>, sqlx::Error> {
        if delta <= 0 {
            return Ok(None);
        }
        let mut tx = self.pool.begin().await?;

        // Bump + conditional flip, returning the new tally and whether THIS bump
        // crossed the threshold (was below target before, at/above after). Only acts
        // on still-active goals.
        let updated = sqlx::query_as::<_, (i64, bool, i64)>(
            r"WITH candidate AS (
                   SELECT id,
                          current_tally AS prior_tally,
                          LEAST(
                              $2::numeric,
                              9223372036854775807::numeric - current_tally::numeric
                          )::bigint AS applied_delta
                     FROM goals
                    WHERE id = $1
                      AND status = 'active'
                      AND (expires_at IS NULL OR expires_at > clock_timestamp())
                      FOR UPDATE
               ),
               updated AS (
                   UPDATE goals AS goal
                      SET current_tally = goal.current_tally + candidate.applied_delta,
                          status = CASE
                              WHEN goal.current_tally + candidate.applied_delta >= goal.target
                                  THEN 'reached'
                              ELSE goal.status
                          END
                     FROM candidate
                    WHERE goal.id = candidate.id
                RETURNING goal.current_tally,
                          goal.target,
                          candidate.prior_tally,
                          candidate.applied_delta
               )
               SELECT current_tally,
                      prior_tally < target AND current_tally >= target AS just_reached,
                      applied_delta
                 FROM updated",
        )
        .bind(goal.to_uuid())
        .bind(delta)
        .fetch_optional(&mut *tx)
        .await;
        let updated = match updated {
            Ok(updated) => updated,
            Err(error)
                if error
                    .as_database_error()
                    .and_then(|database| database.constraint())
                    == Some("goals_expired_progress") =>
            {
                // The UPDATE may have evaluated its predicate before waiting
                // on a concurrent row lock. The trigger rechecks wall-clock
                // expiry at write time; preserve the public expired/no-op
                // contract when that final fence wins.
                return Ok(None);
            }
            Err(error) => return Err(error),
        };

        let Some((new_tally, just_reached, applied_delta)) = updated else {
            // Goal unknown / not active — nothing to do; let the tx drop (rollback).
            return Ok(None);
        };

        // Audit the contribution.
        sqlx::query(r"INSERT INTO goal_events (id, goal_id, delta) VALUES ($1, $2, $3)")
            .bind(Uuid::from_u128(ulid::Ulid::new().0))
            .bind(goal.to_uuid())
            .bind(applied_delta)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(Some((new_tally, just_reached)))
    }

    /// Cancel a goal as the stream's current canonical owner. Effective access is
    /// locked before the stream and goal aggregate rows. Unknown goals, missing
    /// streams, former/foreign owners, and owners whose room/account access was
    /// revoked all return the same [`GoalCancelOutcome::NotFound`].
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn cancel_goal(
        &self,
        goal: GoalId,
        actor: ParticipantId,
    ) -> Result<GoalCancelOutcome, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let goal_id = goal.to_uuid();
        let stream_id = sqlx::query_scalar::<_, Uuid>("SELECT stream_id FROM goals WHERE id = $1")
            .bind(goal_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(stream_id) = stream_id else {
            tx.commit().await?;
            return Ok(GoalCancelOutcome::NotFound);
        };

        // Resolve the governance route without an aggregate lock, then enter
        // room/workspace/identity locks before stream -> goal. Every denied
        // branch stays opaque to former and foreign owners.
        let resolved_stream = sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
            "SELECT owner_id, room_id FROM streams WHERE id = $1",
        )
        .bind(stream_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(resolved_stream) = resolved_stream else {
            tx.commit().await?;
            return Ok(GoalCancelOutcome::NotFound);
        };
        if resolved_stream.0 != actor.to_uuid()
            || !has_effective_stream_access(&mut tx, resolved_stream.1, actor).await?
        {
            tx.commit().await?;
            return Ok(GoalCancelOutcome::NotFound);
        }

        let locked_stream = sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
            "SELECT owner_id, room_id FROM streams WHERE id = $1 FOR UPDATE",
        )
        .bind(stream_id)
        .fetch_optional(&mut *tx)
        .await?;
        if locked_stream != Some(resolved_stream) {
            tx.commit().await?;
            return Ok(GoalCancelOutcome::NotFound);
        }

        let locked_goal = sqlx::query_as::<_, (Uuid, String)>(
            "SELECT stream_id, status FROM goals WHERE id = $1 FOR UPDATE",
        )
        .bind(goal_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((locked_goal_stream, status)) = locked_goal else {
            tx.commit().await?;
            return Ok(GoalCancelOutcome::NotFound);
        };
        if locked_goal_stream != stream_id {
            tx.commit().await?;
            return Ok(GoalCancelOutcome::NotFound);
        }
        if status == "cancelled" {
            tx.commit().await?;
            return Ok(GoalCancelOutcome::AlreadyCancelled);
        }

        let updated = sqlx::query(
            r"UPDATE goals SET status = 'cancelled'
               WHERE id = $1
                 AND stream_id = $2
                 AND status <> 'cancelled'",
        )
        .bind(goal_id)
        .bind(stream_id)
        .execute(&mut *tx)
        .await?;
        debug_assert_eq!(updated.rows_affected(), 1);
        tx.commit().await?;
        Ok(GoalCancelOutcome::Cancelled)
    }
}

#[cfg(test)]
#[path = "goals/audit_tests.rs"]
mod audit_tests;

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored goals_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::{NewStream, StreamRepo};
    use aero_common::StreamProtocol;

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

    async fn creator_stream(p: &PgPool) -> (ParticipantId, Ulid) {
        let owner = creator(p).await;
        let stream = StreamRepo::new(p.clone())
            .create(NewStream {
                owner_id: owner,
                room_id: None,
                title: format!("goal-stream-{owner}"),
                protocol: StreamProtocol::Rtmp,
                stream_key: None,
            })
            .await
            .unwrap();
        (owner, stream.id)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn goals_tally_consistent_under_repeated_progress() {
        let p = pool();
        let repo = GoalRepo::new(p.clone());
        let (owner, stream) = creator_stream(&p).await;

        let goal = repo
            .create_goal_authorized(stream, owner, "100 gifts", None, "gifts", 100, None)
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
        assert_eq!(
            repo.get_goal(goal).await.unwrap().unwrap().current_tally,
            90
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn goals_threshold_crossing_flips_status_once() {
        let p = pool();
        let repo = GoalRepo::new(p.clone());
        let (owner, stream) = creator_stream(&p).await;

        let goal = repo
            .create_goal_authorized(stream, owner, "Reach 30", None, "points", 30, None)
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
        assert_eq!(
            repo.get_goal(goal).await.unwrap().unwrap().status,
            "reached"
        );

        // Once reached, further bumps are no-ops (status no longer active): None.
        assert!(repo.add_progress(goal, 100).await.unwrap().is_none());
        // Tally unchanged by the no-op bump.
        assert_eq!(
            repo.get_goal(goal).await.unwrap().unwrap().current_tally,
            35
        );

        // The reached goal drops out of the active listing.
        assert!(repo
            .list_active(stream)
            .await
            .unwrap()
            .iter()
            .all(|g| g.id != goal));
        assert!(repo
            .list_all(stream)
            .await
            .unwrap()
            .iter()
            .any(|g| g.id == goal));
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn goals_cancel_is_owner_gated_and_idempotent() {
        let p = pool();
        let repo = GoalRepo::new(p.clone());
        let (owner, stream) = creator_stream(&p).await;
        let stranger = creator(&p).await;

        let goal = repo
            .create_goal_authorized(stream, owner, "Cancelable", None, "viewers", 50, None)
            .await
            .unwrap();

        // A non-owner cannot cancel.
        assert_eq!(
            repo.cancel_goal(goal, stranger).await.unwrap(),
            GoalCancelOutcome::NotFound
        );
        assert_eq!(repo.get_goal(goal).await.unwrap().unwrap().status, "active");

        // Owner cancels; second cancel is a no-op.
        assert_eq!(
            repo.cancel_goal(goal, owner).await.unwrap(),
            GoalCancelOutcome::Cancelled
        );
        assert_eq!(
            repo.cancel_goal(goal, owner).await.unwrap(),
            GoalCancelOutcome::AlreadyCancelled
        );
        assert_eq!(
            repo.get_goal(goal).await.unwrap().unwrap().status,
            "cancelled"
        );

        // A cancelled goal no longer accepts progress.
        assert!(repo.add_progress(goal, 10).await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migrations through 0222"]
    async fn expired_goals_are_inactive_and_do_not_accept_progress() {
        let p = pool();
        let repo = GoalRepo::new(p.clone());
        let (owner, stream) = creator_stream(&p).await;
        let goal = repo
            .create_goal_authorized(
                stream,
                owner,
                "Short window",
                None,
                "gifts",
                10,
                Some(time::OffsetDateTime::now_utc() + time::Duration::milliseconds(100)),
            )
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(repo
            .list_active(stream)
            .await
            .unwrap()
            .iter()
            .all(|item| item.id != goal));
        assert!(repo.add_progress(goal, 1).await.unwrap().is_none());
        assert_eq!(
            repo.get_goal(goal).await.unwrap().unwrap().status,
            "expired"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migrations through 0222"]
    async fn active_goal_quota_is_linearizable_and_raw_writes_are_fenced() {
        let p = pool();
        let repo = GoalRepo::new(p.clone());
        let (owner, stream) = creator_stream(&p).await;
        let stranger = creator(&p).await;

        for index in 0..(MAX_ACTIVE_GOALS_PER_STREAM - 1) {
            repo.create_goal_authorized(
                stream,
                owner,
                &format!("goal-{index}"),
                None,
                "gifts",
                100,
                None,
            )
            .await
            .unwrap();
        }
        let repo_a = repo.clone();
        let repo_b = repo.clone();
        let (a, b) = tokio::join!(
            repo_a.create_goal_authorized(stream, owner, "contender-a", None, "gifts", 100, None,),
            repo_b.create_goal_authorized(stream, owner, "contender-b", None, "gifts", 100, None,)
        );
        assert_eq!(
            usize::from(a.is_ok()) + usize::from(b.is_ok()),
            1,
            "exactly one contender consumes the final active slot"
        );
        assert!(matches!(
            a.as_ref().err().or_else(|| b.as_ref().err()),
            Some(GoalCreateError::LimitReached)
        ));
        assert_eq!(
            repo.list_active(stream).await.unwrap().len(),
            usize::try_from(MAX_ACTIVE_GOALS_PER_STREAM).unwrap()
        );

        let raw = sqlx::query(
            r"INSERT INTO goals
                  (id, stream_id, creator_id, title, metric_type, target)
               VALUES ($1, $2, $3, 'forged', 'gifts', 10)",
        )
        .bind(GoalId::new().to_uuid())
        .bind(Uuid::from_u128(stream.0))
        .bind(stranger.to_uuid())
        .execute(&p)
        .await
        .expect_err("a non-owner raw goal must be rejected");
        assert_eq!(
            raw.as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .as_deref(),
            Some("42501")
        );
    }
}
