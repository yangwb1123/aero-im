//! Transactional message mutations and their durable room events.

use aero_common::{
    Block, Error, Message, MessageEditId, MessageId, ParticipantId, RoomEvent, RoomId, WorkspaceId,
};
use sqlx::{Postgres, Transaction};

use super::{searchable_of, MessageRepo, MessageRow};
use crate::blob::BlobRepo;
use crate::event_outbox::{EventOutboxKind, EventOutboxRepo};
use crate::message_side_effect::{MessageSideEffectKind, MessageSideEffectRepo};

/// A committed edit and the outbox row appended in the same transaction.
#[derive(Debug, Clone)]
pub struct OutboxedMessageEdit {
    pub message: Message,
    pub outbox_id: uuid::Uuid,
}

/// A committed delete and the outbox row appended in the same transaction.
#[derive(Debug, Clone, Copy)]
pub struct OutboxedMessageDelete {
    pub room_id: RoomId,
    pub outbox_id: uuid::Uuid,
}

/// A committed recall (撤回) and the outbox row appended in the same
/// transaction. The message carries the system placeholder blocks plus
/// `recalled_at`/`recalled_by`.
#[derive(Debug, Clone)]
pub struct OutboxedMessageRecall {
    pub message: Message,
    pub outbox_id: uuid::Uuid,
}

impl MessageRepo {
    /// Apply a trusted system edit and append `RoomEvent::Edited` atomically.
    ///
    /// This deliberately bypasses user membership, authorship, and channel
    /// posting policy. HTTP/user entry points must call
    /// [`Self::edit_outboxed_authorized`] instead.
    pub async fn edit_outboxed_system(
        &self,
        id: MessageId,
        blocks: Vec<Block>,
        expected_version: i32,
        history_actor: Option<ParticipantId>,
        traceparent: Option<&str>,
    ) -> Result<Option<OutboxedMessageEdit>, Error> {
        let mut tx = self.pool.begin().await?;
        let Some(existing) = Self::lock_message_in_tx(&mut tx, id).await? else {
            return Ok(None);
        };
        let edited = Self::edit_locked_outboxed_in_tx(
            &mut tx,
            existing,
            blocks,
            expected_version,
            history_actor,
            traceparent,
        )
        .await?;
        if edited.is_some() {
            tx.commit().await?;
        }
        Ok(edited)
    }

    pub(super) async fn edit_locked_outboxed_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        existing: Message,
        blocks: Vec<Block>,
        expected_version: i32,
        history_actor: Option<ParticipantId>,
        traceparent: Option<&str>,
    ) -> Result<Option<OutboxedMessageEdit>, Error> {
        let id = existing.id;
        let blocks_json = serde_json::to_value(&blocks)?;
        let searchable = searchable_of(&blocks);
        let edited_at = time::OffsetDateTime::now_utc();
        if existing.deleted_at.is_some() {
            return Ok(None);
        }
        // A recalled message is terminal for content: its body is the system
        // placeholder. System editors (unfurl/transcribe) must NOT resurrect the
        // original blocks post-recall — fence here, not only in the user path.
        if existing.recalled_at.is_some() {
            return Ok(None);
        }
        if existing.version != expected_version {
            return Err(Error::Conflict(
                "message was edited concurrently; reload and retry".into(),
            ));
        }
        let attachment_viewer = history_actor.unwrap_or(existing.sender_id);
        if !BlobRepo::lock_message_attachments_in_tx(
            tx,
            &blocks,
            attachment_viewer,
            existing.room_id,
        )
        .await?
        {
            return Err(Error::Forbidden(
                "message contains an unavailable attachment".into(),
            ));
        }

        if let Some(actor) = history_actor {
            let history_id = MessageEditId::new();
            sqlx::query(
                r"INSERT INTO message_edits (id, message_id, editor_id, blocks)
                  VALUES ($1, $2, $3, $4)",
            )
            .bind(history_id.to_uuid())
            .bind(id.to_uuid())
            .bind(actor.to_uuid())
            .bind(serde_json::to_value(&existing.blocks)?)
            .execute(&mut **tx)
            .await?;
        }

        let row = sqlx::query_as::<_, MessageRow>(
            r"UPDATE messages
                  SET blocks = $1,
                      searchable_text = $2,
                      edited_at = $3,
                      embedding = NULL,
                      version = version + 1
                WHERE id = $4
                  AND deleted_at IS NULL
                  AND version = $5
            RETURNING id, room_id, sender_id, blocks, reply_to, metadata,
                      created_at, edited_at, deleted_at, recalled_at, recalled_by, expires_at, version",
        )
        .bind(blocks_json)
        .bind(searchable)
        .bind(edited_at)
        .bind(id.to_uuid())
        .bind(expected_version)
        .fetch_optional(&mut **tx)
        .await?;
        let Some(row) = row else {
            return Err(Error::Conflict(
                "message edit raced with another mutation".into(),
            ));
        };
        let message = Message::from(row);
        let outbox_id = EventOutboxRepo::insert_room_event_in_tx(
            tx,
            id,
            message.room_id,
            EventOutboxKind::Edited,
            &RoomEvent::Edited(message.clone()),
            traceparent.map(str::to_owned),
            None,
        )
        .await?;
        MessageSideEffectRepo::insert_in_tx(
            tx,
            id,
            message.version,
            &[
                MessageSideEffectKind::Embed,
                MessageSideEffectKind::Moderate,
            ],
        )
        .await?;
        Ok(Some(OutboxedMessageEdit { message, outbox_id }))
    }

    /// Apply a system-generated voice transcript and append the resulting
    /// `Edited` event atomically. Returns `None` when the message disappeared,
    /// was deleted, or no untranslated voice block remained.
    pub async fn update_voice_transcript_outboxed(
        &self,
        id: MessageId,
        transcript: &str,
        traceparent: Option<&str>,
    ) -> Result<Option<OutboxedMessageEdit>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let Some(mut message) = Self::lock_message_in_tx(&mut tx, id).await? else {
            tx.rollback().await?;
            return Ok(None);
        };
        if message.deleted_at.is_some() {
            tx.rollback().await?;
            return Ok(None);
        }
        // Recalled messages carry the placeholder body (no Voice block): skip
        // them explicitly so a late transcript can never rewrite recall state.
        if message.recalled_at.is_some() {
            tx.rollback().await?;
            return Ok(None);
        }
        let mut changed = false;
        for block in &mut message.blocks {
            if let Block::Voice {
                transcript: current,
                ..
            } = block
            {
                if current.is_none() {
                    *current = Some(transcript.to_owned());
                    changed = true;
                }
            }
        }
        if !changed {
            tx.rollback().await?;
            return Ok(None);
        }

        let blocks_json = serde_json::to_value(&message.blocks)
            .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        let searchable = searchable_of(&message.blocks);
        let row = sqlx::query_as::<_, MessageRow>(
            r"UPDATE messages
                  SET blocks = $1,
                      searchable_text = $2,
                      edited_at = NOW(),
                      embedding = NULL,
                      version = version + 1
                WHERE id = $3 AND deleted_at IS NULL
            RETURNING id, room_id, sender_id, blocks, reply_to, metadata,
                      created_at, edited_at, deleted_at, recalled_at, recalled_by, expires_at, version",
        )
        .bind(blocks_json)
        .bind(searchable)
        .bind(id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Ok(None);
        };
        let message = Message::from(row);
        let outbox_id = EventOutboxRepo::insert_room_event_in_tx(
            &mut tx,
            id,
            message.room_id,
            EventOutboxKind::Edited,
            &RoomEvent::Edited(message.clone()),
            traceparent.map(str::to_owned),
            None,
        )
        .await?;
        MessageSideEffectRepo::insert_in_tx(
            &mut tx,
            id,
            message.version,
            &[
                MessageSideEffectKind::Embed,
                MessageSideEffectKind::Moderate,
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(Some(OutboxedMessageEdit { message, outbox_id }))
    }

    /// Apply a trusted system soft-delete and append its audit/outbox effects.
    ///
    /// This deliberately bypasses user membership and authorship. HTTP/user
    /// entry points must call [`Self::soft_delete_outboxed_authorized`].
    #[allow(clippy::too_many_arguments)]
    pub async fn soft_delete_outboxed_system(
        &self,
        id: MessageId,
        workspace: Option<WorkspaceId>,
        audit_actor: Option<ParticipantId>,
        audit_action: Option<&str>,
        detail: serde_json::Value,
        event_actor: ParticipantId,
        traceparent: Option<&str>,
    ) -> Result<Option<OutboxedMessageDelete>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let Some(existing) = Self::lock_message_in_tx(&mut tx, id).await? else {
            return Ok(None);
        };
        let deleted = Self::soft_delete_locked_outboxed_in_tx(
            &mut tx,
            existing,
            workspace,
            audit_actor,
            audit_action,
            detail,
            event_actor,
            traceparent,
        )
        .await?;
        if deleted.is_some() {
            tx.commit().await?;
        }
        Ok(deleted)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn soft_delete_locked_outboxed_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        existing: Message,
        workspace: Option<WorkspaceId>,
        audit_actor: Option<ParticipantId>,
        audit_action: Option<&str>,
        detail: serde_json::Value,
        event_actor: ParticipantId,
        traceparent: Option<&str>,
    ) -> Result<Option<OutboxedMessageDelete>, sqlx::Error> {
        let id = existing.id;
        if existing.deleted_at.is_some() {
            return Ok(None);
        }

        let blob_ids = super::attached_blob_ids(
            &serde_json::to_value(&existing.blocks)
                .map_err(|error| sqlx::Error::Encode(Box::new(error)))?,
        );
        let updated = sqlx::query(
            r"UPDATE messages
                  SET deleted_at = NOW(),
                      blocks = '[]'::jsonb,
                      searchable_text = '',
                      embedding = NULL,
                      version = version + 1
                WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id.to_uuid())
        .execute(&mut **tx)
        .await?;
        if updated.rows_affected() == 0 {
            return Ok(None);
        }
        Self::cleanup_visible_associations_in_tx(tx, &[id.to_uuid()]).await?;
        Self::enqueue_unreferenced_blobs_in_tx(tx, &blob_ids).await?;

        if let (Some(workspace), Some(action)) = (workspace, audit_action) {
            crate::audit::AuditRepo::append_in_tx(
                tx,
                workspace,
                audit_actor,
                action,
                Some(&id.to_string()),
                detail,
            )
            .await?;
        }

        let room_id = existing.room_id;
        let event = RoomEvent::Deleted {
            room_id,
            message_id: id,
            by: event_actor,
        };
        let outbox_id = EventOutboxRepo::insert_room_event_in_tx(
            tx,
            id,
            room_id,
            EventOutboxKind::Deleted,
            &event,
            traceparent.map(str::to_owned),
            None,
        )
        .await?;
        Ok(Some(OutboxedMessageDelete { room_id, outbox_id }))
    }

    pub(crate) async fn append_deleted_event_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        id: MessageId,
        room_id: RoomId,
    ) -> Result<uuid::Uuid, sqlx::Error> {
        EventOutboxRepo::insert_room_event_in_tx(
            tx,
            id,
            room_id,
            EventOutboxKind::Deleted,
            &RoomEvent::Deleted {
                room_id,
                message_id: id,
                by: ParticipantId::nil(),
            },
            None,
            None,
        )
        .await
    }

    pub(crate) async fn lock_message_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        id: MessageId,
    ) -> Result<Option<Message>, sqlx::Error> {
        let row = sqlx::query_as::<_, MessageRow>(
            r"SELECT id, room_id, sender_id, blocks, reply_to, metadata,
                     created_at, edited_at, deleted_at, recalled_at, recalled_by, expires_at, version
                FROM messages
               WHERE id = $1
               FOR UPDATE",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut **tx)
        .await?;
        Ok(row.map(Message::from))
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::message::{MessageIdempotency, NewMessage};
    use crate::{EventOutboxRepo, ParticipantRepo, RoomRepo, WorkspaceRepo};
    use aero_common::{RoomKind, WorkspaceId, WorkspaceRole};
    use time::{Duration, OffsetDateTime};

    fn pool() -> sqlx::PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_lazy(&url)
            .expect("valid DATABASE_URL")
    }

    async fn fixture(
        pool: &sqlx::PgPool,
    ) -> (
        aero_common::Participant,
        aero_common::Room,
        MessageRepo,
        MessageId,
    ) {
        let unique = uuid::Uuid::new_v4();
        let participant = ParticipantRepo::new(pool.clone())
            .create_human(crate::participant::NewHuman {
                email: format!("aggregate-events-{unique}@example.test"),
                display_name: format!("aggregate-events-{unique}"),
                password_hash: "test".into(),
            })
            .await
            .unwrap();
        WorkspaceRepo::new(pool.clone())
            .add_member(
                WorkspaceId::from_uuid(uuid::Uuid::nil()),
                participant.id,
                WorkspaceRole::Member,
            )
            .await
            .unwrap();
        let room = RoomRepo::new(pool.clone())
            .create(
                RoomKind::Group,
                Some("aggregate-events".into()),
                participant.id,
            )
            .await
            .unwrap();
        let repo = MessageRepo::new(pool.clone());
        let inserted = repo
            .insert_outboxed(
                NewMessage {
                    room_id: room.id,
                    sender_id: participant.id,
                    blocks: vec![Block::text("v1-private")],
                    reply_to: None,
                    metadata: serde_json::Value::Null,
                    expires_at: None,
                },
                Some(MessageIdempotency::new(uuid::Uuid::new_v4(), [7; 32])),
                vec![participant.id],
                None,
            )
            .await
            .unwrap();
        let message_id = inserted.message().id;
        (participant, room, repo, message_id)
    }

    async fn cleanup(
        pool: &sqlx::PgPool,
        participant: ParticipantId,
        room: RoomId,
        message: MessageId,
    ) {
        sqlx::query("DELETE FROM message_side_effect_jobs WHERE message_id = $1")
            .bind(message.to_uuid())
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM event_outbox WHERE message_id = $1")
            .bind(message.to_uuid())
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM message_send_keys WHERE message_id = $1")
            .bind(message.to_uuid())
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(message.to_uuid())
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM room_members WHERE room_id = $1")
            .bind(room.to_uuid())
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(participant.to_uuid())
            .execute(pool)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migrations through 0163"]
    async fn create_edit_delete_append_and_claim_in_strict_aggregate_order() {
        let pool = pool();
        let (participant, room, messages, message_id) = fixture(&pool).await;
        let edited = messages
            .edit_outboxed_authorized(
                message_id,
                participant.id,
                vec![Block::text("v2-current")],
                1,
                true,
                None,
            )
            .await
            .unwrap()
            .expect("edit");
        messages
            .soft_delete_outboxed_authorized(message_id, participant.id, None)
            .await
            .unwrap()
            .expect("delete");

        let rows: Vec<(String, i64, serde_json::Value)> = sqlx::query_as(
            r"SELECT event_kind, aggregate_version, payload
                FROM event_outbox
               WHERE message_id = $1
               ORDER BY aggregate_version",
        )
        .bind(message_id.to_uuid())
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows.iter()
                .map(|(kind, _, _)| kind.as_str())
                .collect::<Vec<_>>(),
            ["message", "edited", "deleted"]
        );
        assert_eq!(
            rows.iter()
                .map(|(_, version, _)| *version)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(edited.message.version, 2);

        let outbox = EventOutboxRepo::new(pool.clone());
        let now = OffsetDateTime::now_utc();
        let lease = Duration::seconds(30);
        let first_row = outbox
            .for_message(message_id)
            .await
            .unwrap()
            .expect("create outbox row");
        assert_eq!(first_row.aggregate_version, 1);
        let second_id = sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT id FROM event_outbox WHERE message_id = $1 AND aggregate_version = 2",
        )
        .bind(message_id.to_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            outbox
                .claim_by_id(second_id, now, lease)
                .await
                .unwrap()
                .is_none(),
            "a later version cannot be claimed before the create event"
        );
        let ours = outbox
            .claim_by_id(first_row.id, now, lease)
            .await
            .unwrap()
            .expect("create is claimable");
        assert_eq!(ours.aggregate_version, 1);
        outbox
            .mark_published(ours.id, ours.attempts, now)
            .await
            .unwrap();
        let second = outbox
            .claim_by_id(second_id, now, lease)
            .await
            .unwrap()
            .expect("edit is claimable after create is published");
        assert_eq!(second.message_id, message_id);
        assert_eq!(second.aggregate_version, 2);

        cleanup(&pool, participant.id, room.id, message_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migrations through 0163"]
    async fn audit_failure_rolls_back_delete_and_outbox_append() {
        let pool = pool();
        let (participant, room, messages, message_id) = fixture(&pool).await;
        let error = messages
            .soft_delete_outboxed_system(
                message_id,
                Some(WorkspaceId::new()),
                Some(participant.id),
                Some("message.deleted"),
                serde_json::json!({}),
                participant.id,
                None,
            )
            .await
            .expect_err("unknown workspace makes audit insert fail");
        assert!(matches!(error, sqlx::Error::Database(_)));
        let current = messages
            .get(message_id)
            .await
            .unwrap()
            .expect("message retained");
        assert!(current.deleted_at.is_none());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM event_outbox WHERE message_id = $1",)
                .bind(message_id.to_uuid())
                .fetch_one(&pool)
                .await
                .unwrap(),
            1,
            "failed mutation appended no delete event"
        );
        cleanup(&pool, participant.id, room.id, message_id).await;
    }
}
