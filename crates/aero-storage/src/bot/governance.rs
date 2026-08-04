//! Transaction-owned bot governance.

use aero_common::{Error, ParticipantId, WorkspaceId};
use uuid::Uuid;

use super::governance_tx::{
    advisory_lock, authorize_owned_bot, bot_not_found, delivery_not_found, lock_active_participant,
    lock_effective_workspace_member, lock_expected_bot, lock_room_member, lock_subscription,
    mint_bot_token, parse_subscription_scope, resolve_delivery_tenant, resolve_owned_bot,
    resolve_room_workspace, subscription_not_found,
};
use super::{
    BotDelivery, BotDeliveryRow, BotEventSubscription, BotRepo, BotSubRow, MAX_BOTS_PER_OWNER,
    MAX_BOT_EXTERNAL_SUBSCRIPTIONS_PER_WORKSPACE, MAX_BOT_SUBSCRIPTIONS_PER_BOT,
};

const BOT_DELIVERY_REQUEUE_AUDIT_ACTION: &str = "bot.delivery.requeued";

impl BotRepo {
    /// Create a bot and persist its initial bearer-token hash atomically.
    ///
    /// Workspace-scoped creation locks the workspace governance boundary and
    /// rechecks the owner's complete effective membership before either the bot
    /// participant or the credential is written.
    pub async fn create_authorized_with_token(
        &self,
        owner: ParticipantId,
        name: &str,
        icon_url: Option<&str>,
        workspace: Option<WorkspaceId>,
    ) -> Result<(ParticipantId, String), Error> {
        let bot_id = ParticipantId::new();
        let token = mint_bot_token();
        let token_hash = crate::revoked_token::hash_token(&token);
        let now = time::OffsetDateTime::now_utc();
        let mut tx = self.pool.begin().await?;

        if workspace.is_some() {
            crate::ownership::lock_membership_governance(&mut tx).await?;
        }
        if let Some(workspace) = workspace {
            lock_effective_workspace_member(&mut tx, workspace, owner, true).await?;
        } else {
            lock_active_participant(&mut tx, owner).await?;
        }

        // Workspace governance is always acquired before quota locks. Every bot
        // create then takes exactly one owner-keyed advisory lock.
        advisory_lock(&mut tx, &format!("aero:bot-owner:{}", owner.to_uuid())).await?;
        let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM bots WHERE owner_id = $1")
            .bind(owner.to_uuid())
            .fetch_one(&mut *tx)
            .await?;
        if count >= MAX_BOTS_PER_OWNER {
            return Err(Error::Conflict("bot quota exceeded".into()));
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
        if let Some(workspace) = workspace {
            sqlx::query(
                r"INSERT INTO workspace_members
                      (workspace_id, participant_id, role, joined_at)
                   VALUES ($1, $2, 'member', $3)",
            )
            .bind(workspace.to_uuid())
            .bind(bot_id.to_uuid())
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            r"INSERT INTO bots
                  (id, owner_id, name, icon_url, workspace_id, token_hash,
                   created_at, updated_at)
              VALUES ($1, $2, $3, $4, $5, $6, $7, $7)",
        )
        .bind(bot_id.to_uuid())
        .bind(owner.to_uuid())
        .bind(name)
        .bind(icon_url)
        .bind(workspace.map(|workspace| workspace.to_uuid()))
        .bind(token_hash)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((bot_id, token))
    }

    /// Rotate a bot token while ownership and workspace access stay locked
    /// through the credential update.
    pub async fn rotate_token_authorized(
        &self,
        bot: ParticipantId,
        actor: ParticipantId,
    ) -> Result<String, Error> {
        let mut tx = self.pool.begin().await?;
        authorize_owned_bot(&mut tx, bot, actor, true).await?;
        let token = mint_bot_token();
        let token_hash = crate::revoked_token::hash_token(&token);
        let updated = sqlx::query(
            "UPDATE bots
                SET token_hash = $3, updated_at = now()
              WHERE id = $1 AND owner_id = $2",
        )
        .bind(bot.to_uuid())
        .bind(actor.to_uuid())
        .bind(token_hash)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(bot_not_found());
        }
        tx.commit().await?;
        Ok(token)
    }

    /// Create a subscription with current ownership, tenant resolution, room
    /// access, quota reservation, and insert in one transaction.
    ///
    /// Any room scope is projected onto its canonical workspace. A
    /// workspace-scoped bot cannot subscribe outside its immutable workspace.
    /// The returned secret exists only for webhook-bound subscriptions.
    #[allow(clippy::too_many_arguments)]
    pub async fn subscribe_authorized(
        &self,
        bot: ParticipantId,
        actor: ParticipantId,
        event_type: &str,
        filters: &serde_json::Value,
        webhook_url: Option<&str>,
    ) -> Result<(Uuid, Option<String>), Error> {
        let requested_scope = parse_subscription_scope(filters)?;
        let mut tx = self.pool.begin().await?;
        let tenant = resolve_owned_bot(&mut tx, bot, actor).await?;

        let room_workspace = if let Some(room) = requested_scope.room {
            Some(resolve_room_workspace(&mut tx, room).await?)
        } else {
            None
        };
        if requested_scope.workspace.is_some()
            && room_workspace.is_some()
            && requested_scope.workspace != room_workspace
        {
            return Err(subscription_not_found());
        }

        let workspace = requested_scope
            .workspace
            .or(room_workspace)
            .or(tenant.workspace);
        if tenant.workspace.is_some() && tenant.workspace != workspace {
            return Err(subscription_not_found());
        }
        if webhook_url.is_some() && workspace.is_none() {
            return Err(Error::Invalid(
                "external bot subscription requires a workspace scope".into(),
            ));
        }

        if let Some(workspace) = workspace {
            let opaque_absent_membership = tenant.workspace.is_none();
            lock_effective_workspace_member(&mut tx, workspace, actor, opaque_absent_membership)
                .await?;
            if let Some(room) = requested_scope.room {
                lock_room_member(&mut tx, room, workspace, actor).await?;
            }
        } else {
            lock_active_participant(&mut tx, actor).await?;
        }
        lock_expected_bot(&mut tx, bot, tenant, true).await?;

        // All external subscription writers take workspace quota before bot
        // quota. Internal-only subscriptions take just the bot quota lock.
        if webhook_url.is_some() {
            let workspace = workspace.expect("webhook scope checked above");
            advisory_lock(
                &mut tx,
                &format!("aero:bot-external-subscriptions:{workspace}"),
            )
            .await?;
        }
        advisory_lock(
            &mut tx,
            &format!("aero:bot-subscriptions:{}", bot.to_uuid()),
        )
        .await?;

        let bot_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM bot_event_subscriptions WHERE bot_id = $1",
        )
        .bind(bot.to_uuid())
        .fetch_one(&mut *tx)
        .await?;
        if bot_count >= MAX_BOT_SUBSCRIPTIONS_PER_BOT {
            return Err(Error::Conflict("bot subscription quota exceeded".into()));
        }
        if let (Some(webhook_url), Some(workspace)) = (webhook_url, workspace) {
            let workspace_count = sqlx::query_scalar::<_, i64>(
                r"SELECT COUNT(*)
                    FROM bot_event_subscriptions
                   WHERE webhook_url IS NOT NULL
                     AND scope_workspace_id = $1",
            )
            .bind(workspace.to_string())
            .fetch_one(&mut *tx)
            .await?;
            if workspace_count >= MAX_BOT_EXTERNAL_SUBSCRIPTIONS_PER_WORKSPACE {
                return Err(Error::Conflict(
                    "workspace external bot subscription quota exceeded".into(),
                ));
            }
            debug_assert!(!webhook_url.is_empty());
        }

        let mut canonical_filters = filters
            .as_object()
            .cloned()
            .ok_or_else(|| Error::Invalid("filters must be a JSON object".into()))?;
        if let Some(room) = requested_scope.room {
            canonical_filters.insert(
                "room_id".into(),
                serde_json::Value::String(room.to_string()),
            );
        }
        if let Some(workspace) = workspace {
            canonical_filters.insert(
                "workspace_id".into(),
                serde_json::Value::String(workspace.to_string()),
            );
        }
        let secret = webhook_url.map(|_| crate::generate_secret());
        let id = Uuid::new_v4();
        let inserted = sqlx::query(
            r"INSERT INTO bot_event_subscriptions
                  (id, bot_id, event_type, filters, webhook_url, webhook_secret)
              SELECT $1, $2, $3, $4, $5, $6
               WHERE EXISTS (
                   SELECT 1
                     FROM bots
                    WHERE id = $2
                      AND owner_id = $7
                      AND workspace_id IS NOT DISTINCT FROM $8
               )",
        )
        .bind(id)
        .bind(bot.to_uuid())
        .bind(event_type)
        .bind(sqlx::types::Json(serde_json::Value::Object(
            canonical_filters,
        )))
        .bind(webhook_url)
        .bind(secret.as_deref())
        .bind(actor.to_uuid())
        .bind(tenant.workspace.map(|workspace| workspace.to_uuid()))
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if inserted != 1 {
            return Err(subscription_not_found());
        }
        tx.commit().await?;
        Ok((id, secret))
    }

    /// List a bot's subscriptions under a transaction-owned owner/access fence.
    pub async fn list_subscriptions_authorized(
        &self,
        bot: ParticipantId,
        actor: ParticipantId,
    ) -> Result<Vec<BotEventSubscription>, Error> {
        let mut tx = self.pool.begin().await?;
        authorize_owned_bot(&mut tx, bot, actor, false).await?;
        let rows = sqlx::query_as::<_, BotSubRow>(
            r"SELECT id, bot_id, event_type, filters, webhook_url, created_at
                FROM bot_event_subscriptions
               WHERE bot_id = $1
               ORDER BY created_at DESC
               LIMIT $2",
        )
        .bind(bot.to_uuid())
        .bind(MAX_BOT_SUBSCRIPTIONS_PER_BOT)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Delete one subscription while its current bot owner/access edge remains
    /// locked. A foreign subscription id is an opaque not-found.
    pub async fn delete_subscription_authorized(
        &self,
        bot: ParticipantId,
        subscription: Uuid,
        actor: ParticipantId,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        authorize_owned_bot(&mut tx, bot, actor, true).await?;
        let locked = lock_subscription(&mut tx, subscription, bot).await?;
        if !locked {
            return Err(subscription_not_found());
        }
        let deleted = sqlx::query(
            "DELETE FROM bot_event_subscriptions
              WHERE id = $1 AND bot_id = $2",
        )
        .bind(subscription)
        .bind(bot.to_uuid())
        .execute(&mut *tx)
        .await?;
        if deleted.rows_affected() != 1 {
            return Err(subscription_not_found());
        }
        tx.commit().await?;
        Ok(())
    }

    /// Rotate a webhook subscription secret under the same bot governance
    /// transaction and return the plaintext exactly once.
    pub async fn rotate_subscription_secret_authorized(
        &self,
        bot: ParticipantId,
        subscription: Uuid,
        actor: ParticipantId,
    ) -> Result<String, Error> {
        let mut tx = self.pool.begin().await?;
        authorize_owned_bot(&mut tx, bot, actor, true).await?;
        if !lock_subscription(&mut tx, subscription, bot).await? {
            return Err(subscription_not_found());
        }
        let secret = crate::generate_secret();
        let updated = sqlx::query(
            r"UPDATE bot_event_subscriptions
                  SET webhook_secret = $3
                WHERE id = $1
                  AND bot_id = $2
                  AND webhook_url IS NOT NULL",
        )
        .bind(subscription)
        .bind(bot.to_uuid())
        .bind(&secret)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(subscription_not_found());
        }
        tx.commit().await?;
        Ok(secret)
    }

    /// List delivery history while the requested bot is still owned by `actor`.
    pub async fn list_deliveries_authorized(
        &self,
        bot: ParticipantId,
        actor: ParticipantId,
        limit: i64,
    ) -> Result<Vec<BotDelivery>, Error> {
        let mut tx = self.pool.begin().await?;
        authorize_owned_bot(&mut tx, bot, actor, false).await?;
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
        .bind(bot.to_uuid())
        .bind(limit.clamp(1, 500))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Requeue one dead delivery with bot ownership, tenant access, subscription
    /// identity, the dead claim generation, and its audit event held atomically.
    pub async fn requeue_delivery_authorized(
        &self,
        bot: ParticipantId,
        delivery: Uuid,
        actor: ParticipantId,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        let bot_tenant = resolve_owned_bot(&mut tx, bot, actor).await?;
        let delivery_tenant = resolve_delivery_tenant(&mut tx, delivery, bot).await?;
        if bot_tenant
            .workspace
            .is_some_and(|workspace| workspace != delivery_tenant.workspace)
        {
            return Err(delivery_not_found());
        }

        lock_effective_workspace_member(
            &mut tx,
            delivery_tenant.workspace,
            actor,
            bot_tenant.workspace.is_none(),
        )
        .await?;
        lock_room_member(
            &mut tx,
            delivery_tenant.room,
            delivery_tenant.workspace,
            actor,
        )
        .await?;
        lock_expected_bot(&mut tx, bot, bot_tenant, true).await?;
        if !lock_subscription(&mut tx, delivery_tenant.subscription, bot).await? {
            return Err(delivery_not_found());
        }

        let locked = sqlx::query_as::<_, (Uuid, Uuid, String, i32, Option<Uuid>)>(
            r"SELECT subscription_id, bot_id, status, attempts, claim_token
                FROM bot_subscription_delivery_outbox
               WHERE id = $1
               FOR UPDATE",
        )
        .bind(delivery)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(delivery_not_found)?;
        if locked.0 != delivery_tenant.subscription
            || locked.1 != bot.to_uuid()
            || locked.2 != "dead"
        {
            return Err(delivery_not_found());
        }

        let updated = sqlx::query(
            r"UPDATE bot_subscription_delivery_outbox AS delivery
                  SET status = 'pending',
                      attempts = 0,
                      available_at = now(),
                      claimed_at = NULL,
                      claim_token = NULL,
                      completed_at = NULL,
                      last_http_status = NULL,
                      last_error = NULL,
                      updated_at = now()
                WHERE delivery.id = $1
                  AND delivery.subscription_id = $2
                  AND delivery.bot_id = $3
                  AND delivery.status = 'dead'
                  AND delivery.claim_token IS NOT DISTINCT FROM $4
                  AND EXISTS (
                      SELECT 1
                        FROM bot_event_subscriptions subscription
                        JOIN bots bot
                          ON bot.id = subscription.bot_id
                        JOIN rooms room
                          ON room.id = delivery.room_id
                       WHERE subscription.id = $2
                         AND subscription.bot_id = $3
                         AND bot.owner_id = $5
                         AND room.workspace_id = $6
                  )",
        )
        .bind(delivery)
        .bind(delivery_tenant.subscription)
        .bind(bot.to_uuid())
        .bind(locked.4)
        .bind(actor.to_uuid())
        .bind(delivery_tenant.workspace.to_uuid())
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(delivery_not_found());
        }

        let target = delivery.to_string();
        crate::audit::AuditRepo::append_in_tx(
            &mut tx,
            delivery_tenant.workspace,
            Some(actor),
            BOT_DELIVERY_REQUEUE_AUDIT_ACTION,
            Some(&target),
            serde_json::json!({
                "bot_id": bot,
                "subscription_id": delivery_tenant.subscription,
                "room_id": delivery_tenant.room,
                "from_status": "dead",
                "to_status": "pending",
                "attempts_reset_from": locked.3,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "governance_tests.rs"]
mod tests;
