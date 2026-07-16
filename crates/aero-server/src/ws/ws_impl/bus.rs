//! Background bus listeners and their per-message handlers, moved verbatim
//! from `ws_impl`. Subscribes to `im.room.*` / `live.stream.*` and fans each
//! decoded event into the local Hub; behaviour unchanged.
use super::*;
use crate::ws::frame;
use futures::StreamExt;

/// Backoff between bus resubscribe attempts after the subscription stream ends
/// (NATS reconnect/drop) or a subscribe call fails. Short — fan-out is offline
/// until we reconnect — but non-zero so a hard-down NATS can't spin a tight loop.
const BUS_RESUBSCRIBE_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);
/// Build a consumer span parented to the W3C `traceparent` stamped on a bus
/// message's envelope (if any), so a message-send trace links across the NATS
/// boundary end to end (ROADMAP5 方向二). An untraced/legacy payload — or one with
/// no active trace context upstream — just yields a fresh span.
fn bus_consume_span(subject: &'static str, payload: &[u8]) -> tracing::Span {
    let span = tracing::info_span!("bus.consume", subject);
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(payload) {
        if let Some(tp) = aero_bus::extract_traceparent(&value) {
            aero_common::telemetry::set_span_parent_from_traceparent(&span, &tp);
        }
    }
    span
}
/// Background loop that subscribes to `im.room.*` and pushes each [`RoomEvent`]
/// into the local Hub. Started once per process at boot; runs until the process
/// exits, resubscribing across NATS reconnects (see [`BUS_RESUBSCRIBE_BACKOFF`]).
pub async fn run_bus_listener(state: AppState) -> anyhow::Result<()> {
    use aero_bus::EventBus;
    use tracing::Instrument as _;
    let bus: Arc<dyn EventBus> = state.bus.clone();
    // Resubscribe loop: a NATS reconnect/drop ends the subscription stream. Without
    // this outer loop the function would return and the boot-time task would exit,
    // silently stopping room-event fan-out on this process *forever*. The durable
    // consumer ("aero-server") resumes from its committed cursor on resubscribe, so
    // acked events are not re-sent and unacked ones are redelivered (at-least-once).
    loop {
        let mut stream = match bus.subscribe("im.room.*", Some("aero-server")).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "im.room.* subscribe failed; retrying");
                tokio::time::sleep(BUS_RESUBSCRIBE_BACKOFF).await;
                continue;
            }
        };
        info!("bus listener started");
        while let Some(sub) = stream.next().await {
            // Continue the producer's distributed trace (ROADMAP5 方向二): a span
            // whose parent is the envelope's W3C traceparent, so this consumer's
            // fan-out nests under the message-send trace across the NATS boundary.
            let span = bus_consume_span("im.room", sub.payload());
            handle_room_event_sub(&state, sub).instrument(span).await;
        }
        warn!("im.room.* subscription stream ended; resubscribing");
        tokio::time::sleep(BUS_RESUBSCRIBE_BACKOFF).await;
    }
}

/// Process one `im.room.*` bus message: lift the `seq` stamp, decode the typed
/// [`RoomEvent`], expand recipients (via cache), and fan out to the local Hub.
async fn handle_room_event_sub(state: &AppState, sub: Box<dyn aero_bus::Subscription + Send>) {
    use aero_common::RoomEvent;
    // Two-phase decode: lift the publish-time `"seq"` stamp off the raw JSON
    // *before* the typed `Deserialize` of the RoomEvent drops unknown keys.
    let parsed: Result<(RoomEvent, Option<u64>), _> =
        serde_json::from_slice::<serde_json::Value>(sub.payload()).and_then(|value| {
            let seq = aero_bus::extract_seq(&value);
            Ok((serde_json::from_value::<RoomEvent>(value)?, seq))
        });
    match parsed {
        Ok((event, seq)) => {
            // NotifyBatch is published once for the whole recipient set; expand it
            // here into one targeted `notify` frame per recipient so each client
            // receives an ordinary frame carrying ITS OWN participant/kind and never
            // sees the rest of the batch. This must stay a special case:
            // `frame::room_event_to_frame_json` collapses a NotifyBatch into a
            // single frame built from `recipients.first()` alone (it has no way to
            // address per-recipient JSON from inside a shared frame string), so
            // reusing the generic single-frame fan-out below for this variant would
            // send every recipient a frame addressed to whichever participant
            // happens to be first in the batch — everyone else's notification
            // silently mismatches their own id and gets dropped/misattributed
            // client-side. Equivalent to the old per-recipient Notify publishes,
            // minus the O(N) NATS traffic.
            if let RoomEvent::NotifyBatch { room_id, message_id, by, delivery_id: _, recipients } = &event {
                for target in recipients {
                    let frame = frame::room_event_to_frame_json(
                        &RoomEvent::Notify {
                            room_id: *room_id,
                            message_id: *message_id,
                            mentioned: target.participant,
                            by: *by,
                            kind: target.kind,
                        },
                        seq,
                    );
                    state.hub.fan_out_raw(&[target.participant], &frame);
                }
                let _ = sub.ack().await;
                return;
            }
            // Every other RoomEvent variant: explicit recipients when the event
            // carries them, otherwise the room's full member list from the
            // cache-backed RoomMemberCache (ROADMAP6 方向四).
            let room = event.room_id();
            let recipients: Arc<[ParticipantId]> = match event.explicit_recipients() {
                list if !list.is_empty() => list.into(),
                _ => {
                    if let Some(rid) = room {
                        state
                            .room_member_cache
                            .get_or_fetch(rid, &state.rooms)
                            .await
                            .unwrap_or_default()
                    } else {
                        Arc::new([])
                    }
                }
            };
            // Note: the RoomEvent path below deliberately avoids counting
            // MESSAGES_SENT_TOTAL — that counter lives in the `send_message`
            // path (`aero-im-core/src/service/messages.rs`), where room_type
            // and optional workspace labels are available. Counting here would
            // DOUBLE-COUNT for the RoomEvent path because every message already
            // flows through send_message → publish_room_event → bus listener.
            // However, the LEGACY envelope fallback (further below) does count
            // because legacy-format messages bypass `send_message` entirely and
            // therefore are never counted upstream.
            // AI answer-cache staleness (ROADMAP 方向一·3): an edited or deleted
            // message must not survive in a cached AI answer. The bus is the one
            // chokepoint every Edited/Deleted (REST, WS, moderation) funnels
            // through, so invalidate the room's answer cache here. Best-effort, and
            // only when an AI backend is configured so non-AI deployments pay
            // nothing (an empty key-index is a no-op regardless).
            if state.ai.is_some()
                && matches!(event, RoomEvent::Edited(_) | RoomEvent::Deleted { .. })
            {
                if let Some(rid) = room {
                    let store = aero_storage::AiContextStore::new(state.redis_client.clone());
                    if let Err(e) = store.cache_answer_invalidate_room(rid).await {
                        tracing::warn!(error = ?e, room = %rid, "answer-cache invalidation failed");
                    }
                }
            }
            let frame = frame::room_event_to_frame_json(&event, seq);
            state.hub.fan_out_raw(&*recipients, &frame);
            let _ = sub.ack().await;
        }
        Err(e) => {
            // Compatibility: accept the legacy raw MessageEnvelope payload too.
            if let Ok(env) =
                serde_json::from_slice::<aero_common::MessageEnvelope>(sub.payload())
            {
                let recipients: Arc<[ParticipantId]> = if env.recipients.is_empty() {
                    state
                        .room_member_cache
                        .get_or_fetch(env.message.room_id, &state.rooms)
                        .await
                        .unwrap_or_default()
                } else {
                    env.recipients.clone().into()
                };
                // Legacy envelope path also carries exactly one new message.
                metrics::inc_counter(names::MESSAGES_SENT_TOTAL, 1);
                let frame = serde_json::json!({
                    "type": "message",
                    "message": env.message,
                });
                state.hub.fan_out_raw(&*recipients, &frame.to_string());
                let _ = sub.ack().await;
                return;
            }
            // Undecodable by any known schema (typed RoomEvent *and* legacy
            // envelope both failed). That's a deterministic failure on the raw
            // bytes -- redelivery will never succeed -- so ACK-drop it rather than
            // nack, otherwise the durable consumer redelivers this poison message
            // forever. The counter flags a producer/schema mismatch to alert on.
            warn!(error = ?e, "bad envelope on bus -- dropping (poison)");
            metrics::inc_counter(names::BUS_POISON_DROPPED_TOTAL, 1);
            let _ = sub.ack().await;
        }
    }
}

/// Process one `live.stream.*` bus message: lift the `seq` stamp, decode the
/// [`StreamEvent`], and fan it out to this node's local watchers. Acks always; an
/// undecodable payload is ack-dropped (poison-safe), never nacked.
async fn handle_stream_event_sub(state: &AppState, sub: Box<dyn aero_bus::Subscription + Send>) {
    // Two-phase decode (as `handle_room_event_sub`): lift the publish-time `"seq"`
    // stamp off the raw JSON before the typed decode drops it.
    let parsed: serde_json::Result<(StreamEvent, Option<u64>)> =
        serde_json::from_slice::<serde_json::Value>(sub.payload()).and_then(|value| {
            let seq = aero_bus::extract_seq(&value);
            Ok((serde_json::from_value::<StreamEvent>(value)?, seq))
        });
    match parsed {
        Ok((event, seq)) => {
            let watchers = state.hub.stream_watchers(event.stream_id());
            if !watchers.is_empty() {
                let frame = ServerFrame::StreamEvent { event };
                // Serialize with seq stamp; reuse the frame serialization helper.
                let json = match serde_json::to_value(&frame) {
                    Ok(mut value) => {
                        aero_bus::stamp_seq(&mut value, seq);
                        value.to_string()
                    }
                    Err(_) => String::new(),
                };
                state.hub.fan_out_raw(&watchers, &json);
            }
            let _ = sub.ack().await;
        }
        Err(e) => {
            warn!(error = ?e, "bad stream event on bus -- dropping (poison)");
            metrics::inc_counter(names::BUS_POISON_DROPPED_TOTAL, 1);
            let _ = sub.ack().await;
        }
    }
}

/// Background loop that subscribes to `live.stream.*` (ephemeral consumer) and
/// pushes each [`StreamEvent`] into the local Hub. Started once per process at
/// boot; runs until the process exits, resubscribing across NATS reconnects.
pub async fn run_live_bus_listener(state: AppState) -> anyhow::Result<()> {
    use aero_bus::EventBus;
    use tracing::Instrument as _;
    let bus: Arc<dyn EventBus> = state.bus.clone();
    loop {
        let mut stream = match bus.subscribe("live.stream.*", None).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "live.stream.* subscribe failed; retrying");
                tokio::time::sleep(BUS_RESUBSCRIBE_BACKOFF).await;
                continue;
            }
        };
        info!("live bus listener started");
        while let Some(sub) = stream.next().await {
            let span = bus_consume_span("live.stream", sub.payload());
            handle_stream_event_sub(&state, sub).instrument(span).await;
        }
        warn!("live.stream.* subscription stream ended; resubscribing");
        tokio::time::sleep(BUS_RESUBSCRIBE_BACKOFF).await;
    }
}
