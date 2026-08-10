//! Background bus listeners and their per-message handlers. Subscribes to
//! `im.room.*` / `live.stream.*`, fans each decoded event into the local Hub,
//! and shuts down cooperatively through a shared cancellation token.
use super::{
    debug, info, metrics, names, warn, AppState, Arc, ParticipantId, ServerFrame, StreamEvent,
};
use crate::ws::frame;
use futures::{Stream, StreamExt};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

/// Backoff between bus resubscribe attempts after the subscription stream ends
/// (NATS reconnect/drop) or a subscribe call fails. Short — fan-out is offline
/// until we reconnect — but non-zero so a hard-down NATS can't spin a tight loop.
const BUS_RESUBSCRIBE_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);
/// Conservative cap for a `JetStream` durable consumer name. Names emitted here
/// use only ASCII letters, digits, `_`, and `-`, and therefore are safe as both
/// NATS consumer names and log fields.
const NATS_DURABLE_NAME_MAX_BYTES: usize = 128;
const IM_DURABLE_PREFIX: &str = "aero-server-";

#[derive(Debug, PartialEq, Eq)]
enum NextOrCancelled<T> {
    Item(T),
    Ended,
    Cancelled,
}

/// Wait for one already-subscribed message or shutdown. `biased` deliberately
/// gives a ready message priority over a simultaneously-ready cancellation: once
/// the broker has handed us a message, its handler must finish and ACK (or
/// intentionally leave it unacked) before the listener exits.
async fn next_or_cancelled<S>(
    stream: &mut S,
    cancel: &CancellationToken,
) -> NextOrCancelled<S::Item>
where
    S: Stream + Unpin,
{
    tokio::select! {
        biased;
        item = stream.next() => match item {
            Some(item) => NextOrCancelled::Item(item),
            None => NextOrCancelled::Ended,
        },
        () = cancel.cancelled() => NextOrCancelled::Cancelled,
    }
}

/// Sleep without making graceful shutdown wait for the full retry delay.
/// Returns `true` when cancellation won.
async fn backoff_or_cancelled(duration: std::time::Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => true,
        () = tokio::time::sleep(duration) => false,
    }
}

/// ACK a handled or deterministically-dropped message. An ACK transport failure
/// is observable and intentionally otherwise non-fatal: `JetStream` will redeliver
/// after `ack_wait`, preserving at-least-once delivery.
async fn ack_or_warn(sub: Box<dyn aero_bus::Subscription + Send>, disposition: &'static str) {
    if let Err(e) = sub.ack().await {
        warn!(
            error = %e,
            subject = %sub.subject(),
            disposition,
            "bus message ACK failed; broker may redeliver"
        );
    }
}

/// Derive the raw instance identity. Deployments should set a unique,
/// restart-stable `AERO_INSTANCE_ID`; local/dev runs fall back to hostname + PID,
/// which is unique among concurrently-running processes on one host.
fn raw_instance_id() -> String {
    if let Ok(id) = std::env::var("AERO_INSTANCE_ID") {
        let id = id.trim();
        if !id.is_empty() {
            return id.to_owned();
        }
    }

    let hostname = std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("COMPUTERNAME")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map_or_else(|| "unknown-host".to_owned(), |s| s.trim().to_owned());
    format!("{hostname}-{}", std::process::id())
}

/// Produce a bounded NATS-safe durable name. The full digest keeps distinct raw
/// instance ids distinct even when cleaning or truncation would otherwise make
/// their readable stems collide (`node/a` vs `node.a`, or two long common
/// prefixes). Identical explicit `AERO_INSTANCE_ID` values intentionally identify
/// the same deployment instance and therefore must not be assigned concurrently.
fn im_durable_name_for(raw_instance: &str) -> String {
    let raw_instance = raw_instance.trim();
    let digest = hex::encode(Sha256::digest(raw_instance.as_bytes()));
    let max_stem_len = NATS_DURABLE_NAME_MAX_BYTES - IM_DURABLE_PREFIX.len() - 1 - digest.len();
    let mut stem = String::with_capacity(max_stem_len);
    let mut previous_dash = false;
    for ch in raw_instance.chars() {
        let cleaned = if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-') {
            ch
        } else {
            '-'
        };
        if cleaned == '-' && previous_dash {
            continue;
        }
        if stem.len() >= max_stem_len {
            break;
        }
        stem.push(cleaned);
        previous_dash = cleaned == '-';
    }
    while stem.ends_with('-') {
        stem.pop();
    }
    if stem.is_empty() {
        stem.push_str("instance");
    }
    format!("{IM_DURABLE_PREFIX}{stem}-{digest}")
}

fn im_durable_name() -> String {
    im_durable_name_for(&raw_instance_id())
}

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
/// is cancelled, resubscribing across NATS reconnects (see
/// [`BUS_RESUBSCRIBE_BACKOFF`]).
pub async fn run_bus_listener(state: AppState, cancel: CancellationToken) -> anyhow::Result<()> {
    use aero_bus::EventBus;
    use tracing::Instrument as _;
    let bus: Arc<dyn EventBus> = state.bus.clone();
    let durable = im_durable_name();
    // Resubscribe loop: a NATS reconnect/drop ends the subscription stream. Without
    // this outer loop the function would return and the boot-time task would exit,
    // silently stopping room-event fan-out on this process *forever*. Each process
    // has its own stable durable consumer so every instance sees every room event;
    // acked events are not re-sent and unacked ones are redelivered (at-least-once).
    loop {
        let subscribed = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(()),
            result = bus.subscribe("im.room.*", Some(&durable)) => result,
        };
        let mut stream = match subscribed {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, %durable, "im.room.* subscribe failed; retrying");
                if backoff_or_cancelled(BUS_RESUBSCRIBE_BACKOFF, &cancel).await {
                    return Ok(());
                }
                continue;
            }
        };
        info!(%durable, "bus listener started");
        loop {
            match next_or_cancelled(&mut stream, &cancel).await {
                NextOrCancelled::Item(sub) => {
                    // Continue the producer's distributed trace (ROADMAP5 方向二):
                    // once fetched, finish handling even if shutdown is requested
                    // concurrently so the message reaches an explicit ACK outcome.
                    let span = bus_consume_span("im.room", sub.payload());
                    handle_room_event_sub(&state, sub).instrument(span).await;
                }
                NextOrCancelled::Ended => break,
                NextOrCancelled::Cancelled => return Ok(()),
            }
        }
        if cancel.is_cancelled() {
            return Ok(());
        }
        warn!("im.room.* subscription stream ended; resubscribing");
        if backoff_or_cancelled(BUS_RESUBSCRIBE_BACKOFF, &cancel).await {
            return Ok(());
        }
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
            // Publisher topology is a cluster event. Fold it into this node's
            // revision before ordinary room fan-out, then issue an executable
            // per-subscriber renegotiation snapshot to local call members.
            if let RoomEvent::Call(aero_common::CallEvent::SfuPublisher {
                call_id,
                publisher,
                active,
                leg_generation,
                ..
            }) = &event
            {
                if let Some(topology) = state.call_supervisor.observe_sfu_publisher(
                    *call_id,
                    publisher.clone(),
                    *leg_generation,
                    *active,
                ) {
                    super::sfu::fan_out_topology(state, &topology, None);
                }
            }
            // A join is also a topology synchronization point. Durable
            // publisher events that were already acked before this process
            // restarted are no longer in memory, so every node replays only
            // the publishers whose owner tasks it currently hosts. Receivers
            // fold identical descriptions idempotently.
            if let RoomEvent::Call(aero_common::CallEvent::Join {
                call_id, room_id, ..
            }) = &event
            {
                for (publisher, leg_generation) in
                    state.call_supervisor.local_sfu_publishers(*call_id)
                {
                    if let Err(error) = state
                        .im
                        .publish_sfu_publisher_state(
                            *call_id,
                            *room_id,
                            publisher,
                            leg_generation,
                            true,
                        )
                        .await
                    {
                        warn!(%call_id, %error, "SFU topology replay on join failed");
                    }
                }
            }
            // Every node consumes the remote join/publisher event, but only a
            // node that already hosts a local SFU island reconciles pulls. This
            // closes the asymmetric A-joins-first/B-joins-later case: B's join
            // causes A to discover and pull B, not only the reverse direction.
            let bridge_reconcile_call = match &event {
                RoomEvent::Call(
                    aero_common::CallEvent::Join { call_id, .. }
                    | aero_common::CallEvent::SfuPublisher { call_id, .. },
                ) => Some(*call_id),
                _ => None,
            };
            if let Some(call_id) = bridge_reconcile_call {
                reconcile_call_bridges(state, call_id).await;
            }
            // A call-end event is cluster-wide lifecycle, not only a browser
            // notification. Every node tears down its local SFU sessions,
            // egress relay, and bridge pulls; at-least-once redelivery is safe
            // because both cleanup paths are idempotent.
            if let RoomEvent::Call(aero_common::CallEvent::End { call_id, .. }) = &event {
                let _lifecycle_guard = state.call_supervisor.lock_sfu_lifecycle(*call_id).await;
                state.hub.call_end(*call_id);
                if let Err(error) = state.call_roster.clear(*call_id).await {
                    warn!(%call_id, ?error, "bus call-end Redis roster clear failed");
                }
                state.call_supervisor.cancel_call(*call_id);
                state.call_orchestrator.cleanup_ended_call(*call_id).await;
            }
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
            if let RoomEvent::NotifyBatch {
                room_id,
                message_id,
                by,
                delivery_id: _,
                recipients,
            } = &event
            {
                let current_members = match state.rooms.delivery_members(*room_id).await {
                    Ok(members) => members
                        .into_iter()
                        .collect::<std::collections::HashSet<_>>(),
                    Err(error) => {
                        warn!(
                            ?error,
                            room = %room_id,
                            subject = %sub.subject(),
                            "notify recipient revalidation failed; leaving bus message unacked"
                        );
                        return;
                    }
                };
                for target in recipients {
                    if !current_members.contains(&target.participant) {
                        continue;
                    }
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
                ack_or_warn(sub, "notify_batch_fanned_out").await;
                return;
            }
            // Every other RoomEvent variant is authorized again at consume time.
            // A room-membership cache is not safe for this decision: membership
            // may have been revoked on another node after the event was produced,
            // and Hub does not perform a second per-frame authorization check.
            // Re-reading PostgreSQL also protects delayed explicit-recipient
            // events (for example directed call signalling) from reaching a user
            // who left the room while the durable consumer was unavailable.
            let directed_call_id = match &event {
                RoomEvent::Call(
                    aero_common::CallEvent::Answer { call_id, .. }
                    | aero_common::CallEvent::Ice { call_id, .. }
                    | aero_common::CallEvent::Roster { call_id, .. }
                    | aero_common::CallEvent::Offer { call_id, .. },
                ) => Some(*call_id),
                _ => None,
            };
            let room = if let Some(room) = event.room_id() {
                Some(room)
            } else if let Some(call_id) = directed_call_id {
                match state.calls.room_id(call_id).await {
                    Ok(Some(room)) => Some(room),
                    Ok(None) => {
                        warn!(
                            %call_id,
                            subject = %sub.subject(),
                            "directed event references unknown call; dropping"
                        );
                        ack_or_warn(sub, "unknown_directed_call_dropped").await;
                        return;
                    }
                    Err(e) => {
                        warn!(
                            error = ?e,
                            %call_id,
                            subject = %sub.subject(),
                            "directed call room authorization failed; leaving bus message unacked"
                        );
                        return;
                    }
                }
            } else {
                None
            };
            let explicit = event.explicit_recipients();
            let recipients: Vec<ParticipantId> = if let Some(rid) = room {
                let current = match state.rooms.delivery_members(rid).await {
                    Ok(members) => members,
                    Err(e) => {
                        // This is transient infrastructure failure, not a poison
                        // payload. Leave the message untouched so JetStream retries
                        // it after ack_wait; an explicit NACK would hot-loop while
                        // PostgreSQL is unhealthy.
                        warn!(
                            error = ?e,
                            room = %rid,
                            subject = %sub.subject(),
                            "room-member authorization failed; leaving bus message unacked"
                        );
                        return;
                    }
                };
                if explicit.is_empty() {
                    current
                } else {
                    let current: std::collections::HashSet<_> = current.into_iter().collect();
                    explicit
                        .into_iter()
                        .filter(|participant| current.contains(participant))
                        .collect()
                }
            } else {
                explicit
            };
            if let Some(call_id) = directed_call_id {
                let Some(target) = recipients.first().copied() else {
                    ack_or_warn(sub, "directed_call_target_no_longer_member").await;
                    return;
                };
                match state.calls.is_participant(call_id, target).await {
                    Ok(true) => {}
                    Ok(false) => {
                        ack_or_warn(sub, "directed_call_target_no_longer_active").await;
                        return;
                    }
                    Err(e) => {
                        warn!(
                            error = ?e,
                            %call_id,
                            %target,
                            subject = %sub.subject(),
                            "directed call participant authorization failed; leaving bus message unacked"
                        );
                        return;
                    }
                }
            }
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
                && matches!(
                    event,
                    RoomEvent::Edited(_) | RoomEvent::Deleted { .. } | RoomEvent::Recalled(_)
                )
            {
                if let Some(rid) = room {
                    let store = aero_storage::AiContextStore::new(state.redis_client.clone());
                    if let Err(e) = store.cache_answer_invalidate_room(rid).await {
                        tracing::warn!(error = ?e, room = %rid, "answer-cache invalidation failed");
                    }
                }
            }
            let frame = frame::room_event_to_frame_json(&event, seq);
            state.hub.fan_out_raw(&recipients, &frame);
            ack_or_warn(sub, "room_event_fanned_out").await;
        }
        Err(e) => {
            // Compatibility: accept the legacy raw MessageEnvelope payload too.
            if let Ok(env) = serde_json::from_slice::<aero_common::MessageEnvelope>(sub.payload()) {
                let current = match state.rooms.delivery_members(env.message.room_id).await {
                    Ok(members) => members,
                    Err(fetch_error) => {
                        warn!(
                            error = ?fetch_error,
                            room = %env.message.room_id,
                            subject = %sub.subject(),
                            "legacy room-member authorization failed; leaving bus message unacked"
                        );
                        return;
                    }
                };
                let recipients: Arc<[ParticipantId]> = if env.recipients.is_empty() {
                    current.into()
                } else {
                    let current: std::collections::HashSet<_> = current.into_iter().collect();
                    env.recipients
                        .iter()
                        .copied()
                        .filter(|participant| current.contains(participant))
                        .collect::<Vec<_>>()
                        .into()
                };
                // Legacy envelope path also carries exactly one new message.
                metrics::inc_counter(names::MESSAGES_SENT_TOTAL, 1);
                let frame = serde_json::json!({
                    "type": "message",
                    "message": env.message,
                });
                state.hub.fan_out_raw(&recipients, &frame.to_string());
                ack_or_warn(sub, "legacy_message_fanned_out").await;
                return;
            }
            // Undecodable by any known schema (typed RoomEvent *and* legacy
            // envelope both failed). That's a deterministic failure on the raw
            // bytes -- redelivery will never succeed -- so ACK-drop it rather than
            // nack, otherwise the durable consumer redelivers this poison message
            // forever. The counter flags a producer/schema mismatch to alert on.
            warn!(error = ?e, "bad envelope on bus -- dropping (poison)");
            metrics::inc_counter(names::BUS_POISON_DROPPED_TOTAL, 1);
            ack_or_warn(sub, "room_event_poison_dropped").await;
        }
    }
}

async fn reconcile_call_bridges(state: &AppState, call_id: aero_common::CallId) {
    if !state.call_supervisor.has_local_call(call_id) {
        return;
    }
    let Some(topology) = state.call_orchestrator.current_topology(call_id).await else {
        // A transient Redis failure is not evidence that peers disappeared.
        return;
    };
    let peers = match topology {
        aero_live_webrtc::CallTopology::ServeLocal => Vec::new(),
        aero_live_webrtc::CallTopology::BridgeTo(peers) => peers,
    };
    let (spawned, cancelled) = state
        .call_supervisor
        .reconcile_bridges(call_id, &peers)
        .await;
    if spawned > 0 || cancelled > 0 {
        debug!(
            %call_id,
            desired = peers.len(),
            spawned,
            cancelled,
            "cross-node call bridges reconciled from cluster event"
        );
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
            let stream_id = event.stream_id();
            let watchers = state.hub.stream_watchers(stream_id);
            if !watchers.is_empty() {
                let frame = ServerFrame::StreamEvent { event };
                // Serialize with seq stamp; reuse the frame serialization helper.
                let json = match serde_json::to_value(&frame) {
                    Ok(mut value) => {
                        aero_bus::stamp_seq(&mut value, seq);
                        value.to_string()
                    }
                    Err(e) => {
                        // Serialization is deterministic for this value. Sending
                        // an empty WS frame would convert an internal error into a
                        // misleading client event, so ACK-drop it as poison and
                        // expose the loss through the existing poison metric.
                        warn!(
                            error = %e,
                            %stream_id,
                            "stream event serialization failed -- dropping (poison)"
                        );
                        metrics::inc_counter(names::BUS_POISON_DROPPED_TOTAL, 1);
                        ack_or_warn(sub, "stream_event_serialize_dropped").await;
                        return;
                    }
                };
                state.hub.fan_out_raw(&watchers, &json);
            }
            ack_or_warn(sub, "stream_event_fanned_out").await;
        }
        Err(e) => {
            warn!(error = ?e, "bad stream event on bus -- dropping (poison)");
            metrics::inc_counter(names::BUS_POISON_DROPPED_TOTAL, 1);
            ack_or_warn(sub, "stream_event_poison_dropped").await;
        }
    }
}

/// Background loop that subscribes to `live.stream.*` (ephemeral consumer) and
/// pushes each [`StreamEvent`] into the local Hub. Started once per process at
/// boot; runs until cancelled, resubscribing across NATS reconnects.
pub async fn run_live_bus_listener(
    state: AppState,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    use aero_bus::EventBus;
    use tracing::Instrument as _;
    let bus: Arc<dyn EventBus> = state.bus.clone();
    loop {
        let subscribed = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(()),
            result = bus.subscribe("live.stream.*", None) => result,
        };
        let mut stream = match subscribed {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "live.stream.* subscribe failed; retrying");
                if backoff_or_cancelled(BUS_RESUBSCRIBE_BACKOFF, &cancel).await {
                    return Ok(());
                }
                continue;
            }
        };
        info!("live bus listener started");
        loop {
            match next_or_cancelled(&mut stream, &cancel).await {
                NextOrCancelled::Item(sub) => {
                    let span = bus_consume_span("live.stream", sub.payload());
                    handle_stream_event_sub(&state, sub).instrument(span).await;
                }
                NextOrCancelled::Ended => break,
                NextOrCancelled::Cancelled => return Ok(()),
            }
        }
        if cancel.is_cancelled() {
            return Ok(());
        }
        warn!("live.stream.* subscription stream ended; resubscribing");
        if backoff_or_cancelled(BUS_RESUBSCRIBE_BACKOFF, &cancel).await {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_name_is_stable_bounded_and_nats_safe() {
        let raw = format!(
            "  node/with spaces.>*\\unicode-{}  ",
            "very-long-segment-".repeat(20)
        );
        let first = im_durable_name_for(&raw);
        let second = im_durable_name_for(&raw);

        assert_eq!(first, second);
        assert!(first.starts_with(IM_DURABLE_PREFIX));
        assert!(first.len() <= NATS_DURABLE_NAME_MAX_BYTES);
        assert!(
            first
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
            "durable name contains an unsafe byte: {first}"
        );
    }

    #[test]
    fn durable_name_digest_prevents_cleaning_and_truncation_collisions() {
        // Both pairs have the same readable form after cleaning/truncation; the
        // digest must preserve the raw instance identity.
        assert_ne!(im_durable_name_for("node/a"), im_durable_name_for("node.a"));
        let common = "x".repeat(400);
        assert_ne!(
            im_durable_name_for(&format!("{common}-one")),
            im_durable_name_for(&format!("{common}-two"))
        );
    }

    #[tokio::test]
    async fn next_wait_cancels_without_a_bus_or_timer_delay() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut stream = futures::stream::pending::<u8>();

        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            next_or_cancelled(&mut stream, &cancel),
        )
        .await
        .expect("pre-cancelled wait must return promptly");

        assert_eq!(outcome, NextOrCancelled::Cancelled);
    }

    #[tokio::test]
    async fn ready_message_wins_a_simultaneous_cancellation() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut stream = futures::stream::iter([7_u8]);

        assert_eq!(
            next_or_cancelled(&mut stream, &cancel).await,
            NextOrCancelled::Item(7)
        );
    }

    #[tokio::test]
    async fn retry_backoff_is_cancellation_aware() {
        let cancel = CancellationToken::new();
        cancel.cancel();

        let cancelled = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            backoff_or_cancelled(std::time::Duration::from_secs(60), &cancel),
        )
        .await
        .expect("pre-cancelled backoff must return promptly");

        assert!(cancelled);
    }
}
