//! Transaction-owned administration for outbound webhook deliveries.

use aero_common::{Error, ParticipantId, RoomId, WebhookDeliveryId, WebhookId, WorkspaceId};
use sqlx::{Postgres, Transaction};

use super::{clamp_limit, row_to_delivery, DeliveryRow, WebhookDelivery, WebhookDeliveryRepo};

const REQUEUE_AUDIT_ACTION: &str = "webhook.delivery.requeued";

#[derive(Clone, Copy)]
struct DeliveryTenant {
    webhook: WebhookId,
    room: RoomId,
    workspace: WorkspaceId,
}

impl WebhookDeliveryRepo {
    /// List one webhook's delivery history while `actor` remains an effective
    /// Owner/Admin of the webhook's real room workspace.
    ///
    /// A caller with no membership in the owning workspace receives an opaque
    /// [`Error::NotFound`]; a current member whose effective role is below
    /// Admin receives [`Error::Forbidden`].
    pub async fn list_for_webhook_authorized(
        &self,
        webhook: WebhookId,
        actor: ParticipantId,
        limit: Option<i64>,
    ) -> Result<Vec<WebhookDelivery>, Error> {
        self.list_authorized(webhook, actor, limit, None).await
    }

    /// List the dead-letter subset for one webhook under the same transaction-
    /// owned tenant and effective-admin fence as
    /// [`Self::list_for_webhook_authorized`].
    pub async fn list_dead_for_webhook_authorized(
        &self,
        webhook: WebhookId,
        actor: ParticipantId,
        limit: Option<i64>,
    ) -> Result<Vec<WebhookDelivery>, Error> {
        self.list_authorized(webhook, actor, limit, Some("dead"))
            .await
    }

    /// Read a delivery by global id without exposing another tenant's row.
    pub async fn get_authorized(
        &self,
        id: WebhookDeliveryId,
        actor: ParticipantId,
    ) -> Result<WebhookDelivery, Error> {
        let mut tx = self.pool.begin().await?;
        let tenant = resolve_delivery_tenant(&mut tx, id).await?;
        lock_admin_tenant(&mut tx, tenant, actor, false, false).await?;
        let row = sqlx::query_as::<_, DeliveryRow>(
            r"SELECT id, webhook_id, event_id, request_body, request_headers,
                     claim_token, status, attempts, last_status_code, last_error,
                     next_attempt_at, created_at, updated_at
                FROM webhook_delivery_log
               WHERE id = $1
                 AND webhook_id = $2",
        )
        .bind(id.to_uuid())
        .bind(tenant.webhook.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(delivery_not_found)?;
        tx.commit().await?;
        Ok(row_to_delivery(row))
    }

    /// Requeue one active webhook's dead delivery in the same transaction that
    /// rechecks tenant identity, current effective Owner/Admin authorization,
    /// the webhook revocation fence, the dead claim generation, and the audit
    /// append.
    ///
    /// The transition rotates `claim_token`, so a worker from the pre-requeue
    /// generation cannot settle the successor even though `attempts` resets.
    pub async fn requeue_authorized(
        &self,
        id: WebhookDeliveryId,
        actor: ParticipantId,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        let tenant = resolve_delivery_tenant(&mut tx, id).await?;
        lock_admin_tenant(&mut tx, tenant, actor, true, true).await?;

        let locked = sqlx::query_as::<_, (uuid::Uuid, String, i32, uuid::Uuid, bool)>(
            r"SELECT webhook_id, status, attempts, claim_token,
                     request_body IS NOT NULL
                FROM webhook_delivery_log
               WHERE id = $1
               FOR UPDATE",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(delivery_not_found)?;
        if locked.0 != tenant.webhook.to_uuid() || locked.1 != "dead" || !locked.4 {
            return Err(delivery_not_found());
        }

        if !transition_dead_generation_in_tx(
            &mut tx,
            id,
            tenant.webhook,
            tenant.workspace,
            locked.3,
        )
        .await?
        {
            return Err(delivery_not_found());
        }

        let target = id.to_string();
        crate::audit::AuditRepo::append_in_tx(
            &mut tx,
            tenant.workspace,
            Some(actor),
            REQUEUE_AUDIT_ACTION,
            Some(&target),
            serde_json::json!({
                "webhook_id": tenant.webhook,
                "room_id": tenant.room,
                "from_status": "dead",
                "to_status": "failed",
                "attempts_reset_from": locked.2,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn list_authorized(
        &self,
        webhook: WebhookId,
        actor: ParticipantId,
        limit: Option<i64>,
        status: Option<&str>,
    ) -> Result<Vec<WebhookDelivery>, Error> {
        let mut tx = self.pool.begin().await?;
        let tenant = resolve_webhook_tenant(&mut tx, webhook).await?;
        lock_admin_tenant(&mut tx, tenant, actor, false, false).await?;
        let limit = clamp_limit(limit);
        let rows = sqlx::query_as::<_, DeliveryRow>(
            r"SELECT id, webhook_id, event_id, request_body, request_headers,
                     claim_token, status, attempts, last_status_code, last_error,
                     next_attempt_at, created_at, updated_at
                FROM webhook_delivery_log
               WHERE webhook_id = $1
                 AND ($2::text IS NULL OR status = $2)
               ORDER BY created_at DESC
               LIMIT $3",
        )
        .bind(webhook.to_uuid())
        .bind(status)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(row_to_delivery).collect())
    }
}

fn delivery_not_found() -> Error {
    Error::NotFound("webhook delivery".into())
}

async fn resolve_webhook_tenant(
    tx: &mut Transaction<'_, Postgres>,
    webhook: WebhookId,
) -> Result<DeliveryTenant, Error> {
    let resolved = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid)>(
        r"SELECT hook.room_id, room.workspace_id
            FROM outgoing_webhooks hook
            JOIN rooms room ON room.id = hook.room_id
           WHERE hook.id = $1",
    )
    .bind(webhook.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(delivery_not_found)?;
    Ok(DeliveryTenant {
        webhook,
        room: RoomId::from_uuid(resolved.0),
        workspace: WorkspaceId::from_uuid(resolved.1),
    })
}

async fn resolve_delivery_tenant(
    tx: &mut Transaction<'_, Postgres>,
    id: WebhookDeliveryId,
) -> Result<DeliveryTenant, Error> {
    let resolved = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, uuid::Uuid)>(
        r"SELECT delivery.webhook_id, hook.room_id, room.workspace_id
            FROM webhook_delivery_log delivery
            JOIN outgoing_webhooks hook ON hook.id = delivery.webhook_id
            JOIN rooms room ON room.id = hook.room_id
           WHERE delivery.id = $1",
    )
    .bind(id.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(delivery_not_found)?;
    Ok(DeliveryTenant {
        webhook: WebhookId::from_uuid(resolved.0),
        room: RoomId::from_uuid(resolved.1),
        workspace: WorkspaceId::from_uuid(resolved.2),
    })
}

/// Lock order: workspace -> actor membership/effective state -> room -> hook.
/// Webhook create/revoke uses the same aggregate order.
async fn lock_admin_tenant(
    tx: &mut Transaction<'_, Postgres>,
    tenant: DeliveryTenant,
    actor: ParticipantId,
    exclusive_hook: bool,
    require_active_hook: bool,
) -> Result<(), Error> {
    let workspace_exists =
        sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(tenant.workspace.to_uuid())
            .fetch_optional(&mut **tx)
            .await?;
    if workspace_exists.is_none() {
        return Err(delivery_not_found());
    }

    // Membership absence is opaque across tenants. Once a caller is known to be
    // in this tenant, insufficient role/effective state is a normal 403.
    let membership_exists = sqlx::query_scalar::<_, bool>(
        r"SELECT true
            FROM workspace_members
           WHERE workspace_id = $1
             AND participant_id = $2
           FOR UPDATE",
    )
    .bind(tenant.workspace.to_uuid())
    .bind(actor.to_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    if membership_exists.is_none() {
        return Err(delivery_not_found());
    }
    crate::workspace::authz::assert_effective_admin_in_tx(tx, tenant.workspace, actor).await?;

    let locked_workspace = sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT workspace_id FROM rooms WHERE id = $1 FOR SHARE",
    )
    .bind(tenant.room.to_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    if locked_workspace != Some(tenant.workspace.to_uuid()) {
        return Err(delivery_not_found());
    }

    let locked_hook = if exclusive_hook {
        sqlx::query_as::<_, (uuid::Uuid, Option<time::OffsetDateTime>)>(
            "SELECT room_id, revoked_at
               FROM outgoing_webhooks
              WHERE id = $1
              FOR UPDATE",
        )
        .bind(tenant.webhook.to_uuid())
        .fetch_optional(&mut **tx)
        .await?
    } else {
        sqlx::query_as::<_, (uuid::Uuid, Option<time::OffsetDateTime>)>(
            "SELECT room_id, revoked_at
               FROM outgoing_webhooks
              WHERE id = $1
              FOR SHARE",
        )
        .bind(tenant.webhook.to_uuid())
        .fetch_optional(&mut **tx)
        .await?
    };
    let Some((locked_room, revoked_at)) = locked_hook else {
        return Err(delivery_not_found());
    };
    if locked_room != tenant.room.to_uuid() || (require_active_hook && revoked_at.is_some()) {
        return Err(delivery_not_found());
    }
    Ok(())
}

async fn transition_dead_generation_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: WebhookDeliveryId,
    webhook: WebhookId,
    workspace: WorkspaceId,
    claim_token: uuid::Uuid,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        r"UPDATE webhook_delivery_log
              SET status = 'failed',
                  attempts = 0,
                  claim_token = gen_random_uuid(),
                  last_error = NULL,
                  last_status_code = NULL,
                  next_attempt_at = now(),
                  updated_at = now()
            WHERE id = $1
              AND webhook_id = $2
              AND status = 'dead'
              AND claim_token = $3
              AND request_body IS NOT NULL
              AND EXISTS (
                  SELECT 1
                    FROM outgoing_webhooks hook
                    JOIN rooms room ON room.id = hook.room_id
                   WHERE hook.id = $2
                     AND room.workspace_id = $4
                     AND hook.revoked_at IS NULL
              )",
    )
    .bind(id.to_uuid())
    .bind(webhook.to_uuid())
    .bind(claim_token)
    .bind(workspace.to_uuid())
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() == 1)
}

#[cfg(test)]
#[path = "admin_tests.rs"]
mod tests;
