//! Bot repository (方向三 — 开放平台).
//!
//! Backs `migrations/0141_bots.sql`. Bots are participants with `kind = 'bot'`
//! that authenticate via their own opaque bearer token.

use aero_common::{ParticipantId, WorkspaceId};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// A bot record (storage projection).
#[derive(Debug, Clone, Serialize)]
pub struct Bot {
    pub id: ParticipantId,
    pub owner_id: ParticipantId,
    pub name: String,
    pub icon_url: Option<String>,
    pub workspace_id: Option<WorkspaceId>,
    /// `true` when a token has been issued (token_hash is non-NULL).
    pub has_token: bool,
    pub created_at: time::OffsetDateTime,
}

#[derive(Clone)]
pub struct BotRepo {
    pool: PgPool,
}

impl BotRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Register a new bot: creates a participant row + bot row atomically.
    pub async fn create(
        &self,
        owner: ParticipantId,
        name: &str,
        icon_url: Option<&str>,
        workspace: Option<WorkspaceId>,
    ) -> Result<ParticipantId, sqlx::Error> {
        let bot_id = ParticipantId::new();
        let now = time::OffsetDateTime::now_utc();

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r"INSERT INTO participants (id, kind, display_name, created_by, created_at)
               VALUES ($1, 'bot', $2, $3, $4)",
        )
        .bind(bot_id.to_uuid())
        .bind(name)
        .bind(owner.to_uuid())
        .bind(now)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r"INSERT INTO bots (id, owner_id, name, icon_url, workspace_id, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, $6)",
        )
        .bind(bot_id.to_uuid())
        .bind(owner.to_uuid())
        .bind(name)
        .bind(icon_url)
        .bind(workspace.map(|w| w.to_uuid()))
        .bind(now)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(bot_id)
    }

    /// List bots for a workspace (or workspace-less system bots).
    pub async fn list_for_workspace(
        &self,
        workspace: Option<WorkspaceId>,
    ) -> Result<Vec<Bot>, sqlx::Error> {
        let rows = sqlx::query_as::<_, BotRow>(
            r"SELECT id, owner_id, name, icon_url, workspace_id, token_hash IS NOT NULL AS has_token, created_at
               FROM bots
              WHERE workspace_id IS NOT DISTINCT FROM $1
              ORDER BY created_at DESC",
        )
        .bind(workspace.map(|w| w.to_uuid()))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Generate and persist a new bot token. Returns the plaintext token
    /// (the caller must present it to the bot owner — it is never stored).
    pub async fn rotate_token(&self, bot_id: ParticipantId) -> Result<Option<String>, sqlx::Error> {
        let token = format!("bot_{}", uuid::Uuid::new_v4());
        let hash = crate::revoked_token::hash_token(&token);
        let result = sqlx::query("UPDATE bots SET token_hash = $2, updated_at = now() WHERE id = $1")
            .bind(bot_id.to_uuid())
            .bind(&hash)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        Ok(Some(token))
    }

    /// Verify a bot token: returns the bot's participant id IFF the token is
    /// active and the bot's participant is not deleted.
    ///
    /// A bot is also a participant (`bots.id == participants.id`,
    /// `kind = 'bot'`), so a deleted / GDPR-erased bot must not authenticate.
    /// We mirror [`PatRepo::verify`](crate::PatRepo::verify): resolve the token
    /// hash to a bot row, but only if an EXISTS check confirms the backing
    /// participant tombstone is clear (`deleted_at IS NULL`). The tombstone is
    /// authoritative — even if a bot row (and its `token_hash`) survived the
    /// participant's soft-delete, the surviving token still cannot grant access.
    pub async fn verify_token(&self, token: &str) -> Result<Option<ParticipantId>, sqlx::Error> {
        let hash = crate::revoked_token::hash_token(token);
        let row = sqlx::query_as::<_, (Uuid,)>(
            r"SELECT b.id FROM bots b
               WHERE b.token_hash = $1
                 AND EXISTS (
                     SELECT 1 FROM participants p
                      WHERE p.id = b.id
                        AND p.deleted_at IS NULL)",
        )
        .bind(&hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id,)| ParticipantId::from_uuid(id)))
    }

    /// The opaque prefix every minted bot token carries (see [`Self::rotate_token`],
    /// which mints `bot_<uuid>`). Exposed so the auth extractor can cheaply tell a
    /// bot token apart from a PAT (`aero_pat_…`) or a JWT before hashing / DB work.
    pub const TOKEN_PREFIX: &'static str = "bot_";
    // ---------- event subscriptions (方向三) ----------

    /// Subscribe a bot to an event type.
    pub async fn subscribe(
        &self,
        bot_id: ParticipantId,
        event_type: &str,
        filters: Option<&serde_json::Value>,
        webhook_url: Option<&str>,
    ) -> Result<Uuid, sqlx::Error> {
        let id = Uuid::new_v4();
        sqlx::query(
            r"INSERT INTO bot_event_subscriptions (id, bot_id, event_type, filters, webhook_url)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(bot_id.to_uuid())
        .bind(event_type)
        .bind(filters)
        .bind(webhook_url)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List subscriptions for a bot.
    pub async fn list_subscriptions(
        &self,
        bot_id: ParticipantId,
    ) -> Result<Vec<BotEventSubscription>, sqlx::Error> {
        let rows = sqlx::query_as::<_, BotSubRow>(
            r"SELECT id, bot_id, event_type, filters, webhook_url, created_at
               FROM bot_event_subscriptions
              WHERE bot_id = $1
              ORDER BY created_at DESC",
        )
        .bind(bot_id.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Delete a subscription (owner-scoped: only the owning bot's).
    pub async fn delete_subscription(
        &self,
        sub_id: Uuid,
        bot_id: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"DELETE FROM bot_event_subscriptions WHERE id = $1 AND bot_id = $2",
        )
        .bind(sub_id)
        .bind(bot_id.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Find all webhook-bound subscriptions for a given event type (for the event
    /// dispatcher). Returns one [`MatchedSubscription`] per row whose
    /// `event_type` matches AND whose backing bot's participant is not soft-deleted
    /// (a GDPR-erased / deleted bot must stop receiving events, mirroring
    /// [`Self::verify_token`]'s tombstone check). Rows with a NULL `webhook_url`
    /// (WS-only delivery, no out-of-band endpoint to POST to) are excluded here so
    /// the dispatcher never has to second-guess a missing URL.
    ///
    /// The per-row `filters` JSON (`{room_id, workspace_id, action_id}`) is
    /// returned verbatim; the dispatcher applies it (cheap pure predicate) — the
    /// SQL only narrows by the indexed `event_type` so this stays a single index
    /// probe regardless of how many distinct rooms have subscribers.
    pub async fn subscriptions_for_event(
        &self,
        event_type: &str,
    ) -> Result<Vec<MatchedSubscription>, sqlx::Error> {
        let rows = sqlx::query_as::<_, MatchedSubRow>(
            r"SELECT s.id, b.id AS bot_id, s.webhook_url, s.filters
               FROM bot_event_subscriptions s
               JOIN bots b ON b.id = s.bot_id
              WHERE s.event_type = $1
                AND s.webhook_url IS NOT NULL
                AND EXISTS (
                    SELECT 1 FROM participants p
                     WHERE p.id = b.id
                       AND p.deleted_at IS NULL)",
        )
        .bind(event_type)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    // ---------- subscription delivery log (方向三 — observability) ----------

    /// Record one bot-subscription delivery attempt (migration 0147).
    ///
    /// Called by the event dispatcher after each POST to a subscription's
    /// `webhook_url`, so a previously-invisible (warn-and-continue) delivery
    /// becomes observable: which subscription/bot, the event type, the outcome
    /// ([`DeliveryStatus`]), the HTTP status (`None` on a transport error), and a
    /// short error excerpt. One-shot (no retry/DLQ lifecycle) — `attempts` is
    /// always 1 today.
    ///
    /// `bot_id` is denormalized into the row so the owner-scoped listing stays a
    /// single index probe even after the subscription row is gone. The dispatcher
    /// treats a failure here as non-fatal (fail-open): an error is propagated to
    /// the caller, which logs and continues rather than aborting delivery.
    pub async fn record_delivery(
        &self,
        subscription_id: Uuid,
        bot_id: ParticipantId,
        event_type: &str,
        status: DeliveryStatus,
        http_status: Option<u16>,
        error: Option<&str>,
    ) -> Result<Uuid, sqlx::Error> {
        let id = Uuid::new_v4();
        sqlx::query(
            r"INSERT INTO bot_subscription_deliveries
                  (id, subscription_id, bot_id, event_type, status, http_status, error, attempts)
               VALUES ($1, $2, $3, $4, $5, $6, $7, 1)",
        )
        .bind(id)
        .bind(subscription_id)
        .bind(bot_id.to_uuid())
        .bind(event_type)
        .bind(status.as_str())
        .bind(http_status.map(i32::from))
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List recent delivery records for one subscription, newest first (capped at
    /// `limit`, clamped to a sane ceiling). Owner-authorization is the caller's
    /// responsibility (the routes layer asserts bot ownership first).
    pub async fn list_deliveries_for_subscription(
        &self,
        subscription_id: Uuid,
        limit: i64,
    ) -> Result<Vec<BotDelivery>, sqlx::Error> {
        let rows = sqlx::query_as::<_, BotDeliveryRow>(
            r"SELECT id, subscription_id, bot_id, event_type, status, http_status, error, attempts, created_at
               FROM bot_subscription_deliveries
              WHERE subscription_id = $1
              ORDER BY created_at DESC
              LIMIT $2",
        )
        .bind(subscription_id)
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// List recent delivery records across every subscription of one bot, newest
    /// first (capped at `limit`). Backs `GET /api/bots/:id/deliveries`; the
    /// denormalized `bot_id` column means this never re-joins through
    /// `bot_event_subscriptions`.
    pub async fn list_deliveries_for_bot(
        &self,
        bot_id: ParticipantId,
        limit: i64,
    ) -> Result<Vec<BotDelivery>, sqlx::Error> {
        let rows = sqlx::query_as::<_, BotDeliveryRow>(
            r"SELECT id, subscription_id, bot_id, event_type, status, http_status, error, attempts, created_at
               FROM bot_subscription_deliveries
              WHERE bot_id = $1
              ORDER BY created_at DESC
              LIMIT $2",
        )
        .bind(bot_id.to_uuid())
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }
}

/// The outcome of a bot-subscription delivery attempt (the `status` column of
/// `bot_subscription_deliveries`). Mirrors the dispatcher's 2xx/else branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    /// The endpoint returned a 2xx.
    Delivered,
    /// The endpoint returned a non-2xx, or the request could not be made at all
    /// (transport-level error — then `http_status` is absent).
    Failed,
}

impl DeliveryStatus {
    /// The DB/wire string for this status (the CHECK-constrained column value).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryStatus::Delivered => "delivered",
            DeliveryStatus::Failed => "failed",
        }
    }

    /// Classify a completed round trip by HTTP status code: 2xx ⇒ `Delivered`,
    /// anything else ⇒ `Failed`. Pure — unit-tested without a DB so the
    /// dispatcher's outcome mapping has a single source of truth.
    #[must_use]
    pub fn from_http_status(status: u16) -> Self {
        if (200..300).contains(&status) {
            DeliveryStatus::Delivered
        } else {
            DeliveryStatus::Failed
        }
    }
}

/// A recorded bot-subscription delivery (storage projection of one
/// `bot_subscription_deliveries` row).
#[derive(Debug, Clone, Serialize)]
pub struct BotDelivery {
    pub id: Uuid,
    pub subscription_id: Uuid,
    pub bot_id: ParticipantId,
    pub event_type: String,
    /// `"delivered"` or `"failed"` (the CHECK-constrained column value).
    pub status: String,
    pub http_status: Option<i32>,
    pub error: Option<String>,
    pub attempts: i32,
    pub created_at: time::OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct BotDeliveryRow {
    id: Uuid,
    subscription_id: Uuid,
    bot_id: Uuid,
    event_type: String,
    status: String,
    http_status: Option<i32>,
    error: Option<String>,
    attempts: i32,
    created_at: time::OffsetDateTime,
}

impl From<BotDeliveryRow> for BotDelivery {
    fn from(r: BotDeliveryRow) -> Self {
        Self {
            id: r.id,
            subscription_id: r.subscription_id,
            bot_id: ParticipantId::from_uuid(r.bot_id),
            event_type: r.event_type,
            status: r.status,
            http_status: r.http_status,
            error: r.error,
            attempts: r.attempts,
            created_at: r.created_at,
        }
    }
}

/// One webhook-bound subscription matched by [`BotRepo::subscriptions_for_event`]
/// — the minimal shape the event dispatcher needs to apply filters and POST.
/// `webhook_url` is guaranteed non-NULL (the query excludes WS-only rows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedSubscription {
    /// The owning subscription's id (used as the delivery correlation handle).
    pub id: Uuid,
    /// The owning bot's participant id.
    pub bot_id: ParticipantId,
    /// The endpoint to POST the (signed) event to.
    pub webhook_url: String,
    /// The per-subscription filter JSON: `{room_id?, workspace_id?, action_id?}`.
    /// `{}` (the column default) means "match every event of this type".
    pub filters: serde_json::Value,
}

#[derive(sqlx::FromRow)]
struct MatchedSubRow {
    id: Uuid,
    bot_id: Uuid,
    webhook_url: String,
    filters: sqlx::types::Json<serde_json::Value>,
}

impl From<MatchedSubRow> for MatchedSubscription {
    fn from(r: MatchedSubRow) -> Self {
        Self {
            id: r.id,
            bot_id: ParticipantId::from_uuid(r.bot_id),
            webhook_url: r.webhook_url,
            filters: r.filters.0,
        }
    }
}

/// A bot event subscription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotEventSubscription {
    pub id: Uuid,
    pub bot_id: ParticipantId,
    pub event_type: String,
    pub filters: serde_json::Value,
    pub webhook_url: Option<String>,
    pub created_at: time::OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct BotSubRow {
    id: Uuid,
    bot_id: Uuid,
    event_type: String,
    filters: sqlx::types::Json<serde_json::Value>,
    webhook_url: Option<String>,
    created_at: time::OffsetDateTime,
}

impl From<BotSubRow> for BotEventSubscription {
    fn from(r: BotSubRow) -> Self {
        Self {
            id: r.id,
            bot_id: ParticipantId::from_uuid(r.bot_id),
            event_type: r.event_type,
            filters: r.filters.0,
            webhook_url: r.webhook_url,
            created_at: r.created_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct BotRow {
    id: Uuid,
    owner_id: Uuid,
    name: String,
    icon_url: Option<String>,
    workspace_id: Option<Uuid>,
    has_token: bool,
    created_at: time::OffsetDateTime,
}

impl From<BotRow> for Bot {
    fn from(r: BotRow) -> Self {
        Self {
            id: ParticipantId::from_uuid(r.id),
            owner_id: ParticipantId::from_uuid(r.owner_id),
            name: r.name,
            icon_url: r.icon_url,
            workspace_id: r.workspace_id.map(WorkspaceId::from_uuid),
            has_token: r.has_token,
            created_at: r.created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_status_maps_http_2xx_to_delivered_else_failed() {
        // The dispatcher records `delivered` only for 2xx; everything else
        // (4xx/5xx, and — via the dispatcher's transport-error branch — a missing
        // status) is `failed`. This pure classifier is the single source of truth.
        assert_eq!(DeliveryStatus::from_http_status(200), DeliveryStatus::Delivered);
        assert_eq!(DeliveryStatus::from_http_status(204), DeliveryStatus::Delivered);
        assert_eq!(DeliveryStatus::from_http_status(299), DeliveryStatus::Delivered);
        assert_eq!(DeliveryStatus::from_http_status(300), DeliveryStatus::Failed);
        assert_eq!(DeliveryStatus::from_http_status(404), DeliveryStatus::Failed);
        assert_eq!(DeliveryStatus::from_http_status(500), DeliveryStatus::Failed);
        assert_eq!(DeliveryStatus::from_http_status(199), DeliveryStatus::Failed);
    }

    #[test]
    fn delivery_status_db_strings_match_check_constraint() {
        // The string written to the CHECK-constrained `status` column must be one
        // of exactly these two — a typo would fail the INSERT at runtime.
        assert_eq!(DeliveryStatus::Delivered.as_str(), "delivered");
        assert_eq!(DeliveryStatus::Failed.as_str(), "failed");
        // And the serde wire form agrees with the DB form.
        assert_eq!(
            serde_json::to_value(DeliveryStatus::Delivered).unwrap(),
            serde_json::json!("delivered")
        );
        assert_eq!(
            serde_json::to_value(DeliveryStatus::Failed).unwrap(),
            serde_json::json!("failed")
        );
    }

    #[test]
    fn rotate_token_uses_bot_prefix() {
        // The auth extractor's `is_well_formed_bot_token` gate keys off this
        // prefix, so a minted token must carry it or no bot could ever authenticate.
        assert_eq!(BotRepo::TOKEN_PREFIX, "bot_");
        // A token shaped like `rotate_token` mints must start with the prefix and
        // have a non-empty body.
        let sample = format!("{}{}", BotRepo::TOKEN_PREFIX, uuid::Uuid::new_v4());
        assert!(sample.starts_with(BotRepo::TOKEN_PREFIX));
        assert!(sample.len() > BotRepo::TOKEN_PREFIX.len());
    }
}

/// DB-backed tests for [`BotRepo`]. Gated `#[ignore]` so the default `cargo test`
/// stays hermetic (no Postgres in CI). Run against a migrated database with:
///
/// ```sh
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored bot_
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

    async fn new_owner(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("bot-owner-{id}"))
            .execute(p)
            .await
            .expect("insert owner");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn bot_token_verifies_then_rotation_invalidates_old() {
        let p = pool();
        let repo = BotRepo::new(p.clone());
        let owner = new_owner(&p).await;
        let bot = repo.create(owner, "ci-bot", None, None).await.unwrap();

        let token = repo.rotate_token(bot).await.unwrap().expect("token minted");
        assert!(token.starts_with(BotRepo::TOKEN_PREFIX), "minted token carries prefix");

        // The active token resolves to the bot's participant id …
        assert_eq!(repo.verify_token(&token).await.unwrap(), Some(bot));
        // … an unknown token does not.
        assert_eq!(repo.verify_token("bot_unknown").await.unwrap(), None);

        // Rotating mints a fresh token and invalidates the old one (only one hash
        // is stored per bot).
        let token2 = repo.rotate_token(bot).await.unwrap().expect("re-minted");
        assert_ne!(token, token2);
        assert_eq!(repo.verify_token(&token).await.unwrap(), None, "old token dead");
        assert_eq!(repo.verify_token(&token2).await.unwrap(), Some(bot));
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn deleted_bot_token_does_not_verify() {
        // The security invariant: once the bot's participant is soft-deleted
        // (`deleted_at` set), its token must stop authenticating — even though the
        // `bots` row and its `token_hash` survive.
        let p = pool();
        let repo = BotRepo::new(p.clone());
        let owner = new_owner(&p).await;
        let bot = repo.create(owner, "doomed-bot", None, None).await.unwrap();

        let token = repo.rotate_token(bot).await.unwrap().expect("token minted");
        assert_eq!(repo.verify_token(&token).await.unwrap(), Some(bot), "active before delete");

        // Soft-delete the bot's participant row (what `delete_participant` does).
        sqlx::query("UPDATE participants SET deleted_at = now() WHERE id = $1")
            .bind(bot.to_uuid())
            .execute(&p)
            .await
            .expect("soft-delete bot participant");

        // The bot row (and its token_hash) is still present …
        let still_there: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM bots WHERE id = $1 AND token_hash IS NOT NULL")
                .bind(bot.to_uuid())
                .fetch_optional(&p)
                .await
                .unwrap();
        assert!(still_there.is_some(), "bots row survives the soft-delete");

        // … but the token must no longer authenticate.
        assert_eq!(
            repo.verify_token(&token).await.unwrap(),
            None,
            "a deleted bot's token must not authenticate"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn delivery_log_records_and_lists_by_subscription_and_bot() {
        // record_delivery persists a row; the two list paths surface it, newest
        // first, scoped correctly. Exercises both the success and failure shapes.
        let p = pool();
        let repo = BotRepo::new(p.clone());
        let owner = new_owner(&p).await;
        let bot = repo.create(owner, "delivery-bot", None, None).await.unwrap();
        let sub = repo
            .subscribe(bot, "message", None, Some("https://example.test/hook"))
            .await
            .unwrap();

        // A delivered (2xx) attempt …
        repo.record_delivery(sub, bot, "message", DeliveryStatus::Delivered, Some(200), None)
            .await
            .unwrap();
        // … then a failed (5xx) attempt with an error excerpt.
        repo.record_delivery(
            sub,
            bot,
            "message",
            DeliveryStatus::Failed,
            Some(503),
            Some("upstream unavailable"),
        )
        .await
        .unwrap();

        let by_sub = repo.list_deliveries_for_subscription(sub, 100).await.unwrap();
        assert_eq!(by_sub.len(), 2, "both attempts recorded for the subscription");
        // Newest first: the failed 503 is the most recent.
        assert_eq!(by_sub[0].status, "failed");
        assert_eq!(by_sub[0].http_status, Some(503));
        assert_eq!(by_sub[0].error.as_deref(), Some("upstream unavailable"));
        assert_eq!(by_sub[1].status, "delivered");
        assert_eq!(by_sub[1].http_status, Some(200));
        assert!(by_sub[1].error.is_none());

        let by_bot = repo.list_deliveries_for_bot(bot, 100).await.unwrap();
        assert_eq!(by_bot.len(), 2, "owner-scoped listing sees both via denormalized bot_id");

        // A different bot sees none of these.
        let other_owner = new_owner(&p).await;
        let other_bot = repo.create(other_owner, "other-bot", None, None).await.unwrap();
        assert!(
            repo.list_deliveries_for_bot(other_bot, 100).await.unwrap().is_empty(),
            "delivery log is bot-scoped"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn delivery_log_cascades_on_subscription_delete() {
        // The FK is ON DELETE CASCADE: removing a subscription removes its
        // delivery rows, so a deleted subscription leaves no orphan log.
        let p = pool();
        let repo = BotRepo::new(p.clone());
        let owner = new_owner(&p).await;
        let bot = repo.create(owner, "cascade-bot", None, None).await.unwrap();
        let sub = repo
            .subscribe(bot, "reaction", None, Some("https://example.test/hook"))
            .await
            .unwrap();
        repo.record_delivery(sub, bot, "reaction", DeliveryStatus::Failed, None, Some("dns"))
            .await
            .unwrap();
        // Transport error ⇒ http_status NULL.
        let before = repo.list_deliveries_for_subscription(sub, 100).await.unwrap();
        assert_eq!(before.len(), 1);
        assert!(before[0].http_status.is_none(), "transport error has no http status");

        assert!(repo.delete_subscription(sub, bot).await.unwrap());
        let after = repo.list_deliveries_for_subscription(sub, 100).await.unwrap();
        assert!(after.is_empty(), "delivery rows cascade-deleted with the subscription");
    }
}
