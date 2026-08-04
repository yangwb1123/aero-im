//! `WebRTC` Selective Forwarding Unit (SFU) — group calls + interactive live.
//!
//! ## Scope
//!
//! A real [`str0m`](https://docs.rs/str0m)-backed SFU: each participant is a
//! sans-IO `str0m` [`Rtc`](str0m::Rtc) instance ([`SfuPeer`]); one publisher's
//! RTP is forwarded to every subscriber that subscribed to that track, with
//! per-subscriber sequence-number / timestamp remapping.
//!
//! - [`SfuRouter`] — keyed by call id, tracks publishers and subscribers with
//!   simple add/remove semantics. Holds the per-call media routing topology.
//! - [`PeerRole`] — `Publisher` / `Subscriber` / `Bidirectional`.
//! - [`SfuPeer`] — wraps a `str0m` `Rtc`: SDP offer/answer + the
//!   `poll_output`/`handle_input` loop exposed as [`peer::PeerProgress`].
//! - [`SfuForwarder`] — the real [`MediaForwarder`]: routes inbound RTP from a
//!   publisher to subscriber peers, writing remapped RTP onto each subscriber's
//!   matching outbound `str0m` stream.
//! - [`remap`] — the **pure**, fully unit-tested routing + RTP header-remap
//!   bookkeeping (`ForwardTable`, `RtpRemapper`), with no IO dependency.
//! - [`codec`] — codec-aware keyframe detection ([`Codec`] +
//!   [`payload_is_keyframe`]) dispatching to the pure payload-descriptor
//!   parsers in [`h264`], [`vp8`] and [`vp9`].
//!
//! ## What's verified vs. pending
//!
//! Compiles against `str0m` 0.19 (RTP mode, pure-Rust crypto backend) and the
//! pure routing/remap logic is unit-tested. `aero-server` owns each peer in a
//! per-participant UDP task and drives the loop through [`SfuPeer`]. The actual
//! ICE/DTLS/SRTP handshake and end-to-end browser forwarding still require real
//! browsers (or a paired str0m endpoint) and are verified in staging rather
//! than in hermetic CI.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use aero_common::{CallId, ParticipantId, SfuSubscription};
use parking_lot::RwLock;
use thiserror::Error;

pub mod av1;
pub mod bridge_frame;
pub mod bwe;
pub mod call_bridge;
pub mod codec;
pub mod forward;
pub mod h264;
pub mod h265;
pub mod peer;
pub mod remap;
pub mod rtcp_fb;
pub mod rtcp_feedback;
pub mod simulcast;
pub mod vp8;
pub mod vp9;

pub use bridge_frame::{
    decode_bound_bridge_frame, decode_bridge_frame, encode_bound_bridge_frame,
    encode_bound_bridge_frame_version, encode_bridge_frame, BoundBridgeFrame,
    BOUND_BRIDGE_COMPAT_VERSION, BOUND_BRIDGE_VERSION,
};
pub use bwe::{BandwidthEstimator, BweConfig, LayerSwitchPolicy, ThroughputEwma};
pub use call_bridge::{
    decide_call_topology, BridgeRtp, CallBridge, CallEgress, CallEgressTap, CallTopology,
    CallUpstream, FakeCallUpstream, LoopbackUpstream,
};
pub use codec::{payload_is_keyframe, Codec};
pub use forward::{PendingRemb, SfuForwarder};
pub use peer::{canonical_mid, BandwidthEstimate, InboundRtp, KeyframeReq, PeerProgress, SfuPeer};
pub use remap::{ForwardTable, ForwardTarget, RemappedRtp, RtpKey, RtpRemapper};
pub use rtcp_fb::{
    encode_remb, BandwidthFeedback, PublisherRembAggregator, Remb, RembAggregatorConfig,
    TwccFeedback, TwccStatus, TwccSummary,
};
pub use rtcp_feedback::{KeyframeGate, ParsedFeedback, PendingKeyframeRequest};
pub use simulcast::{
    ForwardDecision, LayerKind, LayerSelector, LayerSelectorTable, LayerSet, SimulcastLayer,
};
pub use str0m::media::{KeyframeRequestKind, Mid, Rid};

/// Errors surfaced by the `str0m`-backed peer / forwarder.
#[derive(Debug, Error)]
pub enum SfuError {
    /// SDP offer/answer parsing or negotiation failed.
    #[error("sdp error: {0}")]
    Sdp(String),
    /// Inbound datagram could not be parsed as STUN/DTLS/RTP.
    #[error("net error: {0}")]
    Net(String),
    /// The underlying `str0m` `Rtc` returned an error.
    #[error("rtc error: {0}")]
    Rtc(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeerRole {
    Publisher,
    Subscriber,
    Bidirectional,
}

/// In-memory roster of an active call. Cheap to clone (Arc inside).
#[derive(Default, Clone)]
pub struct SfuRouter {
    inner: Arc<RwLock<HashMap<CallId, CallState>>>,
}

#[derive(Default)]
struct CallState {
    peers: HashMap<ParticipantId, PeerRole>,
    /// Peers whose browser/media leg is owned by this process.
    ///
    /// Bridge fan-in also needs synthetic entries in `peers` for ordinary SFU
    /// forwarding, but those entries must never be advertised as this node's
    /// call-route ownership.
    local_peers: HashSet<ParticipantId>,
    /// Reference count of active bridge pulls that expose each remote
    /// publisher. Route hand-offs may briefly make two upstream bridges expose
    /// the same publisher, so a set would let the first detach remove a peer
    /// still used by the second.
    bridged_peers: HashMap<ParticipantId, usize>,
    /// `(publisher, published_mid)`. MIDs are scoped to a peer connection, so
    /// multiple publishers in one call may legitimately use `"0"`/`"1"`.
    tracks: HashSet<(ParticipantId, String)>,
    /// `subscriber -> set of published mids they receive`.
    subscriptions: HashMap<ParticipantId, HashSet<String>>,
}

impl SfuRouter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register or refresh a browser/media peer owned by this process.
    pub fn add_peer(&self, call: CallId, peer: ParticipantId, role: PeerRole) {
        let mut w = self.inner.write();
        let state = w.entry(call).or_default();
        state.peers.insert(peer, role);
        state.local_peers.insert(peer);
    }

    /// Register a remote publisher pulled through a node-to-node call bridge.
    ///
    /// Bridged publishers participate in the local forwarding topology, but
    /// are deliberately excluded from [`Self::roster_snapshot`] so this node
    /// cannot claim their call-route lease and create a reverse bridge.
    pub fn add_bridged_peer(&self, call: CallId, peer: ParticipantId, role: PeerRole) -> bool {
        let mut w = self.inner.write();
        let state = w.entry(call).or_default();
        state.peers.entry(peer).or_insert(role);
        let references = state.bridged_peers.entry(peer).or_default();
        let install_bridge_sink = *references == 0 && !state.local_peers.contains(&peer);
        *references = references.saturating_add(1);
        install_bridge_sink
    }

    /// Release one bridge's reference to a synthetic remote publisher.
    ///
    /// A coincident local leg wins: detaching its bridge representation must
    /// not tear down the browser-owned peer or its tracks. Returns whether the
    /// bridge-owned forwarder sink is no longer needed. A missing router entry
    /// also returns `true`: call cleanup may have removed the topology before
    /// the bridge task observes cancellation, but its sink must still detach.
    pub fn release_bridged_peer(&self, call: CallId, peer: ParticipantId) -> bool {
        let mut w = self.inner.write();
        let Some(state) = w.get_mut(&call) else {
            return true;
        };
        let Some(references) = state.bridged_peers.get_mut(&peer) else {
            return !state.local_peers.contains(&peer);
        };
        *references = references.saturating_sub(1);
        let mut release_sink = false;
        if *references == 0 {
            state.bridged_peers.remove(&peer);
            if !state.local_peers.contains(&peer) {
                state.peers.remove(&peer);
                state.subscriptions.remove(&peer);
                state.tracks.retain(|(owner, _)| *owner != peer);
                release_sink = true;
            }
        }
        if state.peers.is_empty() {
            w.remove(&call);
        }
        release_sink
    }

    /// Remove this process's local ownership of `peer`.
    ///
    /// A live bridge reference keeps the participant in the forwarding
    /// topology as a synthetic publisher. Returns whether the call now has no
    /// locally-owned peers, regardless of synthetic bridge peers.
    pub fn remove_peer(&self, call: CallId, peer: ParticipantId) -> bool {
        let mut w = self.inner.write();
        let Some(state) = w.get_mut(&call) else {
            return false;
        };
        state.local_peers.remove(&peer);
        state.subscriptions.remove(&peer);
        if state.bridged_peers.get(&peer).copied().unwrap_or(0) == 0 {
            state.peers.remove(&peer);
            state.tracks.retain(|(owner, _)| *owner != peer);
        } else {
            // The local browser leg left while a bridge still exposes this
            // participant. Preserve the synthetic publisher and its tracks.
            state.peers.insert(peer, PeerRole::Publisher);
        }
        let last_local = state.local_peers.is_empty();
        if state.peers.is_empty() {
            w.remove(&call);
        }
        last_local
    }

    pub fn add_track(&self, call: CallId, mid: &str, owner: ParticipantId) {
        let mut w = self.inner.write();
        let state = w.entry(call).or_default();
        state.tracks.insert((owner, mid.to_owned()));
    }

    pub fn add_subscription(&self, call: CallId, mid: &str, subscriber: ParticipantId) {
        let mut w = self.inner.write();
        let state = w.entry(call).or_default();
        state
            .subscriptions
            .entry(subscriber)
            .or_default()
            .insert(mid.to_owned());
    }

    /// The publisher that owns `mid`, if known.
    #[must_use]
    pub fn owner_of(&self, call: CallId, mid: &str) -> Option<ParticipantId> {
        let r = self.inner.read();
        r.get(&call).and_then(|state| {
            state
                .tracks
                .iter()
                .find_map(|(owner, track_mid)| (track_mid == mid).then_some(*owner))
        })
    }

    /// Whether this exact publisher owns `mid` in `call`.
    #[must_use]
    pub fn has_track(&self, call: CallId, publisher: ParticipantId, mid: &str) -> bool {
        self.inner
            .read()
            .get(&call)
            .is_some_and(|state| state.tracks.contains(&(publisher, mid.to_owned())))
    }

    /// Explicit publisher-scoped track snapshot for a call.
    #[must_use]
    pub fn published_tracks(&self, call: CallId) -> Vec<(ParticipantId, String)> {
        self.inner
            .read()
            .get(&call)
            .map_or_else(Vec::new, |state| state.tracks.iter().cloned().collect())
    }

    #[must_use]
    pub fn subscribers_for(&self, call: CallId, mid: &str) -> Vec<ParticipantId> {
        let r = self.inner.read();
        let Some(state) = r.get(&call) else {
            return Vec::new();
        };
        state
            .subscriptions
            .iter()
            .filter_map(|(pid, set)| if set.contains(mid) { Some(*pid) } else { None })
            .collect()
    }

    #[must_use]
    pub fn participants(&self, call: CallId) -> Vec<ParticipantId> {
        let r = self.inner.read();
        r.get(&call)
            .map(|s| s.peers.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Browser/media participants whose live leg is owned by this process.
    ///
    /// Unlike [`Self::participants`], this excludes synthetic publishers
    /// injected by call bridges.
    #[must_use]
    pub fn local_participants(&self, call: CallId) -> Vec<ParticipantId> {
        let r = self.inner.read();
        r.get(&call)
            .map(|state| state.local_peers.iter().copied().collect())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn has_local_participants(&self, call: CallId) -> bool {
        self.inner
            .read()
            .get(&call)
            .is_some_and(|state| !state.local_peers.is_empty())
    }

    #[must_use]
    pub fn call_count(&self) -> usize {
        self.inner.read().len()
    }

    /// Every active call id (those with at least one peer). Used by the server's
    /// cross-node call-route heartbeat (ROADMAP3 方向二) to enumerate this node's
    /// locally-hosted calls without a separate tracked set.
    #[must_use]
    pub fn calls(&self) -> Vec<CallId> {
        self.inner.read().keys().copied().collect()
    }

    /// A flat snapshot of `(call, participant)` for every browser/media leg
    /// this node owns, across all active calls. Synthetic publishers pulled
    /// from another node stay in the forwarding topology but are excluded from
    /// this call-route heartbeat source of truth.
    #[must_use]
    pub fn roster_snapshot(&self) -> Vec<(CallId, ParticipantId)> {
        let r = self.inner.read();
        r.iter()
            .flat_map(|(call, state)| state.local_peers.iter().map(move |p| (*call, *p)))
            .collect()
    }
}

/// Abstraction the SFU loop uses to fan a publisher's RTP out to subscribers.
///
/// Bridge callers should use [`MediaForwarder::forward_bridge_rtp`], which
/// retains the parsed RTP header fields required by the real
/// [`SfuForwarder::on_rtp`] path. `forward_rtp` remains as a compatibility seam
/// for existing server wrappers; its byte payload is a complete encoded
/// [`BridgeRtp`], not bare media bytes.
#[async_trait::async_trait]
pub trait MediaForwarder: Send + Sync {
    async fn forward_rtp(&self, call: CallId, mid: &str, packet: bytes::Bytes);

    /// Forward one decrypted packet from a local server-owned media session.
    ///
    /// The default keeps non-SFU test adapters source-compatible.
    fn forward_inbound_rtp(
        &self,
        _call: CallId,
        _publisher: ParticipantId,
        _packet: &InboundRtp,
    ) -> usize {
        0
    }

    /// Attach the command sink for a production media session.
    ///
    /// The session task remains the sole owner of its [`SfuPeer`]; forwarding
    /// enqueues bounded write commands through this sink instead of moving the
    /// peer into a second polling owner. Non-SFU implementations may leave the
    /// default no-op behavior.
    fn attach_peer_sink(
        &self,
        _call: CallId,
        _participant: ParticipantId,
        _sink: Arc<dyn SfuPeerSink>,
    ) -> bool {
        false
    }

    /// Attach feedback control for a publisher pulled from `bridge`.
    ///
    /// Production forwarders keep these sinks separate from locally-owned
    /// media-session sinks: local ownership always wins while present, and a
    /// still-live bridge automatically becomes the fallback after local
    /// ownership leaves.
    fn attach_bridge_peer_sink(
        &self,
        _call: CallId,
        _participant: ParticipantId,
        _bridge: &str,
        _sink: Arc<dyn SfuPeerSink>,
    ) -> bool {
        false
    }

    /// Detach a production media-session sink and all subscriber routing state.
    fn detach_peer_sink(&self, _call: CallId, _participant: ParticipantId) -> bool {
        false
    }

    /// Detach only one bridge's publisher-feedback sink.
    fn detach_bridge_peer_sink(
        &self,
        _call: CallId,
        _participant: ParticipantId,
        _bridge: &str,
    ) -> bool {
        false
    }

    /// Replace a reconnecting peer owner while preserving routes that source
    /// this participant's unchanged publisher tracks.
    ///
    /// The reconnecting participant's subscriber routes are always cleared and
    /// must be installed again for the new session generation.
    fn reset_peer_sink(&self, call: CallId, participant: ParticipantId) -> bool {
        self.detach_peer_sink(call, participant)
    }

    /// Atomically replace one subscriber's explicit publisher-scoped routes.
    fn replace_subscriptions(
        &self,
        _call: CallId,
        _subscriber: ParticipantId,
        _routes: &[SfuSubscription],
    ) -> usize {
        0
    }

    /// Relay subscriber PLI/FIR to the exact publisher owner task.
    fn forward_keyframe_request(
        &self,
        _call: CallId,
        _subscriber: ParticipantId,
        _out_mid: &str,
        _kind: str0m::media::KeyframeRequestKind,
    ) -> bool {
        false
    }

    /// Relay a REMB/TWCC estimate through publisher-facing aggregation.
    fn forward_bandwidth_estimate(
        &self,
        _call: CallId,
        _subscriber: ParticipantId,
        _out_mid: Option<&str>,
        _bitrate_bps: u64,
    ) -> usize {
        0
    }

    /// Forward one metadata-complete packet pulled across the call bridge.
    ///
    /// The default preserves compatibility with wrappers that only delegate
    /// [`Self::forward_rtp`]. [`SfuForwarder`] overrides this method to avoid the
    /// encode/decode hop and invoke its header-aware hot path directly.
    async fn forward_bridge_rtp(&self, call: CallId, packet: BridgeRtp) -> usize {
        let mid = packet.mid.to_string();
        let encoded = bytes::Bytes::from(encode_bridge_frame(&packet));
        self.forward_rtp(call, &mid, encoded).await;
        // Compatibility wrappers expose no delivery result. The packet was still
        // forwarded; callers that use `SfuForwarder` directly get its exact count.
        0
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NullForwarder;

#[async_trait::async_trait]
impl MediaForwarder for NullForwarder {
    async fn forward_rtp(&self, _call: CallId, _mid: &str, _packet: bytes::Bytes) {}

    async fn forward_bridge_rtp(&self, _call: CallId, _packet: BridgeRtp) -> usize {
        0
    }
}

/// Bounded command endpoint owned by a server-side SFU media session.
///
/// [`SfuForwarder`] calls this synchronously from its RTP hot path. Returning
/// `false` means the session is closed or its bounded command queue is full, so
/// the realtime packet is dropped rather than blocking every publisher.
pub trait SfuPeerSink: Send + Sync {
    fn try_write_rtp(&self, packet: InboundRtp) -> bool;

    /// Queue PLI/FIR onto the publisher's unique peer-owner task.
    fn try_request_keyframe(
        &self,
        _mid: str0m::media::Mid,
        _kind: str0m::media::KeyframeRequestKind,
    ) -> bool {
        false
    }

    /// Queue PLI/FIR for one exact simulcast layer when `rid` is present.
    ///
    /// Implementations that predate RID-aware feedback keep their MID-scoped
    /// behavior through this default; bridge transports override it so one
    /// layer's keyframe cannot accidentally open another layer's startup gate.
    fn try_request_keyframe_for_rid(
        &self,
        mid: str0m::media::Mid,
        _rid: Option<str0m::media::Rid>,
        kind: str0m::media::KeyframeRequestKind,
    ) -> bool {
        self.try_request_keyframe(mid, kind)
    }

    /// Queue REMB onto the publisher's unique peer-owner task.
    fn try_request_remb(&self, _mid: str0m::media::Mid, _bitrate_bps: u64) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_remove_peer_clears_call_when_empty() {
        let r = SfuRouter::new();
        let call = CallId::new();
        let a = ParticipantId::new();
        let b = ParticipantId::new();
        r.add_peer(call, a, PeerRole::Publisher);
        r.add_peer(call, b, PeerRole::Subscriber);
        assert_eq!(r.call_count(), 1);
        assert!(!r.remove_peer(call, a));
        assert!(r.remove_peer(call, b));
        assert_eq!(r.call_count(), 0);
    }

    #[test]
    fn subscribers_for_routes_correctly() {
        let r = SfuRouter::new();
        let call = CallId::new();
        let pub_ = ParticipantId::new();
        let sub1 = ParticipantId::new();
        let sub2 = ParticipantId::new();
        r.add_peer(call, pub_, PeerRole::Publisher);
        r.add_peer(call, sub1, PeerRole::Subscriber);
        r.add_peer(call, sub2, PeerRole::Subscriber);
        r.add_track(call, "0", pub_);
        r.add_subscription(call, "0", sub1);
        r.add_subscription(call, "0", sub2);
        let s = r.subscribers_for(call, "0");
        assert_eq!(s.len(), 2);
        assert!(s.contains(&sub1));
        assert!(s.contains(&sub2));
    }

    #[test]
    fn owner_of_resolves_publisher() {
        let r = SfuRouter::new();
        let call = CallId::new();
        let pub_ = ParticipantId::new();
        r.add_track(call, "v0", pub_);
        assert_eq!(r.owner_of(call, "v0"), Some(pub_));
        assert_eq!(r.owner_of(call, "missing"), None);
    }

    #[test]
    fn track_dropped_when_owner_leaves() {
        let r = SfuRouter::new();
        let call = CallId::new();
        let pub_ = ParticipantId::new();
        r.add_peer(call, pub_, PeerRole::Publisher);
        r.add_track(call, "0", pub_);
        r.remove_peer(call, pub_);
        assert_eq!(r.call_count(), 0);
    }

    #[test]
    fn bridged_peers_are_forwarded_but_never_enter_the_local_route_roster() {
        let router = SfuRouter::new();
        let call = CallId::new();
        let local = ParticipantId::new();
        let remote = ParticipantId::new();

        router.add_peer(call, local, PeerRole::Bidirectional);
        assert!(router.add_bridged_peer(call, remote, PeerRole::Publisher));
        assert!(!router.add_bridged_peer(call, remote, PeerRole::Publisher));

        assert!(router.participants(call).contains(&remote));
        assert_eq!(router.local_participants(call), vec![local]);
        assert_eq!(router.roster_snapshot(), vec![(call, local)]);

        assert!(
            !router.release_bridged_peer(call, remote),
            "the first overlapping bridge release keeps the synthetic peer"
        );
        assert!(router.participants(call).contains(&remote));
        assert!(
            router.release_bridged_peer(call, remote),
            "the final bridge release removes the synthetic peer"
        );
        assert!(!router.participants(call).contains(&remote));
        assert!(router.has_local_participants(call));
    }

    #[test]
    fn bridge_detach_cannot_remove_a_coincident_local_peer() {
        let router = SfuRouter::new();
        let call = CallId::new();
        let participant = ParticipantId::new();

        assert!(router.add_bridged_peer(call, participant, PeerRole::Publisher));
        router.add_peer(call, participant, PeerRole::Bidirectional);

        assert!(!router.release_bridged_peer(call, participant));
        assert_eq!(router.participants(call), vec![participant]);
        assert_eq!(router.roster_snapshot(), vec![(call, participant)]);
    }

    #[test]
    fn local_leave_preserves_an_active_bridged_representation() {
        let router = SfuRouter::new();
        let call = CallId::new();
        let participant = ParticipantId::new();

        router.add_peer(call, participant, PeerRole::Bidirectional);
        router.add_track(call, "video0", participant);
        assert!(
            !router.add_bridged_peer(call, participant, PeerRole::Publisher),
            "a bridge must not replace the local owner sink"
        );

        assert!(
            router.remove_peer(call, participant),
            "the local route roster is now empty"
        );
        assert!(router.participants(call).contains(&participant));
        assert!(router.has_track(call, participant, "video0"));
        assert!(router.roster_snapshot().is_empty());

        assert!(router.release_bridged_peer(call, participant));
        assert!(router.participants(call).is_empty());
    }
}
