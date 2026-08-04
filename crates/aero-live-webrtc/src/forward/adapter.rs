//! [`MediaForwarder`](crate::MediaForwarder) trait-object adapter for the
//! concrete [`SfuForwarder`](super::SfuForwarder).

use std::sync::Arc;

use aero_common::{CallId, ParticipantId, SfuSubscription};
use str0m::media::KeyframeRequestKind;
use tracing::trace;

use super::SfuForwarder;
use crate::{BridgeRtp, InboundRtp, SfuPeerSink};

#[async_trait::async_trait]
impl crate::MediaForwarder for SfuForwarder {
    async fn forward_rtp(&self, call: CallId, mid: &str, packet: bytes::Bytes) {
        let Some(bridged) = crate::decode_bridge_frame(&packet) else {
            trace!(%call, mid, "bridge forwarder: dropping incomplete RTP frame");
            return;
        };
        if bridged.mid.to_string() != mid {
            trace!(
                %call,
                expected_mid = mid,
                actual_mid = %bridged.mid,
                "bridge forwarder: dropping mismatched MID"
            );
            return;
        }
        let _ = if self
            .inner
            .lock()
            .peer_sinks
            .keys()
            .any(|(active_call, _)| *active_call == call)
        {
            self.on_call_bridge_rtp(call, &bridged)
        } else {
            // Compatibility for the original direct-owned-peer API. Production
            // server sessions always take the call-scoped branch above.
            self.on_bridge_rtp(&bridged)
        };
    }

    async fn forward_bridge_rtp(&self, call: CallId, packet: BridgeRtp) -> usize {
        if self
            .inner
            .lock()
            .peer_sinks
            .keys()
            .any(|(active_call, _)| *active_call == call)
        {
            self.on_call_bridge_rtp(call, &packet)
        } else {
            self.on_bridge_rtp(&packet)
        }
    }

    fn forward_inbound_rtp(
        &self,
        call: CallId,
        publisher: ParticipantId,
        packet: &InboundRtp,
    ) -> usize {
        self.on_call_rtp(call, publisher, packet)
    }

    fn attach_peer_sink(
        &self,
        call: CallId,
        participant: ParticipantId,
        sink: Arc<dyn SfuPeerSink>,
    ) -> bool {
        Self::attach_peer_sink(self, call, participant, sink)
    }

    fn attach_bridge_peer_sink(
        &self,
        call: CallId,
        participant: ParticipantId,
        bridge: &str,
        sink: Arc<dyn SfuPeerSink>,
    ) -> bool {
        Self::attach_bridge_peer_sink(self, call, participant, bridge, sink)
    }

    fn detach_peer_sink(&self, call: CallId, participant: ParticipantId) -> bool {
        Self::detach_peer_sink(self, call, participant)
    }

    fn detach_bridge_peer_sink(
        &self,
        call: CallId,
        participant: ParticipantId,
        bridge: &str,
    ) -> bool {
        Self::detach_bridge_peer_sink(self, call, participant, bridge)
    }

    fn reset_peer_sink(&self, call: CallId, participant: ParticipantId) -> bool {
        Self::reset_peer_sink(self, call, participant)
    }

    fn replace_subscriptions(
        &self,
        call: CallId,
        subscriber: ParticipantId,
        routes: &[SfuSubscription],
    ) -> usize {
        self.replace_call_subscriptions(call, subscriber, routes)
    }

    fn forward_keyframe_request(
        &self,
        call: CallId,
        subscriber: ParticipantId,
        out_mid: &str,
        kind: KeyframeRequestKind,
    ) -> bool {
        self.on_call_keyframe_request(call, subscriber, out_mid, kind)
    }

    fn forward_bandwidth_estimate(
        &self,
        call: CallId,
        subscriber: ParticipantId,
        out_mid: Option<&str>,
        bitrate_bps: u64,
    ) -> usize {
        self.on_call_bandwidth_estimate(call, subscriber, out_mid, bitrate_bps)
    }
}
