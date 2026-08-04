//! Commit-time authorization for user-authored message mutations.

use aero_common::{Block, Error, MessageId, ParticipantId, RoomId, WorkspaceId};
use sqlx::{Postgres, Transaction};

use super::events::{OutboxedMessageDelete, OutboxedMessageEdit};
use super::MessageRepo;

#[derive(Debug, Clone, Copy)]
pub(crate) enum PostPolicy {
    Enforce,
    Ignore,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LockedRoomWriteAccess {
    pub workspace: WorkspaceId,
}

/// Return whether `actor` is separated from any other current room member by an
/// information barrier.
///
/// The caller already owns the workspace `FOR SHARE` aggregate fence acquired by
/// `aero_effective_room_access`, plus the room `FOR SHARE` lock. Every production
/// information-barrier and user-group membership writer takes the conflicting
/// workspace `FOR UPDATE` lock first, so this one `EXISTS` query observes either
/// the complete policy change or the state before it. The query does not
/// materialize or loop over room members in application memory.
async fn has_barred_room_recipient(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    room: RoomId,
    actor: ParticipantId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r"SELECT EXISTS (
              SELECT 1
                FROM info_barriers barrier
               WHERE barrier.workspace_id = $1
                 AND (
                       (
                           EXISTS (
                               SELECT 1
                                 FROM user_group_members actor_side
                                WHERE actor_side.group_id = barrier.group_a
                                  AND actor_side.participant_id = $3
                           )
                           AND EXISTS (
                               SELECT 1
                                 FROM user_group_members recipient_side
                                 JOIN room_members recipient
                                   ON recipient.room_id = $2
                                  AND recipient.participant_id =
                                      recipient_side.participant_id
                                WHERE recipient_side.group_id = barrier.group_b
                                  AND recipient.participant_id <> $3
                           )
                       )
                    OR (
                           EXISTS (
                               SELECT 1
                                 FROM user_group_members actor_side
                                WHERE actor_side.group_id = barrier.group_b
                                  AND actor_side.participant_id = $3
                           )
                           AND EXISTS (
                               SELECT 1
                                 FROM user_group_members recipient_side
                                 JOIN room_members recipient
                                   ON recipient.room_id = $2
                                  AND recipient.participant_id =
                                      recipient_side.participant_id
                                WHERE recipient_side.group_id = barrier.group_a
                                  AND recipient.participant_id <> $3
                           )
                       )
                 )
          )",
    )
    .bind(workspace.to_uuid())
    .bind(room.to_uuid())
    .bind(actor.to_uuid())
    .fetch_one(&mut **tx)
    .await
}

/// Fence effective room access and, when requested, announcement posting policy.
///
/// Migration 0197's database helper owns the canonical workspace → room →
/// membership lock order. The room and workspace-member rows it locks make the
/// following `post_policy` / role read stable until commit. The workspace
/// aggregate fence also linearizes the send-time information-barrier check with
/// barrier CRUD and user-group membership changes, so a barrier added after a DM
/// was created still stops new messages.
pub(crate) async fn lock_effective_message_write_access(
    tx: &mut Transaction<'_, Postgres>,
    room: RoomId,
    actor: ParticipantId,
    post_policy: PostPolicy,
) -> Result<Option<LockedRoomWriteAccess>, sqlx::Error> {
    let effective: bool = sqlx::query_scalar("SELECT aero_effective_room_access($1, $2, NULL)")
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .fetch_one(&mut **tx)
        .await?;
    if !effective {
        return Ok(None);
    }

    let state = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, String)>(
        "SELECT workspace_id, created_by, post_policy
           FROM rooms
          WHERE id = $1
          FOR SHARE",
    )
    .bind(room.to_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    let Some((workspace, created_by, policy)) = state else {
        return Ok(None);
    };

    if matches!(post_policy, PostPolicy::Enforce)
        && policy == "admins"
        && created_by != actor.to_uuid()
    {
        let role = sqlx::query_scalar::<_, String>(
            "SELECT role
               FROM workspace_members
              WHERE workspace_id = $1 AND participant_id = $2
              FOR UPDATE",
        )
        .bind(workspace)
        .bind(actor.to_uuid())
        .fetch_optional(&mut **tx)
        .await?;
        if !matches!(role.as_deref(), Some("owner" | "admin")) {
            return Ok(None);
        }
    }

    let workspace = WorkspaceId::from_uuid(workspace);
    if matches!(post_policy, PostPolicy::Enforce)
        && has_barred_room_recipient(tx, workspace, room, actor).await?
    {
        return Ok(None);
    }

    Ok(Some(LockedRoomWriteAccess { workspace }))
}

async fn resolve_message_target(
    tx: &mut Transaction<'_, Postgres>,
    id: MessageId,
) -> Result<Option<(RoomId, ParticipantId)>, sqlx::Error> {
    let target = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid)>(
        "SELECT room_id, sender_id
           FROM messages
          WHERE id = $1",
    )
    .bind(id.to_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    Ok(target.map(|(room, sender)| (RoomId::from_uuid(room), ParticipantId::from_uuid(sender))))
}

impl MessageRepo {
    /// User-authorized edit path.
    ///
    /// The message target is first resolved without a row lock. Authorization
    /// then follows workspace → room → membership, and only afterward locks and
    /// revalidates the message's immutable room/sender identity and version.
    pub async fn edit_outboxed_authorized(
        &self,
        id: MessageId,
        actor: ParticipantId,
        blocks: Vec<Block>,
        expected_version: i32,
        record_history: bool,
        traceparent: Option<&str>,
    ) -> aero_common::Result<Option<OutboxedMessageEdit>> {
        let mut tx = self.pool.begin().await?;
        let Some((resolved_room, resolved_sender)) = resolve_message_target(&mut tx, id).await?
        else {
            return Ok(None);
        };
        if lock_effective_message_write_access(&mut tx, resolved_room, actor, PostPolicy::Enforce)
            .await?
            .is_none()
        {
            return Err(Error::Forbidden(
                "message edit authority was revoked before commit".into(),
            ));
        }

        let Some(existing) = Self::lock_message_in_tx(&mut tx, id).await? else {
            return Ok(None);
        };
        if existing.room_id != resolved_room || existing.sender_id != resolved_sender {
            return Err(Error::Conflict(
                "message identity changed concurrently; reload and retry".into(),
            ));
        }
        if existing.sender_id != actor {
            return Err(Error::Forbidden("only sender may edit".into()));
        }

        let edited = Self::edit_locked_outboxed_in_tx(
            &mut tx,
            existing,
            blocks,
            expected_version,
            record_history.then_some(actor),
            traceparent,
        )
        .await?;
        if edited.is_some() {
            tx.commit().await?;
        }
        Ok(edited)
    }

    /// User-authorized delete path.
    ///
    /// Announcement posting policy is intentionally irrelevant to deletion, but
    /// effective room access and sender ownership remain fenced until the
    /// tombstone, audit row, blob cleanup, and outbox event all commit.
    pub async fn soft_delete_outboxed_authorized(
        &self,
        id: MessageId,
        actor: ParticipantId,
        traceparent: Option<&str>,
    ) -> aero_common::Result<Option<OutboxedMessageDelete>> {
        let mut tx = self.pool.begin().await?;
        let Some((resolved_room, resolved_sender)) = resolve_message_target(&mut tx, id).await?
        else {
            return Ok(None);
        };
        let Some(access) =
            lock_effective_message_write_access(&mut tx, resolved_room, actor, PostPolicy::Ignore)
                .await?
        else {
            return Err(Error::Forbidden(
                "message delete authority was revoked before commit".into(),
            ));
        };

        let Some(existing) = Self::lock_message_in_tx(&mut tx, id).await? else {
            return Ok(None);
        };
        if existing.room_id != resolved_room || existing.sender_id != resolved_sender {
            return Err(Error::Conflict(
                "message identity changed concurrently; reload and retry".into(),
            ));
        }
        if existing.sender_id != actor {
            return Err(Error::Forbidden("only sender may delete".into()));
        }
        if existing.deleted_at.is_some() {
            return Ok(None);
        }

        let digest: String = existing.searchable_text().chars().take(120).collect();
        let detail = serde_json::json!({
            "room_id": existing.room_id,
            "digest": digest,
        });
        let deleted = Self::soft_delete_locked_outboxed_in_tx(
            &mut tx,
            existing,
            Some(access.workspace),
            Some(actor),
            Some("message.deleted"),
            detail,
            actor,
            traceparent,
        )
        .await?;
        if deleted.is_some() {
            tx.commit().await?;
        }
        Ok(deleted)
    }
}
