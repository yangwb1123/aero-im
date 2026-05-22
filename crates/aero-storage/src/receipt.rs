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
