use super::*;

/// Undo every layer written by an in-flight group Join.
///
/// A failed canonical re-read can mean `CallEnd` already performed cleanup
/// immediately before this Join recreated a bridge. Generation ownership keeps
/// the rollback from deleting a replacement leg that won on this node.
pub(super) async fn rollback_group_join_generation(
    state: &AppState,
    call_id: CallId,
    participant: ParticipantId,
    leg_generation: i64,
) {
    let local_owned = state
        .call_orchestrator
        .local_leg_generation(call_id, participant)
        .await
        == Some(leg_generation);
    let (_, topology) = state
        .call_supervisor
        .remove_sfu_session_generation_with_topology(call_id, participant, leg_generation);
    if let Some(topology) = topology {
        super::super::sfu::fan_out_topology(state, &topology, None);
    }
    if local_owned {
        state.hub.call_leave(call_id, participant);
    }
    if let Err(error) = state
        .call_roster
        .leave_generation(call_id, participant, leg_generation)
        .await
    {
        warn!(
            ?error,
            %call_id,
            %participant,
            leg_generation,
            "group-call join rollback Redis cleanup failed"
        );
    }
    let call_empty = match state
        .call_orchestrator
        .leave_group_call_generation(call_id, participant, leg_generation)
        .await
    {
        Ok(leave) => leave.call_empty,
        Err(error) => {
            // Live cleanup must still happen when persistence is unavailable;
            // exact generation ownership prevents this fallback from deleting
            // a replacement leg on this node.
            warn!(
                ?error,
                %call_id,
                %participant,
                leg_generation,
                "group-call join rollback durable leave failed"
            );
            state
                .call_orchestrator
                .cleanup_group_call_participant_generation(call_id, participant, leg_generation)
                .await
        }
    };
    if local_owned && call_empty {
        state.call_supervisor.cancel_call(call_id);
    }
}
