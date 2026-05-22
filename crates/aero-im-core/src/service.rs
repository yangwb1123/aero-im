//! `ImService` — the IM business facade.
//!
//! Coordinates the storage repositories and the event bus. All business
//! invariants (membership checks, validation, authorization) live here so
//! HTTP/WS handlers stay thin.
//!
//! See `docs/specs/2026-05-22-aero-im-design.md` §4.2 for the protocol contract.

use std::sync::Arc;

use aero_bus::traits::BusError;
use aero_bus::EventBus;
use aero_common::{
    Block, CallEvent, CallId, CallKind, CallMode, CallSession, Error, Message, MessageEnvelope,
    MessageId, ParticipantId, ReactionOp, ReactionSummary, ReadReceipt, Result, Room, RoomEvent,
    RoomId, RoomKind,
};
use aero_storage::{
    message::NewMessage, AiJobKind, AiJobRepo, CallRepo, MessageRepo, ParticipantRepo,
    ReactionRepo, ReceiptRepo, RoomRepo,
};
use async_trait::async_trait;
use std::collections::BTreeMap;
use tracing::{instrument, warn};

use crate::events::ImEvent;
use crate::moderator::{ModerationVerdict, Moderator};
use crate::validation::validate_blocks;

const EVENTS_SUBJECT: &str = "im.events";

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

async fn publish_event<T: serde::Serialize>(
    bus: &dyn BusSink,
    subject: &str,
    value: &T,
) -> std::result::Result<(), BusError> {
    let bytes = serde_json::to_vec(value)?;
    bus.publish_bytes(subject, bytes.into()).await
}

/// IM business facade. Cheap to clone (repositories wrap `Arc<PgPool>`).
#[derive(Clone)]
pub struct ImService {
    rooms: RoomRepo,
    messages: MessageRepo,
    #[allow(dead_code)] // Reserved for mention lookups, P3+.
    participants: ParticipantRepo,
    receipts: ReceiptRepo,
    reactions: ReactionRepo,
    calls: CallRepo,
    ai_jobs: AiJobRepo,
    bus: Arc<dyn BusSink>,
    moderator: Arc<dyn Moderator>,
}

impl ImService {
    /// Construct from any [`EventBus`] implementation. Repositories are passed
    /// in to keep this crate independent of `PgPool`/Redis types.
    ///
    /// Picks a default moderator from `AERO_BLOCKED_WORDS` env at construction
    /// time, or `AllowAllModerator` if unset.
    #[allow(clippy::too_many_arguments)]
    pub fn new<B: EventBus + 'static>(
        rooms: RoomRepo,
        messages: MessageRepo,
        participants: ParticipantRepo,
        receipts: ReceiptRepo,
        reactions: ReactionRepo,
        calls: CallRepo,
        ai_jobs: AiJobRepo,
        bus: Arc<B>,
    ) -> Self {
        let moderator: Arc<dyn Moderator> = match crate::moderator::KeywordModerator::from_env() {
            Some(m) => Arc::new(m),
            None => Arc::new(crate::moderator::AllowAllModerator),
        };
        Self {
            rooms,
            messages,
            participants,
            receipts,
            reactions,
            calls,
            ai_jobs,
            bus: bus as Arc<dyn BusSink>,
            moderator,
        }
    }

    /// Inject a custom moderator (overrides env default).
    #[must_use]
    pub fn with_moderator(mut self, moderator: Arc<dyn Moderator>) -> Self {
        self.moderator = moderator;
        self
    }

    /// Lower-level constructor for tests with a pre-built bus sink.
    #[allow(clippy::too_many_arguments)]
    pub fn from_sink(
        rooms: RoomRepo,
        messages: MessageRepo,
        participants: ParticipantRepo,
        receipts: ReceiptRepo,
        reactions: ReactionRepo,
        calls: CallRepo,
        ai_jobs: AiJobRepo,
        bus: Arc<dyn BusSink>,
    ) -> Self {
        Self {
            rooms,
            messages,
            participants,
            receipts,
            reactions,
            calls,
            ai_jobs,
            bus,
            moderator: Arc::new(crate::moderator::AllowAllModerator),
        }
    }

    /// NATS subject used for per-room broadcast (see spec §4.2).
    #[must_use]
    pub fn room_subject(room: RoomId) -> String {
        format!("im.room.{room}")
    }

    // ---------------------------------------------------------- ROOM lifecycle

    /// Create a new room.
    #[instrument(skip(self), fields(?creator, ?kind))]
    pub async fn create_room(
        &self,
        creator: ParticipantId,
        kind: RoomKind,
        name: Option<String>,
    ) -> Result<Room> {
        let room = self.rooms.create(kind, name, creator).await?;
        let event = ImEvent::RoomCreated(room.clone());
        if let Err(err) = publish_event(
            self.bus.as_ref(),
            &format!("{EVENTS_SUBJECT}.room.created"),
            &event,
        )
        .await
        {
            warn!(?err, room_id = %room.id, "publish RoomCreated failed");
        }
        Ok(room)
    }

    /// Add a member to a room. `actor` must themselves be a member.
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
            warn!(?err, %room, %member, "publish MemberAdded failed");
        }
        Ok(())
    }

    /// List rooms the participant belongs to.
    #[instrument(skip(self), fields(?who))]
    pub async fn list_my_rooms(&self, who: ParticipantId) -> Result<Vec<Room>> {
        Ok(self.rooms.rooms_for(who).await?)
    }

    /// Snapshot the current member set of a room (for callee fan-out, etc).
    pub async fn room_members(&self, room: RoomId) -> Result<Vec<ParticipantId>> {
        Ok(self.rooms.members(room).await?)
    }

    // ---------------------------------------------------------- MESSAGES

    /// Persist a new message and broadcast it. Returns the persisted message.
    /// Bus failures are *not* fatal — the message is durable in PG.
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
        if !self.rooms.is_member(room, sender).await? {
            return Err(Error::Forbidden(format!(
                "sender {sender} is not a member of room {room}"
            )));
        }
        validate_blocks(&blocks)?;
        if let ModerationVerdict::Block(reason) = self.moderator.check(&blocks) {
            return Err(Error::Invalid(reason));
        }

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

        let recipients = self.rooms.members(room).await.unwrap_or_else(|err| {
            warn!(?err, %room, "fetching recipients failed; publishing without fan-out hint");
            Vec::new()
        });
        let envelope = MessageEnvelope { message: message.clone(), recipients };

        self.publish_room_event(room, &RoomEvent::Message(envelope)).await;

        // Best-effort enqueue an embed job (AI worker will pick it up).
        if !message.searchable_text().is_empty() {
            if let Err(err) = self
                .ai_jobs
                .enqueue(
                    AiJobKind::Embed,
                    Some(message.id.to_uuid()),
                    serde_json::json!({"room_id": room.to_string()}),
                )
                .await
            {
                warn!(?err, message_id = %message.id, "enqueue embed job failed");
            }
        }

        Ok(message)
    }

    /// Edit a message. Only the sender may edit; soft-deleted messages refuse.
    #[instrument(skip(self, blocks), fields(?actor, ?id))]
    pub async fn edit_message(
        &self,
        actor: ParticipantId,
        id: MessageId,
        blocks: Vec<Block>,
    ) -> Result<Message> {
        let existing = self
            .messages
            .get(id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("message {id}")))?;
        if existing.deleted_at.is_some() {
            return Err(Error::Conflict("message is deleted".into()));
        }
        if existing.sender_id != actor {
            return Err(Error::Forbidden("only sender may edit".into()));
        }
        validate_blocks(&blocks)?;

        let updated = self
            .messages
            .edit(id, blocks)
            .await?
            .ok_or_else(|| Error::Conflict("edit raced with delete".into()))?;

        self.publish_room_event(updated.room_id, &RoomEvent::Edited(updated.clone()))
            .await;

        if !updated.searchable_text().is_empty() {
            if let Err(err) = self
                .ai_jobs
                .enqueue(
                    AiJobKind::Embed,
                    Some(updated.id.to_uuid()),
                    serde_json::json!({"room_id": updated.room_id.to_string()}),
                )
                .await
            {
                warn!(?err, %id, "enqueue re-embed failed");
            }
        }
        Ok(updated)
    }

    /// Soft-delete a message. Sender or room-owner may delete.
    #[instrument(skip(self), fields(?actor, ?id))]
    pub async fn delete_message(&self, actor: ParticipantId, id: MessageId) -> Result<()> {
        let existing = self
            .messages
            .get(id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("message {id}")))?;
        if existing.deleted_at.is_some() {
            return Ok(());
        }
        if existing.sender_id != actor && !self.rooms.is_member(existing.room_id, actor).await? {
            return Err(Error::Forbidden("only sender or room member may delete".into()));
        }
        if existing.sender_id != actor {
            return Err(Error::Forbidden("only sender may delete in P2".into()));
        }
        self.messages.soft_delete(id).await?;
        self.publish_room_event(
            existing.room_id,
            &RoomEvent::Deleted {
                room_id: existing.room_id,
                message_id: id,
                by: actor,
            },
        )
        .await;
        Ok(())
    }

    /// Paginated history. `before` is exclusive.
    #[instrument(skip(self), fields(?who, ?room, ?before, limit))]
    pub async fn history(
        &self,
        who: ParticipantId,
        room: RoomId,
        before: Option<MessageId>,
        limit: i64,
    ) -> Result<Vec<Message>> {
        if !self.rooms.is_member(room, who).await? {
            return Err(Error::Forbidden(format!("{who} is not a member of room {room}")));
        }
        Ok(self.messages.list_recent(room, before, limit).await?)
    }

    /// Fetch reaction aggregates for a batch of messages.
    pub async fn reactions_for(
        &self,
        message_ids: &[MessageId],
    ) -> Result<BTreeMap<MessageId, Vec<ReactionSummary>>> {
        Ok(self.reactions.summaries_for(message_ids).await?)
    }

    // ---------------------------------------------------------- REACTIONS

    /// Toggle a reaction on a message. Caller must be a member of the message's room.
    #[instrument(skip(self), fields(?actor, ?message_id, emoji))]
    pub async fn toggle_reaction(
        &self,
        actor: ParticipantId,
        message_id: MessageId,
        emoji: &str,
    ) -> Result<ReactionOp> {
        let msg = self
            .messages
            .get(message_id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("message {message_id}")))?;
        if !self.rooms.is_member(msg.room_id, actor).await? {
            return Err(Error::Forbidden("not a room member".into()));
        }
        if emoji.is_empty() || emoji.len() > 32 {
            return Err(Error::Invalid("emoji length".into()));
        }
        let op = self.reactions.toggle(message_id, actor, emoji).await?;
        self.publish_room_event(
            msg.room_id,
            &RoomEvent::Reaction {
                room_id: msg.room_id,
                message_id,
                participant: actor,
                emoji: emoji.to_owned(),
                op,
            },
        )
        .await;
        Ok(op)
    }

    // ---------------------------------------------------------- READ RECEIPTS

    /// Move a participant's read cursor in a room.
    #[instrument(skip(self), fields(?actor, ?room, ?last_read))]
    pub async fn mark_read(
        &self,
        actor: ParticipantId,
        room: RoomId,
        last_read: MessageId,
    ) -> Result<ReadReceipt> {
        if !self.rooms.is_member(room, actor).await? {
            return Err(Error::Forbidden("not a room member".into()));
        }
        let receipt = self.receipts.mark_read(room, actor, last_read).await?;
        self.publish_room_event(
            room,
            &RoomEvent::Read {
                room_id: room,
                participant: actor,
                last_message_id: last_read,
                at: receipt.updated_at,
            },
        )
        .await;
        Ok(receipt)
    }

    pub async fn receipts_for(&self, room: RoomId) -> Result<Vec<ReadReceipt>> {
        Ok(self.receipts.list_for_room(room).await?)
    }

    // ---------------------------------------------------------- TYPING

    /// Broadcast a typing indicator. No persistence.
    pub async fn typing(
        &self,
        actor: ParticipantId,
        room: RoomId,
        on: bool,
    ) -> Result<()> {
        if !self.rooms.is_member(room, actor).await? {
            return Err(Error::Forbidden("not a room member".into()));
        }
        self.publish_room_event(
            room,
            &RoomEvent::Typing { room_id: room, participant: actor, on },
        )
        .await;
        Ok(())
    }

    // ---------------------------------------------------------- CALLS (P3)

    /// Start a call session (1:1 or group) and broadcast an invite to the callees.
    #[instrument(skip(self, sdp), fields(?initiator, ?room, ?kind, ?mode))]
    pub async fn start_call(
        &self,
        initiator: ParticipantId,
        room: RoomId,
        kind: CallKind,
        mode: CallMode,
        sdp: String,
    ) -> Result<CallSession> {
        if !self.rooms.is_member(room, initiator).await? {
            return Err(Error::Forbidden("not a room member".into()));
        }
        let mut callees = self.rooms.members(room).await?;
        callees.retain(|p| *p != initiator);
        let call_id = CallId::new();
        let session = self
            .calls
            .start(call_id, room, initiator, kind, mode, &callees)
            .await?;
        self.publish_room_event(
            room,
            &RoomEvent::Call(CallEvent::Invite {
                call_id,
                room_id: room,
                from: initiator,
                to: callees,
                kind,
                sdp,
            }),
        )
        .await;
        Ok(session)
    }

    /// Forward an answer/ICE/end signaling event to the targeted participant(s).
    /// `room` is required for routing on the per-room subject.
    pub async fn relay_call_event(&self, room: RoomId, event: CallEvent) -> Result<()> {
        if let CallEvent::End { call_id, reason, .. } = &event {
            if let Err(err) = self.calls.end(*call_id, reason).await {
                warn!(?err, %call_id, "persist call end failed");
            }
        }
        self.publish_room_event(room, &RoomEvent::Call(event)).await;
        Ok(())
    }

    // ---------------------------------------------------------- internal

    async fn publish_room_event(&self, room: RoomId, event: &RoomEvent) {
        let subject = Self::room_subject(room);
        if let Err(err) = publish_event(self.bus.as_ref(), &subject, event).await {
            warn!(?err, %subject, "publish RoomEvent failed");
        }
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
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let bus = Arc::new(MockBus::default());
        rt.block_on(async {
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
