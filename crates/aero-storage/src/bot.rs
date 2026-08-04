//! Bot repository (方向三 — 开放平台).
//!
//! Backs `migrations/0141_bots.sql`. Bots are participants with `kind = 'bot'`
//! that authenticate via their own opaque bearer token.

use aero_common::{ParticipantId, WorkspaceId};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

mod governance;
mod governance_tx;

/// Maximum bots one participant may own across all workspaces.
pub const MAX_BOTS_PER_OWNER: i64 = 16;
/// Maximum event subscriptions retained for one bot.
pub const MAX_BOT_SUBSCRIPTIONS_PER_BOT: i64 = 32;
/// Safety-page ceiling for workspace-wide bot listings (many owners may share it).
pub const MAX_BOTS_PER_WORKSPACE_PAGE: i64 = 200;
/// Maximum external webhook candidates materialized for one room event.
pub const MAX_BOT_EVENT_CANDIDATES: usize = 1_024;
/// Maximum external bot subscriptions retained across one workspace.
pub const MAX_BOT_EXTERNAL_SUBSCRIPTIONS_PER_WORKSPACE: i64 = 1_024;
const BOT_EVENT_CANDIDATE_PROBE_LIMIT: i64 = 1_025;

/// A bounded bot/subscription write failed.
#[derive(Debug, thiserror::Error)]
pub enum BotWriteError {
    #[error("bot quota exceeded")]
    BotQuotaExceeded,
    #[error("bot subscription quota exceeded")]
    SubscriptionQuotaExceeded,
    #[error("workspace external bot subscription quota exceeded")]
    WorkspaceSubscriptionQuotaExceeded,
    #[error("external bot subscription requires a canonical workspace scope")]
    MissingWorkspaceScope,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// A bot record (storage projection).
#[derive(Debug, Clone, Serialize)]
pub struct Bot {
    pub id: ParticipantId,
    pub owner_id: ParticipantId,
    pub name: String,
    pub icon_url: Option<String>,
    pub workspace_id: Option<WorkspaceId>,
    /// `true` when a token has been issued (`token_hash` is non-NULL).
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
    #[cfg(test)]
    pub async fn create(
        &self,
        owner: ParticipantId,
        name: &str,
        icon_url: Option<&str>,
        workspace: Option<WorkspaceId>,
    ) -> Result<ParticipantId, BotWriteError> {
        let bot_id = ParticipantId::new();
        let now = time::OffsetDateTime::now_utc();

        let mut tx = self.pool.begin().await?;
        // Serialize the owner-scoped count + inserts. A hash collision merely
        // adds harmless contention; it cannot weaken the quota.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("aero:bot-owner:{}", owner.to_uuid()))
            .execute(&mut *tx)
            .await?;
        let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM bots WHERE owner_id = $1")
            .bind(owner.to_uuid())
            .fetch_one(&mut *tx)
            .await?;
        if count >= MAX_BOTS_PER_OWNER {
            tx.commit().await?;
            return Err(BotWriteError::BotQuotaExceeded);
        }

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
              ORDER BY created_at DESC
              LIMIT $2",
        )
        .bind(workspace.map(|w| w.to_uuid()))
        .bind(MAX_BOTS_PER_WORKSPACE_PAGE)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Generate and persist a new bot token. Returns the plaintext token
    /// (the caller must present it to the bot owner — it is never stored).
    #[cfg(test)]
    pub async fn rotate_token(&self, bot_id: ParticipantId) -> Result<Option<String>, sqlx::Error> {
        let token = format!("bot_{}", uuid::Uuid::new_v4());
        let hash = crate::revoked_token::hash_token(&token);
        let result =
            sqlx::query("UPDATE bots SET token_hash = $2, updated_at = now() WHERE id = $1")
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

    /// The opaque prefix every minted bot token carries (see
    /// [`Self::rotate_token_authorized`]). Exposed so the auth extractor can
    /// cheaply tell a bot token apart from a PAT or JWT before hashing / DB work.
    pub const TOKEN_PREFIX: &'static str = "bot_";
    // ---------- event subscriptions (方向三) ----------

    /// Subscribe a bot to an event type.
    #[cfg(test)]
    pub async fn subscribe(
        &self,
        bot_id: ParticipantId,
        event_type: &str,
        filters: Option<&serde_json::Value>,
        webhook_url: Option<&str>,
        webhook_secret: Option<&str>,
    ) -> Result<Uuid, BotWriteError> {
        let id = Uuid::new_v4();
        let workspace_scope = if webhook_url.is_some() {
            Some(
                filters
                    .and_then(serde_json::Value::as_object)
                    .and_then(|object| object.get("workspace_id"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|workspace| !workspace.is_empty())
                    .ok_or(BotWriteError::MissingWorkspaceScope)?
                    .to_owned(),
            )
        } else {
            None
        };
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("aero:bot-subscriptions:{}", bot_id.to_uuid()))
            .execute(&mut *tx)
            .await?;
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM bot_event_subscriptions WHERE bot_id = $1",
        )
        .bind(bot_id.to_uuid())
        .fetch_one(&mut *tx)
        .await?;
        if count >= MAX_BOT_SUBSCRIPTIONS_PER_BOT {
            tx.commit().await?;
            return Err(BotWriteError::SubscriptionQuotaExceeded);
        }

        if let Some(workspace_scope) = workspace_scope.as_deref() {
            // Every external room subscription is projected onto its canonical
            // workspace by the API. Serialize the workspace-wide count so two
            // bots cannot concurrently cross the shared fan-out ceiling.
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                .bind(format!("aero:bot-external-subscriptions:{workspace_scope}"))
                .execute(&mut *tx)
                .await?;
            let workspace_count = sqlx::query_scalar::<_, i64>(
                r"SELECT COUNT(*)
                    FROM bot_event_subscriptions
                   WHERE webhook_url IS NOT NULL
                     AND scope_workspace_id = $1",
            )
            .bind(workspace_scope)
            .fetch_one(&mut *tx)
            .await?;
            if workspace_count >= MAX_BOT_EXTERNAL_SUBSCRIPTIONS_PER_WORKSPACE {
                tx.commit().await?;
                return Err(BotWriteError::WorkspaceSubscriptionQuotaExceeded);
            }
        }

        sqlx::query(
            r"INSERT INTO bot_event_subscriptions
                  (id, bot_id, event_type, filters, webhook_url, webhook_secret)
               VALUES ($1, $2, $3, COALESCE($4, '{}'::JSONB), $5, $6)",
        )
        .bind(id)
        .bind(bot_id.to_uuid())
        .bind(event_type)
        .bind(filters)
        .bind(webhook_url)
        .bind(webhook_secret)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// List subscriptions for a bot.
    #[cfg(test)]
    pub async fn list_subscriptions(
        &self,
        bot_id: ParticipantId,
    ) -> Result<Vec<BotEventSubscription>, sqlx::Error> {
        let rows = sqlx::query_as::<_, BotSubRow>(
            r"SELECT id, bot_id, event_type, filters, webhook_url, created_at
               FROM bot_event_subscriptions
              WHERE bot_id = $1
              ORDER BY created_at DESC
              LIMIT $2",
        )
        .bind(bot_id.to_uuid())
        .bind(MAX_BOT_SUBSCRIPTIONS_PER_BOT)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Delete a subscription (owner-scoped: only the owning bot's).
    #[cfg(test)]
    pub async fn delete_subscription(
        &self,
        sub_id: Uuid,
        bot_id: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result =
            sqlx::query(r"DELETE FROM bot_event_subscriptions WHERE id = $1 AND bot_id = $2")
                .bind(sub_id)
                .bind(bot_id.to_uuid())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Replace a webhook-bound subscription's HMAC secret. Owner scoping is
    /// enforced by `bot_id`; WS-only subscriptions cannot mint a meaningless
    /// secret. The plaintext is supplied by the API and never exposed by list
    /// queries, matching the room-webhook secret model.
    #[cfg(test)]
    pub async fn rotate_subscription_secret(
        &self,
        sub_id: Uuid,
        bot_id: ParticipantId,
        secret: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE bot_event_subscriptions
                  SET webhook_secret = $3
                WHERE id = $1
                  AND bot_id = $2
                  AND webhook_url IS NOT NULL",
        )
        .bind(sub_id)
        .bind(bot_id.to_uuid())
        .bind(secret)
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
    /// The generated scope columns are indexed projections of `filters`. The
    /// query uses them to restrict candidates to this event's room/workspace
    /// before returning the full JSON for the dispatcher's final pure predicate.
    pub async fn subscriptions_for_event(
        &self,
        event_type: &str,
        room_id: aero_common::RoomId,
        workspace_id: Option<WorkspaceId>,
    ) -> Result<(Vec<MatchedSubscription>, bool), sqlx::Error> {
        let mut rows = sqlx::query_as::<_, MatchedSubRow>(
            r"SELECT s.id, b.id AS bot_id, b.owner_id, s.webhook_url,
                     s.webhook_secret, s.filters
               FROM bot_event_subscriptions s
               JOIN bots b ON b.id = s.bot_id
              WHERE s.event_type = $1
                AND s.webhook_url IS NOT NULL
                AND s.webhook_secret IS NOT NULL
                AND (
                    s.scope_room_id = $2
                    OR (
                        s.scope_room_id IS NULL
                        AND s.scope_workspace_id = $3
                    )
                )
                AND EXISTS (
                    SELECT 1 FROM participants p
                     WHERE p.id = b.id
                       AND p.deleted_at IS NULL)
              ORDER BY s.id
              LIMIT $4",
        )
        .bind(event_type)
        .bind(room_id.to_string())
        .bind(workspace_id.map(|workspace| workspace.to_string()))
        .bind(BOT_EVENT_CANDIDATE_PROBE_LIMIT)
        .fetch_all(&self.pool)
        .await?;
        let truncated = rows.len() > MAX_BOT_EVENT_CANDIDATES;
        rows.truncate(MAX_BOT_EVENT_CANDIDATES);
        Ok((rows.into_iter().map(Into::into).collect(), truncated))
    }

    /// Resolve the current webhook target for one queued subscription delivery.
    ///
    /// The target and secret are intentionally read at send time: rotating a
    /// secret takes effect for retries, while deleting/soft-deleting the bot
    /// makes the target disappear before any further external request.
    pub async fn subscription_delivery_target(
        &self,
        subscription_id: Uuid,
    ) -> Result<Option<MatchedSubscription>, sqlx::Error> {
        let row = sqlx::query_as::<_, MatchedSubRow>(
            r"SELECT s.id, b.id AS bot_id, b.owner_id, s.webhook_url,
                     s.webhook_secret, s.filters
               FROM bot_event_subscriptions s
               JOIN bots b ON b.id = s.bot_id
              WHERE s.id = $1
                AND s.webhook_url IS NOT NULL
                AND s.webhook_secret IS NOT NULL
                AND EXISTS (
                    SELECT 1 FROM participants p
                     WHERE p.id = b.id
                       AND p.deleted_at IS NULL)",
        )
        .bind(subscription_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    // ---------- subscription delivery log (方向三 — observability) ----------

    /// Record one bot-subscription delivery attempt (migration 0147).
    ///
    /// Called by the event dispatcher after each POST to a subscription's
    /// `webhook_url`, so a previously-invisible (warn-and-continue) delivery
    /// becomes observable: which subscription/bot, the event type, the outcome
    /// ([`DeliveryStatus`]), the HTTP status (`None` on a transport error), and a
    /// short error excerpt. [`Self::record_delivery_attempt`] records the numbered
    /// retries from the durable worker; this wrapper retains the original
    /// single-attempt API for existing callers.
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
        self.record_delivery_attempt(
            subscription_id,
            bot_id,
            event_type,
            status,
            http_status,
            error,
            1,
        )
        .await
    }

    /// Record one numbered attempt from the durable bot-delivery worker.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_delivery_attempt(
        &self,
        subscription_id: Uuid,
        bot_id: ParticipantId,
        event_type: &str,
        status: DeliveryStatus,
        http_status: Option<u16>,
        error: Option<&str>,
        attempts: i32,
    ) -> Result<Uuid, sqlx::Error> {
        let id = Uuid::new_v4();
        sqlx::query(
            r"INSERT INTO bot_subscription_deliveries
                  (id, subscription_id, bot_id, event_type, status, http_status, error, attempts)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id)
        .bind(subscription_id)
        .bind(bot_id.to_uuid())
        .bind(event_type)
        .bind(status.as_str())
        .bind(http_status.map(i32::from))
        .bind(error)
        .bind(attempts.max(1))
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List recent delivery records for one subscription, newest first (capped at
    /// `limit`, clamped to a sane ceiling). Owner-authorization is the caller's
    /// responsibility (the routes layer asserts bot ownership first).
    #[cfg(test)]
    pub async fn list_deliveries_for_subscription(
        &self,
        subscription_id: Uuid,
        limit: i64,
    ) -> Result<Vec<BotDelivery>, sqlx::Error> {
        let rows = sqlx::query_as::<_, BotDeliveryRow>(
            r"SELECT id, subscription_id, bot_id, event_type, status,
                     http_status, error, attempts, created_at, event_id
                FROM (
                    SELECT d.id, d.subscription_id, d.bot_id, d.event_type,
                           d.status, d.http_status, d.error, d.attempts,
                           d.created_at, NULL::UUID AS event_id
                      FROM bot_subscription_deliveries d
                     WHERE d.subscription_id = $1
                    UNION ALL
                    SELECT q.id, q.subscription_id, q.bot_id, q.event_type,
                           'dead'::TEXT AS status,
                           q.last_http_status AS http_status,
                           q.last_error AS error, q.attempts,
                           q.completed_at AS created_at, q.event_id
                      FROM bot_subscription_delivery_outbox q
                     WHERE q.subscription_id = $1
                       AND q.status = 'dead'
                ) AS history
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
    #[cfg(test)]
    pub async fn list_deliveries_for_bot(
        &self,
        bot_id: ParticipantId,
        limit: i64,
    ) -> Result<Vec<BotDelivery>, sqlx::Error> {
        let rows = sqlx::query_as::<_, BotDeliveryRow>(
            r"SELECT id, subscription_id, bot_id, event_type, status,
                     http_status, error, attempts, created_at, event_id
                FROM (
                    SELECT d.id, d.subscription_id, d.bot_id, d.event_type,
                           d.status, d.http_status, d.error, d.attempts,
                           d.created_at, NULL::UUID AS event_id
                      FROM bot_subscription_deliveries d
                     WHERE d.bot_id = $1
                    UNION ALL
                    SELECT q.id, q.subscription_id, q.bot_id, q.event_type,
                           'dead'::TEXT AS status,
                           q.last_http_status AS http_status,
                           q.last_error AS error, q.attempts,
                           q.completed_at AS created_at, q.event_id
                      FROM bot_subscription_delivery_outbox q
                     WHERE q.bot_id = $1
                       AND q.status = 'dead'
                ) AS history
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
    /// Attempt rows are `"delivered"`/`"failed"`; durable DLQ summary rows are
    /// `"dead"`.
    pub status: String,
    pub http_status: Option<i32>,
    pub error: Option<String>,
    pub attempts: i32,
    pub created_at: time::OffsetDateTime,
    /// Present on a durable DLQ summary row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<Uuid>,
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
    event_id: Option<Uuid>,
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
            event_id: r.event_id,
        }
    }
}

/// One webhook-bound subscription matched by [`BotRepo::subscriptions_for_event`]
/// — the minimal shape the materializer needs to filter and the worker needs to
/// sign a later POST.
/// `webhook_url` is guaranteed non-NULL (the query excludes WS-only rows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedSubscription {
    /// The owning subscription's id (used as the delivery correlation handle).
    pub id: Uuid,
    /// The owning bot's participant id.
    pub bot_id: ParticipantId,
    /// Human participant who owns the bot. Dispatchers must re-authorize this
    /// identity against the event room at send time so a broad subscription
    /// cannot leak events across tenants, and access removal/deactivation takes
    /// effect before an external HTTP request is made.
    pub owner_id: ParticipantId,
    /// The endpoint to POST the (signed) event to.
    pub webhook_url: String,
    /// Per-subscription HMAC secret. It is selected only on the delivery path
    /// and is never exposed by [`BotRepo::list_subscriptions`].
    pub webhook_secret: String,
    /// The per-subscription filter JSON: `{room_id?, workspace_id?, action_id?}`.
    /// `{}` (the column default) means "match every event of this type" that the
    /// owner is still authorized to receive; the dispatcher performs that
    /// canonical room-access check immediately before external delivery.
    pub filters: serde_json::Value,
}

#[derive(sqlx::FromRow)]
struct MatchedSubRow {
    id: Uuid,
    bot_id: Uuid,
    owner_id: Uuid,
    webhook_url: String,
    webhook_secret: String,
    filters: sqlx::types::Json<serde_json::Value>,
}

impl From<MatchedSubRow> for MatchedSubscription {
    fn from(r: MatchedSubRow) -> Self {
        Self {
            id: r.id,
            bot_id: ParticipantId::from_uuid(r.bot_id),
            owner_id: ParticipantId::from_uuid(r.owner_id),
            webhook_url: r.webhook_url,
            webhook_secret: r.webhook_secret,
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
        assert_eq!(
            DeliveryStatus::from_http_status(200),
            DeliveryStatus::Delivered
        );
        assert_eq!(
            DeliveryStatus::from_http_status(204),
            DeliveryStatus::Delivered
        );
        assert_eq!(
            DeliveryStatus::from_http_status(299),
            DeliveryStatus::Delivered
        );
        assert_eq!(
            DeliveryStatus::from_http_status(300),
            DeliveryStatus::Failed
        );
        assert_eq!(
            DeliveryStatus::from_http_status(404),
            DeliveryStatus::Failed
        );
        assert_eq!(
            DeliveryStatus::from_http_status(500),
            DeliveryStatus::Failed
        );
        assert_eq!(
            DeliveryStatus::from_http_status(199),
            DeliveryStatus::Failed
        );
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

    #[test]
    fn containment_limits_bound_one_owners_external_fanout() {
        assert_eq!(MAX_BOTS_PER_OWNER, 16);
        assert_eq!(MAX_BOT_SUBSCRIPTIONS_PER_BOT, 32);
        assert_eq!(MAX_BOTS_PER_OWNER * MAX_BOT_SUBSCRIPTIONS_PER_BOT, 512);
        assert_eq!(MAX_BOTS_PER_WORKSPACE_PAGE, 200);
        assert_eq!(MAX_BOT_EVENT_CANDIDATES, 1_024);
        assert_eq!(MAX_BOT_EXTERNAL_SUBSCRIPTIONS_PER_WORKSPACE, 1_024);
    }

    #[tokio::test]
    async fn external_subscription_requires_workspace_projection_before_io() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://aero:aero@localhost/aero")
            .unwrap();
        let repo = BotRepo::new(pool);
        let filters = serde_json::json!({ "room_id": aero_common::RoomId::new().to_string() });
        let result = repo
            .subscribe(
                ParticipantId::new(),
                "message",
                Some(&filters),
                Some("https://example.test/hook"),
                Some("test-secret-0123456789abcdef0123456789"),
            )
            .await;
        assert!(matches!(result, Err(BotWriteError::MissingWorkspaceScope)));
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
#[path = "bot/db_tests.rs"]
mod db_tests;
