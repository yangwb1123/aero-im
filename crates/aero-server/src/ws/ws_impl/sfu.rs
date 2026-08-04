//! Server-owned SFU signaling and revisioned topology fan-out.

use std::time::Duration;

use aero_common::{
    CallId, CallMode, ParticipantId, RoomId, SfuPublisherDescription, SfuSubscription,
};
use axum::extract::ws::Message;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::ServerFrame;
use crate::sfu_media::{SfuMediaError, SfuSessionEnded, SfuTopologySnapshot};
use crate::state::AppState;

fn send_error(tx: &mpsc::Sender<Message>, code: &'static str, msg: String) {
    let _ = tx.try_send(Message::Text(
        serde_json::to_string(&ServerFrame::Error { code, msg }).unwrap_or_default(),
    ));
}

async fn assert_active_call(
    state: &AppState,
    participant: ParticipantId,
    call_id: CallId,
    room_id: RoomId,
    tx: &mpsc::Sender<Message>,
) -> anyhow::Result<bool> {
    if super::active_call_for_frame(
        state,
        participant,
        call_id,
        room_id,
        Some(CallMode::Sfu),
        tx,
    )
    .await
    .is_none()
    {
        return Ok(false);
    }
    if !state
        .call_supervisor
        .has_call_participant(call_id, participant)
    {
        send_error(
            tx,
            "call_not_joined",
            "join or accept the call before negotiating server media".into(),
        );
        return Ok(false);
    }
    Ok(true)
}

fn topology_json(topology: &SfuTopologySnapshot, subscriber: ParticipantId) -> String {
    serde_json::to_string(&ServerFrame::CallSfuRenegotiate {
        call_id: topology.call_id,
        revision: topology.revision,
        publishers: topology.publishers.clone(),
        required_recv_slots: topology.required_recv_slots(subscriber),
    })
    .unwrap_or_default()
}

/// Push a per-subscriber topology snapshot to every local member of this call.
pub(super) fn fan_out_topology(
    state: &AppState,
    topology: &SfuTopologySnapshot,
    exclude: Option<ParticipantId>,
) {
    for participant in state.hub.call_members(topology.call_id) {
        if exclude == Some(participant) {
            continue;
        }
        state
            .hub
            .fan_out_raw(&[participant], &topology_json(topology, participant));
    }
}

pub(super) async fn handle_offer(
    state: &AppState,
    participant: ParticipantId,
    call_id: CallId,
    room_id: RoomId,
    leg_generation: i64,
    sdp: String,
    tx: &mpsc::Sender<Message>,
) -> anyhow::Result<()> {
    if let Err(error) = aero_signaling::validate_sdp(&sdp) {
        send_error(tx, "invalid_call", error.to_string());
        return Ok(());
    }
    if !assert_active_call(state, participant, call_id, room_id, tx).await? {
        return Ok(());
    }
    if leg_generation <= 0
        || !state
            .calls
            .participant_generation_matches(call_id, participant, leg_generation, true)
            .await?
    {
        send_error(
            tx,
            "stale_call_leg",
            "join the call again before negotiating server media".into(),
        );
        return Ok(());
    }
    match state
        .call_supervisor
        .accept_sfu_offer(call_id, participant, leg_generation, &sdp)
        .await
    {
        Ok(answer) => {
            // Binding/SDP negotiation awaits socket and DNS work. If CallEnd
            // won during that interval, remove the just-created owner task
            // before any answer/topology/publisher event becomes observable.
            let generation_current = state
                .calls
                .participant_generation_matches(call_id, participant, leg_generation, true)
                .await?;
            if !generation_current
                || !assert_active_call(state, participant, call_id, room_id, tx).await?
            {
                let (_, topology) = state
                    .call_supervisor
                    .remove_sfu_session_generation_with_topology(
                        call_id,
                        participant,
                        leg_generation,
                    );
                if let Some(topology) = topology {
                    fan_out_topology(state, &topology, None);
                }
                state
                    .call_orchestrator
                    .cleanup_group_call_participant_generation(call_id, participant, leg_generation)
                    .await;
                return Ok(());
            }
            let topology = answer.topology;
            let required_recv_slots = topology.required_recv_slots(participant);
            let _ = tx.try_send(Message::Text(
                serde_json::to_string(&ServerFrame::CallSfuAnswer {
                    call_id,
                    sdp: answer.sdp,
                    local_addr: answer.local_addr.to_string(),
                    mids: answer.mids,
                    session_generation: answer.session_generation,
                    revision: topology.revision,
                    publishers: topology.publishers.clone(),
                    required_recv_slots,
                })
                .unwrap_or_default(),
            ));
            // Existing local members must add a distinct receive slot for this
            // publisher. The offering participant already received the same
            // revision in its answer.
            fan_out_topology(state, &topology, Some(participant));
            if let Some(publisher) = state.call_supervisor.sfu_publisher(call_id, participant) {
                state
                    .im
                    .publish_sfu_publisher_state(call_id, room_id, publisher, leg_generation, true)
                    .await?;
            }
        }
        Err(error) => {
            warn!(%call_id, %participant, %error, "SFU offer negotiation failed");
            send_error(tx, "sfu_negotiation", error.to_string());
        }
    }
    Ok(())
}

pub(super) async fn handle_ice(
    state: &AppState,
    participant: ParticipantId,
    call_id: CallId,
    room_id: RoomId,
    session_generation: u64,
    candidate: serde_json::Value,
    tx: &mpsc::Sender<Message>,
) -> anyhow::Result<()> {
    if let Err(error) = aero_signaling::validate_ice_candidate(&candidate) {
        send_error(tx, "invalid_call", error.to_string());
        return Ok(());
    }
    if !assert_active_call(state, participant, call_id, room_id, tx).await? {
        return Ok(());
    }
    let candidate_sdp = candidate
        .get("candidate")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    match state
        .call_supervisor
        .add_sfu_ice_for_generation(call_id, participant, session_generation, candidate_sdp)
        .await
    {
        Ok(()) => {
            let _ = tx.try_send(Message::Text(
                serde_json::to_string(&ServerFrame::CallSfuIceAck {
                    call_id,
                    session_generation,
                })
                .unwrap_or_default(),
            ));
        }
        Err(error) => {
            warn!(%call_id, %participant, %error, "SFU trickle ICE failed");
            send_error(tx, "sfu_ice", error.to_string());
        }
    }
    Ok(())
}

pub(super) struct SubscribeRequest {
    pub(super) call_id: CallId,
    pub(super) room_id: RoomId,
    pub(super) session_generation: u64,
    pub(super) revision: u64,
    pub(super) tracks: Vec<SfuSubscription>,
}

pub(super) async fn handle_subscribe(
    state: &AppState,
    participant: ParticipantId,
    request: SubscribeRequest,
    tx: &mpsc::Sender<Message>,
) -> anyhow::Result<()> {
    if !assert_active_call(state, participant, request.call_id, request.room_id, tx).await? {
        return Ok(());
    }
    match state.call_supervisor.replace_sfu_subscriptions(
        request.call_id,
        participant,
        request.session_generation,
        request.revision,
        request.tracks,
    ) {
        Ok(queued_keyframes) => {
            debug!(
                call_id = %request.call_id,
                %participant,
                revision = request.revision,
                queued_keyframes,
                "SFU subscription routes installed"
            );
            let _ = tx.try_send(Message::Text(
                serde_json::to_string(&ServerFrame::CallSfuSubscribed {
                    call_id: request.call_id,
                    session_generation: request.session_generation,
                    revision: request.revision,
                })
                .unwrap_or_default(),
            ));
        }
        Err(SfuMediaError::StaleTopology { .. }) => {
            let topology = state.call_supervisor.sfu_topology(request.call_id);
            let _ = tx.try_send(Message::Text(topology_json(&topology, participant)));
            send_error(
                tx,
                "sfu_topology_stale",
                SfuMediaError::StaleTopology {
                    expected: topology.revision,
                    actual: request.revision,
                }
                .to_string(),
            );
        }
        Err(error) => {
            send_error(tx, "sfu_subscription", error.to_string());
        }
    }
    Ok(())
}

/// Publish a server-authored removal so other nodes drop this publisher and
/// trigger their local subscribers to renegotiate.
pub(super) async fn publish_departure(
    state: &AppState,
    participant: ParticipantId,
    call_id: CallId,
    room_id: RoomId,
    leg_generation: i64,
) -> anyhow::Result<()> {
    state
        .im
        .publish_sfu_publisher_state(
            call_id,
            room_id,
            SfuPublisherDescription {
                participant,
                tracks: Vec::new(),
            },
            leg_generation,
            false,
        )
        .await?;
    Ok(())
}

/// Drain spontaneous media-owner exits and converge every topology surface.
///
/// Explicit leave/reconnect/end paths remove the registry generation before
/// aborting its task and therefore never enter this stream. Each received item
/// is a genuine current-generation loss: remove its logical local/Redis/route
/// membership, publish the inactive publisher cluster event, and push the
/// already-revisioned topology to remaining local subscribers.
pub(super) async fn run_lifecycle_events(
    state: AppState,
    mut events: mpsc::UnboundedReceiver<SfuSessionEnded>,
    cancel: CancellationToken,
) {
    loop {
        let event = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            event = events.recv() => event,
        };
        let Some(event) = event else {
            return;
        };

        // Serialize this participant-exit convergence with media starts and
        // logical rejoin for the same call. The incarnation check happens only
        // after acquiring the guard; if rejoin won first, every old DB/Hub/
        // Redis/publisher side effect is skipped.
        let _lifecycle_guard = state
            .call_supervisor
            .lock_sfu_lifecycle(event.call_id)
            .await;
        if !state.call_supervisor.is_current_ended_sfu_session(
            event.call_id,
            event.participant,
            event.session_generation,
        ) {
            debug!(
                call_id = %event.call_id,
                participant = %event.participant,
                session_generation = event.session_generation,
                "skipping stale SFU session-end lifecycle event"
            );
            continue;
        }

        // The owner task has already stopped forwarding media, but every
        // externally visible roster/topology cleanup waits for the durable
        // active→left transition. Keep retrying transient DB failures so a
        // media-task crash cannot silently leave an authorization leg active.
        let mut retry_delay = Duration::from_millis(100);
        let owned_local_generation = state
            .call_orchestrator
            .local_leg_generation(event.call_id, event.participant)
            .await
            == Some(event.leg_generation);
        let leave = loop {
            match state
                .call_orchestrator
                .leave_group_call_generation(event.call_id, event.participant, event.leg_generation)
                .await
            {
                Ok(leave) => break leave,
                Err(error) => {
                    warn!(
                        call_id = %event.call_id,
                        participant = %event.participant,
                        ?error,
                        retry_delay_ms = retry_delay.as_millis(),
                        "SFU session-end durable leave failed; retrying"
                    );
                }
            }
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(retry_delay) => {}
            }
            retry_delay = retry_delay.saturating_mul(2).min(Duration::from_secs(5));
        };

        if owned_local_generation {
            state.hub.call_leave(event.call_id, event.participant);
        }
        if leave.transitioned {
            fan_out_topology(&state, &event.topology, None);
            if let Err(error) = state
                .call_roster
                .leave_generation(event.call_id, event.participant, event.leg_generation)
                .await
            {
                warn!(
                    call_id = %event.call_id,
                    participant = %event.participant,
                    ?error,
                    "SFU session-end Redis roster cleanup failed"
                );
            }

            let call = match state.calls.get(event.call_id).await {
                Ok(call) => call,
                Err(error) => {
                    warn!(
                        call_id = %event.call_id,
                        participant = %event.participant,
                        ?error,
                        "SFU session ended but canonical call lookup failed"
                    );
                    None
                }
            };
            if let Some(call) = &call {
                if call.mode == CallMode::Sfu && call.ended_at.is_none() {
                    if let Err(error) = publish_departure(
                        &state,
                        event.participant,
                        event.call_id,
                        call.room_id,
                        event.leg_generation,
                    )
                    .await
                    {
                        warn!(
                            call_id = %event.call_id,
                            participant = %event.participant,
                            %error,
                            "SFU session-end publisher departure publish failed"
                        );
                    }
                }
            }
        } else {
            debug!(
                call_id = %event.call_id,
                participant = %event.participant,
                leg_generation = event.leg_generation,
                "stale SFU exit cleaned only node-local state"
            );
        }

        // The durable leave and external cleanup above may take seconds. Fence
        // bridge teardown to the exact media-source epoch that ended so a
        // reconnect during that window cannot be killed by this old event.
        if let Some(generation) = event.ended_egress_generation {
            let cancelled = state
                .call_supervisor
                .cancel_ended_media_epoch(event.call_id, generation);
            debug!(
                call_id = %event.call_id,
                participant = %event.participant,
                cancelled,
                orchestrator_empty = leave.call_empty,
                "last spontaneous SFU session exit cancelled call media bridges"
            );
        }
        let completed = state.call_supervisor.complete_ended_sfu_session(
            event.call_id,
            event.participant,
            event.session_generation,
        );
        if !completed {
            debug!(
                call_id = %event.call_id,
                participant = %event.participant,
                session_generation = event.session_generation,
                "SFU lifecycle marker was already consumed by authoritative call cleanup"
            );
        }
    }
}
