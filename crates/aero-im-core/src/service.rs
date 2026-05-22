//! `ImService` — the IM business facade.
//!
//! Coordinates the storage repositories (`RoomRepo`, `MessageRepo`,
//! `ParticipantRepo`) and the event bus (`EventBus`). All business invariants
//! (membership checks, validation) live here so HTTP/WS handlers stay thin.
//!
//! See `docs/specs/2026-05-22-aero-im-design.md` §4.2 for the protocol contract.

use std::sync::Arc;

use aero_bus::traits::BusError;
use aero_bus::EventBus;
use aero_common::{
    Block, Error, Message, MessageEnvelope, MessageId, ParticipantId, Result, Room, RoomId,
    RoomKind,
};
use aero_storage::{
    message::NewMessage, MessageRepo, ParticipantRepo, RoomRepo,
};
use async_trait::async_trait;
use tracing::instrument;

use crate::events::ImEvent;
use crate::validation::validate_blocks;

/// Subject prefix used for high-level control-plane events.
const EVENTS_SUBJECT: &str = "im.events";

/// Object-safe view of [`EventBus`] used internally for dependency injection.
///
/// The upstream [`EventBus`] trait declares a generic default method (`publish_json<T>`)
/// which makes it not dyn-compatible. Rather than patching `aero-bus`, we wrap any
/// [`EventBus`] implementation in this object-safe shim. The blanket impl below lets
/// callers pass `Arc::new(JetStreamBus::connect(...).await?)` directly via
/// [`ImService::new`].
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

/// Serialize a value and publish it via an object-safe bus handle.
async fn publish_event<T: serde::Serialize>(
    bus: &dyn BusSink,
    subject: &str,
    value: &T,
) -> std::result::Result<(), BusError> {
    let bytes = serde_json::to_vec(value)?;
    bus.publish_bytes(subject, bytes.into()).await
}

/// IM business facade.
///
/// Holds Postgres repositories (cheap clones — they wrap an `Arc<PgPool>`) and an
/// `Arc<dyn BusSink>` shim around any [`EventBus`] for NATS publishing. Designed to
/// live behind an `Arc` in the Axum app state.
#[derive(Clone)]
pub struct ImService {
    rooms: RoomRepo,
    messages: MessageRepo,
    #[allow(dead_code)] // Reserved for P2 (profile lookups, mentions).
    participants: ParticipantRepo,
    bus: Arc<dyn BusSink>,
}

impl ImService {
    /// Construct from any [`EventBus`] implementation. The argument is converted into
    /// the object-safe [`BusSink`] shim so the service can be polymorphic over the
    /// concrete bus type (NATS in prod, an in-memory mock in tests).
    ///
    /// Spec §4.2 calls for `Arc<dyn EventBus>`; because the upstream `EventBus` trait
    /// is not dyn-compatible (it carries a generic default method), we route through
    /// [`BusSink`] without changing the dependency-injection ergonomics.
    pub fn new<B: EventBus + 'static>(
        rooms: RoomRepo,
        messages: MessageRepo,
        participants: ParticipantRepo,
        bus: Arc<B>,
    ) -> Self {
        Self {
            rooms,
            messages,
            participants,
            bus: bus as Arc<dyn BusSink>,
        }
    }

    /// Lower-level constructor for tests or custom adapters that already hold a
    /// `BusSink` trait object.
    pub fn from_sink(
        rooms: RoomRepo,
        messages: MessageRepo,
        participants: ParticipantRepo,
        bus: Arc<dyn BusSink>,
    ) -> Self {
        Self { rooms, messages, participants, bus }
    }

    /// NATS subject used for per-room message broadcast (see spec §4.2).
    #[must_use]
    pub fn room_subject(room: RoomId) -> String {
        format!("im.room.{room}")
    }

    /// Create a new room. The `creator` is automatically inserted as `owner` by the
    /// storage layer.
    #[instrument(skip(self), fields(?creator, ?kind))]
    pub async fn create_room(
        &self,
        creator: ParticipantId,
        kind: RoomKind,
        name: Option<String>,
    ) -> Result<Room> {
        let room = self.rooms.create(kind, name, creator).await?;
        // Best-effort high-level event; never block the caller on bus failures.
        let event = ImEvent::RoomCreated(room.clone());
        if let Err(err) = publish_event(
            self.bus.as_ref(),
            &format!("{EVENTS_SUBJECT}.room.created"),
            &event,
        )
        .await
        {
            tracing::warn!(?err, room_id = %room.id, "publish RoomCreated failed");
        }
        Ok(room)
    }

    /// Add a member to a room.
    ///
    /// P1 authorization: `actor` must themselves be a member of the room.
    /// TODO(P2): replace with role-based check (`owner`/`admin` only) by reading the
    /// `room_members.role` column.
    #[instrument(skip(self), fields(?actor, ?room, ?member))]
    pub async fn add_member(
        &self,
        actor: ParticipantId,
        room: RoomId,
        member: ParticipantId,
    ) -> Result<()> {
        if !self.rooms.is_member(room, actor).await? {
            return Err(Error::Forbidden(format!(
                "actor {actor} is not a member of room {room}"
            )));
        }
        self.rooms.add_member(room, member).await?;
        let event = ImEvent::MemberAdded { room, participant: member };
        if let Err(err) = publish_event(
            self.bus.as_ref(),
            &format!("{EVENTS_SUBJECT}.room.member_added"),
            &event,
        )
        .await
        {
            tracing::warn!(?err, %room, %member, "publish MemberAdded failed");
        }
        Ok(())
    }

    /// List rooms the participant belongs to.
    #[instrument(skip(self), fields(?who))]
    pub async fn list_my_rooms(&self, who: ParticipantId) -> Result<Vec<Room>> {
        let rooms = self.rooms.rooms_for(who).await?;
        Ok(rooms)
    }

    /// Persist a new message and broadcast it on the per-room NATS subject.
    ///
    /// Flow:
    /// 1. Membership check (`Error::Forbidden` if `sender` is not in the room).
    /// 2. Block validation (`Error::Invalid` on failure).
    /// 3. Insert into `messages`.
    /// 4. Fetch recipients for the envelope.
    /// 5. Publish [`MessageEnvelope`] on `im.room.{room_id}`.
    ///
    /// Returns the persisted [`Message`]. Bus failures are *not* fatal — the message
    /// is durable in PG and a downstream replay job can republish later.
    #[instrument(
        skip(self, blocks),
        fields(?sender, ?room, block_count = blocks.len(), reply_to = ?reply_to)
    )]
    pub async fn send_message(
        &self,
        sender: ParticipantId,
        room: RoomId,
        blocks: Vec<Block>,
        reply_to: Option<MessageId>,
    ) -> Result<Message> {
        // 1. Membership check.
        if !self.rooms.is_member(room, sender).await? {
            return Err(Error::Forbidden(format!(
                "sender {sender} is not a member of room {room}"
            )));
        }

        // 2. Block validation.
        validate_blocks(&blocks)?;

        // 3. Persist.
        let message = self
            .messages
            .insert(NewMessage {
                room_id: room,
                sender_id: sender,
                blocks,
                reply_to,
                metadata: serde_json::Value::Null,
            })
            .await?;

        // 4. Recipients = current room members at publish time.
        let recipients = self.rooms.members(room).await.unwrap_or_else(|err| {
            tracing::warn!(?err, %room, "fetching recipients failed; publishing without fan-out hint");
            Vec::new()
        });

        // 5. Publish.
        let envelope = MessageEnvelope { message: message.clone(), recipients };
        let subject = Self::room_subject(room);
        if let Err(err) = publish_event(self.bus.as_ref(), &subject, &envelope).await {
            tracing::warn!(?err, %subject, message_id = %message.id, "publish MessageSent failed");
        }

        Ok(message)
    }

    /// Paginated history. `before` is exclusive — pass the oldest already-known
    /// `MessageId` to fetch the next page.
    #[instrument(skip(self), fields(?who, ?room, ?before, limit))]
    pub async fn history(
        &self,
        who: ParticipantId,
        room: RoomId,
        before: Option<MessageId>,
        limit: i64,
    ) -> Result<Vec<Message>> {
        if !self.rooms.is_member(room, who).await? {
            return Err(Error::Forbidden(format!(
                "{who} is not a member of room {room}"
            )));
        }
        let msgs = self.messages.list_recent(room, before, limit).await?;
        Ok(msgs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::MockBus;

    #[test]
    fn room_subject_is_well_formed() {
        let room = RoomId::new();
        let subject = ImService::room_subject(room);
        assert!(subject.starts_with("im.room."));
        assert!(subject.ends_with(&room.to_string()));
    }

    #[test]
    fn mock_bus_records_publish() {
        // Sanity check that the mock works the way the DB-integration tests rely on.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let bus = Arc::new(MockBus::default());
        rt.block_on(async {
            // Concrete-type call (not via `dyn`) — `publish_json` is allowed here.
            bus.publish_json("im.test", &serde_json::json!({"k": "v"}))
                .await
                .unwrap();
        });
        let log = bus.published.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].0, "im.test");
    }

    #[test]
    fn publish_event_helper_through_dyn() {
        // Ensures the object-safe helper actually flows through a `&dyn BusSink`.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let bus = Arc::new(MockBus::default());
        let dyn_bus: Arc<dyn super::BusSink> = bus.clone();
        rt.block_on(async {
            super::publish_event(
                dyn_bus.as_ref(),
                "im.events.room.created",
                &serde_json::json!({"ok": true}),
            )
            .await
            .unwrap();
        });
        let log = bus.published.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].0, "im.events.room.created");
        assert!(std::str::from_utf8(&log[0].1).unwrap().contains("\"ok\":true"));
    }
}
