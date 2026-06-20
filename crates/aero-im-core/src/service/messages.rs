//! Message operations — send, edit, delete, moderate-delete.
//!
//! Extracted from `service.rs` as part of REFACTOR_PLAN.md Step 1.
//! Uses `pub(crate)` fields/methods on `ImService` — these are module-internal
//! and should NOT be considered a public API.

use aero_common::{
    metrics::{self, names},
    Block, Error, Message, MessageEnvelope, MessageId, ParticipantId, Result, RoomEvent,
    RoomId, RoomKind, WorkspaceId,
};
use aero_storage::{message::NewMessage, AiJobKind};
use crate::{
    validate_blocks, ImService, ModerationVerdict,
};
use crate::service::orig::{
    spam_content_hash, per_tenant_metrics_enabled, WORKSPACE_NONE,
};
use tracing::{instrument, warn, Instrument};

impl ImService {
    /// Send a message into a room: access → moderator → spam → PII → persist →
    /// notify → AI embed/moderate → metrics.
    #[instrument(skip(self, blocks), fields(?sender, ?room))]
    pub async fn send_message(
        &self,
        sender: ParticipantId,
        room: RoomId,
        blocks: Vec<Block>,
        reply_to: Option<MessageId>,
        expires_at: Option<time::OffsetDateTime>,
    ) -> Result<Message> {
        let started = std::time::Instant::now();
        if !self.rooms.is_member(room, sender).await? {
            return Err(Error::Forbidden(format!(
                "sender {sender} is not a member of room {room}"
            )));
        }
        self.assert_can_post(sender, room).await?;
        validate_blocks(&blocks)?;
        if let ModerationVerdict::Block(reason) = self.moderator.check(&blocks) {
            return Err(Error::Invalid(reason));
        }

        // Behavioral spam/flood guard
        if let Some(guard) = self.spam_guard.as_ref() {
            let content_hash = spam_content_hash(&blocks);
            if let crate::SpamDecision::Throttle(reason) =
                guard.record(sender, room, content_hash, std::time::Instant::now()).await
            {
                tracing::warn!(%sender, %room, ?reason, "spam guard throttled");
                return Err(Error::RateLimited);
            }
        }

        // PII guard
        if let Some(detector) = self.pii_detector.as_ref() {
            let text = blocks
                .iter()
                .flat_map(|b| {
                    b.searchable_text().into_iter().chain(b.extra_searchable_text())
                })
                .collect::<Vec<_>>()
                .join("\n");
            let kinds = detector.scan(&text);
            if !kinds.is_empty() {
                let tags = kinds.iter().map(|k| k.tag()).collect::<Vec<_>>().join(", ");
                tracing::warn!(%sender, %room, pii = %tags, "PII guard blocked");
                return Err(Error::Invalid(format!(
                    "message blocked: it appears to contain sensitive personal information ({tags})"
                )));
            }
        }

        // Auto-mod rules
        if let Some(ref rule_repo) = self.auto_mod_rules.as_ref() {
            if let Ok(Some(workspace)) = self.rooms.room_workspace(room).await {
                if let Ok(rules) = rule_repo.list_for_workspace(workspace).await {
                    let text: String = blocks.iter().filter_map(|b| {
                        if let Block::Text { content, .. } = b { Some(content.as_str()) } else { None }
                    }).collect::<Vec<_>>().join(" ");
                    for rule in &rules {
                        if rule.action == "block" && rule.matches(&text) {
                            return Err(Error::Invalid("blocked by auto-mod rule".into()));
                        }
                    }
                }
            }
        }

        let message = self.messages.insert(NewMessage {
            room_id: room,
            sender_id: sender,
            blocks,
            reply_to,
            metadata: serde_json::Value::Null,
            expires_at,
        }).await?;

        let recipients = self.rooms.members(room).await.unwrap_or_else(|err| {
            warn!(?err, %room, "fetching recipients failed");
            Vec::new()
        });
        let envelope = MessageEnvelope {
            message: message.clone(),
            recipients: recipients.clone(),
        };

        self.publish_room_event(room, &RoomEvent::Message(envelope)).await;

        // Detached notification dispatch
        {
            let svc = self.clone();
            let msg = message.clone();
            let dispatch = tokio::spawn(
                async move { svc.dispatch_notifications(&msg, &recipients).await }
                    .in_current_span(),
            );
            #[cfg(test)]
            if let Err(err) = dispatch.await {
                warn!(?err, "notification dispatch panicked");
            }
            #[cfg(not(test))]
            drop(dispatch);
        }

        // AI embed + moderate jobs
        let searchable = message.searchable_text();
        if !searchable.is_empty() {
            let ws = self.rooms.room_workspace(room).await.ok().flatten().map(|w| w.to_uuid());
            if let Err(err) = self.ai_jobs.enqueue(
                AiJobKind::Embed, Some(message.id.to_uuid()), ws,
                serde_json::json!({"room_id": room.to_string()}),
            ).await { warn!(?err, "enqueue embed failed"); }
            if let Err(err) = self.ai_jobs.enqueue(
                AiJobKind::Moderate, Some(message.id.to_uuid()), ws,
                serde_json::json!({"text": searchable}),
            ).await { warn!(?err, "enqueue moderate failed"); }
        }

        // Metrics
        let room_type = match self.rooms.room_kind(room).await {
            Ok(Some(RoomKind::Direct)) => "direct",
            Ok(Some(RoomKind::Group)) => "group",
            Ok(Some(RoomKind::Channel)) => "channel",
            _ => "unknown",
        };
        metrics::inc_counter_labeled(names::MESSAGES_SENT_TOTAL, 1, &[("room_type", room_type)]);
        if per_tenant_metrics_enabled() {
            let ws_label = self.rooms.room_workspace(room).await
                .ok().flatten()
                .map_or_else(|| WORKSPACE_NONE.to_string(), |w| w.to_string());
            metrics::inc_counter_labeled(names::MESSAGES_SENT_TOTAL, 1, &[("workspace", ws_label.as_str())]);
        }
        metrics::observe_histogram_labeled(
            names::MESSAGE_PROCESSING_DURATION_SECONDS,
            started.elapsed().as_secs_f64(),
            &[("op", "send")],
        );
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
        let started = std::time::Instant::now();
        let existing = self.messages.get(id).await?
            .ok_or_else(|| Error::NotFound(format!("message {id}")))?;
        if existing.deleted_at.is_some() {
            return Err(Error::Conflict("message is deleted".into()));
        }
        if existing.sender_id != actor {
            return Err(Error::Forbidden("only sender may edit".into()));
        }
        validate_blocks(&blocks)?;

        if let Some(ref history) = self.message_edits {
            if let Ok(old) = serde_json::to_value(&existing.blocks) {
                if let Err(err) = history.record(id, actor, &old).await {
                    warn!(?err, %id, "record edit history failed");
                }
            }
        }

        let updated = self.messages.edit(id, blocks).await?
            .ok_or_else(|| Error::Conflict("edit raced with delete".into()))?;

        self.publish_room_event(updated.room_id, &RoomEvent::Edited(updated.clone())).await;

        let edit_text = updated.searchable_text();
        if !edit_text.is_empty() {
            let ws = self.rooms.room_workspace(updated.room_id).await
                .ok().flatten().map(|w| w.to_uuid());
            if let Err(err) = self.ai_jobs.enqueue(
                AiJobKind::Embed, Some(updated.id.to_uuid()), ws,
                serde_json::json!({"room_id": updated.room_id.to_string()}),
            ).await { warn!(?err, %id, "re-embed failed"); }
            if let Err(err) = self.ai_jobs.enqueue(
                AiJobKind::Moderate, Some(updated.id.to_uuid()), ws,
                serde_json::json!({"text": edit_text}),
            ).await { warn!(?err, %id, "re-moderate failed"); }
        }
        metrics::inc_counter(names::MESSAGES_EDITED_TOTAL, 1);
        metrics::observe_histogram_labeled(
            names::MESSAGE_PROCESSING_DURATION_SECONDS,
            started.elapsed().as_secs_f64(),
            &[("op", "edit")],
        );
        Ok(updated)
    }

    /// Soft-delete a message. Sender or room-owner may delete.
    #[instrument(skip(self), fields(?actor, ?id))]
    pub async fn delete_message(&self, actor: ParticipantId, id: MessageId) -> Result<()> {
        let started = std::time::Instant::now();
        let existing = self.messages.get(id).await?
            .ok_or_else(|| Error::NotFound(format!("message {id}")))?;
        if existing.deleted_at.is_some() { return Ok(()); }
        if existing.sender_id != actor {
            return Err(Error::Forbidden("only sender may delete in P2".into()));
        }
        self.messages.soft_delete(id).await?;
        self.publish_room_event(
            existing.room_id,
            &RoomEvent::Deleted { room_id: existing.room_id, message_id: id, by: actor },
        ).await;
        metrics::inc_counter(names::MESSAGES_DELETED_TOTAL, 1);
        metrics::observe_histogram_labeled(
            names::MESSAGE_PROCESSING_DURATION_SECONDS,
            started.elapsed().as_secs_f64(),
            &[("op", "delete")],
        );
        Ok(())
    }

    /// System action: soft-delete a message flagged by AI moderation.
    #[instrument(skip(self), fields(?message_id, reason))]
    pub async fn moderate_delete(
        &self,
        message_id: MessageId,
        workspace: Option<WorkspaceId>,
        reason: &str,
        digest: &str,
    ) -> Result<()> {
        let existing = self.messages.get(message_id).await?
            .ok_or_else(|| Error::NotFound(format!("message {message_id}")))?;
        if existing.deleted_at.is_some() { return Ok(()); }
        match workspace {
            Some(ws) => {
                let detail = serde_json::json!({
                    "room_id": existing.room_id, "reason": reason, "digest": digest,
                });
                self.messages.soft_delete_moderated(message_id, ws, detail).await?;
            }
            None => { self.messages.soft_delete(message_id).await?; }
        }
        warn!(%message_id, reason, "message removed by AI moderation");
        self.publish_room_event(
            existing.room_id,
            &RoomEvent::Deleted { room_id: existing.room_id, message_id, by: existing.sender_id },
        ).await;
        Ok(())
    }
}
