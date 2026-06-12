//! Subscription tier levels — extended per-creator tier definitions.
//!
//! Backs `migrations/0109_subscription_tiers.sql`. A creator defines tiers with
//! a name, price, display position, and a freeform JSONB `benefits` bag (e.g.
//! badge colour, emote slots, ad-free flag). Distinct from the existing
//! `creator_tiers` table (migration 0051) which the `SubscriptionRepo` manages —
//! this is the richer, position-ordered, benefits-aware counterpart.

use aero_common::{Error, ParticipantId};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// One creator subscription tier row.
#[derive(Debug, Clone, Serialize)]
pub struct SubscriptionTier {
    pub id: Uuid,
    pub creator_id: Uuid,
    pub name: String,
    pub price_cents: i32,
    pub position: i32,
    pub benefits: serde_json::Value,
}

#[derive(sqlx::FromRow)]
struct TierRow {
    id: Uuid,
    creator_id: Uuid,
    name: String,
    price_cents: i32,
    position: i32,
    benefits: serde_json::Value,
}

impl From<TierRow> for SubscriptionTier {
    fn from(r: TierRow) -> Self {
        Self {
            id: r.id,
            creator_id: r.creator_id,
            name: r.name,
            price_cents: r.price_cents,
            position: r.position,
            benefits: r.benefits,
        }
    }
}

/// Repository for the extended `subscription_tiers` table (migration 0109).
#[derive(Clone)]
pub struct SubscriptionTierRepo {
    pub pg: PgPool,
}

impl SubscriptionTierRepo {
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Create a new tier for `creator`. `position` defaults to `0`; callers may
    /// reorder tiers with a subsequent UPDATE if needed.
    pub async fn create(
        &self,
        creator: ParticipantId,
        name: &str,
        price_cents: i32,
        benefits: serde_json::Value,
    ) -> Result<SubscriptionTier, Error> {
        let row: TierRow = sqlx::query_as(
            "INSERT INTO subscription_tiers (creator_id, name, price_cents, benefits) \
             VALUES ($1, $2, $3, $4) \
             RETURNING id, creator_id, name, price_cents, position, benefits",
        )
        .bind(creator.to_uuid())
        .bind(name)
        .bind(price_cents)
        .bind(&benefits)
        .fetch_one(&self.pg)
        .await
        .map_err(Error::from)?;
        Ok(row.into())
    }

    /// All tiers for `creator`, ordered by `position` then `name`.
    pub async fn list_for_creator(
        &self,
        creator: ParticipantId,
    ) -> Result<Vec<SubscriptionTier>, Error> {
        let rows: Vec<TierRow> = sqlx::query_as(
            "SELECT id, creator_id, name, price_cents, position, benefits \
             FROM subscription_tiers \
             WHERE creator_id = $1 \
             ORDER BY position, name",
        )
        .bind(creator.to_uuid())
        .fetch_all(&self.pg)
        .await
        .map_err(Error::from)?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Look up a single tier by its `id`.
    pub async fn get(&self, id: Uuid) -> Result<Option<SubscriptionTier>, Error> {
        let row: Option<TierRow> = sqlx::query_as(
            "SELECT id, creator_id, name, price_cents, position, benefits \
             FROM subscription_tiers \
             WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pg)
        .await
        .map_err(Error::from)?;
        Ok(row.map(Into::into))
    }

    /// Delete a tier owned by `creator`. Returns `true` if a row was removed.
    /// The `creator_id` guard makes the delete a no-op for non-owners.
    pub async fn delete(&self, id: Uuid, creator: ParticipantId) -> Result<bool, Error> {
        let r = sqlx::query(
            "DELETE FROM subscription_tiers WHERE id = $1 AND creator_id = $2",
        )
        .bind(id)
        .bind(creator.to_uuid())
        .execute(&self.pg)
        .await
        .map_err(Error::from)?;
        Ok(r.rows_affected() > 0)
    }
}
