//! Read-receipt repository.
//!
//! One row per (room, participant) recording the highest message id that the
//! participant has seen. Upserted on every `mark_read` call.

use aero_common::{Error, MessageId, ParticipantId, ReadReceipt, Result as AeroResult, RoomId};
use sqlx::{PgPool, Postgres, Transaction};

#[derive(Clone)]
pub struct ReceiptRepo {
    pool: PgPool,
}

impl ReceiptRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Low-level idempotent UPSERT. Refuses to roll the cursor backwards.
    ///
    /// Request paths must use [`Self::mark_read_authorized`], which holds
    /// effective room access and message containment through commit.
    pub async fn mark_read(
        &self,
        room: RoomId,
        participant: ParticipantId,
        last_read: MessageId,
    ) -> Result<ReadReceipt, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let receipt = mark_read_in_tx(&mut tx, room, participant, last_read).await?;
        tx.commit().await?;
        Ok(receipt)
    }

    /// Advance a read cursor only while `participant` still has effective room
    /// access and `last_read` is a message in that exact room.
    ///
    /// The database access helper owns the workspace → room → membership lock
    /// order. The message identity is then locked before the receipt UPSERT, so
    /// membership revocation and cross-room message substitution cannot race the
    /// committed cursor.
    pub async fn mark_read_authorized(
        &self,
        room: RoomId,
        participant: ParticipantId,
        last_read: MessageId,
    ) -> AeroResult<ReadReceipt> {
        let mut tx = self.pool.begin().await?;
        let allowed: bool = sqlx::query_scalar("SELECT aero_effective_room_access($1, $2, NULL)")
            .bind(room.to_uuid())
            .bind(participant.to_uuid())
            .fetch_one(&mut *tx)
            .await?;
        if !allowed {
            return Err(Error::Forbidden(
                "read-receipt room access was revoked before commit".into(),
            ));
        }

        let message_in_room = sqlx::query_scalar::<_, bool>(
            "SELECT true
               FROM messages
              WHERE id = $1 AND room_id = $2
              FOR SHARE",
        )
        .bind(last_read.to_uuid())
        .bind(room.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !message_in_room {
            return Err(Error::NotFound("message in room".into()));
        }

        let receipt = mark_read_in_tx(&mut tx, room, participant, last_read).await?;
        tx.commit().await?;
        Ok(receipt)
    }

    /// Move the caller's cursor immediately before `target`, atomically
    /// rechecking effective access to the target's canonical room.
    ///
    /// Returns `(room, predecessor, updated_at)`. A missing predecessor means
    /// the target is the room's first visible message and the receipt was
    /// deleted so the whole room becomes unread.
    pub async fn mark_unread_authorized(
        &self,
        participant: ParticipantId,
        target: MessageId,
    ) -> AeroResult<(RoomId, Option<MessageId>, time::OffsetDateTime)> {
        let mut tx = self.pool.begin().await?;
        let resolved_room =
            sqlx::query_scalar::<_, uuid::Uuid>("SELECT room_id FROM messages WHERE id = $1")
                .bind(target.to_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .map(RoomId::from_uuid)
                .ok_or_else(|| Error::NotFound(format!("message {target}")))?;

        let allowed: bool = sqlx::query_scalar("SELECT aero_effective_room_access($1, $2, NULL)")
            .bind(resolved_room.to_uuid())
            .bind(participant.to_uuid())
            .fetch_one(&mut *tx)
            .await?;
        if !allowed {
            return Err(Error::Forbidden(
                "mark-unread room access was revoked before commit".into(),
            ));
        }

        let locked_room = sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT room_id FROM messages WHERE id = $1 FOR SHARE",
        )
        .bind(target.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .map(RoomId::from_uuid)
        .ok_or_else(|| Error::NotFound(format!("message {target}")))?;
        if locked_room != resolved_room {
            return Err(Error::Conflict(
                "message room changed concurrently; reload and retry".into(),
            ));
        }

        let predecessor = sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT id
               FROM messages
              WHERE room_id = $1
                AND id < $2
                AND deleted_at IS NULL
                AND (expires_at IS NULL OR expires_at > now())
              ORDER BY id DESC
              LIMIT 1
              FOR SHARE",
        )
        .bind(resolved_room.to_uuid())
        .bind(target.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .map(MessageId::from_uuid);
        let updated_at = set_cursor_in_tx(&mut tx, resolved_room, participant, predecessor).await?;
        tx.commit().await?;
        Ok((resolved_room, predecessor, updated_at))
    }

    /// Set the read cursor to an exact value, **bypassing the monotonic guard**
    /// that [`mark_read`](Self::mark_read) enforces — the triage "mark as unread"
    /// primitive, which deliberately rolls the cursor *backwards* so a room
    /// re-badges as unread.
    ///
    /// `last_read = Some(id)` upserts the receipt to exactly `id` (forward or
    /// backward). `last_read = None` deletes the receipt entirely, which the
    /// unread query
    /// ([`MessageRepo::unread_counts_by_room`](crate::MessageRepo::unread_counts_by_room),
    /// a `LEFT JOIN` that treats a missing receipt as "all unread") reads as the
    /// whole room being unread — the natural representation of "no read cursor"
    /// given the `read_receipts.last_read_message_id` column is `NOT NULL`.
    ///
    /// Unlike [`mark_read`](Self::mark_read) this performs no membership check;
    /// callers must gate access first. Migration 0201 still rejects a message
    /// cursor that does not belong to `room`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert/delete.
    pub async fn set_cursor(
        &self,
        room: RoomId,
        participant: ParticipantId,
        last_read: Option<MessageId>,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        set_cursor_in_tx(&mut tx, room, participant, last_read).await?;
        tx.commit().await?;
        Ok(())
    }

    /// All receipts for a room (one per participant who's read anything).
    pub async fn list_for_room(&self, room: RoomId) -> Result<Vec<ReadReceipt>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, uuid::Uuid, time::OffsetDateTime)>(
            r"SELECT room_id, participant_id, last_read_message_id, updated_at
               FROM read_receipts WHERE room_id = $1",
        )
        .bind(room.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(rid, pid, mid, at)| ReadReceipt {
                room_id: RoomId::from_uuid(rid),
                participant_id: ParticipantId::from_uuid(pid),
                last_read_message_id: MessageId::from_uuid(mid),
                updated_at: at,
            })
            .collect())
    }

    pub async fn get(
        &self,
        room: RoomId,
        participant: ParticipantId,
    ) -> Result<Option<ReadReceipt>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid, time::OffsetDateTime)>(
            r"SELECT last_read_message_id, updated_at
               FROM read_receipts WHERE room_id = $1 AND participant_id = $2",
        )
        .bind(room.to_uuid())
        .bind(participant.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(mid, at)| ReadReceipt {
            room_id: room,
            participant_id: participant,
            last_read_message_id: MessageId::from_uuid(mid),
            updated_at: at,
        }))
    }
}

async fn mark_read_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    room: RoomId,
    participant: ParticipantId,
    last_read: MessageId,
) -> Result<ReadReceipt, sqlx::Error> {
    let now = time::OffsetDateTime::now_utc();
    sqlx::query(
        r"INSERT INTO read_receipts (room_id, participant_id, last_read_message_id, updated_at)
           VALUES ($1, $2, $3, $4)
           ON CONFLICT (room_id, participant_id) DO UPDATE
           SET last_read_message_id = EXCLUDED.last_read_message_id,
               updated_at = EXCLUDED.updated_at
           WHERE EXCLUDED.last_read_message_id > read_receipts.last_read_message_id",
    )
    .bind(room.to_uuid())
    .bind(participant.to_uuid())
    .bind(last_read.to_uuid())
    .bind(now)
    .execute(&mut **tx)
    .await?;

    // Return the durable cursor, not merely the requested id. A stale retry that
    // loses the monotonic comparison must not broadcast a fictitious rollback.
    let (actual_message, actual_updated_at) =
        sqlx::query_as::<_, (uuid::Uuid, time::OffsetDateTime)>(
            "SELECT last_read_message_id, updated_at
               FROM read_receipts
              WHERE room_id = $1 AND participant_id = $2
              FOR UPDATE",
        )
        .bind(room.to_uuid())
        .bind(participant.to_uuid())
        .fetch_one(&mut **tx)
        .await?;
    Ok(ReadReceipt {
        room_id: room,
        participant_id: participant,
        last_read_message_id: MessageId::from_uuid(actual_message),
        updated_at: actual_updated_at,
    })
}

async fn set_cursor_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    room: RoomId,
    participant: ParticipantId,
    last_read: Option<MessageId>,
) -> Result<time::OffsetDateTime, sqlx::Error> {
    let now = time::OffsetDateTime::now_utc();
    match last_read {
        Some(message) => {
            sqlx::query(
                r"INSERT INTO read_receipts
                     (room_id, participant_id, last_read_message_id, updated_at)
                   VALUES ($1, $2, $3, $4)
                   ON CONFLICT (room_id, participant_id) DO UPDATE
                   SET last_read_message_id = EXCLUDED.last_read_message_id,
                       updated_at = EXCLUDED.updated_at",
            )
            .bind(room.to_uuid())
            .bind(participant.to_uuid())
            .bind(message.to_uuid())
            .bind(now)
            .execute(&mut **tx)
            .await?;
        }
        None => {
            sqlx::query(
                "DELETE FROM read_receipts
                  WHERE room_id = $1 AND participant_id = $2",
            )
            .bind(room.to_uuid())
            .bind(participant.to_uuid())
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(now)
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored receipt
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::{RoomRepo, WorkspaceRepo};
    use aero_common::{RoomKind, WorkspaceRole};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway participant + room so the test is self-contained.
    async fn fixture(p: &PgPool) -> (RoomId, ParticipantId) {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(actor.to_uuid())
            .bind(format!("receipt-actor-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");
        let room = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
             VALUES ($1,'group',$2,$3, now(), '00000000-0000-0000-0000-000000000000')",
        )
        .bind(room.to_uuid())
        .bind("receipt-room")
        .bind(actor.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        (room, actor)
    }

    async fn insert_message(p: &PgPool, id: MessageId, room: RoomId, sender: ParticipantId) {
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks)
             VALUES ($1, $2, $3, '[]'::jsonb)",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(sender.to_uuid())
        .execute(p)
        .await
        .expect("insert receipt anchor message");
    }

    /// `set_cursor` is non-monotonic: it can roll the read cursor *backwards*
    /// (the mark-as-unread primitive), unlike the forward-only `mark_read`, and
    /// `None` clears the receipt so the whole room re-badges unread.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn set_cursor_can_move_backward_and_clear() {
        let p = pool();
        let repo = ReceiptRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;

        // Two crafted, byte-ordered ids: `low` < `high` (Postgres uuid order).
        let high = MessageId::from_uuid(
            uuid::Uuid::parse_str("ffffffff-ffff-ffff-ffff-ffffffffffff").expect("uuid"),
        );
        let low = MessageId::from_uuid(
            uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("uuid"),
        );
        insert_message(&p, high, room, actor).await;
        insert_message(&p, low, room, actor).await;

        // Forward-only `mark_read` parks the cursor at `high`.
        repo.mark_read(room, actor, high).await.expect("mark_read");
        assert_eq!(
            repo.get(room, actor)
                .await
                .unwrap()
                .unwrap()
                .last_read_message_id,
            high
        );

        // `mark_read` refuses to roll back (monotonic guard) — cursor stays at `high`.
        repo.mark_read(room, actor, low).await.expect("mark_read");
        assert_eq!(
            repo.get(room, actor)
                .await
                .unwrap()
                .unwrap()
                .last_read_message_id,
            high,
            "mark_read is forward-only"
        );

        // `set_cursor` DOES roll back — the mark-as-unread move.
        repo.set_cursor(room, actor, Some(low))
            .await
            .expect("set_cursor back");
        assert_eq!(
            repo.get(room, actor)
                .await
                .unwrap()
                .unwrap()
                .last_read_message_id,
            low,
            "set_cursor rolls the cursor backward"
        );

        // `None` clears the receipt → whole room re-badges unread.
        repo.set_cursor(room, actor, None)
            .await
            .expect("set_cursor clear");
        assert!(
            repo.get(room, actor).await.unwrap().is_none(),
            "set_cursor(None) clears the read cursor"
        );

        // Cleanup (cascades remove any receipt rows too).
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(actor.to_uuid())
            .execute(&p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migrations through 0201"]
    async fn authorized_cursor_is_room_contained_and_rechecks_current_access() {
        let p = pool();
        let workspaces = WorkspaceRepo::new(p.clone());
        let rooms = RoomRepo::new(p.clone());
        let receipts = ReceiptRepo::new(p.clone());
        let actor = ParticipantId::new();
        let successor = ParticipantId::new();
        for participant in [actor, successor] {
            sqlx::query(
                "INSERT INTO participants (id, kind, display_name)
                 VALUES ($1, 'human', $2)",
            )
            .bind(participant.to_uuid())
            .bind(format!("receipt-auth-{participant}"))
            .execute(&p)
            .await
            .unwrap();
        }
        let workspace = workspaces
            .create(
                "Receipt auth".into(),
                format!("receipt-{}", uuid::Uuid::new_v4().simple()),
                actor,
            )
            .await
            .unwrap();
        workspaces
            .add_member(workspace.id, successor, WorkspaceRole::Owner)
            .await
            .unwrap();
        let first = rooms
            .create_in_workspace_authorized(
                workspace.id,
                RoomKind::Group,
                Some("first".into()),
                actor,
            )
            .await
            .unwrap();
        let second = rooms
            .create_in_workspace_authorized(
                workspace.id,
                RoomKind::Group,
                Some("second".into()),
                actor,
            )
            .await
            .unwrap();
        let message_base = uuid::Uuid::new_v4().as_u128() & !0xffff_u128;
        let first_message = MessageId::from_uuid(uuid::Uuid::from_u128(message_base + 1));
        let later_first_message = MessageId::from_uuid(uuid::Uuid::from_u128(message_base + 2));
        let second_message = MessageId::from_uuid(uuid::Uuid::from_u128(message_base + 3));
        insert_message(&p, first_message, first.id, actor).await;
        insert_message(&p, later_first_message, first.id, actor).await;
        insert_message(&p, second_message, second.id, actor).await;

        let cross_room = receipts
            .mark_read_authorized(first.id, actor, second_message)
            .await
            .unwrap_err();
        assert!(matches!(cross_room, Error::NotFound(_)));
        assert!(receipts.get(first.id, actor).await.unwrap().is_none());

        let direct_error = sqlx::query(
            "INSERT INTO read_receipts
                 (room_id, participant_id, last_read_message_id)
             VALUES ($1, $2, $3)",
        )
        .bind(first.id.to_uuid())
        .bind(actor.to_uuid())
        .bind(second_message.to_uuid())
        .execute(&p)
        .await
        .unwrap_err();
        assert_eq!(
            direct_error
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::constraint),
            Some("read_receipts_message_room_chk")
        );

        let receipt = receipts
            .mark_read_authorized(first.id, actor, first_message)
            .await
            .unwrap();
        assert_eq!(receipt.last_read_message_id, first_message);
        let (unread_room, predecessor, _) = receipts
            .mark_unread_authorized(actor, later_first_message)
            .await
            .unwrap();
        assert_eq!(unread_room, first.id);
        assert_eq!(predecessor, Some(first_message));

        workspaces
            .remove_member_authorized(workspace.id, successor, actor)
            .await
            .unwrap();
        let revoked = receipts
            .mark_read_authorized(first.id, actor, first_message)
            .await
            .unwrap_err();
        assert!(matches!(revoked, Error::Forbidden(_)));
        let unread_revoked = receipts
            .mark_unread_authorized(actor, later_first_message)
            .await
            .unwrap_err();
        assert!(matches!(unread_revoked, Error::Forbidden(_)));

        workspaces
            .delete_authorized(workspace.id, successor)
            .await
            .unwrap();
        for participant in [actor, successor] {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(participant.to_uuid())
                .execute(&p)
                .await
                .ok();
        }
    }
}
