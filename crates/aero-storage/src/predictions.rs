//! Community-predictions / channel-betting repository.
//!
//! Backs `migrations/0092_predictions.sql`. Twitch-style "Channel Prediction": a
//! creator opens a prediction (a question + 2+ outcomes), viewers STAKE channel
//! points on one outcome, the creator LOCKS staking then RESOLVES to a winning
//! outcome, and winners are paid PROPORTIONALLY from the total pool. This is built
//! ON TOP of the channel-points ledger ([`ChannelPointsRepo`](crate::ChannelPointsRepo)
//! / `points_ledger`, 0089): staking DEBITS the viewer's balance with the
//! prediction's creator; payout/refund CREDITS it. Distinct from a poll (0027),
//! which is plain voting with no stakes / payouts.
//!
//! Three tables:
//!   * `predictions`         — the question + lifecycle (`open`/`locked`/`resolved`/
//!     `cancelled`) + (on resolve) `winning_outcome_idx`.
//!   * `prediction_outcomes` — the 2+ labelled outcomes, keyed by a 0-based `idx`.
//!   * `prediction_stakes`   — one row per `(prediction, viewer)` wager, UNIQUE on
//!     that pair (a second stake is rejected), with `payout` stamped on settle.
//!
//! The CRITICAL invariant is double-spend safety on [`PredictionRepo::stake`]: the
//! debit is a conditional `UPDATE points_ledger SET balance = balance - $ WHERE
//! viewer = $ AND creator = $ AND balance >= $ RETURNING` (mirroring
//! [`ChannelPointsRepo::redeem`](crate::ChannelPointsRepo::redeem)) and the stake
//! insert happen in the SAME transaction — so two concurrent stakes against one
//! balance cannot both succeed. Settlement ([`PredictionRepo::resolve`] /
//! [`PredictionRepo::cancel`]) likewise runs in ONE transaction, crediting every
//! payout/refund and stamping the stakes atomically.
//!
//! Purely additive: a NEW [`PredictionRepo`]; no existing repo is touched.
//! `stream_id` / `creator_id` / `viewer_id` are plain UUID columns (not cascading
//! FKs), mirroring [`GoalRepo`](crate::GoalRepo) / [`BanAppealRepo`](crate::BanAppealRepo).

use aero_common::{ParticipantId, PredictionId};
use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use ulid::Ulid;
use uuid::Uuid;

/// Why a stake was rejected. Kept distinct from [`sqlx::Error`] so the server maps
/// each case to a clean status (409 / 400 / 404) instead of a generic 500.
#[derive(Debug, thiserror::Error)]
pub enum StakeError {
    /// The prediction is unknown.
    #[error("prediction not found")]
    NotFound,
    /// The prediction is not `open` (already locked / resolved / cancelled), so it
    /// is not accepting stakes.
    #[error("prediction is not open for staking")]
    BadState,
    /// The chosen `outcome_idx` is not one of the prediction's outcomes.
    #[error("no such outcome")]
    BadOutcome,
    /// The viewer's balance with the creator is below the staked points (the atomic
    /// debit removed nothing).
    #[error("insufficient points")]
    InsufficientPoints,
    /// The viewer already staked on this prediction (one stake per viewer).
    #[error("already staked on this prediction")]
    AlreadyStaked,
    /// A storage error occurred.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Why a resolve was rejected.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// The prediction is unknown.
    #[error("prediction not found")]
    NotFound,
    /// The prediction is not in a resolvable state (`open` / `locked`) — it is
    /// already resolved or cancelled.
    #[error("prediction is not resolvable")]
    BadState,
    /// The chosen winning `outcome_idx` is not one of the prediction's outcomes.
    #[error("no such outcome")]
    BadOutcome,
    /// A storage error occurred.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// One labelled outcome of a prediction — a `prediction_outcomes` row.
#[derive(Debug, Clone, Serialize)]
pub struct PredictionOutcome {
    /// 0-based index; stakes and the resolution refer to this.
    pub idx: i32,
    pub label: String,
}

/// A community prediction — a `predictions` row plus its outcomes.
#[derive(Debug, Clone, Serialize)]
pub struct Prediction {
    pub id: PredictionId,
    pub stream_id: Ulid,
    pub creator_id: ParticipantId,
    pub question: String,
    /// `open` / `locked` / `resolved` / `cancelled`.
    pub status: String,
    pub winning_outcome_idx: Option<i32>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub locked_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub resolved_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    /// The 2+ outcomes, ascending by `idx`.
    pub outcomes: Vec<PredictionOutcome>,
}

const PREDICTION_COLUMNS: &str = "id, stream_id, creator_id, question, status, \
                                  winning_outcome_idx, created_at, locked_at, resolved_at, \
                                  expires_at";

type PredictionRow = (
    Uuid,
    Uuid,
    Uuid,
    String,
    String,
    Option<i32>,
    OffsetDateTime,
    Option<OffsetDateTime>,
    Option<OffsetDateTime>,
    Option<OffsetDateTime>,
);

/// Build a [`Prediction`] from its base row + (separately fetched) outcomes.
fn prediction_from_parts(r: PredictionRow, outcomes: Vec<PredictionOutcome>) -> Prediction {
    let (id, stream_id, creator_id, question, status, winning_outcome_idx, created_at, locked_at, resolved_at, expires_at) =
        r;
    Prediction {
        id: PredictionId::from_uuid(id),
        stream_id: Ulid(stream_id.as_u128()),
        creator_id: ParticipantId::from_uuid(creator_id),
        question,
        status,
        winning_outcome_idx,
        created_at,
        locked_at,
        resolved_at,
        expires_at,
        outcomes,
    }
}

/// Repository over the prediction tables.
///
/// Cheap to clone — wraps a [`PgPool`]; feature modules build one inline via
/// [`PredictionRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct PredictionRepo {
    pool: PgPool,
}

impl PredictionRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Open a prediction on a stream with 2+ outcomes, returning it (status `open`).
    /// Inserts the `predictions` row and one `prediction_outcomes` row per label
    /// (0-based `idx`) in one transaction. The caller validates owner-gating.
    ///
    /// # Errors
    /// - [`sqlx::Error`] (a generic row error) when fewer than 2 outcomes are given
    ///   (mapped by the caller to a 400), or on any storage error.
    pub async fn create_prediction(
        &self,
        stream: Ulid,
        creator: ParticipantId,
        question: &str,
        outcomes: &[String],
        expires: Option<OffsetDateTime>,
    ) -> Result<Prediction, sqlx::Error> {
        if outcomes.len() < 2 {
            // A prediction needs at least two outcomes to bet between. Surface as a
            // protocol error the server maps to 400 (it pre-validates too).
            return Err(sqlx::Error::Protocol(
                "a prediction requires at least 2 outcomes".into(),
            ));
        }
        let id = PredictionId::new();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r"INSERT INTO predictions (id, stream_id, creator_id, question, expires_at)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id.to_uuid())
        .bind(Uuid::from_u128(stream.0))
        .bind(creator.to_uuid())
        .bind(question)
        .bind(expires)
        .execute(&mut *tx)
        .await?;

        let mut models = Vec::with_capacity(outcomes.len());
        for (i, label) in outcomes.iter().enumerate() {
            let idx = i32::try_from(i).unwrap_or(i32::MAX);
            sqlx::query(
                r"INSERT INTO prediction_outcomes (prediction_id, idx, label)
                   VALUES ($1, $2, $3)",
            )
            .bind(id.to_uuid())
            .bind(idx)
            .bind(label)
            .execute(&mut *tx)
            .await?;
            models.push(PredictionOutcome { idx, label: label.clone() });
        }
        tx.commit().await?;

        Ok(Prediction {
            id,
            stream_id: stream,
            creator_id: creator,
            question: question.to_owned(),
            status: "open".to_owned(),
            winning_outcome_idx: None,
            created_at: OffsetDateTime::now_utc(),
            locked_at: None,
            resolved_at: None,
            expires_at: expires,
            outcomes: models,
        })
    }

    /// Fetch the outcomes of a prediction (ascending by `idx`), within an executor.
    async fn fetch_outcomes<'e, E>(
        exec: E,
        prediction: PredictionId,
    ) -> Result<Vec<PredictionOutcome>, sqlx::Error>
    where
        E: sqlx::PgExecutor<'e>,
    {
        let rows = sqlx::query_as::<_, (i32, String)>(
            r"SELECT idx, label FROM prediction_outcomes
               WHERE prediction_id = $1 ORDER BY idx ASC",
        )
        .bind(prediction.to_uuid())
        .fetch_all(exec)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(idx, label)| PredictionOutcome { idx, label })
            .collect())
    }

    /// Fetch one prediction by id (with its outcomes), or `None`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the queries.
    pub async fn get(&self, prediction: PredictionId) -> Result<Option<Prediction>, sqlx::Error> {
        let sql = format!("SELECT {PREDICTION_COLUMNS} FROM predictions WHERE id = $1");
        let Some(row) = sqlx::query_as::<_, PredictionRow>(&sql)
            .bind(prediction.to_uuid())
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(None);
        };
        let outcomes = Self::fetch_outcomes(&self.pool, prediction).await?;
        Ok(Some(prediction_from_parts(row, outcomes)))
    }

    /// A stream's `open` predictions (with outcomes), newest first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the queries.
    pub async fn list_open(&self, stream: Ulid) -> Result<Vec<Prediction>, sqlx::Error> {
        let sql = format!(
            "SELECT {PREDICTION_COLUMNS}
               FROM predictions
              WHERE stream_id = $1 AND status = 'open'
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, PredictionRow>(&sql)
            .bind(Uuid::from_u128(stream.0))
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let id = PredictionId::from_uuid(row.0);
            let outcomes = Self::fetch_outcomes(&self.pool, id).await?;
            out.push(prediction_from_parts(row, outcomes));
        }
        Ok(out)
    }

    /// Stake `points` of the viewer's channel points (with the prediction's creator)
    /// on `outcome_idx`. In ONE transaction: verify the prediction is `open` and the
    /// outcome exists, ATOMICALLY debit the ledger (conditional `WHERE balance >=
    /// points`), then insert the stake (UNIQUE `(prediction, viewer)` => already
    /// staked).
    ///
    /// Double-spend safety: the debit removes a row only if the balance covers the
    /// stake; if it removes nothing the tx rolls back with
    /// [`StakeError::InsufficientPoints`] — two concurrent stakes against one balance
    /// can never both succeed.
    ///
    /// # Errors
    /// - [`StakeError::NotFound`] / [`StakeError::BadState`] / [`StakeError::BadOutcome`].
    /// - [`StakeError::InsufficientPoints`] if the balance does not cover the stake.
    /// - [`StakeError::AlreadyStaked`] if the viewer already staked here.
    /// - [`StakeError::Db`] on any storage error.
    pub async fn stake(
        &self,
        prediction: PredictionId,
        viewer: ParticipantId,
        outcome_idx: i32,
        points: i64,
    ) -> Result<(), StakeError> {
        if points <= 0 {
            // A stake must be positive (the CHECK enforces it too); surface early as
            // the same "bad outcome / bad request" class the server maps to 400.
            return Err(StakeError::BadOutcome);
        }
        let mut tx = self.pool.begin().await?;

        // Resolve the prediction (status + creator) inside the tx so the gating and
        // the debited creator are consistent with the stake.
        let row = sqlx::query_as::<_, (String, Uuid)>(
            r"SELECT status, creator_id FROM predictions WHERE id = $1",
        )
        .bind(prediction.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some((status, creator_uuid)) = row else {
            return Err(StakeError::NotFound);
        };
        if status != "open" {
            return Err(StakeError::BadState);
        }

        // The chosen outcome must exist.
        let outcome_ok = sqlx::query_scalar::<_, i32>(
            r"SELECT idx FROM prediction_outcomes WHERE prediction_id = $1 AND idx = $2",
        )
        .bind(prediction.to_uuid())
        .bind(outcome_idx)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !outcome_ok {
            return Err(StakeError::BadOutcome);
        }

        // Conditional debit against the viewer's balance with THIS prediction's
        // creator — only succeeds if the balance covers the stake. A missing ledger
        // row removes nothing => insufficient. Mirrors `ChannelPointsRepo::redeem`.
        let debited = sqlx::query_scalar::<_, i64>(
            r"UPDATE points_ledger
                 SET balance = balance - $3
               WHERE viewer_id = $1 AND creator_id = $2 AND balance >= $3
               RETURNING balance",
        )
        .bind(viewer.to_uuid())
        .bind(creator_uuid)
        .bind(points)
        .fetch_optional(&mut *tx)
        .await?;
        if debited.is_none() {
            return Err(StakeError::InsufficientPoints);
        }

        // Record the stake. A UNIQUE violation on (prediction, viewer) means the
        // viewer already staked here — surface as AlreadyStaked (the debit rolls back
        // with the tx).
        let stake_id = aero_common::PredictionStakeId::new();
        let res = sqlx::query(
            r"INSERT INTO prediction_stakes (id, prediction_id, outcome_idx, viewer_id, points)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(stake_id.to_uuid())
        .bind(prediction.to_uuid())
        .bind(outcome_idx)
        .bind(viewer.to_uuid())
        .bind(points)
        .execute(&mut *tx)
        .await;
        if let Err(e) = res {
            if is_unique_violation(&e) {
                // tx drops -> rollback (the debit is undone).
                return Err(StakeError::AlreadyStaked);
            }
            return Err(StakeError::Db(e));
        }

        tx.commit().await?;
        Ok(())
    }

    /// Lock a prediction (`open` -> `locked`), closing staking. Returns `true` if
    /// THIS call locked an open prediction (idempotent: `false` when unknown / not
    /// open).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn lock(&self, prediction: PredictionId) -> Result<bool, sqlx::Error> {
        let res = sqlx::query(
            r"UPDATE predictions SET status = 'locked', locked_at = now()
               WHERE id = $1 AND status = 'open'",
        )
        .bind(prediction.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Resolve a prediction to `winning_idx`, paying winners PROPORTIONALLY from the
    /// total pool. In ONE transaction: require status `open` / `locked`, verify the
    /// outcome exists, sum the total + winning pools, then either REFUND every staker
    /// (if `winning_pool = 0` — nobody picked the winner) or pay each winner
    /// `floor(stake * total_pool / winning_pool)` (losers get 0), crediting
    /// `points_ledger` and stamping each stake's `payout`. Sets status `resolved`,
    /// `winning_outcome_idx`, `resolved_at`. Returns the per-viewer `(viewer, payout)`
    /// list (only non-zero payouts).
    ///
    /// The proportional multiply uses `i128` to avoid `BIGINT` overflow on large
    /// pools. Each credit is the existing ledger upsert (a viewer with no prior row
    /// gets one), so a winner is always paid even if they had a 0 balance after
    /// staking.
    ///
    /// # Errors
    /// - [`ResolveError::NotFound`] / [`ResolveError::BadState`] / [`ResolveError::BadOutcome`].
    /// - [`ResolveError::Db`] on any storage error.
    pub async fn resolve(
        &self,
        prediction: PredictionId,
        winning_idx: i32,
    ) -> Result<Vec<(ParticipantId, i64)>, ResolveError> {
        let mut tx = self.pool.begin().await?;

        // Resolve + gate inside the tx.
        let row = sqlx::query_as::<_, (String,)>(
            r"SELECT status FROM predictions WHERE id = $1",
        )
        .bind(prediction.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some((status,)) = row else {
            return Err(ResolveError::NotFound);
        };
        if status != "open" && status != "locked" {
            return Err(ResolveError::BadState);
        }

        // The winning outcome must exist.
        let outcome_ok = sqlx::query_scalar::<_, i32>(
            r"SELECT idx FROM prediction_outcomes WHERE prediction_id = $1 AND idx = $2",
        )
        .bind(prediction.to_uuid())
        .bind(winning_idx)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !outcome_ok {
            return Err(ResolveError::BadOutcome);
        }

        // Load every stake (id, viewer, outcome, points). The whole settlement is
        // computed from this in-memory set so the math is auditable + overflow-safe.
        let stakes = sqlx::query_as::<_, (Uuid, Uuid, i32, i64)>(
            r"SELECT id, viewer_id, outcome_idx, points
               FROM prediction_stakes WHERE prediction_id = $1",
        )
        .bind(prediction.to_uuid())
        .fetch_all(&mut *tx)
        .await?;

        let total_pool: i128 = stakes.iter().map(|s| i128::from(s.3)).sum();
        let winning_pool: i128 = stakes
            .iter()
            .filter(|s| s.2 == winning_idx)
            .map(|s| i128::from(s.3))
            .sum();

        // No winner picked the outcome => refund every staker their own stake.
        // Otherwise each winner gets floor(stake * total_pool / winning_pool).
        let refund_all = winning_pool == 0;

        let mut payouts: Vec<(ParticipantId, i64)> = Vec::new();
        for (stake_uuid, viewer_uuid, outcome_idx, points) in &stakes {
            let payout: i64 = if refund_all {
                *points
            } else if *outcome_idx == winning_idx {
                // floor(points * total_pool / winning_pool) — i128 multiply avoids
                // BIGINT overflow; the result fits back in i64 (it is <= total_pool,
                // itself a sum of i64 stakes within a single prediction).
                let raw = i128::from(*points) * total_pool / winning_pool;
                i64::try_from(raw).unwrap_or(i64::MAX)
            } else {
                0
            };

            // Stamp the payout on the stake (always, including the 0 a loser gets).
            sqlx::query(r"UPDATE prediction_stakes SET payout = $2 WHERE id = $1")
                .bind(stake_uuid)
                .bind(payout)
                .execute(&mut *tx)
                .await?;

            if payout > 0 {
                // Credit the winner/refundee's ledger with the prediction's creator.
                credit_ledger(&mut tx, *viewer_uuid, prediction, payout, "prediction_payout")
                    .await?;
                payouts.push((ParticipantId::from_uuid(*viewer_uuid), payout));
            }
        }

        // Stamp the prediction resolved.
        sqlx::query(
            r"UPDATE predictions
                 SET status = 'resolved', winning_outcome_idx = $2, resolved_at = now()
               WHERE id = $1",
        )
        .bind(prediction.to_uuid())
        .bind(winning_idx)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(payouts)
    }

    /// Cancel a prediction, REFUNDING every staker their own stake (credit back) and
    /// setting status `cancelled`. In ONE transaction. Returns `true` if THIS call
    /// cancelled an `open`/`locked` prediction (idempotent: `false` when unknown /
    /// already resolved / already cancelled — those leave stakes untouched).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the transaction.
    pub async fn cancel(&self, prediction: PredictionId) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        // Only an open/locked prediction is cancellable; stamp it cancelled and
        // confirm THIS call did it (RETURNING). A resolved/cancelled one is a no-op.
        let cancelled = sqlx::query_scalar::<_, Uuid>(
            r"UPDATE predictions SET status = 'cancelled'
               WHERE id = $1 AND status IN ('open', 'locked')
               RETURNING id",
        )
        .bind(prediction.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !cancelled {
            return Ok(false);
        }

        // Refund every staker their own stake, stamping payout = points.
        let stakes = sqlx::query_as::<_, (Uuid, Uuid, i64)>(
            r"SELECT id, viewer_id, points FROM prediction_stakes WHERE prediction_id = $1",
        )
        .bind(prediction.to_uuid())
        .fetch_all(&mut *tx)
        .await?;
        for (stake_uuid, viewer_uuid, points) in &stakes {
            sqlx::query(r"UPDATE prediction_stakes SET payout = $2 WHERE id = $1")
                .bind(stake_uuid)
                .bind(points)
                .execute(&mut *tx)
                .await?;
            credit_ledger(&mut tx, *viewer_uuid, prediction, *points, "prediction_refund")
                .await?;
        }

        tx.commit().await?;
        Ok(true)
    }
}

/// One row in a viewer's prediction history — a join of a `prediction_stakes` row
/// with the parent `predictions` row.
///
/// `Serialize` so the handler can return it directly as JSON.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ViewerPredictionRow {
    pub prediction_id: uuid::Uuid,
    pub question: String,
    pub outcome_idx: i32,
    pub points: i64,
    pub payout: i64,
    pub status: String,
    pub created_at: OffsetDateTime,
}

/// Aggregate analytics for a creator's predictions on a stream.
///
/// `Serialize` so the handler can return it directly as JSON.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PredictionAnalytics {
    pub total_predictions: i64,
    pub resolved_predictions: i64,
    pub total_points_staked: i64,
    pub avg_participants: i64,
}

impl PredictionRepo {
    /// Paginated list of a viewer's prediction history (their stakes across all
    /// predictions), newest stake first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_viewer(
        &self,
        viewer: ParticipantId,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ViewerPredictionRow>, sqlx::Error> {
        sqlx::query_as::<_, ViewerPredictionRow>(
            r"SELECT ps.prediction_id,
                     p.question,
                     ps.outcome_idx,
                     ps.points,
                     COALESCE(ps.payout, 0) AS payout,
                     p.status,
                     ps.created_at
               FROM prediction_stakes ps
               JOIN predictions p ON p.id = ps.prediction_id
              WHERE ps.viewer_id = $1
              ORDER BY ps.created_at DESC
              LIMIT $2 OFFSET $3",
        )
        .bind(viewer.to_uuid())
        .bind(limit.max(1).min(100))
        .bind(offset.max(0))
        .fetch_all(&self.pool)
        .await
    }

    /// Aggregate analytics for all predictions on a given stream: total
    /// predictions created, how many resolved, total channel-points staked
    /// across all predictions, and average participant count per prediction.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn creator_analytics(
        &self,
        stream_id: Ulid,
    ) -> Result<PredictionAnalytics, sqlx::Error> {
        sqlx::query_as::<_, PredictionAnalytics>(
            r"SELECT
                COUNT(DISTINCT p.id)::bigint                                  AS total_predictions,
                COUNT(DISTINCT p.id) FILTER (WHERE p.status = 'resolved')::bigint
                                                                              AS resolved_predictions,
                COALESCE(SUM(ps.points), 0)::bigint                           AS total_points_staked,
                COALESCE(
                    (SUM(COUNT(ps.id)) OVER () / NULLIF(COUNT(DISTINCT p.id), 0)),
                    0
                )::bigint                                                      AS avg_participants
               FROM predictions p
               LEFT JOIN prediction_stakes ps ON ps.prediction_id = p.id
              WHERE p.stream_id = $1
              GROUP BY ()
            ",
        )
        .bind(Uuid::from_u128(stream_id.0))
        .fetch_optional(&self.pool)
        .await
        .map(|opt| {
            opt.unwrap_or(PredictionAnalytics {
                total_predictions: 0,
                resolved_predictions: 0,
                total_points_staked: 0,
                avg_participants: 0,
            })
        })
    }
}

/// Whether a [`sqlx::Error`] is a Postgres UNIQUE-constraint violation (SQLSTATE
/// `23505`) — used to map the `(prediction, viewer)` collision to
/// [`StakeError::AlreadyStaked`].
fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
}

/// Credit a viewer's channel-points balance with `prediction`'s creator by `amount`
/// (the ledger upsert), and append the `points_earn_history` audit row — inside the
/// caller's transaction. Resolves the creator from the prediction so the credit lands
/// on the same `(viewer, creator)` ledger the stake debited.
async fn credit_ledger(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    viewer_uuid: Uuid,
    prediction: PredictionId,
    amount: i64,
    reason: &str,
) -> Result<(), sqlx::Error> {
    // The creator is fixed per prediction; sub-select it so the credit always lands
    // on the right ledger pair even though `prediction_stakes` keeps only the viewer.
    let creator_uuid = sqlx::query_scalar::<_, Uuid>(
        r"SELECT creator_id FROM predictions WHERE id = $1",
    )
    .bind(prediction.to_uuid())
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query(
        r"INSERT INTO points_ledger (viewer_id, creator_id, balance)
           VALUES ($1, $2, $3)
           ON CONFLICT (viewer_id, creator_id)
           DO UPDATE SET balance = points_ledger.balance + EXCLUDED.balance",
    )
    .bind(viewer_uuid)
    .bind(creator_uuid)
    .bind(amount)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        r"INSERT INTO points_earn_history (id, viewer_id, creator_id, delta, reason)
           VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(Uuid::from_u128(ulid::Ulid::new().0))
    .bind(viewer_uuid)
    .bind(creator_uuid)
    .bind(amount)
    .bind(reason)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    /// The proportional payout math: floor(stake * total / winning), i128 to avoid
    /// overflow. Pure helper-free check of the formula used in `resolve`.
    #[test]
    fn proportional_payout_floors() {
        let total: i128 = 300; // 100 + 200 staked total
        let winning: i128 = 100; // only the single winner's pool
        let stake: i128 = 100;
        let payout = stake * total / winning;
        assert_eq!(payout, 300); // sole winner takes the whole pool

        // Two winners splitting against a losing pool: total 300, winning 100+50=150.
        let winning2: i128 = 150;
        let a = 100i128 * 300 / winning2; // 200
        let b = 50i128 * 300 / winning2; // 100
        assert_eq!(a, 200);
        assert_eq!(b, 100);
        assert_eq!(a + b, 300, "winners split the full pool (exact here)");

        // Flooring: total 301, winning 150, stake 100 -> floor(20066/150)=200.
        let floored = 100i128 * 301 / 150;
        assert_eq!(floored, 200);
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored predictions_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::ChannelPointsRepo;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn participant(p: &PgPool, label: &str) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(id.to_uuid())
            .bind(format!("pred-{label}-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn predictions_stake_debits_and_rejects_dupes() {
        let p = pool();
        let cp = ChannelPointsRepo::new(p.clone());
        let repo = PredictionRepo::new(p.clone());
        let stream = Ulid::new();
        let creator = participant(&p, "creator").await;
        let viewer = participant(&p, "viewer").await;
        let poor = participant(&p, "poor").await;

        // Fund the viewer with the creator.
        cp.earn(viewer, creator, 100, "watch").await.unwrap();

        let pred = repo
            .create_prediction(
                stream,
                creator,
                "Will they win?",
                &["yes".into(), "no".into()],
                None,
            )
            .await
            .unwrap();
        assert_eq!(pred.outcomes.len(), 2);
        assert_eq!(pred.status, "open");

        // Stake 40 on outcome 0 -> balance drops to 60.
        repo.stake(pred.id, viewer, 0, 40).await.unwrap();
        assert_eq!(cp.balance(viewer, creator).await.unwrap(), 60);

        // A second stake (any outcome) is rejected — one stake per viewer.
        let err = repo.stake(pred.id, viewer, 1, 10).await.unwrap_err();
        assert!(matches!(err, StakeError::AlreadyStaked));
        assert_eq!(cp.balance(viewer, creator).await.unwrap(), 60, "no extra debit");

        // A staker with no points cannot stake.
        let err = repo.stake(pred.id, poor, 0, 5).await.unwrap_err();
        assert!(matches!(err, StakeError::InsufficientPoints));

        // A bad outcome index is rejected.
        let other = participant(&p, "other").await;
        cp.earn(other, creator, 10, "watch").await.unwrap();
        let err = repo.stake(pred.id, other, 9, 5).await.unwrap_err();
        assert!(matches!(err, StakeError::BadOutcome));
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn predictions_resolve_pays_winners_proportionally() {
        let p = pool();
        let cp = ChannelPointsRepo::new(p.clone());
        let repo = PredictionRepo::new(p.clone());
        let stream = Ulid::new();
        let creator = participant(&p, "creator").await;
        let alice = participant(&p, "alice").await; // bets on the WINNER (0)
        let bob = participant(&p, "bob").await; // bets on the LOSER (1)

        cp.earn(alice, creator, 100, "watch").await.unwrap();
        cp.earn(bob, creator, 200, "watch").await.unwrap();

        let pred = repo
            .create_prediction(stream, creator, "Pick one", &["A".into(), "B".into()], None)
            .await
            .unwrap();

        // Alice stakes 100 on A (idx 0); Bob stakes 200 on B (idx 1).
        repo.stake(pred.id, alice, 0, 100).await.unwrap();
        repo.stake(pred.id, bob, 1, 200).await.unwrap();
        assert_eq!(cp.balance(alice, creator).await.unwrap(), 0);
        assert_eq!(cp.balance(bob, creator).await.unwrap(), 0);

        // Resolve to A: total_pool = 300, winning_pool = 100 (just Alice).
        // Alice's payout = floor(100 * 300 / 100) = 300; Bob gets 0.
        let payouts = repo.resolve(pred.id, 0).await.unwrap();
        assert_eq!(payouts.len(), 1);
        assert_eq!(payouts[0], (alice, 300));
        assert_eq!(cp.balance(alice, creator).await.unwrap(), 300, "winner paid the full pool");
        assert_eq!(cp.balance(bob, creator).await.unwrap(), 0, "loser paid nothing");

        let got = repo.get(pred.id).await.unwrap().unwrap();
        assert_eq!(got.status, "resolved");
        assert_eq!(got.winning_outcome_idx, Some(0));
        assert!(got.resolved_at.is_some());

        // Re-resolving is rejected (bad state).
        let err = repo.resolve(pred.id, 0).await.unwrap_err();
        assert!(matches!(err, ResolveError::BadState));
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn predictions_resolve_no_winner_refunds_all() {
        let p = pool();
        let cp = ChannelPointsRepo::new(p.clone());
        let repo = PredictionRepo::new(p.clone());
        let stream = Ulid::new();
        let creator = participant(&p, "creator").await;
        let alice = participant(&p, "alice").await;
        let bob = participant(&p, "bob").await;

        cp.earn(alice, creator, 50, "watch").await.unwrap();
        cp.earn(bob, creator, 70, "watch").await.unwrap();

        let pred = repo
            .create_prediction(
                stream,
                creator,
                "Three-way",
                &["A".into(), "B".into(), "C".into()],
                None,
            )
            .await
            .unwrap();

        // Both stake on A (0) and B (1); resolve to C (2) — nobody picked the winner.
        repo.stake(pred.id, alice, 0, 50).await.unwrap();
        repo.stake(pred.id, bob, 1, 70).await.unwrap();

        let payouts = repo.resolve(pred.id, 2).await.unwrap();
        // Everyone refunded their own stake.
        assert_eq!(payouts.len(), 2);
        assert_eq!(cp.balance(alice, creator).await.unwrap(), 50);
        assert_eq!(cp.balance(bob, creator).await.unwrap(), 70);
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn predictions_cancel_refunds_all() {
        let p = pool();
        let cp = ChannelPointsRepo::new(p.clone());
        let repo = PredictionRepo::new(p.clone());
        let stream = Ulid::new();
        let creator = participant(&p, "creator").await;
        let alice = participant(&p, "alice").await;

        cp.earn(alice, creator, 80, "watch").await.unwrap();
        let pred = repo
            .create_prediction(stream, creator, "Cancelable", &["A".into(), "B".into()], None)
            .await
            .unwrap();
        repo.stake(pred.id, alice, 0, 80).await.unwrap();
        assert_eq!(cp.balance(alice, creator).await.unwrap(), 0);

        // Cancel refunds the stake; a second cancel is a no-op.
        assert!(repo.cancel(pred.id).await.unwrap());
        assert!(!repo.cancel(pred.id).await.unwrap());
        assert_eq!(cp.balance(alice, creator).await.unwrap(), 80, "stake refunded");
        assert_eq!(repo.get(pred.id).await.unwrap().unwrap().status, "cancelled");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn predictions_cannot_stake_after_lock_or_resolve() {
        let p = pool();
        let cp = ChannelPointsRepo::new(p.clone());
        let repo = PredictionRepo::new(p.clone());
        let stream = Ulid::new();
        let creator = participant(&p, "creator").await;
        let alice = participant(&p, "alice").await;
        let bob = participant(&p, "bob").await;

        cp.earn(alice, creator, 100, "watch").await.unwrap();
        cp.earn(bob, creator, 100, "watch").await.unwrap();

        let pred = repo
            .create_prediction(stream, creator, "Locked soon", &["A".into(), "B".into()], None)
            .await
            .unwrap();
        repo.stake(pred.id, alice, 0, 10).await.unwrap();

        // Lock -> the prediction is no longer open; staking is rejected.
        assert!(repo.lock(pred.id).await.unwrap());
        assert!(!repo.lock(pred.id).await.unwrap(), "second lock is a no-op");
        let err = repo.stake(pred.id, bob, 0, 10).await.unwrap_err();
        assert!(matches!(err, StakeError::BadState));

        // Resolve a LOCKED prediction (open/locked are both resolvable).
        repo.resolve(pred.id, 0).await.unwrap();

        // Staking on a resolved prediction is likewise rejected.
        let err = repo.stake(pred.id, bob, 0, 10).await.unwrap_err();
        assert!(matches!(err, StakeError::BadState));
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn predictions_create_requires_two_outcomes() {
        let p = pool();
        let repo = PredictionRepo::new(p.clone());
        let creator = participant(&p, "creator").await;
        let err = repo
            .create_prediction(Ulid::new(), creator, "Only one", &["solo".into()], None)
            .await;
        assert!(err.is_err(), "fewer than 2 outcomes is rejected");
    }
}
