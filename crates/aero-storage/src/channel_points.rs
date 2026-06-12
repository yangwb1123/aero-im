//! Channel-points + custom-reward redemption repository.
//!
//! Backs `migrations/0089_channel_points.sql`. Twitch-style "Channel Points": a
//! viewer accrues points with a creator, then spends them to redeem a creator-defined
//! custom reward. Four tables:
//!
//!   * `points_ledger` — the running balance per `(viewer, creator)` pair.
//!   * `points_earn_history` — an append-only +/- audit with a reason.
//!   * `reward_definitions` — a creator's redeemable-reward catalog.
//!   * `redemption_queue` — one row per viewer redemption, `pending` until resolved
//!     to `fulfilled` / `rejected`.
//!
//! The CRITICAL invariant is double-spend safety: [`ChannelPointsRepo::redeem`]
//! debits the balance with a conditional `UPDATE ... WHERE balance >= cost RETURNING`
//! and inserts the queue row in the SAME transaction, so two concurrent redemptions
//! against one balance cannot both succeed — the second sees no row and errors with
//! [`RedeemError::InsufficientPoints`].
//!
//! Purely additive: a NEW [`ChannelPointsRepo`]; no existing repo is touched.
//! `viewer_id`/`creator_id` are plain UUID columns (not cascading FKs), mirroring
//! [`StreamModRepo`](crate::StreamModRepo)/[`RaidRepo`](crate::RaidRepo).

use aero_common::{ParticipantId, RedemptionId, RewardId};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// Why a redemption was rejected. Kept distinct from [`sqlx::Error`] so the server
/// maps `InsufficientPoints` to a clean 409 instead of a generic 500.
#[derive(Debug, thiserror::Error)]
pub enum RedeemError {
    /// The viewer's balance with the creator is below the reward's cost (or the
    /// reward is disabled / unknown). The atomic debit removed nothing.
    #[error("insufficient points")]
    InsufficientPoints,
    /// The reward does not exist (or is disabled).
    #[error("reward not found or disabled")]
    RewardUnavailable,
    /// A storage error occurred.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// A creator's custom-reward definition — a `reward_definitions` row.
#[derive(Debug, Clone, Serialize)]
pub struct Reward {
    pub id: RewardId,
    pub creator_id: ParticipantId,
    pub title: String,
    pub cost: i64,
    pub auto_fulfill: bool,
    pub enabled: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

const REWARD_COLUMNS: &str = "id, creator_id, title, cost, auto_fulfill, enabled, created_at";

type RewardRow = (Uuid, Uuid, String, i64, bool, bool, time::OffsetDateTime);

fn reward_row_to_model(r: RewardRow) -> Reward {
    let (id, creator_id, title, cost, auto_fulfill, enabled, created_at) = r;
    Reward {
        id: RewardId::from_uuid(id),
        creator_id: ParticipantId::from_uuid(creator_id),
        title,
        cost,
        auto_fulfill,
        enabled,
        created_at,
    }
}

/// One redemption-queue entry — a `redemption_queue` row.
#[derive(Debug, Clone, Serialize)]
pub struct Redemption {
    pub id: RedemptionId,
    pub reward_id: RewardId,
    pub viewer_id: ParticipantId,
    /// `pending` / `fulfilled` / `rejected`.
    pub status: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub resolved_at: Option<time::OffsetDateTime>,
}

const REDEMPTION_COLUMNS: &str = "id, reward_id, viewer_id, status, created_at, resolved_at";

type RedemptionRow = (Uuid, Uuid, Uuid, String, time::OffsetDateTime, Option<time::OffsetDateTime>);

fn redemption_row_to_model(r: RedemptionRow) -> Redemption {
    let (id, reward_id, viewer_id, status, created_at, resolved_at) = r;
    Redemption {
        id: RedemptionId::from_uuid(id),
        reward_id: RewardId::from_uuid(reward_id),
        viewer_id: ParticipantId::from_uuid(viewer_id),
        status,
        created_at,
        resolved_at,
    }
}

/// Whether `status` is a valid redemption resolution (`fulfilled` / `rejected`).
/// Pure, so the rule is unit-tested without a database.
#[must_use]
pub fn is_resolution_status(status: &str) -> bool {
    matches!(status, "fulfilled" | "rejected")
}

/// Repository over the channel-points tables.
///
/// Cheap to clone — wraps a [`PgPool`]; feature modules build one inline via
/// [`ChannelPointsRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct ChannelPointsRepo {
    pool: PgPool,
}

impl ChannelPointsRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Credit (or debit, with a negative `delta`) a viewer's balance with a creator,
    /// recording the move in `points_earn_history`. Upserts the ledger row, then
    /// appends the audit row, in one transaction. Returns the new balance.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the transaction.
    pub async fn earn(
        &self,
        viewer: ParticipantId,
        creator: ParticipantId,
        delta: i64,
        reason: &str,
    ) -> Result<i64, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let new_balance = sqlx::query_scalar::<_, i64>(
            r"INSERT INTO points_ledger (viewer_id, creator_id, balance)
               VALUES ($1, $2, $3)
               ON CONFLICT (viewer_id, creator_id)
               DO UPDATE SET balance = points_ledger.balance + EXCLUDED.balance
               RETURNING balance",
        )
        .bind(viewer.to_uuid())
        .bind(creator.to_uuid())
        .bind(delta)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            r"INSERT INTO points_earn_history (id, viewer_id, creator_id, delta, reason)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(Uuid::from_u128(ulid::Ulid::new().0))
        .bind(viewer.to_uuid())
        .bind(creator.to_uuid())
        .bind(delta)
        .bind(reason)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(new_balance)
    }

    /// The viewer's current balance with a creator (`0` if no ledger row yet).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn balance(
        &self,
        viewer: ParticipantId,
        creator: ParticipantId,
    ) -> Result<i64, sqlx::Error> {
        let bal = sqlx::query_scalar::<_, Option<i64>>(
            r"SELECT balance FROM points_ledger WHERE viewer_id = $1 AND creator_id = $2",
        )
        .bind(viewer.to_uuid())
        .bind(creator.to_uuid())
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        Ok(bal.unwrap_or(0))
    }

    /// Define a new custom reward for a creator, returning its generated id.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn create_reward(
        &self,
        creator: ParticipantId,
        title: &str,
        cost: i64,
        auto_fulfill: bool,
    ) -> Result<RewardId, sqlx::Error> {
        let id = RewardId::new();
        sqlx::query(
            r"INSERT INTO reward_definitions (id, creator_id, title, cost, auto_fulfill)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id.to_uuid())
        .bind(creator.to_uuid())
        .bind(title)
        .bind(cost.max(0))
        .bind(auto_fulfill)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// A creator's reward catalog (enabled + disabled), newest first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_rewards(&self, creator: ParticipantId) -> Result<Vec<Reward>, sqlx::Error> {
        let sql = format!(
            "SELECT {REWARD_COLUMNS}
               FROM reward_definitions
              WHERE creator_id = $1
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, RewardRow>(&sql)
            .bind(creator.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(reward_row_to_model).collect())
    }

    /// Fetch one reward by id, or `None`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get_reward(&self, reward: RewardId) -> Result<Option<Reward>, sqlx::Error> {
        let sql = format!("SELECT {REWARD_COLUMNS} FROM reward_definitions WHERE id = $1");
        let row = sqlx::query_as::<_, RewardRow>(&sql)
            .bind(reward.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(reward_row_to_model))
    }

    /// Redeem a reward for a viewer: ATOMICALLY debit the viewer's balance with the
    /// reward's creator by the reward's cost (only if the balance covers it) and
    /// enqueue a `pending` redemption row, in one transaction. Returns the new
    /// `(redemption_id, reward, new_balance)`.
    ///
    /// Double-spend safety: the debit is a conditional
    /// `UPDATE ... WHERE balance >= cost RETURNING balance`. If it removes no row
    /// (balance too low or no ledger row), the transaction rolls back and the call
    /// errors with [`RedeemError::InsufficientPoints`] — two concurrent redemptions
    /// against one balance can never both succeed.
    ///
    /// # Errors
    /// - [`RedeemError::RewardUnavailable`] if the reward is unknown or disabled.
    /// - [`RedeemError::InsufficientPoints`] if the balance does not cover the cost.
    /// - [`RedeemError::Db`] on any storage error.
    pub async fn redeem(
        &self,
        reward_id: RewardId,
        viewer: ParticipantId,
    ) -> Result<(RedemptionId, Reward, i64), RedeemError> {
        let mut tx = self.pool.begin().await?;

        // Resolve the reward (enabled only) inside the tx so the cost/creator are
        // consistent with the debit.
        let sql = format!(
            "SELECT {REWARD_COLUMNS} FROM reward_definitions WHERE id = $1 AND enabled = true"
        );
        let reward = sqlx::query_as::<_, RewardRow>(&sql)
            .bind(reward_id.to_uuid())
            .fetch_optional(&mut *tx)
            .await?
            .map(reward_row_to_model)
            .ok_or(RedeemError::RewardUnavailable)?;

        // Conditional debit — only succeeds if the balance covers the cost. A free
        // reward (cost 0) still requires a ledger row to exist (>= 0 always true once
        // a row is present); we therefore treat a missing row as insufficient.
        let new_balance = sqlx::query_scalar::<_, i64>(
            r"UPDATE points_ledger
                 SET balance = balance - $3
               WHERE viewer_id = $1 AND creator_id = $2 AND balance >= $3
               RETURNING balance",
        )
        .bind(viewer.to_uuid())
        .bind(reward.creator_id.to_uuid())
        .bind(reward.cost)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(RedeemError::InsufficientPoints)?;

        // Audit the spend.
        sqlx::query(
            r"INSERT INTO points_earn_history (id, viewer_id, creator_id, delta, reason)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(Uuid::from_u128(ulid::Ulid::new().0))
        .bind(viewer.to_uuid())
        .bind(reward.creator_id.to_uuid())
        .bind(-reward.cost)
        .bind(format!("redeem:{}", reward.title))
        .execute(&mut *tx)
        .await?;

        // Enqueue the pending redemption (auto-fulfill resolves immediately).
        let redemption_id = RedemptionId::new();
        let status = if reward.auto_fulfill { "fulfilled" } else { "pending" };
        sqlx::query(
            r"INSERT INTO redemption_queue (id, reward_id, viewer_id, status, resolved_at)
               VALUES ($1, $2, $3, $4, CASE WHEN $4 = 'pending' THEN NULL ELSE now() END)",
        )
        .bind(redemption_id.to_uuid())
        .bind(reward_id.to_uuid())
        .bind(viewer.to_uuid())
        .bind(status)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok((redemption_id, reward, new_balance))
    }

    /// Resolve a pending redemption to `fulfilled` / `rejected`. Returns `true` if a
    /// pending row was resolved (idempotent: `false` when already resolved / unknown
    /// / `status` invalid). NOTE: rejection does NOT refund (a creator policy choice);
    /// a refund, if desired, is a separate `earn(..)` credit by the caller.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn resolve_redemption(
        &self,
        redemption: RedemptionId,
        status: &str,
    ) -> Result<bool, sqlx::Error> {
        if !is_resolution_status(status) {
            return Ok(false);
        }
        let res = sqlx::query(
            r"UPDATE redemption_queue
                 SET status = $2, resolved_at = now()
               WHERE id = $1 AND status = 'pending'",
        )
        .bind(redemption.to_uuid())
        .bind(status)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Fetch one redemption by id, or `None`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get_redemption(
        &self,
        redemption: RedemptionId,
    ) -> Result<Option<Redemption>, sqlx::Error> {
        let sql = format!("SELECT {REDEMPTION_COLUMNS} FROM redemption_queue WHERE id = $1");
        let row = sqlx::query_as::<_, RedemptionRow>(&sql)
            .bind(redemption.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(redemption_row_to_model))
    }

    /// The redemption queue for a creator's rewards, optionally filtered by `status`.
    /// Oldest first (FIFO fulfillment). Joins `redemption_queue` to the creator's
    /// `reward_definitions` so a creator sees redemptions across all their rewards.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_queue(
        &self,
        creator: ParticipantId,
        status: Option<&str>,
    ) -> Result<Vec<Redemption>, sqlx::Error> {
        let base = "SELECT rq.id, rq.reward_id, rq.viewer_id, rq.status, rq.created_at, \
                    rq.resolved_at
               FROM redemption_queue rq
               JOIN reward_definitions rd ON rd.id = rq.reward_id
              WHERE rd.creator_id = $1";
        let order = " ORDER BY rq.created_at ASC, rq.id ASC";
        let filter = if status.is_some() { " AND rq.status = $2" } else { "" };
        let sql = [base, filter, order].concat();

        let mut q = sqlx::query_as::<_, RedemptionRow>(&sql).bind(creator.to_uuid());
        if let Some(s) = status {
            q = q.bind(s);
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(redemption_row_to_model).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_status_rule() {
        assert!(is_resolution_status("fulfilled"));
        assert!(is_resolution_status("rejected"));
        assert!(!is_resolution_status("pending"));
        assert!(!is_resolution_status("approved"));
        assert!(!is_resolution_status(""));
    }
}

/// One earn/spend history row — a `points_earn_history` row projection.
///
/// `Serialize` so the handler can return it directly as JSON.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct EarnHistoryRow {
    pub id: uuid::Uuid,
    pub delta: i64,
    pub reason: String,
    pub created_at: time::OffsetDateTime,
}

impl ChannelPointsRepo {
    /// A viewer's earn/spend timeline with a creator, newest first, paginated.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_earn_history(
        &self,
        viewer: ParticipantId,
        creator: ParticipantId,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<EarnHistoryRow>, sqlx::Error> {
        sqlx::query_as::<_, EarnHistoryRow>(
            r"SELECT id, delta, reason, created_at
               FROM points_earn_history
              WHERE viewer_id = $1 AND creator_id = $2
              ORDER BY created_at DESC
              LIMIT $3 OFFSET $4",
        )
        .bind(viewer.to_uuid())
        .bind(creator.to_uuid())
        .bind(limit.max(1).min(100))
        .bind(offset.max(0))
        .fetch_all(&self.pool)
        .await
    }
}

// ----- Expiry methods (migration 0113) -----
impl ChannelPointsRepo {
    /// Zero out any viewer/creator ledger balances whose `earn_expires_at` has
    /// passed. Returns the number of rows zeroed. Balances are set to `0`
    /// (not deleted) so the row history is preserved. Called by the retention
    /// sweep loop in `bin/aero-server.rs`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn sweep_expired_points(&self) -> Result<u64, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE points_ledger \
             SET balance = 0 \
             WHERE earn_expires_at IS NOT NULL \
             AND earn_expires_at < NOW() \
             AND balance > 0",
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Set (or clear) the expiry for a viewer's balance with a creator. Pass
    /// `days <= 0` to clear (set to `NULL`). The sweep task zeros the balance
    /// when the timestamp passes.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn set_expiry(
        &self,
        viewer: ParticipantId,
        creator: ParticipantId,
        days: i32,
    ) -> Result<(), sqlx::Error> {
        if days <= 0 {
            sqlx::query(
                "UPDATE points_ledger SET earn_expires_at = NULL \
                 WHERE viewer_id = $1 AND creator_id = $2",
            )
            .bind(viewer.to_uuid())
            .bind(creator.to_uuid())
            .execute(&self.pool)
            .await?;
        } else {
            // $1 = days string (for interval concat), $2 = viewer_id, $3 = creator_id.
            sqlx::query(
                "UPDATE points_ledger \
                 SET earn_expires_at = NOW() + ($1 || ' days')::interval \
                 WHERE viewer_id = $2 AND creator_id = $3",
            )
            .bind(days.to_string())
            .bind(viewer.to_uuid())
            .bind(creator.to_uuid())
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// The `earn_expires_at` timestamp for a viewer's balance with a creator.
    /// Returns `None` when the balance never expires or no ledger row exists.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get_expiry(
        &self,
        viewer: ParticipantId,
        creator: ParticipantId,
    ) -> Result<Option<time::OffsetDateTime>, sqlx::Error> {
        let row: Option<(Option<time::OffsetDateTime>,)> = sqlx::query_as(
            "SELECT earn_expires_at FROM points_ledger \
             WHERE viewer_id = $1 AND creator_id = $2",
        )
        .bind(viewer.to_uuid())
        .bind(creator.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(ts,)| ts))
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored channel_points_
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

    async fn participant(p: &PgPool, label: &str) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(id.to_uuid())
            .bind(format!("cp-{label}-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn channel_points_ledger_isolated_per_pair() {
        let p = pool();
        let repo = ChannelPointsRepo::new(p.clone());
        let viewer = participant(&p, "viewer").await;
        let creator_a = participant(&p, "creatorA").await;
        let creator_b = participant(&p, "creatorB").await;

        repo.earn(viewer, creator_a, 100, "watch").await.unwrap();
        repo.earn(viewer, creator_b, 30, "watch").await.unwrap();

        // Each (viewer, creator) pair is an independent balance.
        assert_eq!(repo.balance(viewer, creator_a).await.unwrap(), 100);
        assert_eq!(repo.balance(viewer, creator_b).await.unwrap(), 30);
        // No ledger row yet => 0, not an error.
        let stranger = participant(&p, "stranger").await;
        assert_eq!(repo.balance(stranger, creator_a).await.unwrap(), 0);

        // Accumulation.
        repo.earn(viewer, creator_a, 50, "gift").await.unwrap();
        assert_eq!(repo.balance(viewer, creator_a).await.unwrap(), 150);
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn channel_points_double_spend_prevented() {
        let p = pool();
        let repo = ChannelPointsRepo::new(p.clone());
        let viewer = participant(&p, "viewer").await;
        let creator = participant(&p, "creator").await;

        // Fund the viewer for exactly ONE redemption of a 100-cost reward.
        repo.earn(viewer, creator, 100, "watch").await.unwrap();
        let reward = repo
            .create_reward(creator, "Highlight my message", 100, false)
            .await
            .unwrap();

        // First redeem succeeds, draining the balance to 0.
        let (rid, got_reward, new_bal) = repo.redeem(reward, viewer).await.unwrap();
        assert_eq!(got_reward.id, reward);
        assert_eq!(new_bal, 0);
        assert_eq!(repo.balance(viewer, creator).await.unwrap(), 0);

        // The pending redemption is queued for the creator.
        let queue = repo.list_queue(creator, Some("pending")).await.unwrap();
        assert!(queue.iter().any(|r| r.id == rid && r.status == "pending"));

        // Second redeem fails — insufficient points, no second queue row.
        let err = repo.redeem(reward, viewer).await.unwrap_err();
        assert!(matches!(err, RedeemError::InsufficientPoints));
        assert_eq!(repo.balance(viewer, creator).await.unwrap(), 0);
        assert_eq!(repo.list_queue(creator, None).await.unwrap().len(), 1);

        // Resolve the one redemption; resolving again is a no-op.
        assert!(repo.resolve_redemption(rid, "fulfilled").await.unwrap());
        assert!(!repo.resolve_redemption(rid, "fulfilled").await.unwrap());
        let got = repo.get_redemption(rid).await.unwrap().unwrap();
        assert_eq!(got.status, "fulfilled");
        assert!(got.resolved_at.is_some());
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn channel_points_auto_fulfill_skips_queue_pending() {
        let p = pool();
        let repo = ChannelPointsRepo::new(p.clone());
        let viewer = participant(&p, "viewer").await;
        let creator = participant(&p, "creator").await;

        repo.earn(viewer, creator, 50, "watch").await.unwrap();
        let reward = repo
            .create_reward(creator, "Auto reward", 10, true)
            .await
            .unwrap();
        let (rid, _r, bal) = repo.redeem(reward, viewer).await.unwrap();
        assert_eq!(bal, 40);
        let got = repo.get_redemption(rid).await.unwrap().unwrap();
        assert_eq!(got.status, "fulfilled", "auto_fulfill resolves immediately");

        // Reward listing shows it.
        let rewards = repo.list_rewards(creator).await.unwrap();
        assert!(rewards.iter().any(|r| r.id == reward));
    }
}
