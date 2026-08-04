//! Revisioned SFU topology/subscription façade.

use aero_common::{CallId, ParticipantId, SfuPublisherDescription, SfuSubscription};
use aero_live_webrtc::{KeyframeRequestKind, Rid};

use super::CallBridgeSupervisor;
use crate::sfu_media::{SfuMediaError, SfuSessionEnded, SfuTopologySnapshot};

impl CallBridgeSupervisor {
    /// Take the sole stream of spontaneous SFU owner-task exits. Explicit
    /// leave/reconnect/end paths remove their registry entry before aborting the
    /// task, so only unexpected current-generation exits appear here.
    pub fn take_sfu_lifecycle_events(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<SfuSessionEnded>> {
        self.media.take_lifecycle_events()
    }

    /// Validate and atomically replace one subscriber's publisher-scoped routes.
    pub fn replace_sfu_subscriptions(
        &self,
        call: CallId,
        subscriber: ParticipantId,
        session_generation: u64,
        revision: u64,
        routes: Vec<SfuSubscription>,
    ) -> Result<usize, SfuMediaError> {
        self.media
            .replace_subscriptions(call, subscriber, session_generation, revision, routes)
    }

    pub async fn add_sfu_ice_for_generation(
        &self,
        call: CallId,
        participant: ParticipantId,
        session_generation: u64,
        candidate: String,
    ) -> Result<(), SfuMediaError> {
        self.media
            .add_remote_candidate_for_generation(call, participant, session_generation, candidate)
            .await
    }

    /// Queue cross-node PLI/FIR feedback on the local publisher owner task.
    pub fn request_sfu_keyframe(
        &self,
        call: CallId,
        publisher: ParticipantId,
        mid: &str,
        rid: Option<Rid>,
        kind: KeyframeRequestKind,
    ) -> Result<(), SfuMediaError> {
        self.media
            .request_publisher_keyframe(call, publisher, mid, rid, kind)
    }

    /// Queue cross-node REMB feedback on the local publisher owner task.
    pub fn request_sfu_remb(
        &self,
        call: CallId,
        publisher: ParticipantId,
        mid: &str,
        bitrate_bps: u64,
    ) -> Result<(), SfuMediaError> {
        self.media
            .request_publisher_remb(call, publisher, mid, bitrate_bps)
    }

    /// Fold a cluster publisher event into this node's revisioned topology.
    pub fn observe_sfu_publisher(
        &self,
        call: CallId,
        publisher: SfuPublisherDescription,
        leg_generation: i64,
        active: bool,
    ) -> Option<SfuTopologySnapshot> {
        self.media
            .observe_publisher(call, publisher, leg_generation, active)
    }

    #[must_use]
    pub fn sfu_topology(&self, call: CallId) -> SfuTopologySnapshot {
        self.media.topology(call)
    }

    #[must_use]
    pub fn sfu_publisher(
        &self,
        call: CallId,
        participant: ParticipantId,
    ) -> Option<SfuPublisherDescription> {
        self.media.publisher(call, participant)
    }

    /// Local owner-task publishers, used to replay topology on call joins.
    #[must_use]
    pub fn local_sfu_publishers(&self, call: CallId) -> Vec<(SfuPublisherDescription, i64)> {
        self.media.local_publishers(call)
    }

    /// Remove one media leg and return the topology clients must renegotiate.
    pub fn remove_sfu_session_with_topology(
        &self,
        call: CallId,
        participant: ParticipantId,
    ) -> (bool, Option<SfuTopologySnapshot>) {
        let removal = self.media.remove_session_with_topology(call, participant);
        if let Some(generation) = removal.ended_egress_generation {
            self.cancel_ended_media_epoch(call, generation);
        }
        (removal.removed, removal.topology)
    }

    /// Remove only the browser media leg belonging to one durable logical
    /// participant incarnation.
    pub fn remove_sfu_session_generation_with_topology(
        &self,
        call: CallId,
        participant: ParticipantId,
        leg_generation: i64,
    ) -> (bool, Option<SfuTopologySnapshot>) {
        let removal =
            self.media
                .remove_session_generation_with_topology(call, participant, leg_generation);
        if let Some(generation) = removal.ended_egress_generation {
            self.cancel_ended_media_epoch(call, generation);
        }
        (removal.removed, removal.topology)
    }

    /// Remove every local media leg for a disconnected participant.
    pub fn remove_sfu_participant_with_topology(
        &self,
        participant: ParticipantId,
    ) -> Vec<(CallId, SfuTopologySnapshot)> {
        let changed = self.media.remove_participant_with_topology(participant);
        for (call, _, generation) in &changed {
            if let Some(generation) = generation {
                self.cancel_ended_media_epoch(*call, *generation);
            }
        }
        changed
            .into_iter()
            .map(|(call, topology, _)| (call, topology))
            .collect()
    }
}
