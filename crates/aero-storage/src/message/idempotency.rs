//! Sender-scoped client-message idempotency and atomic message outbox writes.

use aero_common::{Error, Message, MessageEnvelope, MessageId, ParticipantId, Result, RoomEvent};

use super::{
    MessageIdempotency, MessageInsertOutcome, MessageRepo, NewMessage, OutboxedMessageInsert,
};
use crate::blob::BlobRepo;
use crate::event_outbox::{EventOutboxKind, EventOutboxRepo, NewEventOutbox};
use crate::message_side_effect::{MessageSideEffectKind, MessageSideEffectRepo};

const MAX_TRACEPARENT_BYTES: usize = 512;

async fn lock_effective_sender_room_access(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    room: aero_common::RoomId,
    sender: ParticipantId,
) -> Result<bool> {
    Ok(super::authorization::lock_effective_message_write_access(
        tx,
        room,
        sender,
        super::authorization::PostPolicy::Enforce,
    )
    .await?
    .is_some())
}

impl MessageRepo {
    /// Resolve a previously-committed client key.
    ///
    /// `request_hash` binds the key to the original room/content/reply/TTL
    /// request. Reusing the same key for different content is a conflict rather
    /// than silently returning an unrelated message.
    pub async fn find_by_client_message_id(
        &self,
        sender: ParticipantId,
        client_message_id: uuid::Uuid,
        request_hash: &[u8; 32],
    ) -> Result<Option<Message>> {
        let row: Option<(uuid::Uuid, i16, Vec<u8>)> = sqlx::query_as(
            r"SELECT message_id, hash_version, request_hash
                FROM message_send_keys
               WHERE sender_id = $1 AND client_message_id = $2",
        )
        .bind(sender.to_uuid())
        .bind(client_message_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some((message_id, hash_version, stored_hash)) = row else {
            return Ok(None);
        };
        if hash_version != 1 {
            return Err(Error::Internal(anyhow::anyhow!(
                "unsupported message request hash version {hash_version}"
            )));
        }
        if stored_hash.as_slice() != request_hash {
            return Err(Error::Conflict(
                "client_message_id was already used for a different message".into(),
            ));
        }
        let message_id = MessageId::from_uuid(message_id);
        let Some(message) = self.get(message_id).await? else {
            return Err(Error::Conflict(format!(
                "canonical message for client_message_id {client_message_id} is no longer available"
            )));
        };
        if message
            .expires_at
            .is_some_and(|expires_at| expires_at <= time::OffsetDateTime::now_utc())
        {
            return Err(Error::Conflict(format!(
                "canonical message for client_message_id {client_message_id} has expired"
            )));
        }
        Ok(Some(message))
    }

    /// Insert a message and claim `(sender, client_message_id)` in one
    /// transaction. Concurrent duplicates race at the ledger primary key; the
    /// loser rolls its speculative message back and returns the winner's
    /// canonical row.
    pub async fn insert_idempotent(
        &self,
        new: NewMessage,
        client_message_id: uuid::Uuid,
        request_hash: [u8; 32],
    ) -> Result<MessageInsertOutcome> {
        if let Some(message) = self
            .find_by_client_message_id(new.sender_id, client_message_id, &request_hash)
            .await?
        {
            return Ok(MessageInsertOutcome::Existing(message));
        }

        let sender = new.sender_id;
        let room = new.room_id;
        let mut tx = self.pool.begin().await?;
        if !lock_effective_sender_room_access(&mut tx, room, sender).await? {
            tx.rollback().await?;
            return Err(Error::Forbidden(
                "room access or posting authority was revoked before the message could commit"
                    .into(),
            ));
        }
        if !Self::lock_reply_parent_in_tx(&mut tx, new.reply_to, room).await? {
            tx.rollback().await?;
            return Err(Error::Invalid(
                "reply_to must reference an existing message in the same room".into(),
            ));
        }
        if !BlobRepo::lock_message_attachments_in_tx(&mut tx, &new.blocks, sender, room).await? {
            tx.rollback().await?;
            return Err(Error::Forbidden(
                "message contains an unavailable attachment".into(),
            ));
        }
        let message = Self::insert_row_in_tx(&mut tx, new).await?;
        let claimed = sqlx::query(
            r"INSERT INTO message_send_keys
                 (sender_id, client_message_id, message_id, request_hash)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (sender_id, client_message_id) DO NOTHING",
        )
        .bind(sender.to_uuid())
        .bind(client_message_id)
        .bind(message.id.to_uuid())
        .bind(request_hash.as_slice())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if claimed {
            tx.commit().await?;
            return Ok(MessageInsertOutcome::Created(message));
        }
        tx.rollback().await?;

        self.find_by_client_message_id(sender, client_message_id, &request_hash)
            .await?
            .map(MessageInsertOutcome::Existing)
            .ok_or_else(|| {
                Error::Internal(anyhow::anyhow!(
                    "idempotency key conflict resolved without a canonical message"
                ))
            })
    }

    /// Atomically insert a message, its optional idempotency ledger row, and the
    /// corresponding unstamped `RoomEvent::Message` outbox row.
    ///
    /// The outbox subject is derived from the message room. The retained
    /// `_recipients` argument is ignored for compatibility: delayed fan-out
    /// always resolves current membership. A concurrent loser at
    /// `(sender, client_message_id)` explicitly rolls back its speculative
    /// message and outbox rows, then returns the winner's canonical pair.
    pub async fn insert_outboxed(
        &self,
        new: NewMessage,
        idempotency: Option<MessageIdempotency>,
        _recipients: Vec<ParticipantId>,
        traceparent: Option<&str>,
    ) -> Result<OutboxedMessageInsert> {
        let traceparent = normalize_traceparent(traceparent)?;
        if let Some(key) = idempotency {
            if let Some(existing) = self
                .find_outboxed_by_client_message_id(
                    new.sender_id,
                    key.client_message_id,
                    &key.request_hash,
                )
                .await?
            {
                return Ok(existing);
            }
        }

        let sender = new.sender_id;
        let room = new.room_id;
        let mut tx = self.pool.begin().await?;
        if !lock_effective_sender_room_access(&mut tx, room, sender).await? {
            tx.rollback().await?;
            return Err(Error::Forbidden(
                "room access or posting authority was revoked before the message could commit"
                    .into(),
            ));
        }
        if !Self::lock_reply_parent_in_tx(&mut tx, new.reply_to, room).await? {
            tx.rollback().await?;
            return Err(Error::Invalid(
                "reply_to must reference an existing message in the same room".into(),
            ));
        }
        if !BlobRepo::lock_message_attachments_in_tx(&mut tx, &new.blocks, sender, room).await? {
            tx.rollback().await?;
            return Err(Error::Forbidden(
                "message contains an unavailable attachment".into(),
            ));
        }
        let message = Self::insert_row_in_tx(&mut tx, new).await?;
        // Do not persist a send-time recipient snapshot. A delayed relay must
        // resolve the room's current members so somebody who left during a NATS
        // outage cannot receive the eventual event.
        let payload = message_event_payload(
            &message,
            idempotency.map(|key| key.client_message_id),
            Vec::new(),
        )?;
        let outbox_id = EventOutboxRepo::insert_in_tx(
            &mut tx,
            NewEventOutbox {
                message_id: message.id,
                event_kind: EventOutboxKind::Message,
                subject: format!("im.room.{}", message.room_id),
                payload,
                traceparent,
            },
        )
        .await?;
        MessageSideEffectRepo::insert_in_tx(
            &mut tx,
            message.id,
            message.version,
            &[
                MessageSideEffectKind::Notifications,
                MessageSideEffectKind::Embed,
                MessageSideEffectKind::Moderate,
            ],
        )
        .await?;

        let claimed = if let Some(key) = idempotency {
            sqlx::query(
                r"INSERT INTO message_send_keys
                     (sender_id, client_message_id, message_id, request_hash)
                   VALUES ($1, $2, $3, $4)
                   ON CONFLICT (sender_id, client_message_id) DO NOTHING",
            )
            .bind(sender.to_uuid())
            .bind(key.client_message_id)
            .bind(message.id.to_uuid())
            .bind(key.request_hash.as_slice())
            .execute(&mut *tx)
            .await?
            .rows_affected()
                > 0
        } else {
            true
        };

        if claimed {
            tx.commit().await?;
            return Ok(OutboxedMessageInsert {
                outcome: MessageInsertOutcome::Created(message),
                outbox_id,
            });
        }

        // The unique ledger key belongs to a concurrently committed winner.
        // Roll back both speculative rows before resolving that canonical pair.
        tx.rollback().await?;
        let key = idempotency.expect("claimed is false only for an idempotent insert");
        self.find_outboxed_by_client_message_id(sender, key.client_message_id, &key.request_hash)
            .await?
            .ok_or_else(|| {
                Error::Internal(anyhow::anyhow!(
                    "idempotency key conflict resolved without a canonical message outbox"
                ))
            })
    }

    async fn find_outboxed_by_client_message_id(
        &self,
        sender: ParticipantId,
        client_message_id: uuid::Uuid,
        request_hash: &[u8; 32],
    ) -> Result<Option<OutboxedMessageInsert>> {
        let Some(message) = self
            .find_by_client_message_id(sender, client_message_id, request_hash)
            .await?
        else {
            return Ok(None);
        };
        let outbox = EventOutboxRepo::new(self.pool.clone())
            .for_message(message.id)
            .await?
            .ok_or_else(|| {
                Error::Internal(anyhow::anyhow!(
                    "canonical message {} has no retained outbox row",
                    message.id
                ))
            })?;
        Ok(Some(OutboxedMessageInsert {
            outcome: MessageInsertOutcome::Existing(message),
            outbox_id: outbox.id,
        }))
    }

    /// Remove deduplication tombstones older than the caller's safety window.
    pub async fn sweep_send_keys_before(
        &self,
        cutoff: time::OffsetDateTime,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM message_send_keys WHERE created_at < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

fn message_event_payload(
    message: &Message,
    client_message_id: Option<uuid::Uuid>,
    recipients: Vec<ParticipantId>,
) -> serde_json::Result<serde_json::Value> {
    serde_json::to_value(RoomEvent::Message(MessageEnvelope {
        message: message.clone(),
        delivery_ordinal: None,
        client_message_id,
        recipients,
    }))
}

fn normalize_traceparent(traceparent: Option<&str>) -> Result<Option<String>> {
    let traceparent = traceparent.filter(|value| !value.is_empty());
    if traceparent.is_some_and(|value| value.len() > MAX_TRACEPARENT_BYTES) {
        return Err(Error::Invalid(format!(
            "traceparent exceeds {MAX_TRACEPARENT_BYTES} bytes"
        )));
    }
    Ok(traceparent.map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::{Block, RoomId};

    #[test]
    fn message_outbox_payload_starts_unstamped() {
        let client_message_id = uuid::Uuid::new_v4();
        let recipient = ParticipantId::new();
        let message = Message {
            id: MessageId::new(),
            room_id: RoomId::new(),
            sender_id: ParticipantId::new(),
            blocks: vec![Block::text("durable")],
            reply_to: None,
            metadata: serde_json::Value::Null,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            edited_at: None,
            deleted_at: None,
            recalled_at: None,
            recalled_by: None,
            expires_at: None,
            version: 1,
        };

        let payload =
            message_event_payload(&message, Some(client_message_id), vec![recipient]).unwrap();
        assert_eq!(
            payload.get("kind").and_then(serde_json::Value::as_str),
            Some("message")
        );
        assert!(payload.get("seq").is_none());
        assert!(payload.get("traceparent").is_none());

        let event: RoomEvent = serde_json::from_value(payload).unwrap();
        let RoomEvent::Message(envelope) = event else {
            panic!("expected message event");
        };
        assert_eq!(envelope.message.id, message.id);
        assert_eq!(envelope.client_message_id, Some(client_message_id));
        assert_eq!(envelope.recipients, vec![recipient]);
    }

    #[test]
    fn traceparent_validation_matches_schema_limit() {
        assert_eq!(normalize_traceparent(None).unwrap(), None);
        assert_eq!(normalize_traceparent(Some("")).unwrap(), None);
        assert_eq!(
            normalize_traceparent(Some("00-abcd")).unwrap().as_deref(),
            Some("00-abcd")
        );
        assert!(matches!(
            normalize_traceparent(Some(&"x".repeat(MAX_TRACEPARENT_BYTES + 1))),
            Err(Error::Invalid(_))
        ));
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::participant::NewHuman;
    use crate::{BlobRepo, NewBlob, ParticipantRepo, RoomRepo, WorkspaceRepo};
    use aero_common::{BlobId, Block, FileKind, RoomKind, WorkspaceId, WorkspaceRole};

    fn pool() -> sqlx::PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(16)
            .connect_lazy(&url)
            .expect("connect_lazy never fails for a well-formed URL")
    }

    async fn participant(pool: &sqlx::PgPool, label: &str) -> aero_common::Participant {
        let unique = uuid::Uuid::new_v4();
        ParticipantRepo::new(pool.clone())
            .create_human(NewHuman {
                email: format!("{label}-{unique}@example.test"),
                display_name: format!("{label}-{unique}"),
                password_hash: "test".into(),
            })
            .await
            .unwrap()
    }

    async fn enroll_default_workspace(pool: &sqlx::PgPool, participants: &[ParticipantId]) {
        let workspaces = WorkspaceRepo::new(pool.clone());
        let workspace = WorkspaceId::from_uuid(uuid::Uuid::nil());
        for participant in participants {
            workspaces
                .add_member(workspace, *participant, WorkspaceRole::Member)
                .await
                .unwrap();
        }
    }

    fn new_message(room_id: aero_common::RoomId, sender_id: ParticipantId) -> NewMessage {
        new_message_with_blocks(room_id, sender_id, vec![Block::text("retry-safe")])
    }

    fn new_message_with_blocks(
        room_id: aero_common::RoomId,
        sender_id: ParticipantId,
        blocks: Vec<Block>,
    ) -> NewMessage {
        NewMessage {
            room_id,
            sender_id,
            blocks,
            reply_to: None,
            metadata: serde_json::Value::Null,
            expires_at: None,
        }
    }

    fn file_block(blob_id: BlobId) -> Block {
        Block::File {
            blob_id,
            kind: FileKind::Document,
            name: "attachment.txt".into(),
            size: 12,
        }
    }

    async fn reserve_blob(
        pool: &sqlx::PgPool,
        owner: ParticipantId,
        workspace: WorkspaceId,
        label: &str,
    ) -> aero_common::Blob {
        BlobRepo::new(pool.clone())
            .reserve_in_scope(
                NewBlob {
                    owner_id: owner,
                    kind: FileKind::Document,
                    name: format!("{label}.txt"),
                    mime: "text/plain".into(),
                    size: 12,
                    sha256: None,
                    storage_key: format!("tests/{label}/{}", uuid::Uuid::new_v4()),
                },
                Some(workspace),
                Some(crate::DEFAULT_STORAGE_REGION),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migrations applied"]
    async fn outboxed_sender_key_commits_one_message_event_key_and_side_effect_set() {
        let pool = pool();
        let sender = participant(&pool, "outboxed-sender").await;
        enroll_default_workspace(&pool, &[sender.id]).await;
        let room = RoomRepo::new(pool.clone())
            .create(
                RoomKind::Group,
                Some("outboxed-idempotency".into()),
                sender.id,
            )
            .await
            .unwrap();
        let repo = MessageRepo::new(pool.clone());
        let key = uuid::Uuid::new_v4();
        let idempotency = MessageIdempotency::new(key, [11_u8; 32]);

        let attempts = (0..12).map(|_| {
            let repo = repo.clone();
            let new = new_message(room.id, sender.id);
            async move {
                repo.insert_outboxed(new, Some(idempotency), vec![sender.id], Some("00-test"))
                    .await
                    .unwrap()
            }
        });
        let outcomes = futures::future::join_all(attempts).await;
        assert_eq!(
            outcomes
                .iter()
                .filter(|result| matches!(result.outcome, MessageInsertOutcome::Created(_)))
                .count(),
            1
        );
        let canonical_message = outcomes[0].message().id;
        let canonical_outbox = outcomes[0].outbox_id;
        assert!(outcomes
            .iter()
            .all(|result| result.message().id == canonical_message));
        assert!(outcomes
            .iter()
            .all(|result| result.outbox_id == canonical_outbox));

        let message_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE room_id = $1")
                .bind(room.id.to_uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
        let key_count: i64 = sqlx::query_scalar(
            r"SELECT COUNT(*) FROM message_send_keys
               WHERE sender_id = $1 AND client_message_id = $2",
        )
        .bind(sender.id.to_uuid())
        .bind(key)
        .fetch_one(&pool)
        .await
        .unwrap();
        let outbox_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM event_outbox WHERE message_id = $1")
                .bind(canonical_message.to_uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
        let side_effect_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM message_side_effect_jobs WHERE message_id = $1",
        )
        .bind(canonical_message.to_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (message_count, key_count, outbox_count, side_effect_count),
            (1, 1, 1, 3)
        );

        let row = EventOutboxRepo::new(pool.clone())
            .pending_for_message(canonical_message)
            .await
            .unwrap()
            .expect("new outbox row is pending");
        assert_eq!(row.id, canonical_outbox);
        assert_eq!(row.subject, format!("im.room.{}", room.id));
        assert_eq!(row.traceparent.as_deref(), Some("00-test"));
        assert!(row.seq.is_none());
        assert!(row.payload.get("seq").is_none());
        let key_string = key.to_string();
        assert_eq!(
            row.payload
                .pointer("/client_message_id")
                .and_then(serde_json::Value::as_str),
            Some(key_string.as_str())
        );

        sqlx::query("DELETE FROM message_side_effect_jobs WHERE message_id = $1")
            .bind(canonical_message.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM event_outbox WHERE message_id = $1")
            .bind(canonical_message.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query(
            "DELETE FROM message_send_keys WHERE sender_id = $1 AND client_message_id = $2",
        )
        .bind(sender.id.to_uuid())
        .bind(key)
        .execute(&pool)
        .await
        .ok();
        sqlx::query("DELETE FROM messages WHERE room_id = $1")
            .bind(room.id.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM room_members WHERE room_id = $1")
            .bind(room.id.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.id.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(sender.id.to_uuid())
            .execute(&pool)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migrations applied"]
    async fn sender_key_is_exactly_once_under_concurrency_and_payload_bound() {
        let pool = pool();
        let sender = participant(&pool, "idempotent-sender").await;
        let other = participant(&pool, "idempotent-other").await;
        enroll_default_workspace(&pool, &[sender.id, other.id]).await;
        let room = RoomRepo::new(pool.clone())
            .create(RoomKind::Group, Some("idempotency".into()), sender.id)
            .await
            .unwrap();
        RoomRepo::new(pool.clone())
            .add_member(room.id, other.id)
            .await
            .unwrap();
        let repo = MessageRepo::new(pool.clone());
        let key = uuid::Uuid::new_v4();
        let hash = [7_u8; 32];

        let attempts = (0..16).map(|_| {
            let repo = repo.clone();
            let new = new_message(room.id, sender.id);
            async move { repo.insert_idempotent(new, key, hash).await.unwrap() }
        });
        let outcomes = futures::future::join_all(attempts).await;
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, MessageInsertOutcome::Created(_)))
                .count(),
            1
        );
        let canonical = outcomes[0].message().id;
        assert!(outcomes
            .iter()
            .all(|outcome| outcome.message().id == canonical));

        let conflict = repo
            .insert_idempotent(new_message(room.id, sender.id), key, [8_u8; 32])
            .await
            .unwrap_err();
        assert!(matches!(conflict, Error::Conflict(_)));

        let other_outcome = repo
            .insert_idempotent(new_message(room.id, other.id), key, hash)
            .await
            .unwrap();
        assert!(!other_outcome.deduplicated());
        assert_ne!(other_outcome.message().id, canonical);

        sqlx::query("DELETE FROM message_send_keys WHERE sender_id = ANY($1)")
            .bind([sender.id.to_uuid(), other.id.to_uuid()])
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM messages WHERE room_id = $1")
            .bind(room.id.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM room_members WHERE room_id = $1")
            .bind(room.id.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.id.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
            .bind([sender.id.to_uuid(), other.id.to_uuid()])
            .execute(&pool)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migrations through 0197"]
    async fn committed_room_revocation_fences_outboxed_message_insert() {
        let pool = pool();
        let workspace_owner = participant(&pool, "message-fence-workspace-owner").await;
        let sender = participant(&pool, "message-fence-sender").await;
        let workspaces = WorkspaceRepo::new(pool.clone());
        let workspace = workspaces
            .create(
                "Message fence".into(),
                format!("message-fence-{}", uuid::Uuid::new_v4()),
                workspace_owner.id,
            )
            .await
            .unwrap();
        workspaces
            .add_member(workspace.id, sender.id, WorkspaceRole::Member)
            .await
            .unwrap();
        let room = RoomRepo::new(pool.clone())
            .create_in_workspace(
                workspace.id,
                RoomKind::Group,
                Some("message-fence".into()),
                sender.id,
            )
            .await
            .unwrap();

        // A sanctioned revocation takes the workspace fence first. The stale
        // send must wait there, then observe the removed room edge rather than
        // inserting a message after the revocation commit.
        let mut revoker = pool.begin().await.unwrap();
        crate::ownership::lock_membership_governance(&mut revoker)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.id.to_uuid())
            .fetch_one(&mut *revoker)
            .await
            .unwrap();
        let repo = MessageRepo::new(pool.clone());
        let room_id = room.id;
        let sender_id = sender.id;
        let send = tokio::spawn(async move {
            repo.insert_outboxed(new_message(room_id, sender_id), None, Vec::new(), None)
                .await
        });
        tokio::task::yield_now().await;
        sqlx::query("DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
            .bind(room_id.to_uuid())
            .bind(sender_id.to_uuid())
            .execute(&mut *revoker)
            .await
            .unwrap();
        revoker.commit().await.unwrap();

        assert!(matches!(send.await.unwrap(), Err(Error::Forbidden(_))));
        let message_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM messages WHERE room_id = $1")
                .bind(room_id.to_uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
        let outbox_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM event_outbox WHERE subject = $1")
                .bind(format!("im.room.{room_id}"))
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((message_count, outbox_count), (0, 0));
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migrations through 0167"]
    async fn outboxed_mutations_authorize_and_serialize_attachment_lifecycle() {
        let pool = pool();
        let sender = participant(&pool, "attachment-sender").await;
        let owner = participant(&pool, "attachment-owner").await;
        let room_repo = RoomRepo::new(pool.clone());
        let room = room_repo
            .create(
                RoomKind::Group,
                Some("attachment-authorization".into()),
                sender.id,
            )
            .await
            .unwrap();
        let workspace = room_repo
            .room_workspace(room.id)
            .await
            .unwrap()
            .expect("room workspace");
        let workspaces = WorkspaceRepo::new(pool.clone());
        workspaces
            .add_member(workspace, sender.id, WorkspaceRole::Member)
            .await
            .unwrap();
        workspaces
            .add_member(workspace, owner.id, WorkspaceRole::Member)
            .await
            .unwrap();
        let messages = MessageRepo::new(pool.clone());
        let blobs = BlobRepo::new(pool.clone());

        let foreign = reserve_blob(&pool, owner.id, workspace, "foreign").await;
        blobs
            .finalize(foreign.id, &foreign.storage_key)
            .await
            .unwrap()
            .expect("foreign upload finalized");

        // A finalized blob is not attachable merely because its ID was guessed.
        let guessed = messages
            .insert_outboxed(
                new_message_with_blocks(room.id, sender.id, vec![file_block(foreign.id)]),
                None,
                vec![],
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(guessed, Error::Forbidden(_)));
        let nonexistent = messages
            .insert_outboxed(
                new_message_with_blocks(room.id, sender.id, vec![file_block(BlobId::new())]),
                None,
                vec![],
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(nonexistent, Error::Forbidden(_)));

        // The edit path applies the same guard before changing version/history.
        let plain = messages
            .insert_outboxed(new_message(room.id, sender.id), None, vec![], None)
            .await
            .unwrap()
            .message()
            .clone();
        let rejected_edit = messages
            .edit_outboxed_authorized(
                plain.id,
                sender.id,
                vec![file_block(foreign.id)],
                plain.version,
                true,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(rejected_edit, Error::Forbidden(_)));
        let unchanged = messages
            .get(plain.id)
            .await
            .unwrap()
            .expect("plain message retained");
        assert_eq!(unchanged.version, plain.version);
        assert_eq!(
            serde_json::to_value(&unchanged.blocks).unwrap(),
            serde_json::to_value(&plain.blocks).unwrap()
        );

        // Upload reservations are invisible until finalized.
        let pending = reserve_blob(&pool, sender.id, workspace, "pending").await;
        let pending_result = messages
            .insert_outboxed(
                new_message_with_blocks(room.id, sender.id, vec![file_block(pending.id)]),
                None,
                vec![],
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(pending_result, Error::Forbidden(_)));

        // The owner can attach a finalized, non-queued object.
        let own = reserve_blob(&pool, sender.id, workspace, "own-finalized").await;
        blobs
            .finalize(own.id, &own.storage_key)
            .await
            .unwrap()
            .expect("own upload finalized");
        let own_message = messages
            .insert_outboxed(
                new_message_with_blocks(room.id, sender.id, vec![file_block(own.id)]),
                None,
                vec![],
                None,
            )
            .await
            .unwrap();
        assert!(matches!(
            own_message.outcome,
            MessageInsertOutcome::Created(_)
        ));

        // Once a foreign blob is referenced in a room the sender currently
        // belongs to, that visible attachment may be reused.
        room_repo.add_member(room.id, owner.id).await.unwrap();
        let source = messages
            .insert_outboxed(
                new_message_with_blocks(room.id, owner.id, vec![file_block(foreign.id)]),
                None,
                vec![],
                None,
            )
            .await
            .unwrap()
            .message()
            .clone();
        let reuse = messages
            .insert_outboxed(
                new_message_with_blocks(room.id, sender.id, vec![file_block(foreign.id)]),
                None,
                vec![],
                None,
            )
            .await
            .unwrap()
            .message()
            .clone();

        messages
            .soft_delete_outboxed_system(
                source.id,
                None,
                None,
                None,
                serde_json::Value::Null,
                owner.id,
                None,
            )
            .await
            .unwrap()
            .expect("source deleted");
        let queued_while_reused: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM blob_gc_queue WHERE blob_id = $1)")
                .bind(foreign.id.to_uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            !queued_while_reused,
            "a remaining live reference prevents ordinary GC"
        );

        messages
            .soft_delete_outboxed_system(
                reuse.id,
                None,
                None,
                None,
                serde_json::Value::Null,
                sender.id,
                None,
            )
            .await
            .unwrap()
            .expect("last reference deleted");
        let queue_intent: Option<bool> =
            sqlx::query_scalar("SELECT force_delete FROM blob_gc_queue WHERE blob_id = $1")
                .bind(foreign.id.to_uuid())
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert_eq!(
            queue_intent,
            Some(false),
            "message cleanup is reference-aware GC"
        );

        let queued_result = messages
            .insert_outboxed(
                new_message_with_blocks(room.id, owner.id, vec![file_block(foreign.id)]),
                None,
                vec![],
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(queued_result, Error::Forbidden(_)));

        sqlx::query(
            "DELETE FROM message_side_effect_jobs
              WHERE message_id IN (SELECT id FROM messages WHERE room_id = $1)",
        )
        .bind(room.id.to_uuid())
        .execute(&pool)
        .await
        .ok();
        sqlx::query(
            "DELETE FROM event_outbox
              WHERE message_id IN (SELECT id FROM messages WHERE room_id = $1)",
        )
        .bind(room.id.to_uuid())
        .execute(&pool)
        .await
        .ok();
        sqlx::query("DELETE FROM messages WHERE room_id = $1")
            .bind(room.id.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM room_members WHERE room_id = $1")
            .bind(room.id.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.id.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM blob_gc_queue WHERE blob_id IN (SELECT id FROM blobs WHERE owner_id = ANY($1))")
            .bind([sender.id.to_uuid(), owner.id.to_uuid()])
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM blobs WHERE owner_id = ANY($1)")
            .bind([sender.id.to_uuid(), owner.id.to_uuid()])
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
            .bind([sender.id.to_uuid(), owner.id.to_uuid()])
            .execute(&pool)
            .await
            .ok();
    }
}
