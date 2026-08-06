//! Commit-time authorization for user-authored message mutations.

use aero_common::{
    Block, Error, Message, MessageEditId, MessageId, ParticipantId, RoomEvent, RoomId, WorkspaceId,
    RECALLED_MESSAGE_PLACEHOLDER,
};
use sqlx::{Postgres, Transaction};

use super::events::{OutboxedMessageDelete, OutboxedMessageEdit, OutboxedMessageRecall};
use super::MessageRepo;
use crate::event_outbox::{EventOutboxKind, EventOutboxRepo};

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

/// Whether `actor` may recall a message in `room`: the author, or a room
/// owner/admin. Re-checked under the caller's room write fence with a row lock
/// on the membership edge, so a concurrent role change cannot slip through the
/// authorization (TOCTOU guard — the preflight read is never authority).
async fn recall_role_allowed_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    room: RoomId,
    actor: ParticipantId,
    sender: ParticipantId,
) -> Result<bool, sqlx::Error> {
    if actor == sender {
        return Ok(true);
    }
    let role = sqlx::query_scalar::<_, String>(
        "SELECT role FROM room_members WHERE room_id = $1 AND participant_id = $2 FOR UPDATE",
    )
    .bind(room.to_uuid())
    .bind(actor.to_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    Ok(matches!(role.as_deref(), Some("owner" | "admin")))
}

impl MessageRepo {
    /// User-authorized recall path (撤回).
    ///
    /// Recall replaces a live message's content with the system placeholder
    /// while keeping the row (`message_id` / room history / audit). Authorization
    /// mirrors the delete path: effective room access is fenced first, then the
    /// message row is locked and its immutable identity re-validated. Unlike
    /// delete, recall is permitted for the sender OR a room owner/admin — the
    /// room role is re-checked under the lock so a concurrent demotion cannot
    /// slip through. Already-recalled and already-deleted messages are rejected
    /// with `Conflict` (recall is a one-shot state transition, deliberately NOT
    /// a silent idempotent no-op like the tombstone path).
    pub async fn recall_outboxed_authorized(
        &self,
        id: MessageId,
        actor: ParticipantId,
        traceparent: Option<&str>,
    ) -> aero_common::Result<Option<OutboxedMessageRecall>> {
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
                "message recall authority was revoked before commit".into(),
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
        if !recall_role_allowed_in_tx(&mut tx, resolved_room, actor, existing.sender_id).await? {
            return Err(Error::Forbidden(
                "only author or room admin may recall".into(),
            ));
        }
        if existing.deleted_at.is_some() {
            return Err(Error::Conflict("message is deleted".into()));
        }
        if existing.recalled_at.is_some() {
            return Err(Error::Conflict("message is already recalled".into()));
        }

        let recalled = Self::recall_locked_outboxed_in_tx(
            &mut tx,
            existing,
            Some(access.workspace),
            actor,
            traceparent,
        )
        .await?;
        if recalled.is_some() {
            tx.commit().await?;
        }
        Ok(recalled)
    }

    /// Transaction-scoped body of the recall: snapshot the original blocks into
    /// `message_edits`, replace `blocks` with the system placeholder, clear
    /// search/embedding, record `recalled_at`/`recalled_by`, bump the version,
    /// enqueue now-unreferenced attachment blobs for GC, append the
    /// `message.recalled` audit row, and append the `Recalled` room event — all
    /// in the caller's transaction. Returns `None` without writing when the
    /// message is missing or already in a terminal state (the caller's row lock
    /// makes this unreachable in practice; the `WHERE` clause is the final
    /// fence).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn recall_locked_outboxed_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        existing: Message,
        workspace: Option<WorkspaceId>,
        actor: ParticipantId,
        traceparent: Option<&str>,
    ) -> Result<Option<OutboxedMessageRecall>, sqlx::Error> {
        let id = existing.id;
        let digest: String = existing.searchable_text().chars().take(120).collect();
        let prior_blocks =
            serde_json::to_value(&existing.blocks).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        // GC targets: the ORIGINAL attachment blobs (bytes are content —
        // removed by the recall). The snapshot below is redacted so history
        // can never reference the destroyed bytes.
        let blob_ids = super::attached_blob_ids(&prior_blocks);
        // Gate round-3 B1: the history snapshot carries NO byte references
        // (`message_edits` is invisible to the blob-GC live-reference scan;
        // text/transcript evidence is preserved, attachment bytes are not).
        let snapshot_blocks = super::redact_blocks_for_recall_snapshot(&existing.blocks);
        let snapshot = serde_json::to_value(&snapshot_blocks)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        // Snapshot the original content before replacing it: the pre-recall body
        // stays reviewable through the message edit-history route (evidence, not
        // a loss). `editor_id` records WHO recalled.
        let history_id = MessageEditId::new();
        sqlx::query(
            r"INSERT INTO message_edits (id, message_id, editor_id, blocks)
              VALUES ($1, $2, $3, $4)",
        )
        .bind(history_id.to_uuid())
        .bind(id.to_uuid())
        .bind(actor.to_uuid())
        .bind(&snapshot)
        .execute(&mut **tx)
        .await?;

        let placeholder = serde_json::to_value(vec![Block::text(RECALLED_MESSAGE_PLACEHOLDER)])
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let recalled_at = time::OffsetDateTime::now_utc();
        let row = sqlx::query_as::<_, super::MessageRow>(
            r"UPDATE messages
                  SET blocks = $1,
                      searchable_text = '',
                      embedding = NULL,
                      recalled_at = $2,
                      recalled_by = $3,
                      version = version + 1
                WHERE id = $4
                  AND recalled_at IS NULL
                  AND deleted_at IS NULL
            RETURNING id, room_id, sender_id, blocks, reply_to, metadata,
                      created_at, edited_at, deleted_at, recalled_at, recalled_by,
                      expires_at, version",
        )
        .bind(&placeholder)
        .bind(recalled_at)
        .bind(actor.to_uuid())
        .bind(id.to_uuid())
        .fetch_optional(&mut **tx)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let message = Message::from(row);

        // Attachment bytes are content: once the placeholder replaces the only
        // live reference, they become garbage (unless another live message still
        // references the same deduped blob).
        Self::enqueue_unreferenced_blobs_in_tx(tx, &blob_ids).await?;

        if let Some(workspace) = workspace {
            crate::audit::AuditRepo::append_in_tx(
                tx,
                workspace,
                Some(actor),
                "message.recalled",
                Some(&id.to_string()),
                serde_json::json!({
                    "room_id": existing.room_id,
                    "digest": digest,
                }),
            )
            .await?;
        }

        let room_id = existing.room_id;
        let outbox_id = EventOutboxRepo::insert_room_event_in_tx(
            tx,
            id,
            room_id,
            EventOutboxKind::Recalled,
            &RoomEvent::Recalled(message.clone()),
            traceparent.map(str::to_owned),
            None,
        )
        .await?;
        Ok(Some(OutboxedMessageRecall { message, outbox_id }))
    }
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
