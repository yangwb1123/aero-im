//! Creator subscriptions / membership tiers (recurring creator support).
//!
//! Backs `migrations/0051_creator_subscriptions.sql`. A creator (a participant)
//! defines named [`CreatorTier`]s (name + monthly price in cents + optional
//! perks). A viewer then subscribes at one tier; there is at most one
//! [`CreatorSubscription`] per (creator, subscriber) pair, so a re-subscribe
//! *upserts* the tier and re-activates the row rather than stacking. Unsubscribe
//! keeps the row but flags it inactive. This is the recurring complement to the
//! existing one-off live gifts.
//!
//! Tier-mutating methods are creator-scoped (`creator_id` in the `WHERE`), so a
//! caller can only edit their own tiers; subscription reads are scoped to the
//! `subscriber_id` / `creator_id` the handler supplies. Purely additive: a NEW
//! [`SubscriptionRepo`]; no existing repo is touched. Both models live here (and
//! are re-exported from the crate root) rather than in `aero-common`, since they
//! are storage-layer projections.

use aero_common::{CreatorTierId, ParticipantId, SubscriptionId};
use serde::Serialize;
use sqlx::PgPool;

/// One creator membership tier — a named price point a viewer can subscribe at.
///
/// A storage-layer projection of a `creator_tiers` row. `Serialize` so a handler
/// can hand the row straight back as JSON; `created_at` renders as RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct CreatorTier {
    /// The tier's unique id.
    pub id: CreatorTierId,
    /// The creator (a participant) the tier belongs to.
    pub creator_id: ParticipantId,
    /// Human-readable tier name (e.g. "Tier 1", "Gold").
    pub name: String,
    /// Monthly price in cents.
    pub price_cents: i32,
    /// Optional free-text description of the tier's perks.
    pub perks: Option<String>,
    /// When the tier was created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// One creator subscription — a viewer's recurring membership to a creator.
///
/// A storage-layer projection of a `creator_subscriptions` row. At most one row
/// exists per (creator, subscriber); `active` is `false` after an unsubscribe.
#[derive(Debug, Clone, Serialize)]
pub struct CreatorSubscription {
    /// The subscription's unique id.
    pub id: SubscriptionId,
    /// The creator (a participant) being subscribed to.
    pub creator_id: ParticipantId,
    /// The subscriber (a participant) doing the subscribing.
    pub subscriber_id: ParticipantId,
    /// The tier the subscriber is currently subscribed at.
    pub tier_id: CreatorTierId,
    /// Whether the subscription is currently active (`false` after unsubscribe).
    pub active: bool,
    /// When the subscription was first created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// The columns a [`CreatorTier`] is built from, in select order.
const TIER_COLUMNS: &str = "id, creator_id, name, price_cents, perks, created_at";

type TierRow = (
    uuid::Uuid,
    uuid::Uuid,
    String,
    i32,
    Option<String>,
    time::OffsetDateTime,
);

fn tier_to_model(r: TierRow) -> CreatorTier {
    let (id, creator_id, name, price_cents, perks, created_at) = r;
    CreatorTier {
        id: CreatorTierId::from_uuid(id),
        creator_id: ParticipantId::from_uuid(creator_id),
        name,
        price_cents,
        perks,
        created_at,
    }
}

/// The columns a [`CreatorSubscription`] is built from, in select order.
const SUB_COLUMNS: &str = "id, creator_id, subscriber_id, tier_id, active, created_at";

type SubRow = (
    uuid::Uuid,
    uuid::Uuid,
    uuid::Uuid,
    uuid::Uuid,
    bool,
    time::OffsetDateTime,
);

fn sub_to_model(r: SubRow) -> CreatorSubscription {
    let (id, creator_id, subscriber_id, tier_id, active, created_at) = r;
    CreatorSubscription {
        id: SubscriptionId::from_uuid(id),
        creator_id: ParticipantId::from_uuid(creator_id),
        subscriber_id: ParticipantId::from_uuid(subscriber_id),
        tier_id: CreatorTierId::from_uuid(tier_id),
        active,
        created_at,
    }
}

/// Repository over the `creator_tiers` + `creator_subscriptions` tables.
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`SubscriptionRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct SubscriptionRepo {
    pool: PgPool,
}

impl SubscriptionRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Define a new membership tier for `creator`, returning its generated id. The
    /// caller is responsible for verifying the creator identity and validating the
    /// name/price.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn create_tier(
        &self,
        creator: ParticipantId,
        name: &str,
        price_cents: i32,
        perks: Option<&str>,
    ) -> Result<CreatorTierId, sqlx::Error> {
        let id = CreatorTierId::new();
        sqlx::query(
            r"INSERT INTO creator_tiers (id, creator_id, name, price_cents, perks)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id.to_uuid())
        .bind(creator.to_uuid())
        .bind(name)
        .bind(price_cents)
        .bind(perks)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List `creator`'s tiers, cheapest first (then newest). Any authed caller may
    /// read a creator's tiers.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_tiers(
        &self,
        creator: ParticipantId,
    ) -> Result<Vec<CreatorTier>, sqlx::Error> {
        let sql = format!(
            "SELECT {TIER_COLUMNS}
               FROM creator_tiers
              WHERE creator_id = $1
              ORDER BY price_cents ASC, created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, TierRow>(&sql)
            .bind(creator.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(tier_to_model).collect())
    }

    /// Fetch one tier by id, or `None` if no such row exists. Not creator-scoped —
    /// the handler uses this to validate that a tier belongs to the target creator
    /// before accepting a subscribe.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get_tier(&self, id: CreatorTierId) -> Result<Option<CreatorTier>, sqlx::Error> {
        let sql = format!("SELECT {TIER_COLUMNS} FROM creator_tiers WHERE id = $1");
        let row = sqlx::query_as::<_, TierRow>(&sql)
            .bind(id.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(tier_to_model))
    }

    /// Delete one of `creator`'s tiers. Returns `true` iff a row was removed —
    /// creator-scoped, so a caller can never delete another creator's tier, and a
    /// second delete (or a stranger's) is a no-op returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn delete_tier(
        &self,
        id: CreatorTierId,
        creator: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM creator_tiers WHERE id = $1 AND creator_id = $2")
            .bind(id.to_uuid())
            .bind(creator.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Subscribe `subscriber` to `creator` at `tier`, returning the subscription's
    /// id. There is at most one row per (creator, subscriber): a re-subscribe
    /// upserts the tier and re-activates the row (so the returned id may be that of
    /// a pre-existing row). The caller is responsible for verifying the tier
    /// belongs to the creator and rejecting a self-subscribe.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert.
    pub async fn subscribe(
        &self,
        creator: ParticipantId,
        subscriber: ParticipantId,
        tier: CreatorTierId,
    ) -> Result<SubscriptionId, sqlx::Error> {
        let id = SubscriptionId::new();
        // ON CONFLICT (creator_id, subscriber_id): re-point at the new tier and
        // re-activate; `RETURNING id` yields the surviving row's id (existing on
        // conflict, freshly-minted otherwise).
        let row: (uuid::Uuid,) = sqlx::query_as(
            r"INSERT INTO creator_subscriptions (id, creator_id, subscriber_id, tier_id, active)
               VALUES ($1, $2, $3, $4, true)
               ON CONFLICT (creator_id, subscriber_id)
               DO UPDATE SET tier_id = EXCLUDED.tier_id, active = true
               RETURNING id",
        )
        .bind(id.to_uuid())
        .bind(creator.to_uuid())
        .bind(subscriber.to_uuid())
        .bind(tier.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(SubscriptionId::from_uuid(row.0))
    }

    /// Unsubscribe `subscriber` from `creator` by flagging the row inactive.
    /// Returns `true` iff an active row was found and deactivated; a second
    /// unsubscribe (or one with no matching row) is a no-op returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn unsubscribe(
        &self,
        creator: ParticipantId,
        subscriber: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE creator_subscriptions
                 SET active = false
               WHERE creator_id = $1 AND subscriber_id = $2 AND active = true",
        )
        .bind(creator.to_uuid())
        .bind(subscriber.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// List `subscriber`'s active subscriptions, newest first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn subscriptions_of(
        &self,
        subscriber: ParticipantId,
    ) -> Result<Vec<CreatorSubscription>, sqlx::Error> {
        let sql = format!(
            "SELECT {SUB_COLUMNS}
               FROM creator_subscriptions
              WHERE subscriber_id = $1 AND active = true
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, SubRow>(&sql)
            .bind(subscriber.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(sub_to_model).collect())
    }

    /// List `creator`'s active subscribers, newest first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn subscribers_of(
        &self,
        creator: ParticipantId,
    ) -> Result<Vec<CreatorSubscription>, sqlx::Error> {
        let sql = format!(
            "SELECT {SUB_COLUMNS}
               FROM creator_subscriptions
              WHERE creator_id = $1 AND active = true
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, SubRow>(&sql)
            .bind(creator.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(sub_to_model).collect())
    }

    /// Gift a subscription from `gifter` to `recipient` on `creator`'s channel
    /// at `tier`, valid for `duration_days` days. There is at most one row per
    /// (creator, subscriber): a gift upserts the gifter + expiry and
    /// re-activates the row. The caller is responsible for verifying the tier
    /// belongs to the creator and rejecting a self-gift.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert.
    pub async fn gift_subscription(
        &self,
        creator: ParticipantId,
        tier_id: uuid::Uuid,
        recipient: ParticipantId,
        gifter: ParticipantId,
        duration_days: i32,
    ) -> Result<(), sqlx::Error> {
        let id = SubscriptionId::new();
        sqlx::query(
            r"INSERT INTO creator_subscriptions
                   (id, creator_id, tier_id, subscriber_id, active, gifter_id, gift_expires_at)
               VALUES ($1, $2, $3, $4, true, $5, now() + ($6 || ' days')::interval)
               ON CONFLICT (creator_id, subscriber_id)
               DO UPDATE SET
                   active = true,
                   tier_id = EXCLUDED.tier_id,
                   gifter_id = EXCLUDED.gifter_id,
                   gift_expires_at = EXCLUDED.gift_expires_at",
        )
        .bind(id.to_uuid())
        .bind(creator.to_uuid())
        .bind(tier_id)
        .bind(recipient.to_uuid())
        .bind(gifter.to_uuid())
        .bind(duration_days.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Top gifters for a creator's channel — `(gifter_id, gift_count)` pairs ordered
    /// by gift count descending. Only counts rows where `gifter_id IS NOT NULL` (i.e.
    /// gifted subscriptions). Useful for a "top gifters" leaderboard UI. `limit` is
    /// clamped to `[1, 100]`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn gift_leaderboard(
        &self,
        creator: ParticipantId,
        limit: i64,
    ) -> Result<Vec<(uuid::Uuid, i64)>, sqlx::Error> {
        let limit = limit.clamp(1, 100);
        let rows: Vec<(uuid::Uuid, i64)> = sqlx::query_as(
            "SELECT gifter_id, COUNT(*) AS gift_count \
             FROM creator_subscriptions \
             WHERE creator_id = $1 AND gifter_id IS NOT NULL \
             GROUP BY gifter_id ORDER BY gift_count DESC LIMIT $2",
        )
        .bind(creator.to_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Whether `subscriber` has an active subscription to `creator`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn is_subscribed(
        &self,
        creator: ParticipantId,
        subscriber: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let row: Option<(uuid::Uuid,)> = sqlx::query_as(
            r"SELECT id FROM creator_subscriptions
               WHERE creator_id = $1 AND subscriber_id = $2 AND active = true",
        )
        .bind(creator.to_uuid())
        .bind(subscriber.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored creator_subscription
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

    /// Create a throwaway participant so the test is self-contained.
    async fn participant(p: &PgPool, label: &str) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("{label}-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn tiers_create_list_get_delete_creator_scoped() {
        let p = pool();
        let repo = SubscriptionRepo::new(p.clone());
        let creator = participant(&p, "creator-sub").await;
        let stranger = participant(&p, "stranger-sub").await;

        // create → list shows it, cheapest first.
        let t1 = repo
            .create_tier(creator, "Gold", 999, Some("emotes"))
            .await
            .unwrap();
        let t2 = repo
            .create_tier(creator, "Bronze", 199, None)
            .await
            .unwrap();
        let listed = repo.list_tiers(creator).await.unwrap();
        assert!(listed.iter().any(|t| t.id == t1));
        assert!(listed.iter().any(|t| t.id == t2));
        assert_eq!(listed.first().map(|t| t.id), Some(t2), "cheapest first");

        // get works; fields round-trip.
        let got = repo.get_tier(t1).await.unwrap().expect("present");
        assert_eq!(got.name, "Gold");
        assert_eq!(got.price_cents, 999);
        assert_eq!(got.perks.as_deref(), Some("emotes"));

        // A stranger's delete is a no-op; the creator's succeeds, the second is a no-op.
        assert!(
            !repo.delete_tier(t1, stranger).await.unwrap(),
            "stranger cannot delete"
        );
        assert!(
            repo.delete_tier(t1, creator).await.unwrap(),
            "creator deletes"
        );
        assert!(
            !repo.delete_tier(t1, creator).await.unwrap(),
            "second delete is a no-op"
        );

        // Cleanup.
        sqlx::query("DELETE FROM creator_tiers WHERE creator_id = $1")
            .bind(creator.to_uuid())
            .execute(&p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn subscribe_upsert_unsubscribe_and_listings() {
        let p = pool();
        let repo = SubscriptionRepo::new(p.clone());
        let creator = participant(&p, "creator-sub2").await;
        let viewer = participant(&p, "viewer-sub2").await;

        let t1 = repo.create_tier(creator, "T1", 100, None).await.unwrap();
        let t2 = repo.create_tier(creator, "T2", 200, None).await.unwrap();

        // First subscribe.
        let sid = repo.subscribe(creator, viewer, t1).await.unwrap();
        assert!(repo.is_subscribed(creator, viewer).await.unwrap());

        // Re-subscribe at a new tier upserts (same row id) and stays active.
        let sid2 = repo.subscribe(creator, viewer, t2).await.unwrap();
        assert_eq!(sid, sid2, "upsert keeps the same row id");
        let subs = repo.subscriptions_of(viewer).await.unwrap();
        assert_eq!(subs.len(), 1, "one row per (creator, subscriber)");
        assert_eq!(subs[0].tier_id, t2, "tier re-pointed");
        assert!(subs[0].active);

        // Creator sees the subscriber.
        let subscribers = repo.subscribers_of(creator).await.unwrap();
        assert!(subscribers.iter().any(|s| s.subscriber_id == viewer));

        // Unsubscribe deactivates; a second is a no-op; listings drop it.
        assert!(
            repo.unsubscribe(creator, viewer).await.unwrap(),
            "deactivates"
        );
        assert!(
            !repo.unsubscribe(creator, viewer).await.unwrap(),
            "second is a no-op"
        );
        assert!(!repo.is_subscribed(creator, viewer).await.unwrap());
        assert!(repo.subscriptions_of(viewer).await.unwrap().is_empty());
        assert!(repo.subscribers_of(creator).await.unwrap().is_empty());

        // Re-subscribe reactivates.
        repo.subscribe(creator, viewer, t1).await.unwrap();
        assert!(repo.is_subscribed(creator, viewer).await.unwrap());

        // Cleanup.
        sqlx::query("DELETE FROM creator_subscriptions WHERE creator_id = $1")
            .bind(creator.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM creator_tiers WHERE creator_id = $1")
            .bind(creator.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
