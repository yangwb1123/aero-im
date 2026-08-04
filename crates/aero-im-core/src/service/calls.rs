//! Call operations — start, relay, end.
//!
//! Extracted from `service.rs` as part of REFACTOR_PLAN.md Step 1d.

use aero_common::{
    CallEvent, CallId, CallKind, CallMode, CallSession, Error, ParticipantId, Result, RoomEvent,
    RoomId, SfuPublisherDescription,
};
use tracing::instrument;

use crate::ImService;

impl ImService {
    /// Resolve an active persisted call and authorize access to its canonical
    /// room. This is the pre-admission guard for a new group-call join: the
    /// caller is not a call participant yet, so it deliberately does not check
    /// `call_participants`.
    pub async fn assert_joinable_call_access(
        &self,
        participant: ParticipantId,
        call_id: CallId,
        claimed_room: RoomId,
        required_mode: CallMode,
    ) -> Result<CallSession> {
        self.calls
            .authorize_joinable(call_id, participant, claimed_room, required_mode)
            .await
    }

    /// Resolve the persisted call first, authorize against its canonical room,
    /// require the caller's claimed room / optional mode to match, and require
    /// an active persisted call leg.
    ///
    /// Call ids are global.  Treating a client-supplied `room_id` as the
    /// authorization boundary lets a member of room A mutate a call that
    /// actually belongs to room B.  Every existing-call mutation should pass
    /// through this guard before touching call, transcript, SFU, Hub, or Redis
    /// state.
    pub async fn assert_active_call_access(
        &self,
        participant: ParticipantId,
        call_id: CallId,
        claimed_room: RoomId,
        required_mode: Option<CallMode>,
    ) -> Result<CallSession> {
        self.calls
            .authorize_active(call_id, participant, claimed_room, required_mode, None)
            .await
    }

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
        let call_id = CallId::new();
        let (session, callees) = self
            .calls
            .start_authorized(call_id, room, initiator, kind, mode)
            .await?;
        self.publish_room_event(
            room,
            &RoomEvent::Call(CallEvent::Invite {
                call_id,
                room_id: room,
                from: initiator,
                to: callees,
                kind,
                mode,
                sdp,
            }),
        )
        .await;
        Ok(session)
    }

    /// Persist an active SFU participant leg as left before publishing the
    /// corresponding lifecycle event.
    ///
    /// The relay owns the compare-and-set. A database error or lost End race
    /// returns without publishing, allowing the WS layer to keep every live
    /// topology surface untouched.
    pub async fn leave_call(
        &self,
        participant: ParticipantId,
        room: RoomId,
        call_id: CallId,
        leg_generation: i64,
    ) -> Result<()> {
        self.relay_call_event(
            participant,
            room,
            CallEvent::Leave {
                call_id,
                room_id: room,
                from: participant,
                leg_generation,
            },
        )
        .await
    }

    /// Publish server-owned SFU publisher state after validating the canonical
    /// active call. Inactive state is allowed after the participant leg has
    /// already been durably marked left; active state still requires an active
    /// persisted leg.
    pub async fn publish_sfu_publisher_state(
        &self,
        call_id: CallId,
        claimed_room: RoomId,
        publisher: SfuPublisherDescription,
        leg_generation: i64,
        active: bool,
    ) -> Result<()> {
        let call = self
            .calls
            .get(call_id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("call {call_id}")))?;
        validate_active_call_claim(&call, claimed_room, Some(CallMode::Sfu))?;
        if !self
            .calls
            .participant_generation_matches(call_id, publisher.participant, leg_generation, active)
            .await?
        {
            return Err(Error::Conflict(
                "call participant incarnation is stale".into(),
            ));
        }
        self.publish_room_event(
            call.room_id,
            &RoomEvent::Call(CallEvent::SfuPublisher {
                call_id,
                room_id: call.room_id,
                publisher,
                active,
                leg_generation,
            }),
        )
        .await;
        Ok(())
    }

    /// Forward an answer/ICE/end signaling event to the targeted participant(s).
    /// `room` is required for routing on the per-room subject.
    ///
    /// `caller` is the authenticated sender (the JWT-verified participant, NOT a
    /// field from the client payload). Every call relay funnels through here. The
    /// persisted call is resolved before any side effect, and access is checked
    /// against its canonical room rather than trusting the frame's `room_id`.
    pub async fn relay_call_event(
        &self,
        caller: ParticipantId,
        room: RoomId,
        event: CallEvent,
    ) -> Result<()> {
        let call_id = call_event_id(&event);
        let required_mode = call_event_requires_sfu(&event).then_some(CallMode::Sfu);
        if call_event_actor(&event).is_some_and(|actor| actor != caller) {
            return Err(Error::Forbidden(
                "call event actor does not match authenticated participant".into(),
            ));
        }
        // The recipient of a directed signaling event (Answer/Ice/Offer) is
        // client-supplied. A call peer is necessarily a room member, so reject a
        // `to` outside the room — otherwise a member could aim signaling (or a fake
        // offer) at an arbitrary participant who isn't in this call's room.
        let directed_to = match &event {
            CallEvent::Answer { to, .. }
            | CallEvent::Ice { to, .. }
            | CallEvent::Offer { to, .. } => Some((*to, false)),
            CallEvent::Roster { to, .. } => Some((*to, true)),
            _ => None,
        };
        if let Some((to, allow_self)) = directed_to {
            // Storage performs the effective-room and active-leg checks under
            // one transaction. This pure precheck only rejects nonsensical
            // self-targeted client signaling without a database round trip.
            validate_directed_recipient(caller, to, allow_self, true, true)?;
        }

        let call = match &event {
            CallEvent::End { reason, .. } => {
                self.calls
                    .end_authorized(call_id, caller, room, reason)
                    .await?
            }
            CallEvent::Answer { to, .. } => {
                self.calls
                    .answer_authorized(call_id, caller, *to, room)
                    .await?
            }
            CallEvent::Leave { leg_generation, .. } => {
                self.calls
                    .leave_authorized_generation(call_id, caller, room, *leg_generation)
                    .await?
            }
            _ => {
                self.calls
                    .authorize_active(
                        call_id,
                        caller,
                        room,
                        required_mode,
                        directed_to.map(|(recipient, _)| recipient),
                    )
                    .await?
            }
        };
        self.publish_room_event(call.room_id, &RoomEvent::Call(event))
            .await;
        Ok(())
    }
}

fn call_event_id(event: &CallEvent) -> CallId {
    match event {
        CallEvent::Invite { call_id, .. }
        | CallEvent::Answer { call_id, .. }
        | CallEvent::Ice { call_id, .. }
        | CallEvent::End { call_id, .. }
        | CallEvent::Caption { call_id, .. }
        | CallEvent::Join { call_id, .. }
        | CallEvent::SfuPublisher { call_id, .. }
        | CallEvent::Leave { call_id, .. }
        | CallEvent::Roster { call_id, .. }
        | CallEvent::Offer { call_id, .. } => *call_id,
    }
}

fn call_event_requires_sfu(event: &CallEvent) -> bool {
    matches!(
        event,
        CallEvent::Join { .. }
            | CallEvent::SfuPublisher { .. }
            | CallEvent::Leave { .. }
            | CallEvent::Roster { .. }
    )
}

fn call_event_actor(event: &CallEvent) -> Option<ParticipantId> {
    match event {
        CallEvent::Invite { from, .. }
        | CallEvent::Answer { from, .. }
        | CallEvent::Ice { from, .. }
        | CallEvent::Caption { from, .. }
        | CallEvent::Join { from, .. }
        | CallEvent::Leave { from, .. }
        | CallEvent::Offer { from, .. } => Some(*from),
        CallEvent::End { by, .. } => Some(*by),
        CallEvent::SfuPublisher { publisher, .. } => Some(publisher.participant),
        CallEvent::Roster { .. } => None,
    }
}

fn validate_directed_recipient(
    caller: ParticipantId,
    recipient: ParticipantId,
    allow_self: bool,
    in_room: bool,
    in_call: bool,
) -> Result<()> {
    if !allow_self && recipient == caller {
        return Err(Error::Forbidden(
            "directed call signaling cannot target the sender".into(),
        ));
    }
    if !in_room || !in_call {
        return Err(Error::Forbidden(
            "recipient is not an active call participant".into(),
        ));
    }
    Ok(())
}

fn validate_active_call_claim(
    call: &CallSession,
    claimed_room: RoomId,
    required_mode: Option<CallMode>,
) -> Result<()> {
    if call.room_id != claimed_room {
        return Err(Error::Forbidden(
            "call does not belong to the claimed room".into(),
        ));
    }
    if call.ended_at.is_some() {
        return Err(Error::Conflict("call has already ended".into()));
    }
    if required_mode.is_some_and(|mode| call.mode != mode) {
        return Err(Error::Conflict(
            "call mode does not support this operation".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        call_event_actor, call_event_requires_sfu, validate_active_call_claim,
        validate_directed_recipient,
    };
    use aero_common::{
        CallEvent, CallId, CallKind, CallMode, CallSession, Error, ParticipantId, RoomId,
    };
    use time::OffsetDateTime;

    fn active_call(room_id: RoomId, mode: CallMode) -> CallSession {
        CallSession {
            id: CallId::new(),
            room_id,
            initiator: ParticipantId::new(),
            kind: CallKind::Video,
            mode,
            started_at: OffsetDateTime::now_utc(),
            ended_at: None,
            end_reason: None,
        }
    }

    #[test]
    fn canonical_call_claim_rejects_room_mode_and_lifecycle_mismatch() {
        let room = RoomId::new();
        let call = active_call(room, CallMode::Sfu);
        assert!(validate_active_call_claim(&call, room, Some(CallMode::Sfu)).is_ok());

        assert!(matches!(
            validate_active_call_claim(&call, RoomId::new(), Some(CallMode::Sfu)),
            Err(Error::Forbidden(_))
        ));
        assert!(matches!(
            validate_active_call_claim(&call, room, Some(CallMode::P2p)),
            Err(Error::Conflict(_))
        ));

        let mut ended = call;
        ended.ended_at = Some(OffsetDateTime::now_utc());
        assert!(matches!(
            validate_active_call_claim(&ended, room, Some(CallMode::Sfu)),
            Err(Error::Conflict(_))
        ));
    }

    #[test]
    fn call_event_mode_and_actor_matrix_matches_protocol_roles() {
        let call_id = CallId::new();
        let room_id = RoomId::new();
        let from = ParticipantId::new();
        let to = ParticipantId::new();
        let offer = CallEvent::Offer {
            call_id,
            from,
            to,
            sdp: "v=0".into(),
        };
        assert!(
            !call_event_requires_sfu(&offer),
            "mesh/P2P offer supports both persisted call modes"
        );
        assert_eq!(call_event_actor(&offer), Some(from));

        let join = CallEvent::Join {
            call_id,
            room_id,
            from,
            kind: CallKind::Video,
            leg_generation: 1,
        };
        assert!(call_event_requires_sfu(&join));
        assert_eq!(call_event_actor(&join), Some(from));

        let end = CallEvent::End {
            call_id,
            room_id,
            by: from,
            reason: "done".into(),
        };
        assert!(!call_event_requires_sfu(&end));
        assert_eq!(call_event_actor(&end), Some(from));
    }

    #[test]
    fn directed_signaling_requires_distinct_active_call_recipient() {
        let caller = ParticipantId::new();
        let recipient = ParticipantId::new();
        assert!(validate_directed_recipient(caller, recipient, false, true, true).is_ok());
        assert!(matches!(
            validate_directed_recipient(caller, caller, false, true, true),
            Err(Error::Forbidden(_))
        ));
        assert!(matches!(
            validate_directed_recipient(caller, recipient, false, false, true),
            Err(Error::Forbidden(_))
        ));
        assert!(matches!(
            validate_directed_recipient(caller, recipient, false, true, false),
            Err(Error::Forbidden(_))
        ));
        assert!(
            validate_directed_recipient(caller, caller, true, true, true).is_ok(),
            "server-authored roster may target the joiner itself"
        );
    }
}
