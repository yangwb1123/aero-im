//! Event publishing to NATS — seq-stamped fan-out for room events.
//!
//! Extracted from `service.rs` as part of REFACTOR_PLAN.md Step 1f.

use aero_bus::{traits::BusError, EventBus};
use aero_common::{
    metrics, ParticipantId, RoomEvent, RoomId,
};
use async_trait::async_trait;
use bytes;
use tracing::warn;

use crate::service::ImService;

/// Cross-IM-event subject prefix used by `publish_event`.
pub(crate) const EVENTS_SUBJECT: &str = "im.events";

/// Object-safe view of [`EventBus`] used internally for dependency injection.
///
/// The upstream [`EventBus`] trait declares a generic default method (`publish_json<T>`)
/// which makes it not dyn-compatible. Rather than patching `aero-bus`, we wrap any
/// [`EventBus`] implementation in this object-safe shim.
#[async_trait]
pub trait BusSink: Send + Sync + 'static {
    async fn publish_bytes(&self, subject: &str, payload: bytes::Bytes)
        -> std::result::Result<(), BusError>;
}

#[async_trait]
impl<T> BusSink for T
where
    T: EventBus + 'static,
{
    async fn publish_bytes(
        &self,
        subject: &str,
        payload: bytes::Bytes,
    ) -> std::result::Result<(), BusError> {
        EventBus::publish(self, subject, payload).await
    }
}

/// Publish a serialisable value to a bus subject.  Thin generic helper shared
/// by `room.rs` (`RoomCreated`, `MemberAdded`) and any future module that emits
/// a non-`RoomEvent` onto the `im.events.*` subject tree.
pub(crate) async fn publish_event<T: serde::Serialize>(
    bus: &dyn BusSink,
    subject: &str,
    value: &T,
) -> std::result::Result<(), BusError> {
    let bytes = serde_json::to_vec(value)?;
    bus.publish_bytes(subject, bytes.into()).await
}

impl ImService {
    pub async fn announce_message_deleted(&self, room: RoomId, message_id: aero_common::MessageId) {
        self.publish_room_event(
            room,
            &RoomEvent::Deleted {
                room_id: room,
                message_id,
                by: ParticipantId::nil(),
            },
        )
        .await;
    }

    pub(crate) async fn publish_room_event(&self, room: RoomId, event: &RoomEvent) {
        let subject = Self::room_subject(room);
        // Publish-time seq stamp (ROADMAP 第三版 方向一): mint the per-room seq
        // BEFORE the bytes hit NATS, so an at-least-once redelivery carries the
        // SAME seq and clients can dedup/order Edited/Deleted/Reaction/Typing
        // events that have no id of their own. `None` (provider unavailable)
        // degrades to an unstamped event — never blocks delivery. Existing
        // consumers deserialize `RoomEvent` with serde, which ignores the
        // unknown `"seq"` field (no event type uses `deny_unknown_fields`).
        let seq = self.seq.next_seq(&subject).await;
        // Cross-bus trace continuity (ROADMAP5 方向二): carry the current span's W3C
        // traceparent on the envelope so the consumer can nest its fan-out under the
        // producer's trace. Like `seq`, it's a sibling key serde ignores on typed
        // decode; `None` (no active trace) leaves the event untraced.
        let traceparent = aero_common::telemetry::current_traceparent();
        let publish = async {
            let mut value = serde_json::to_value(event)?;
            aero_bus::stamp_seq(&mut value, seq);
            aero_bus::stamp_traceparent(&mut value, traceparent.as_deref());
            let bytes = serde_json::to_vec(&value)?;
            self.bus.publish_bytes(&subject, bytes.into()).await
        };
        if let Err(err) = publish.await {
            warn!(?err, %subject, "publish RoomEvent failed");
            metrics::inc_counter(
                metrics::names::NATS_PUBLISH_ERRORS_TOTAL,
                1,
            );
        }
    }
}
