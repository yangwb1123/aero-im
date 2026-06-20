//! Reaction operations — toggle, list aggregates.
//!
//! Extracted from `service.rs` as part of REFACTOR_PLAN.md Step 1b.

use aero_common::{
    Error, MessageId, NotificationKind, ParticipantId, ReactionOp, ReactionSummary, Result,
    RoomEvent,
};
use std::collections::BTreeMap;
use tracing::{instrument, warn};

use crate::ImService;

impl ImService {
    /// Fetch reaction aggregates for a batch of messages.
    pub async fn reactions_for(
        &self,
        message_ids: &[MessageId],
    ) -> Result<BTreeMap<MessageId, Vec<ReactionSummary>>> {
        Ok(self.reactions.summaries_for(message_ids).await?)
    }

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
        // Reaction spam limit (ROADMAP12 migration 0118): the per-user distinct-emoji
        // cap is enforced ATOMICALLY inside `toggle_capped` (advisory-locked on
        // (message, participant)), so two concurrent new-emoji Adds can't both pass a
        // check-then-insert race. A Remove is always allowed; `None` cap = unlimited.
        // Best-effort cap lookup: a missing room row / lookup error → no cap.
        let cap = self
            .rooms
            .get_max_reactions_per_user(msg.room_id)
            .await
            .ok()
            .flatten()
            .map(i64::from);
        let op = match self.reactions.toggle_capped(message_id, actor, emoji, cap).await? {
            Some(op) => op,
            None => {
                return Err(Error::Invalid(format!(
                    "reaction limit: max {} reactions per message",
                    cap.unwrap_or(0)
                )));
            }
        };
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
        // Reaction notification: a freshly-ADDED reaction to someone else's message
        // drops a durable inbox entry for the author (never self-notify on your own
        // reaction). Best-effort + gated like every other notification (mute / DND /
        // snooze via `should_notify`); only when a NotificationRepo is wired.
        if op == ReactionOp::Add && actor != msg.sender_id {
            if let Some(repo) = self.notifications.as_ref() {
                if self.should_notify(msg.sender_id, msg.room_id).await {
                    if let Err(err) = repo
                        .insert(
                            msg.sender_id,
                            msg.room_id,
                            message_id,
                            NotificationKind::Reaction,
                            Some(actor),
                        )
                        .await
                    {
                        warn!(?err, recipient = ?msg.sender_id, "persist reaction notification failed");
                    } else {
                        self.publish_room_event(
                            msg.room_id,
                            &RoomEvent::Notify {
                                room_id: msg.room_id,
                                message_id,
                                mentioned: msg.sender_id,
                                by: actor,
                                kind: NotificationKind::Reaction,
                            },
                        )
                        .await;
                    }
                }
            }
        }
        Ok(op)
    }
}
