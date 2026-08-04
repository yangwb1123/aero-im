use aero_common::{ParticipantId, RoomId, WebhookId};

use super::management::lock_actor_room_access;
use super::{WebhookRepo, WebhookWriteError};

/// IDs created by one atomic incoming-webhook registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncomingWebhookIds {
    pub bot_id: ParticipantId,
    pub webhook_id: WebhookId,
}

impl WebhookRepo {
    /// Atomically create the dedicated bot, workspace/room memberships, and
    /// incoming webhook while storing only `token_hash`.
    ///
    /// The room-kind check mirrors [`crate::RoomRepo::add_member`]: direct and
    /// marker group-DM membership cannot be extended through a webhook. Creator
    /// room/effective-workspace access is rechecked under the transaction before
    /// any write. A failure on any insert rolls back every preceding insert.
    pub async fn create_incoming(
        &self,
        room: RoomId,
        bot_name: &str,
        token_hash: &str,
        label: Option<&str>,
        created_by: ParticipantId,
    ) -> Result<IncomingWebhookIds, WebhookWriteError> {
        let bot_id = ParticipantId::new();
        let webhook_id = WebhookId::new();
        let created_at = time::OffsetDateTime::now_utc();
        let mut tx = self.pool.begin().await?;

        let locked_room = lock_actor_room_access(&mut tx, room, created_by, None).await?;
        if locked_room.kind == "direct" || locked_room.is_group_dm {
            return Err(WebhookWriteError::FixedMembership);
        }

        sqlx::query(
            r"INSERT INTO participants
                 (id, kind, display_name, avatar_url, created_by, created_at)
               VALUES ($1, 'bot', $2, NULL, $3, $4)",
        )
        .bind(bot_id.to_uuid())
        .bind(bot_name)
        .bind(created_by.to_uuid())
        .bind(created_at)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r"INSERT INTO workspace_members
                 (workspace_id, participant_id, role, joined_at)
               VALUES ($1, $2, 'member', $3)",
        )
        .bind(locked_room.workspace_id)
        .bind(bot_id.to_uuid())
        .bind(created_at)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r"INSERT INTO room_members
                 (room_id, participant_id, role, joined_at)
               VALUES ($1, $2, 'member', $3)",
        )
        .bind(room.to_uuid())
        .bind(bot_id.to_uuid())
        .bind(created_at)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r"INSERT INTO incoming_webhooks (id, room_id, bot_id, token_hash, label, created_by)
               VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(webhook_id.to_uuid())
        .bind(room.to_uuid())
        .bind(bot_id.to_uuid())
        .bind(token_hash)
        .bind(label)
        .bind(created_by.to_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(IncomingWebhookIds { bot_id, webhook_id })
    }

    /// Revoke an incoming hook and atomically remove its dedicated bot from the
    /// bound room and workspace. Returns the resolved `(room_id, bot_id)`, or
    /// `None` when the hook no longer exists.
    pub async fn revoke_incoming_and_cleanup(
        &self,
        id: WebhookId,
        actor: ParticipantId,
    ) -> Result<(RoomId, ParticipantId), WebhookWriteError> {
        let mut tx = self.pool.begin().await?;
        crate::ownership::lock_membership_governance(&mut tx).await?;

        // Resolve without locking the hook. Governance transactions lock
        // workspace -> rooms -> membership edges; taking the hook first would
        // invert that order and can deadlock a concurrent membership change.
        let row = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, uuid::Uuid)>(
            r"SELECT hook.room_id, hook.bot_id, room.workspace_id
                FROM incoming_webhooks hook
                JOIN rooms room ON room.id = hook.room_id
               WHERE hook.id = $1",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let (room_id, bot_id, workspace_id) = row.ok_or(WebhookWriteError::NotFound)?;
        let room = RoomId::from_uuid(room_id);
        lock_actor_room_access(&mut tx, room, actor, Some(workspace_id)).await?;

        // The workspace-membership guard may inspect every channel this bot
        // owns. Lock those rooms plus the hook's bound room in UUID order before
        // either membership edge, matching the guard's own lock order.
        let locked_rooms = sqlx::query_scalar::<_, uuid::Uuid>(
            r"SELECT room.id
                FROM rooms room
               WHERE room.workspace_id = $1
                 AND (
                     room.id = $2
                     OR EXISTS (
                         SELECT 1
                           FROM room_members membership
                          WHERE membership.room_id = room.id
                            AND membership.participant_id = $3
                            AND membership.role = 'owner'
                     )
               )
               ORDER BY room.id
               FOR UPDATE OF room",
        )
        .bind(workspace_id)
        .bind(room_id)
        .bind(bot_id)
        .fetch_all(&mut *tx)
        .await?;
        if !locked_rooms.contains(&room_id) {
            return Err(WebhookWriteError::NotFound);
        }

        sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT participant_id
               FROM workspace_members
              WHERE workspace_id = $1 AND participant_id = $2
              FOR UPDATE",
        )
        .bind(workspace_id)
        .bind(bot_id)
        .fetch_optional(&mut *tx)
        .await?;
        sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT participant_id
               FROM room_members
              WHERE room_id = $1 AND participant_id = $2
              FOR UPDATE",
        )
        .bind(room_id)
        .bind(bot_id)
        .fetch_optional(&mut *tx)
        .await?;

        // Lock the hook last and revalidate the unlocked resolution before any
        // delete. Hook identity is immutable in normal writes; a mismatch is an
        // internal integrity violation rather than permission to touch new rows.
        let locked_hook = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid)>(
            "SELECT room_id, bot_id
               FROM incoming_webhooks
              WHERE id = $1
              FOR UPDATE",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let (locked_room_id, locked_bot_id) = locked_hook.ok_or(WebhookWriteError::NotFound)?;
        if locked_room_id != room_id || locked_bot_id != bot_id {
            return Err(WebhookWriteError::NotFound);
        }

        sqlx::query(
            "DELETE FROM workspace_members
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace_id)
        .bind(bot_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM room_members
              WHERE room_id = $1 AND participant_id = $2",
        )
        .bind(room_id)
        .bind(bot_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE incoming_webhooks
                SET revoked_at = COALESCE(revoked_at, now())
              WHERE id = $1",
        )
        .bind(id.to_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((room, ParticipantId::from_uuid(bot_id)))
    }
}
