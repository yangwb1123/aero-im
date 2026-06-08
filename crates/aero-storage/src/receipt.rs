//! Read-receipt repository.
//!
//! One row per (room, participant) recording the highest message id that the
//! participant has seen. Upserted on every `mark_read` call.

use aero_common::{MessageId, ParticipantId, ReadReceipt, RoomId};
use sqlx::PgPool;

#[derive(Clone)]
pub struct ReceiptRepo {
    pool: PgPool,
}

impl ReceiptRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Idempotent UPSERT. Refuses to roll the cursor backwards.
    pub async fn mark_read(
        &self,
        room: RoomId,
        participant: ParticipantId,
        last_read: MessageId,
    ) -> Result<ReadReceipt, sqlx::Error> {
        let now = time::OffsetDateTime::now_utc();
        sqlx::query(
            r#"INSERT INTO read_receipts (room_id, participant_id, last_read_message_id, updated_at)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (room_id, participant_id) DO UPDATE
               SET last_read_message_id = EXCLUDED.last_read_message_id,
                   updated_at = EXCLUDED.updated_at
               WHERE EXCLUDED.last_read_message_id > read_receipts.last_read_message_id"#,
        )
        .bind(room.to_uuid())
        .bind(participant.to_uuid())
        .bind(last_read.to_uuid())
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(ReadReceipt {
            room_id: room,
            participant_id: participant,
            last_read_message_id: last_read,
            updated_at: now,
        })
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
    /// callers must gate access first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert/delete.
    pub async fn set_cursor(
        &self,
        room: RoomId,
        participant: ParticipantId,
        last_read: Option<MessageId>,
    ) -> Result<(), sqlx::Error> {
        match last_read {
            Some(mid) => {
                sqlx::query(
                    r"INSERT INTO read_receipts (room_id, participant_id, last_read_message_id, updated_at)
                       VALUES ($1, $2, $3, $4)
                       ON CONFLICT (room_id, participant_id) DO UPDATE
                       SET last_read_message_id = EXCLUDED.last_read_message_id,
                           updated_at = EXCLUDED.updated_at",
                )
                .bind(room.to_uuid())
                .bind(participant.to_uuid())
                .bind(mid.to_uuid())
                .bind(time::OffsetDateTime::now_utc())
                .execute(&self.pool)
                .await?;
            }
            None => {
                sqlx::query(
                    r"DELETE FROM read_receipts WHERE room_id = $1 AND participant_id = $2",
                )
                .bind(room.to_uuid())
                .bind(participant.to_uuid())
                .execute(&self.pool)
                .await?;
            }
        }
        Ok(())
    }

    /// All receipts for a room (one per participant who's read anything).
    pub async fn list_for_room(&self, room: RoomId) -> Result<Vec<ReadReceipt>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, uuid::Uuid, time::OffsetDateTime)>(
            r#"SELECT room_id, participant_id, last_read_message_id, updated_at
               FROM read_receipts WHERE room_id = $1"#,
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
            r#"SELECT last_read_message_id, updated_at
               FROM read_receipts WHERE room_id = $1 AND participant_id = $2"#,
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

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored receipt
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

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
             VALUES ($1,'channel',$2,$3, now(), '00000000-0000-0000-0000-000000000000')",
        )
        .bind(room.to_uuid())
        .bind("receipt-room")
        .bind(actor.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        (room, actor)
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

        // Forward-only `mark_read` parks the cursor at `high`.
        repo.mark_read(room, actor, high).await.expect("mark_read");
        assert_eq!(
            repo.get(room, actor).await.unwrap().unwrap().last_read_message_id,
            high
        );

        // `mark_read` refuses to roll back (monotonic guard) — cursor stays at `high`.
        repo.mark_read(room, actor, low).await.expect("mark_read");
        assert_eq!(
            repo.get(room, actor).await.unwrap().unwrap().last_read_message_id,
            high,
            "mark_read is forward-only"
        );

        // `set_cursor` DOES roll back — the mark-as-unread move.
        repo.set_cursor(room, actor, Some(low)).await.expect("set_cursor back");
        assert_eq!(
            repo.get(room, actor).await.unwrap().unwrap().last_read_message_id,
            low,
            "set_cursor rolls the cursor backward"
        );

        // `None` clears the receipt → whole room re-badges unread.
        repo.set_cursor(room, actor, None).await.expect("set_cursor clear");
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
}
