//! Durable room-event relay.
//!
//! Message creation stores an unstamped [`RoomEvent`](aero_common::RoomEvent)
//! beside the message in PostgreSQL. These methods claim committed rows, assign
//! one stable per-subject sequence, and publish with the row's stable event id as
//! the NATS de-duplication key.

use aero_common::{
    metrics::{self, names},
    Error, Message, MessageEnvelope, Result, RoomEvent,
};
use aero_storage::{event_outbox::EventOutboxKind, EventOutboxRepo, EventOutboxRow};
use anyhow::{anyhow, Context};
use time::{Duration, OffsetDateTime};
use tracing::{debug, warn};

use crate::service::ImService;

const OUTBOX_LEASE: Duration = Duration::seconds(30);

impl ImService {
    /// Claim and relay up to `limit` due message events.
    ///
    /// Publication failures are durably re-parked with exponential backoff and
    /// therefore do not fail the whole batch. A claim-query failure is returned
    /// so the supervising loop can surface database outages.
    pub async fn dispatch_event_outbox_batch(&self, limit: i64) -> Result<usize> {
        let repo = EventOutboxRepo::new(self.messages.pool.clone());
        let rows = repo
            .claim_due(OffsetDateTime::now_utc(), OUTBOX_LEASE, limit)
            .await
            .map_err(Error::from)?;
        let mut published = 0;
        for row in rows {
            if self.finish_claimed_outbox(&repo, row).await {
                published += 1;
            }
        }
        Ok(published)
    }

    /// Relay one known row immediately after its enclosing business transaction
    /// commits. Returning `false` is harmless: the background batch relay owns the
    /// durable retry path.
    pub async fn dispatch_event_outbox_id(&self, id: uuid::Uuid) -> Result<bool> {
        let repo = EventOutboxRepo::new(self.messages.pool.clone());
        let Some(row) = repo
            .claim_by_id(id, OffsetDateTime::now_utc(), OUTBOX_LEASE)
            .await
            .map_err(Error::from)?
        else {
            return Ok(false);
        };
        Ok(self.finish_claimed_outbox(&repo, row).await)
    }

    async fn finish_claimed_outbox(&self, repo: &EventOutboxRepo, row: EventOutboxRow) -> bool {
        let id = row.id;
        let event_id = row.event_id;
        let message_id = row.message_id;
        let attempts = row.attempts;
        match self.publish_claimed_outbox(repo, &row).await {
            Ok(()) => {
                debug!(%id, %event_id, %message_id, attempts, "room-event outbox event published");
                true
            }
            Err(error) => {
                let now = OffsetDateTime::now_utc();
                match repo
                    .mark_failed(id, attempts, now, &error.to_string())
                    .await
                {
                    Err(mark_error) => warn!(
                        error = ?mark_error,
                        publish_error = %error,
                        %id,
                        %event_id,
                        %message_id,
                        attempts,
                        "message outbox publish failed and claim could not be re-parked"
                    ),
                    Ok(true) => warn!(
                        error = %error,
                        %id,
                        %event_id,
                        %message_id,
                        attempts,
                        "message outbox publish failed; retry scheduled"
                    ),
                    Ok(false) => debug!(
                        error = %error,
                        %id,
                        %event_id,
                        %message_id,
                        attempts,
                        "message outbox failure belongs to a superseded lease"
                    ),
                }
                false
            }
        }
    }

    async fn publish_claimed_outbox(
        &self,
        repo: &EventOutboxRepo,
        row: &EventOutboxRow,
    ) -> anyhow::Result<()> {
        // Canvas operations use this mature room-event queue but are not message
        // aggregates. Their immutable, transaction-validated payload is already
        // canonical and must not be suppressed merely because the compatibility
        // `message_id` column contains the op id.
        let current = if row.event_kind == EventOutboxKind::CanvasOp {
            None
        } else {
            self.messages
                .get(row.message_id)
                .await
                .context("read current message before outbox publish")?
        };
        let Some(mut payload) =
            materialize_outbox_payload(row, current.as_ref(), OffsetDateTime::now_utc())
                .context("materialize current outbox payload")?
        else {
            let marked = repo
                .mark_published(row.id, row.attempts, OffsetDateTime::now_utc())
                .await
                .context("complete suppressed outbox event")?;
            if !marked {
                return Err(anyhow!("suppressed outbox event lease was superseded"));
            }
            debug!(
                outbox_id = %row.id,
                event_id = %row.event_id,
                message_id = %row.message_id,
                aggregate_version = row.aggregate_version,
                kind = row.event_kind.as_str(),
                "suppressed stale message outbox event"
            );
            return Ok(());
        };
        if row.event_kind == EventOutboxKind::Notify {
            let room_id = current
                .as_ref()
                .map(|message| message.room_id)
                .ok_or_else(|| anyhow!("notify outbox message disappeared"))?;
            let members = self
                .rooms
                .members(room_id)
                .await
                .context("revalidate notify recipients")?;
            let Some(filtered) =
                filter_notify_recipients(&payload, &members).context("filter notify recipients")?
            else {
                let marked = repo
                    .mark_published(row.id, row.attempts, OffsetDateTime::now_utc())
                    .await
                    .context("complete empty notify outbox event")?;
                if !marked {
                    return Err(anyhow!("empty notify outbox event lease was superseded"));
                }
                return Ok(());
            };
            payload = filtered;
        }

        let seq = match row.seq {
            Some(seq) => Some(seq),
            None => match self.seq.next_seq(&row.subject).await {
                Some(candidate) => Some(
                    repo.assign_seq_if_absent(row.id, candidate)
                        .await
                        .context("persist outbox event sequence")?
                        .ok_or_else(|| anyhow!("outbox row vanished before sequence assignment"))?,
                ),
                // Preserve the established fail-open sequence contract: Redis
                // unavailability may create a gap/unstamped event but never stalls
                // message delivery.
                None => None,
            },
        };

        let bytes = outbox_wire_bytes(
            &payload,
            row.event_id,
            row.aggregate_version,
            seq,
            row.traceparent.as_deref(),
        )
        .context("serialize outbox event")?;
        if let Err(error) = self
            .bus
            .publish_bytes_idempotent(&row.subject, bytes.into(), &row.event_id.to_string())
            .await
        {
            metrics::inc_counter(names::NATS_PUBLISH_ERRORS_TOTAL, 1);
            return Err(anyhow!(error).context("publish outbox event to NATS"));
        }

        let marked = repo
            .mark_published(row.id, row.attempts, OffsetDateTime::now_utc())
            .await
            .context("mark outbox event published")?;
        if !marked {
            return Err(anyhow!("published outbox row was no longer pending"));
        }
        Ok(())
    }
}

/// Rebuild a delayed event from current durable state, or suppress it when a
/// newer mutation made the original event obsolete.
fn materialize_outbox_payload(
    row: &EventOutboxRow,
    current: Option<&Message>,
    now: OffsetDateTime,
) -> serde_json::Result<Option<serde_json::Value>> {
    let current_live = current.filter(|message| {
        message.deleted_at.is_none()
            && message
                .expires_at
                .map_or(true, |expires_at| expires_at > now)
    });
    match row.event_kind {
        EventOutboxKind::Message => {
            let Some(message) = current_live else {
                return Ok(None);
            };
            let original: RoomEvent = serde_json::from_value(row.payload.clone())?;
            let client_message_id = match original {
                RoomEvent::Message(envelope) => envelope.client_message_id,
                _ => None,
            };
            serde_json::to_value(RoomEvent::Message(MessageEnvelope {
                message: message.clone(),
                delivery_ordinal: row.delivery_ordinal,
                client_message_id,
                // Current membership is resolved by the consumer. Persisting a
                // send-time snapshot could leak a delayed event to a departed
                // member after a long broker outage.
                recipients: Vec::new(),
            }))
            .map(Some)
        }
        EventOutboxKind::Edited => {
            let Some(message) = current_live else {
                return Ok(None);
            };
            let original: RoomEvent = serde_json::from_value(row.payload.clone())?;
            let original_version = match original {
                RoomEvent::Edited(message) => message.version,
                _ => return Ok(None),
            };
            if message.version > original_version {
                return Ok(None);
            }
            serde_json::to_value(RoomEvent::Edited(message.clone())).map(Some)
        }
        EventOutboxKind::Deleted | EventOutboxKind::CanvasOp => Ok(Some(row.payload.clone())),
        EventOutboxKind::Notify | EventOutboxKind::Reaction => {
            Ok(current_live.map(|_| row.payload.clone()))
        }
        EventOutboxKind::Recalled => {
            // Mirror the Edited semantics: a delayed recall event is suppressed
            // when a later mutation (delete, or any post-recall version bump)
            // already superseded it, or when the message is no longer live.
            let Some(message) = current_live else {
                return Ok(None);
            };
            let original: RoomEvent = serde_json::from_value(row.payload.clone())?;
            let original_version = match original {
                RoomEvent::Recalled(message) => message.version,
                _ => return Ok(None),
            };
            if message.version > original_version {
                return Ok(None);
            }
            serde_json::to_value(RoomEvent::Recalled(message.clone())).map(Some)
        }
    }
}

fn filter_notify_recipients(
    payload: &serde_json::Value,
    current_members: &[aero_common::ParticipantId],
) -> serde_json::Result<Option<serde_json::Value>> {
    let event: RoomEvent = serde_json::from_value(payload.clone())?;
    let RoomEvent::NotifyBatch {
        room_id,
        message_id,
        by,
        delivery_id,
        mut recipients,
    } = event
    else {
        return Ok(None);
    };
    let members = current_members
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    recipients.retain(|target| members.contains(&target.participant));
    if recipients.is_empty() {
        return Ok(None);
    }
    serde_json::to_value(RoomEvent::NotifyBatch {
        room_id,
        message_id,
        by,
        delivery_id,
        recipients,
    })
    .map(Some)
}

fn outbox_wire_bytes(
    payload: &serde_json::Value,
    event_id: uuid::Uuid,
    aggregate_version: i64,
    seq: Option<u64>,
    traceparent: Option<&str>,
) -> serde_json::Result<Vec<u8>> {
    let mut value = payload.clone();
    if let serde_json::Value::Object(object) = &mut value {
        object.insert(
            "event_id".into(),
            serde_json::Value::String(event_id.to_string()),
        );
        object.insert(
            "aggregate_version".into(),
            serde_json::Value::Number(aggregate_version.into()),
        );
    }
    aero_bus::stamp_seq(&mut value, seq);
    aero_bus::stamp_traceparent(&mut value, traceparent);
    serde_json::to_vec(&value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::{
        Block, MessageId, NotificationKind, NotifyTarget, ParticipantId, ReactionOp, RoomId,
    };

    fn row(kind: EventOutboxKind, payload: serde_json::Value) -> EventOutboxRow {
        EventOutboxRow {
            id: uuid::Uuid::new_v4(),
            event_id: uuid::Uuid::new_v4(),
            message_id: MessageId::new(),
            event_kind: kind,
            aggregate_version: 1,
            delivery_ordinal: Some(17),
            subject: "im.room.test".into(),
            payload,
            traceparent: None,
            seq: None,
            attempts: 1,
            available_at: OffsetDateTime::UNIX_EPOCH,
            claimed_at: None,
            published_at: None,
            last_error: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn message(version: i32, text: &str) -> Message {
        Message {
            id: MessageId::new(),
            room_id: RoomId::new(),
            sender_id: ParticipantId::new(),
            blocks: vec![Block::text(text)],
            reply_to: None,
            metadata: serde_json::Value::Null,
            created_at: OffsetDateTime::UNIX_EPOCH,
            edited_at: None,
            deleted_at: None,
            recalled_at: None,
            recalled_by: None,
            expires_at: None,
            version,
        }
    }

    #[test]
    fn relay_stamps_persisted_metadata_without_mutating_stored_payload() {
        let stored = serde_json::json!({
            "kind": "deleted",
            "room_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "message_id": "01ARZ3NDEKTSV4RRFFQ69G5FAW",
            "by": "01ARZ3NDEKTSV4RRFFQ69G5FAX"
        });
        let event_id = uuid::Uuid::new_v4();
        let bytes = outbox_wire_bytes(
            &stored,
            event_id,
            3,
            Some(42),
            Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        )
        .unwrap();
        let wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(wire["event_id"], event_id.to_string());
        assert_eq!(wire["aggregate_version"], 3);
        assert_eq!(wire["seq"], 42);
        assert_eq!(
            wire["traceparent"],
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        );
        assert!(stored.get("seq").is_none());
        assert!(stored.get("traceparent").is_none());
        assert!(stored.get("event_id").is_none());
        assert!(stored.get("aggregate_version").is_none());
    }

    #[test]
    fn identical_retry_metadata_produces_identical_wire_bytes() {
        let stored = serde_json::json!({"kind": "message", "envelope": {"x": 1}});
        let event_id = uuid::Uuid::new_v4();
        let first = outbox_wire_bytes(&stored, event_id, 4, Some(7), Some("trace")).unwrap();
        let retry = outbox_wire_bytes(&stored, event_id, 4, Some(7), Some("trace")).unwrap();
        assert_eq!(first, retry);
    }

    #[test]
    fn delayed_create_is_rebuilt_from_current_blocks_without_recipient_snapshot() {
        let old = message(1, "secret-old");
        let mut current = old.clone();
        current.blocks = vec![Block::text("current")];
        current.version = 2;
        let source = serde_json::to_value(RoomEvent::Message(MessageEnvelope {
            message: old,
            delivery_ordinal: Some(17),
            client_message_id: Some(uuid::Uuid::new_v4()),
            recipients: vec![ParticipantId::new()],
        }))
        .unwrap();
        let materialized = materialize_outbox_payload(
            &row(EventOutboxKind::Message, source),
            Some(&current),
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap()
        .expect("live create is delivered");
        let RoomEvent::Message(envelope) = serde_json::from_value(materialized).unwrap() else {
            panic!("message event");
        };
        assert_eq!(
            serde_json::to_value(&envelope.message.blocks).unwrap(),
            serde_json::to_value(&current.blocks).unwrap()
        );
        assert!(envelope.recipients.is_empty());
        assert!(!serde_json::to_string(&envelope)
            .unwrap()
            .contains("secret-old"));
    }

    #[test]
    fn stale_edit_and_any_predelete_payload_are_suppressed() {
        let old = message(1, "old");
        let current = message(2, "new");
        let edit = row(
            EventOutboxKind::Edited,
            serde_json::to_value(RoomEvent::Edited(old.clone())).unwrap(),
        );
        assert!(
            materialize_outbox_payload(&edit, Some(&current), OffsetDateTime::UNIX_EPOCH)
                .unwrap()
                .is_none()
        );

        let mut deleted = current;
        deleted.deleted_at = Some(OffsetDateTime::UNIX_EPOCH);
        let create = row(
            EventOutboxKind::Message,
            serde_json::to_value(RoomEvent::Message(MessageEnvelope {
                message: old,
                delivery_ordinal: Some(17),
                client_message_id: None,
                recipients: Vec::new(),
            }))
            .unwrap(),
        );
        assert!(
            materialize_outbox_payload(&create, Some(&deleted), OffsetDateTime::UNIX_EPOCH)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn recalled_payload_is_delivered_at_version_and_suppressed_when_superseded() {
        // Gap ① (plan §5.1): the Recalled materialization arm must mirror the
        // Edited semantics — deliver the placeholder event while the live row
        // still carries the recall version, suppress it once a later mutation
        // (post-recall edit would be fenced, but delete/version bump) moved on.
        let mut recalled = message(2, "[此消息已被撤回]");
        recalled.recalled_at = Some(OffsetDateTime::UNIX_EPOCH);
        recalled.recalled_by = Some(ParticipantId::new());
        let payload = serde_json::to_value(RoomEvent::Recalled(recalled.clone())).unwrap();
        let recalled_row = row(EventOutboxKind::Recalled, payload.clone());

        // Live row at the same version → the recall event is delivered with the
        // current (placeholder) blocks, not the send-time original.
        let materialized = materialize_outbox_payload(
            &recalled_row,
            Some(&recalled),
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap()
        .expect("recall at matching version is delivered");
        let RoomEvent::Recalled(delivered) = serde_json::from_value(materialized).unwrap() else {
            panic!("recalled event");
        };
        assert_eq!(delivered.version, 2);
        assert!(delivered.recalled_at.is_some());
        assert_eq!(
            serde_json::to_value(&delivered.blocks).unwrap(),
            serde_json::to_value(&recalled.blocks).unwrap(),
            "materialized recall carries the current placeholder blocks"
        );

        // A later mutation bumped the version past the recall → suppressed.
        let mut superseded = recalled.clone();
        superseded.version = 3;
        assert!(
            materialize_outbox_payload(&recalled_row, Some(&superseded), OffsetDateTime::UNIX_EPOCH)
                .unwrap()
                .is_none(),
            "recall superseded by a later mutation is suppressed"
        );

        // Message tombstoned → suppressed (Deleted frame already conveys it).
        let mut deleted = recalled.clone();
        deleted.deleted_at = Some(OffsetDateTime::UNIX_EPOCH);
        assert!(
            materialize_outbox_payload(&recalled_row, Some(&deleted), OffsetDateTime::UNIX_EPOCH)
                .unwrap()
                .is_none()
        );

        // Row gone entirely → suppressed.
        assert!(
            materialize_outbox_payload(&recalled_row, None, OffsetDateTime::UNIX_EPOCH)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reaction_payload_is_durable_for_live_message_and_suppressed_after_delete() {
        let current = message(1, "reactable");
        let payload = serde_json::to_value(RoomEvent::Reaction {
            room_id: current.room_id,
            message_id: current.id,
            participant: ParticipantId::new(),
            emoji: "✅".into(),
            op: ReactionOp::Add,
        })
        .unwrap();
        let reaction = row(EventOutboxKind::Reaction, payload.clone());
        assert_eq!(
            materialize_outbox_payload(&reaction, Some(&current), OffsetDateTime::UNIX_EPOCH)
                .unwrap(),
            Some(payload)
        );

        let mut deleted = current;
        deleted.deleted_at = Some(OffsetDateTime::UNIX_EPOCH);
        assert!(
            materialize_outbox_payload(&reaction, Some(&deleted), OffsetDateTime::UNIX_EPOCH)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn canvas_op_payload_is_delivered_without_a_message_aggregate() {
        let payload = serde_json::json!({
            "kind": "canvas_op",
            "room_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "canvas_id": "01ARZ3NDEKTSV4RRFFQ69G5FAW",
            "op_id": uuid::Uuid::new_v4(),
            "op_seq": 1,
            "author_id": "01ARZ3NDEKTSV4RRFFQ69G5FAX",
            "op": {"type": "insert", "text": "durable"}
        });
        let canvas_op = row(EventOutboxKind::CanvasOp, payload.clone());
        assert_eq!(
            materialize_outbox_payload(&canvas_op, None, OffsetDateTime::UNIX_EPOCH).unwrap(),
            Some(payload)
        );
    }

    #[test]
    fn notify_materialization_removes_departed_recipients() {
        let room_id = RoomId::new();
        let message_id = MessageId::new();
        let actor = ParticipantId::new();
        let current = ParticipantId::new();
        let departed = ParticipantId::new();
        let payload = serde_json::to_value(RoomEvent::NotifyBatch {
            room_id,
            message_id,
            by: actor,
            delivery_id: uuid::Uuid::new_v4(),
            recipients: vec![
                NotifyTarget {
                    participant: current,
                    kind: NotificationKind::Mention,
                },
                NotifyTarget {
                    participant: departed,
                    kind: NotificationKind::Reply,
                },
            ],
        })
        .unwrap();
        let filtered = filter_notify_recipients(&payload, &[current])
            .unwrap()
            .expect("one member remains");
        let RoomEvent::NotifyBatch { recipients, .. } = serde_json::from_value(filtered).unwrap()
        else {
            panic!("notify batch");
        };
        assert_eq!(recipients.len(), 1);
        assert_eq!(recipients[0].participant, current);
        assert!(filter_notify_recipients(&payload, &[]).unwrap().is_none());
    }
}
