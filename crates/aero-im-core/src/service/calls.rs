//! Call operations — start, relay, end.
//!
//! Extracted from `service.rs` as part of REFACTOR_PLAN.md Step 1d.

use aero_common::{
    CallEvent, CallId, CallKind, CallMode, CallSession, Error, ParticipantId,
    Result, RoomEvent, RoomId, RoomKind,
};
use tracing::{instrument, warn};

use crate::ImService;

impl ImService {
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
        // Blocking (1:1): a direct-message call must not connect users who have
        // blocked each other — mirrors the DM-open guard (server `dm.rs`) so a
        // pre-existing DM can't be used to ring someone after a block, and closes
        // the gap that notification suppression can't (a call is a live WS event,
        // not a notification). Group-room calls are unaffected. Best-effort /
        // fail-open on a lookup error, like the notification block-filter.
        if let Some(block_repo) = self.block_repo.as_ref() {
            if self.rooms.room_kind(room).await? == Some(RoomKind::Direct) {
                for callee in &callees {
                    let blocked = block_repo.is_blocked(initiator, *callee).await.unwrap_or(false)
                        || block_repo.is_blocked(*callee, initiator).await.unwrap_or(false);
                    if blocked {
                        return Err(Error::Forbidden("blocked".into()));
                    }
                }
            }
        }
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
}
