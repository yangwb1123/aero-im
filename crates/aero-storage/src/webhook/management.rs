use aero_common::{ParticipantId, RoomId, WebhookId};

use super::WebhookRepo;

/// Stable domain outcomes for actor-authorized webhook management writes.
#[derive(Debug, thiserror::Error)]
pub enum WebhookWriteError {
    #[error("webhook target not found")]
    NotFound,
    #[error("direct and group-DM membership is fixed")]
    FixedMembership,
    #[error("webhook actor no longer has room or workspace access")]
    Forbidden,
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

pub(super) struct LockedWebhookRoom {
    pub workspace_id: uuid::Uuid,
    pub kind: String,
    pub is_group_dm: bool,
}

/// Resolve and lock a webhook room plus the actor's effective access edges.
///
/// The explicit `FOR UPDATE` workspace fence is load-bearing. Migration 0189's
/// `workspace_members_active_owner_guard` requests the same lock even for a
/// non-owner insert, so taking only the canonical helper's SHARE lock would let
/// concurrent webhook creates deadlock while both upgrade SHARE -> UPDATE.
///
/// Lock order is workspace -> canonical actor access -> room -> hook (the
/// caller locks the hook last for global-id mutations).
pub(super) async fn lock_actor_room_access(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    room: RoomId,
    actor: ParticipantId,
    expected_workspace: Option<uuid::Uuid>,
) -> Result<LockedWebhookRoom, WebhookWriteError> {
    // Resolve the immutable tenant edge without a row lock so the transaction
    // can acquire the workspace fence before any room lock.
    let resolved = sqlx::query_as::<_, (String, bool, uuid::Uuid)>(
        "SELECT kind, is_group_dm, workspace_id
           FROM rooms
          WHERE id = $1",
    )
    .bind(room.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(WebhookWriteError::NotFound)?;
    if expected_workspace.is_some_and(|workspace| workspace != resolved.2) {
        return Err(WebhookWriteError::NotFound);
    }

    // Take the strongest lock needed by every webhook mutation up front. This
    // also serializes later SHARE -> UPDATE room upgrades within one workspace.
    let workspace_exists =
        sqlx::query_scalar::<_, uuid::Uuid>("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(resolved.2)
            .fetch_optional(&mut **tx)
            .await?;
    if workspace_exists.is_none() {
        return Err(WebhookWriteError::NotFound);
    }

    // The canonical function locks/rechecks workspace membership, participant
    // liveness, deactivation, human-only mandatory 2FA, the room identity, and
    // the actor's room-membership edge in the governance order.
    let effective_access: bool =
        sqlx::query_scalar("SELECT aero_effective_room_access($1, $2, $3)")
            .bind(room.to_uuid())
            .bind(actor.to_uuid())
            .bind(resolved.2)
            .fetch_one(&mut **tx)
            .await?;
    if !effective_access {
        let current_workspace =
            sqlx::query_scalar::<_, uuid::Uuid>("SELECT workspace_id FROM rooms WHERE id = $1")
                .bind(room.to_uuid())
                .fetch_optional(&mut **tx)
                .await?;
        return if current_workspace == Some(resolved.2) {
            Err(WebhookWriteError::Forbidden)
        } else {
            Err(WebhookWriteError::NotFound)
        };
    }

    // Upgrade the room fence while the workspace UPDATE lock prevents another
    // canonical writer from forming a lock cycle. Re-read every identity field.
    let locked = sqlx::query_as::<_, (String, bool, uuid::Uuid)>(
        "SELECT kind, is_group_dm, workspace_id
           FROM rooms
          WHERE id = $1
          FOR UPDATE",
    )
    .bind(room.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(WebhookWriteError::NotFound)?;
    if locked.2 != resolved.2 {
        return Err(WebhookWriteError::NotFound);
    }

    Ok(LockedWebhookRoom {
        workspace_id: locked.2,
        kind: locked.0,
        is_group_dm: locked.1,
    })
}

impl WebhookRepo {
    /// Create an outgoing webhook after atomically revalidating `created_by`.
    ///
    /// `events` empty means all events.
    pub async fn create_outgoing(
        &self,
        room: RoomId,
        url: &str,
        secret: &str,
        events: &[String],
        label: Option<&str>,
        created_by: ParticipantId,
    ) -> Result<WebhookId, WebhookWriteError> {
        let id = WebhookId::new();
        let mut tx = self.pool.begin().await?;
        lock_actor_room_access(&mut tx, room, created_by, None).await?;

        sqlx::query(
            r"INSERT INTO outgoing_webhooks (id, room_id, url, secret, events, label, created_by)
               VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(url)
        .bind(secret)
        .bind(events)
        .bind(label)
        .bind(created_by.to_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Atomically authorize `actor` against the hook's owning room and revoke it.
    ///
    /// The hook row is locked last and its room identity is revalidated, so a
    /// global webhook id cannot cross a tenant boundary between resolution and
    /// commit. Already-revoked rows remain idempotent successes.
    pub async fn revoke_outgoing(
        &self,
        id: WebhookId,
        actor: ParticipantId,
    ) -> Result<RoomId, WebhookWriteError> {
        let mut tx = self.pool.begin().await?;

        let resolved = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid)>(
            r"SELECT hook.room_id, room.workspace_id
                FROM outgoing_webhooks hook
                JOIN rooms room ON room.id = hook.room_id
               WHERE hook.id = $1",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(WebhookWriteError::NotFound)?;
        let room = RoomId::from_uuid(resolved.0);
        lock_actor_room_access(&mut tx, room, actor, Some(resolved.1)).await?;

        let locked_room = sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT room_id
               FROM outgoing_webhooks
              WHERE id = $1
              FOR UPDATE",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(WebhookWriteError::NotFound)?;
        if locked_room != resolved.0 {
            return Err(WebhookWriteError::NotFound);
        }

        sqlx::query(
            r"UPDATE outgoing_webhooks
                 SET revoked_at = COALESCE(revoked_at, now())
               WHERE id = $1",
        )
        .bind(id.to_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(room)
    }
}
