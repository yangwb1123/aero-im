//! Read-receipt + typing-indicator operations.
//!
//! Extracted from `service.rs` as part of REFACTOR_PLAN.md Step 1c.

use aero_common::{MessageId, ParticipantId, ReadReceipt, Result, RoomEvent, RoomId};
use tracing::instrument;

use crate::ImService;

impl ImService {
    /// Move a participant's read cursor in a room.
    #[instrument(skip(self), fields(?actor, ?room, ?last_read))]
    pub async fn mark_read(
        &self,
        actor: ParticipantId,
        room: RoomId,
        last_read: MessageId,
    ) -> Result<ReadReceipt> {
        self.assert_room_access(actor, room).await?;
        let receipt = self
            .receipts
            .mark_read_authorized(room, actor, last_read)
            .await?;
        self.publish_room_event(
            room,
            &RoomEvent::Read {
                room_id: room,
                participant: actor,
                last_message_id: receipt.last_read_message_id,
                at: receipt.updated_at,
            },
        )
        .await;
        Ok(receipt)
    }

    pub async fn receipts_for(&self, room: RoomId) -> Result<Vec<ReadReceipt>> {
        Ok(self.receipts.list_for_room(room).await?)
    }

    /// Broadcast a typing indicator. No persistence.
    pub async fn typing(&self, actor: ParticipantId, room: RoomId, on: bool) -> Result<()> {
        self.assert_room_access(actor, room).await?;
        self.publish_room_event(
            room,
            &RoomEvent::Typing {
                room_id: room,
                participant: actor,
                on,
            },
        )
        .await;
        Ok(())
    }
}
