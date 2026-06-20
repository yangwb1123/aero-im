//! Unread-count aggregation per room.
//!
//! Extracted from `message/orig.rs` as part of REFACTOR_PLAN.md Step 2.

use aero_common::{ParticipantId, RoomId};

use super::MessageRepo;

impl MessageRepo {
    /// Count unread messages per room for a participant.
    pub async fn unread_counts_by_room(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<(RoomId, u32)>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid, i64)>(
            r"SELECT m.room_id, COUNT(*)
               FROM messages m
               JOIN room_members rm
                 ON rm.room_id = m.room_id AND rm.participant_id = $1
               LEFT JOIN read_receipts rr
                 ON rr.room_id = m.room_id AND rr.participant_id = $1
               WHERE m.deleted_at IS NULL
                 AND m.sender_id <> $1
                 AND (rr.last_read_message_id IS NULL OR m.id > rr.last_read_message_id)
               GROUP BY m.room_id",
        )
        .bind(participant.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(r, c)| (RoomId::from_uuid(r), u32::try_from(c).unwrap_or(u32::MAX)))
            .collect())
    }
}
