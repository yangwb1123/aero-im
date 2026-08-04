//! Pin operations — pin, unpin, list, broadcast room events.
//!
//! Extracted from `service.rs` as part of REFACTOR_PLAN.md Step 1e.

use aero_common::{
    Error, MessageId, ParticipantId, PinOp, PinnedMessage, Result, RoomEvent, RoomId,
};
use aero_storage::PinRepo;
use tracing::instrument;

use crate::ImService;

impl ImService {
    /// Reference to the wired pin store, or a clear internal error if the service
    /// was built without [`with_pins`](Self::with_pins).
    fn pins(&self) -> Result<&PinRepo> {
        self.pins.as_ref().ok_or_else(|| {
            Error::Internal(anyhow::anyhow!(
                "ImService used for a pin operation without a PinRepo (call ImService::with_pins)"
            ))
        })
    }

    /// Pin a message in a room. Requires the actor to have room access and the
    /// message to actually belong to the room. Broadcasts `RoomEvent::Pin` on a
    /// newly-created pin. Returns `true` if a new pin was created (idempotent).
    #[instrument(skip(self), fields(?actor, ?room, ?message))]
    pub async fn pin_message(
        &self,
        actor: ParticipantId,
        room: RoomId,
        message: MessageId,
    ) -> Result<bool> {
        // Fast entry preflight; PinRepo repeats the canonical access decision
        // under the same transaction as message containment and insertion.
        self.assert_room_access(actor, room).await?;
        let created = self.pins()?.pin_authorized(room, message, actor).await?;
        // The authorized repository returns only after commit, so no event can
        // advertise a pin that later rolls back.
        if created {
            self.publish_room_event(
                room,
                &RoomEvent::Pin {
                    room_id: room,
                    message_id: message,
                    by: actor,
                    op: PinOp::Pin,
                },
            )
            .await;
        }
        Ok(created)
    }

    /// Unpin a message. Requires room access. Broadcasts `RoomEvent::Pin`
    /// (`Unpin`) when a pin was actually removed. Returns `true` if removed.
    #[instrument(skip(self), fields(?actor, ?room, ?message))]
    pub async fn unpin_message(
        &self,
        actor: ParticipantId,
        room: RoomId,
        message: MessageId,
    ) -> Result<bool> {
        // Fast entry preflight; commit-time authorization remains storage-owned.
        self.assert_room_access(actor, room).await?;
        let removed = self.pins()?.unpin_authorized(room, message, actor).await?;
        // Publish only after the authorized delete committed.
        if removed {
            self.publish_room_event(
                room,
                &RoomEvent::Pin {
                    room_id: room,
                    message_id: message,
                    by: actor,
                    op: PinOp::Unpin,
                },
            )
            .await;
        }
        Ok(removed)
    }

    /// List a room's pinned messages (newest first). Requires room access.
    pub async fn list_pins(
        &self,
        actor: ParticipantId,
        room: RoomId,
    ) -> Result<Vec<PinnedMessage>> {
        // Keep the public seam's fast preflight, then repeat/hold current access
        // through the storage read transaction.
        self.assert_room_access(actor, room).await?;
        self.pins()?.list_authorized(room, actor).await
    }

    /// Broadcast an already-constructed [`RoomEvent`] on the room's subject.
    /// Thin public seam (mirrors [`relay_call_event`](Self::relay_call_event))
    /// letting feature modules that own their own storage — e.g. polls — fan out a
    /// room event without re-implementing the bus plumbing. Best-effort: a publish
    /// failure is logged, never surfaced.
    pub async fn broadcast_room_event(&self, room: RoomId, event: RoomEvent) {
        self.publish_room_event(room, &event).await;
    }
}
