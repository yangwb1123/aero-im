//! Reaction operations — toggle, list aggregates.
//!
//! Extracted from `service.rs` as part of REFACTOR_PLAN.md Step 1b.

use aero_common::{
    MessageId, NotificationKind, ParticipantId, ReactionOp, ReactionSummary, Result, RoomEvent,
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

    /// Batch reactions scoped to what `viewer` may see — only messages in rooms
    /// they belong to (the storage `JOIN room_members` is the boundary). Use this
    /// for any caller-facing batch lookup so reaction counts / reactor ids never
    /// leak across rooms.
    ///
    /// # Errors
    /// Propagates any storage error.
    pub async fn reactions_for_accessible(
        &self,
        viewer: ParticipantId,
        message_ids: &[MessageId],
    ) -> Result<BTreeMap<MessageId, Vec<ReactionSummary>>> {
        Ok(self
            .reactions
            .summaries_for_accessible(viewer, message_ids)
            .await?)
    }

    /// Toggle a reaction through the transaction-owned storage path, then make a
    /// best-effort immediate attempt to dispatch its durable outbox event.
    #[instrument(skip(self), fields(?actor, ?message_id, emoji))]
    pub async fn toggle_reaction(
        &self,
        actor: ParticipantId,
        message_id: MessageId,
        emoji: &str,
    ) -> Result<ReactionOp> {
        let traceparent = aero_common::telemetry::current_traceparent();
        let toggled = self
            .reactions
            .toggle_authorized_outboxed(message_id, actor, emoji, traceparent.as_deref())
            .await?;
        if let Err(error) = self.dispatch_event_outbox_id(toggled.outbox_id).await {
            warn!(
                ?error,
                outbox_id = %toggled.outbox_id,
                message_id = %message_id,
                "fast reaction-event outbox dispatch failed"
            );
        }

        // Reaction notification: a freshly-ADDED reaction to someone else's message
        // drops a durable inbox entry for the author (never self-notify on your own
        // reaction). Best-effort + gated like every other notification (mute / DND /
        // snooze via `should_notify`); only when a NotificationRepo is wired.
        if toggled.op == ReactionOp::Add && actor != toggled.message_sender {
            if let Some(repo) = self.notifications.as_ref() {
                if self
                    .should_notify(toggled.message_sender, toggled.room_id)
                    .await
                {
                    if let Err(err) = repo
                        .insert(
                            toggled.message_sender,
                            toggled.room_id,
                            message_id,
                            NotificationKind::Reaction,
                            Some(actor),
                        )
                        .await
                    {
                        warn!(
                            ?err,
                            recipient = ?toggled.message_sender,
                            "persist reaction notification failed"
                        );
                    } else {
                        self.publish_room_event(
                            toggled.room_id,
                            &RoomEvent::Notify {
                                room_id: toggled.room_id,
                                message_id,
                                mentioned: toggled.message_sender,
                                by: actor,
                                kind: NotificationKind::Reaction,
                            },
                        )
                        .await;
                    }
                }
            }
        }
        Ok(toggled.op)
    }
}
