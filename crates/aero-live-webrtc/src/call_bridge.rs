//! Inter-node group-call **media bridge**: pull remote participants' RTP
//! **once** per node and fan it into the local SFU.
//!
//! ## Why bridge
//!
//! The [`SfuRouter`] is per-process: a group call whose participants land on
//! different nodes has, today, N disjoint islands of media. The cross-node
//! `CallRouteRegistry` (in `aero-storage`) records which node hosts which
//! participant; this module owns the leg that makes those islands hear each
//! other:
//!
//! - **Serve local** — every participant is on this node; the SFU forwards as
//!   it always has. No inter-node traffic.
//! - **Bridge from peers** — for each *other* node hosting participants of the
//!   call, open **one** [`CallBridge`] that pulls those participants' RTP and
//!   re-publishes it into the local [`SfuRouter`] as **synthetic publisher
//!   peers** — so local subscribers receive remote media through exactly the
//!   same forwarding path as local publishers. One inter-node flow per node
//!   pair, amortized across all local subscribers.
//!
//! Which of the two applies is the **pure** [`decide_call_topology`] policy.
//!
//! This mirrors `aero-live-whip`'s cascade ([`UpstreamSource`] →
//! `CascadeRelay`) one layer down the stack: a transport-abstracted
//! [`CallUpstream`] plus a pump loop, with keyframe-gated startup so the first
//! packets fanned out for each bridged track form a decodable random-access
//! point.
//!
//! ## Production transport boundary
//!
//! Everything here is unit-tested against [`FakeCallUpstream`], a scripted
//! in-memory source; socket ownership deliberately lives in `aero-server`.
//! Its production `NodeRtpPullerFactory` and `UdpRtpEgress` implement framed
//! plain-RTP pull/push, while a secret-gated HTTP control plane carries
//! subscribe and PLI/FIR/REMB feedback commands. Localhost tests cover those
//! transports. A deployment still needs a real two-node reachability run in
//! staging.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aero_common::{CallId, ParticipantId};
use async_trait::async_trait;
use bytes::Bytes;
use str0m::media::{KeyframeRequestKind, Mid, Pt, Rid};
use str0m::rtp::{ExtensionValues, SeqNo, Ssrc};
use tokio::sync::broadcast;
use tracing::{debug, trace};

use crate::{InboundRtp, MediaForwarder, PeerRole, SfuPeerSink, SfuRouter};

static NEXT_BRIDGE_SINK_OWNER: AtomicU64 = AtomicU64::new(1);
const KEYFRAME_RETRY_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct BridgeTrack {
    participant: ParticipantId,
    mid: Mid,
    rid: Option<Rid>,
}

impl BridgeTrack {
    fn from_rtp(rtp: &BridgeRtp) -> Self {
        Self {
            participant: rtp.participant,
            mid: rtp.mid,
            rid: rtp.rid,
        }
    }
}

/// One RTP packet pulled from (or exposed to) a peer node, attributed to the
/// participant that published it and the track (`mid`) it belongs to.
///
/// `Bytes` is reference-counted, so fanning a packet out is zero-copy. Besides
/// the media payload, this carries every RTP header field the header-aware
/// [`crate::SfuForwarder::on_rtp`] path needs to remap and write the packet on a
/// subscriber leg. Transport-local extensions are deliberately not copied
/// between nodes; MID and RID are retained explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeRtp {
    /// The remote participant whose media this packet carries.
    pub participant: ParticipantId,
    /// The publisher-scoped track id on the owning node. It may collide with
    /// another participant's MID because production routes key the source by
    /// `(participant, mid)`.
    pub mid: Mid,
    /// Simulcast restriction identifier, when the source packet carried one.
    pub rid: Option<Rid>,
    /// Negotiated RTP payload type.
    pub pt: Pt,
    /// Extended sequence number, including rollover state.
    pub seq_no: SeqNo,
    /// RTP media timestamp in the codec clock.
    pub rtp_time: u32,
    /// Publisher-facing synchronization source.
    pub ssrc: Ssrc,
    /// RTP marker bit.
    pub marker: bool,
    /// RTP media payload (without its wire header).
    pub payload: Bytes,
    /// `true` if this packet starts a decodable random-access point.
    pub keyframe: bool,
    /// Whether a newly attached bridge must wait for [`Self::keyframe`] before
    /// forwarding this track. Video codecs set this; audio/unknown codecs do
    /// not have a keyframe concept and must pass immediately.
    pub requires_keyframe: bool,
}

impl BridgeRtp {
    /// Build a bridged packet.
    #[must_use]
    pub fn new(
        participant: ParticipantId,
        mid: &str,
        payload: impl Into<Bytes>,
        keyframe: bool,
    ) -> Self {
        Self {
            participant,
            mid: Mid::from(mid),
            rid: None,
            pt: Pt::from(96),
            seq_no: SeqNo::from(0),
            rtp_time: 0,
            ssrc: Ssrc::from(0),
            marker: false,
            payload: payload.into(),
            keyframe,
            requires_keyframe: true,
        }
    }

    /// Override whether this packet's track is subject to bridge keyframe
    /// startup gating.
    #[must_use]
    pub fn with_keyframe_requirement(mut self, requires_keyframe: bool) -> Self {
        self.requires_keyframe = requires_keyframe;
        self
    }

    /// Lift a local, decrypted RTP packet into the complete bridge
    /// representation. This is the only constructor the production SFU media
    /// path should use; [`Self::new`] keeps compact test fixtures ergonomic.
    #[must_use]
    pub fn from_inbound(participant: ParticipantId, rtp: &InboundRtp) -> Self {
        Self {
            participant,
            mid: rtp.mid,
            rid: rtp.rid,
            pt: rtp.pt,
            seq_no: rtp.seq_no,
            rtp_time: rtp.rtp_time,
            ssrc: rtp.ssrc,
            marker: rtp.marker,
            payload: Bytes::copy_from_slice(&rtp.payload),
            keyframe: rtp.is_keyframe,
            requires_keyframe: rtp.requires_keyframe,
        }
    }

    /// Rebuild the header-aware packet consumed by
    /// [`crate::SfuForwarder::on_rtp`] on the pulling node.
    ///
    /// Wall-clock arrival time and hop-local RTP extensions are intentionally
    /// refreshed at this node. MID and RID are restored into `ext_vals` because
    /// those two values describe media routing rather than one transport hop.
    #[must_use]
    pub fn to_inbound(&self) -> InboundRtp {
        let ext_vals = ExtensionValues {
            mid: Some(self.mid),
            rid: self.rid,
            ..ExtensionValues::default()
        };
        InboundRtp {
            mid: self.mid,
            pt: self.pt,
            seq_no: self.seq_no,
            rtp_time: self.rtp_time,
            ssrc: self.ssrc,
            marker: self.marker,
            ext_vals,
            wallclock: std::time::Instant::now(),
            payload: self.payload.to_vec(),
            rid: self.rid,
            is_keyframe: self.keyframe,
            requires_keyframe: self.requires_keyframe,
        }
    }
}

/// Remote participants' RTP this node pulls from the node that owns them.
///
/// A `CallUpstream` yields [`BridgeRtp`] packets pulled from one peer node for
/// one call. [`next_rtp`](CallUpstream::next_rtp) is asynchronous: it awaits
/// the next packet and returns `None` at end-of-stream (the remote node's last
/// participant left, or the link dropped).
///
/// `aero-server` supplies the production UDP implementation. It announces its
/// receive address through the cluster-authenticated bridge endpoint, decodes
/// metadata-complete frames into [`BridgeRtp`], and exposes a bounded feedback
/// sink that POSTs PLI/FIR/REMB to the publisher's owning node.
#[async_trait]
pub trait CallUpstream: Send {
    /// The call being bridged.
    fn call_id(&self) -> CallId;

    /// Public base URL of the peer node this upstream pulls from.
    fn node_url(&self) -> &str;

    /// Build a participant-bound feedback sink for PLI/FIR/REMB flowing back
    /// to the peer node that owns this publisher.
    fn feedback_sink(&self, _participant: ParticipantId) -> Option<Arc<dyn SfuPeerSink>> {
        None
    }

    /// Await and return the next packet, or `None` at end-of-stream.
    async fn next_rtp(&mut self) -> Option<BridgeRtp>;
}

/// Wires a [`CallUpstream`] into the local [`SfuRouter`] so remote
/// participants' RTP fans out to this node's **local** subscribers.
///
/// Each remote participant is registered as a **synthetic publisher peer**
/// (with its tracks) the first time one of its packets arrives, so local
/// subscribers can subscribe to the bridged mids exactly as they would to a
/// local publisher's. Packets are then handed to the [`MediaForwarder`] — the
/// same fan-out path local RTP takes.
///
/// Drive the bridge with [`run`](Self::run) (production: the server spawns one
/// per `BridgeTo` target) or step it deterministically with
/// [`pump_once`](Self::pump_once) (tests).
///
/// ## Keyframe-gated startup
///
/// Constructed with [`new`](Self::new), the bridge withholds each bridged
/// track's fan-out until that track's first keyframe packet arrives —
/// pre-keyframe packets are dropped so the first bytes local subscribers
/// receive per track are decodable. The gate is **per publisher + MID + RID**
/// (different participants and simulcast layers routinely share a MID). Use
/// [`new_passthrough`](Self::new_passthrough) to forward every packet
/// immediately (no gating).
pub struct CallBridge<S: CallUpstream, F: MediaForwarder> {
    source: S,
    router: SfuRouter,
    forwarder: F,
    /// When `true`, withhold each publisher track until its first keyframe.
    require_keyframe: bool,
    /// Publisher-scoped tracks whose keyframe gate has opened or which do not
    /// require a gate (unused in passthrough mode). MIDs are not call-global:
    /// different publishers commonly negotiate the same `"0"`/`"1"` values.
    started_tracks: HashSet<BridgeTrack>,
    /// Synthetic publisher peers this bridge registered, removed on detach.
    registered: HashSet<ParticipantId>,
    /// Participant-bound feedback sinks retained so a closed video gate can
    /// periodically retry PLI instead of relying on a single best-effort POST.
    feedback_sinks: HashMap<ParticipantId, Arc<dyn SfuPeerSink>>,
    /// Last successfully queued bridge PLI per publisher-scoped track.
    keyframe_requested_at: HashMap<BridgeTrack, tokio::time::Instant>,
    /// Video layers still waiting for a random-access point. Keeping this set
    /// separate lets the production loop retry PLI even if RTP goes silent.
    waiting_tracks: HashSet<BridgeTrack>,
    /// Incarnation-fenced key for publisher feedback sinks. A replacement pull
    /// to the same node must not be deleted by the old aborted task's late Drop.
    sink_owner: String,
}

impl<S: CallUpstream, F: MediaForwarder> CallBridge<S, F> {
    /// Create a bridge with **keyframe-gated** startup (per bridged mid).
    pub fn new(source: S, router: SfuRouter, forwarder: F) -> Self {
        let sink_owner = format!(
            "{:020}",
            NEXT_BRIDGE_SINK_OWNER.fetch_add(1, Ordering::Relaxed)
        );
        Self {
            source,
            router,
            forwarder,
            require_keyframe: true,
            started_tracks: HashSet::new(),
            registered: HashSet::new(),
            feedback_sinks: HashMap::new(),
            keyframe_requested_at: HashMap::new(),
            waiting_tracks: HashSet::new(),
            sink_owner,
        }
    }

    /// Create a bridge that forwards **every** packet immediately (no
    /// keyframe gating).
    pub fn new_passthrough(source: S, router: SfuRouter, forwarder: F) -> Self {
        let sink_owner = format!(
            "{:020}",
            NEXT_BRIDGE_SINK_OWNER.fetch_add(1, Ordering::Relaxed)
        );
        Self {
            source,
            router,
            forwarder,
            require_keyframe: false,
            started_tracks: HashSet::new(),
            registered: HashSet::new(),
            feedback_sinks: HashMap::new(),
            keyframe_requested_at: HashMap::new(),
            waiting_tracks: HashSet::new(),
            sink_owner,
        }
    }

    /// The call this bridge serves.
    #[must_use]
    pub fn call_id(&self) -> CallId {
        self.source.call_id()
    }

    /// The peer node this bridge pulls from.
    #[must_use]
    pub fn upstream_node(&self) -> &str {
        self.source.node_url()
    }

    /// True once fan-out has started for `mid` (its keyframe gate has opened,
    /// or gating is disabled).
    #[must_use]
    pub fn has_started(&self, mid: &str) -> bool {
        !self.require_keyframe
            || self
                .started_tracks
                .iter()
                .any(|track| track.mid.to_string() == mid)
    }

    /// Whether fan-out has started for one exact publisher-scoped track.
    #[must_use]
    pub fn has_started_for(&self, publisher: ParticipantId, mid: &str) -> bool {
        !self.require_keyframe
            || self
                .started_tracks
                .iter()
                .any(|track| track.participant == publisher && track.mid.to_string() == mid)
    }

    /// Whether one exact simulcast layer's startup gate has opened.
    #[must_use]
    pub fn has_started_for_rid(
        &self,
        publisher: ParticipantId,
        mid: Mid,
        rid: Option<Rid>,
    ) -> bool {
        !self.require_keyframe
            || self.started_tracks.contains(&BridgeTrack {
                participant: publisher,
                mid,
                rid,
            })
    }

    /// Remote participants currently registered as synthetic publishers.
    #[must_use]
    pub fn synthetic_peer_count(&self) -> usize {
        self.registered.len()
    }

    /// Pull **one** packet from upstream and, if the track's keyframe gate
    /// allows, register its publisher (first sighting) and fan it out.
    ///
    /// Returns:
    /// - `Some(n)` — a packet was pulled and forwarded; `n` is the delivery
    ///   count reported by the forwarder (`0` if nobody was written to, which
    ///   is not an error).
    /// - `Some(0)` is *also* returned when the packet was **dropped** by the
    ///   keyframe gate. Callers that care can disambiguate via
    ///   [`has_started`](Self::has_started).
    /// - `None` — upstream reached end-of-stream.
    pub async fn pump_once(&mut self) -> Option<usize> {
        let rtp = self.source.next_rtp().await?;
        Some(self.handle_rtp(rtp).await)
    }

    async fn handle_rtp(&mut self, rtp: BridgeRtp) -> usize {
        let call = self.source.call_id();
        let mid = rtp.mid.to_string();

        // Register control routing on first sight, even when this packet is a
        // pre-keyframe delta that the media gate will drop. Attaching the sink
        // can immediately send the initial PLI back to the remote owner and
        // avoids waiting for a periodic keyframe to break the startup cycle.
        if self.registered.insert(rtp.participant) {
            self.router
                .add_bridged_peer(call, rtp.participant, PeerRole::Publisher);
            if let Some(sink) = self.source.feedback_sink(rtp.participant) {
                self.forwarder.attach_bridge_peer_sink(
                    call,
                    rtp.participant,
                    &self.sink_owner,
                    sink.clone(),
                );
                self.feedback_sinks.insert(rtp.participant, sink);
            }
        }
        if !self.router.has_track(call, rtp.participant, &mid) {
            self.router.add_track(call, &mid, rtp.participant);
        }

        // Per-publisher-track keyframe gate: until this source's first
        // keyframe, drop its packets so fan-out begins at a decodable
        // random-access point. MIDs alone are not unique across publishers.
        let track = BridgeTrack::from_rtp(&rtp);
        if !rtp.requires_keyframe {
            self.started_tracks.insert(track);
            self.waiting_tracks.remove(&track);
            self.keyframe_requested_at.remove(&track);
        }
        if self.require_keyframe && rtp.requires_keyframe && !self.started_tracks.contains(&track) {
            if !rtp.keyframe {
                self.waiting_tracks.insert(track);
                self.request_keyframe_if_due(track, tokio::time::Instant::now());
                trace!(
                    %mid,
                    rid = ?rtp.rid.map(|rid| rid.to_string()),
                    "call-bridge: dropping pre-keyframe RTP"
                );
                return 0;
            }
            self.keyframe_requested_at.remove(&track);
            self.waiting_tracks.remove(&track);
            self.started_tracks.insert(track);
            debug!(
                publisher = %rtp.participant,
                %mid,
                rid = ?rtp.rid.map(|rid| rid.to_string()),
                "call-bridge: keyframe gate opened, fan-out started"
            );
        }

        self.forwarder.forward_bridge_rtp(call, rtp).await
    }

    fn request_keyframe_if_due(&mut self, track: BridgeTrack, now: tokio::time::Instant) {
        let retry_due = self.keyframe_requested_at.get(&track).map_or(true, |last| {
            now.duration_since(*last) >= KEYFRAME_RETRY_INTERVAL
        });
        if !retry_due {
            return;
        }
        let queued = self
            .feedback_sinks
            .get(&track.participant)
            .is_some_and(|sink| {
                sink.try_request_keyframe_for_rid(track.mid, track.rid, KeyframeRequestKind::Pli)
            });
        if queued {
            self.keyframe_requested_at.insert(track, now);
        }
        trace!(
            mid = %track.mid,
            rid = ?track.rid.map(|rid| rid.to_string()),
            queued,
            "call-bridge: requested keyframe for closed gate"
        );
    }

    fn retry_waiting_keyframes(&mut self) {
        let now = tokio::time::Instant::now();
        let waiting: Vec<_> = self.waiting_tracks.iter().copied().collect();
        for track in waiting {
            self.request_keyframe_if_due(track, now);
        }
    }

    /// Remove every synthetic publisher peer this bridge registered from the
    /// local router (their tracks go with them). Called by [`run`](Self::run)
    /// at end-of-stream; idempotent.
    pub fn detach(&mut self) {
        let call = self.source.call_id();
        for participant in self.registered.drain() {
            self.forwarder
                .detach_bridge_peer_sink(call, participant, &self.sink_owner);
            self.router.release_bridged_peer(call, participant);
        }
        self.started_tracks.clear();
        self.feedback_sinks.clear();
        self.keyframe_requested_at.clear();
        self.waiting_tracks.clear();
    }

    /// Drive the bridge to completion: repeatedly [`pump_once`](Self::pump_once)
    /// until upstream signals end-of-stream, then [`detach`](Self::detach) the
    /// synthetic peers.
    ///
    /// This is the production driver: the server spawns one per
    /// [`CallTopology::BridgeTo`] target node. It performs no I/O of its own;
    /// all socket work lives behind the [`CallUpstream`] abstraction.
    pub async fn run(mut self) {
        let call = self.source.call_id();
        debug!(%call, node = self.source.node_url(), "call-bridge: pull loop started");
        let mut retry = tokio::time::interval(KEYFRAME_RETRY_INTERVAL);
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        retry.tick().await;
        loop {
            enum Next {
                Packet(Option<BridgeRtp>),
                Retry,
            }
            let next = tokio::select! {
                packet = self.source.next_rtp() => Next::Packet(packet),
                _ = retry.tick() => Next::Retry,
            };
            match next {
                Next::Packet(Some(packet)) => {
                    self.handle_rtp(packet).await;
                }
                Next::Packet(None) => break,
                Next::Retry => self.retry_waiting_keyframes(),
            }
        }
        self.detach();
        debug!(%call, "call-bridge: upstream ended, synthetic peers detached");
    }
}

impl<S: CallUpstream, F: MediaForwarder> Drop for CallBridge<S, F> {
    fn drop(&mut self) {
        self.detach();
    }
}

// ───────────────────────────── egress bus ──────────────────────────────────

/// Default per-call egress fan-out buffer (packets a slow puller may lag by).
const DEFAULT_EGRESS_CAPACITY: usize = 64;

/// Egress side of the bridge: exposes **local** participants' RTP for remote
/// pullers.
///
/// The node's SFU loop publishes each local publisher's inbound RTP here (one
/// `CallEgress` per call); each peer node bridging this call holds one
/// [`CallEgressTap`]. A lagging tap skips dropped packets rather than stalling
/// the publisher — correct for realtime media.
///
/// `aero-server` consumes each tap in its production UDP egress relay and
/// frames packets for every cluster-authenticated puller address. The
/// [`LoopbackUpstream`] remains the in-process composition fixture.
#[derive(Clone)]
pub struct CallEgress {
    call_id: CallId,
    tx: broadcast::Sender<BridgeRtp>,
}

impl CallEgress {
    /// Create an egress for `call_id` with the default buffer capacity.
    #[must_use]
    pub fn new(call_id: CallId) -> Self {
        Self::with_capacity(call_id, DEFAULT_EGRESS_CAPACITY)
    }

    /// Create an egress with an explicit fan-out buffer capacity.
    #[must_use]
    pub fn with_capacity(call_id: CallId, capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { call_id, tx }
    }

    /// The call this egress exposes.
    #[must_use]
    pub fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Publish one local packet to every attached tap. Returns the number of
    /// taps that received it (`0` when no peer node is currently pulling —
    /// not an error).
    pub fn publish(&self, rtp: BridgeRtp) -> usize {
        self.tx.send(rtp).map_or(0, |n| n)
    }

    /// Open a new tap. The tap receives packets published *after* this call;
    /// earlier packets are not replayed. Dropping the tap detaches it.
    #[must_use]
    pub fn tap(&self) -> CallEgressTap {
        CallEgressTap {
            call_id: self.call_id,
            rx: self.tx.subscribe(),
        }
    }

    /// Number of taps currently attached.
    #[must_use]
    pub fn tap_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

/// One remote puller's view of a [`CallEgress`].
pub struct CallEgressTap {
    call_id: CallId,
    rx: broadcast::Receiver<BridgeRtp>,
}

impl CallEgressTap {
    /// The call this tap reads.
    #[must_use]
    pub fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Await the next packet, or `None` once the egress is gone (the call's
    /// last local publisher dropped). A lagged tap skips the overwritten
    /// packets and keeps reading.
    pub async fn next_rtp(&mut self) -> Option<BridgeRtp> {
        loop {
            match self.rx.recv().await {
                Ok(rtp) => return Some(rtp),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    trace!(skipped, "call-egress: tap lagged, skipping");
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

/// In-process [`CallUpstream`] backed by a local [`CallEgressTap`] — the
/// loopback analogue of the real node-to-node puller. Exercises the
/// egress → bridge composition end-to-end without a socket (and is usable for
/// same-process bridging in tests/tooling).
pub struct LoopbackUpstream {
    node_url: String,
    tap: CallEgressTap,
}

impl LoopbackUpstream {
    /// Wrap `tap` as an upstream that claims to pull from `node_url`.
    #[must_use]
    pub fn new(node_url: impl Into<String>, tap: CallEgressTap) -> Self {
        Self {
            node_url: node_url.into(),
            tap,
        }
    }
}

#[async_trait]
impl CallUpstream for LoopbackUpstream {
    fn call_id(&self) -> CallId {
        self.tap.call_id()
    }

    fn node_url(&self) -> &str {
        &self.node_url
    }

    async fn next_rtp(&mut self) -> Option<BridgeRtp> {
        self.tap.next_rtp().await
    }
}

// ───────────────────────────── topology policy ─────────────────────────────

/// What a node should do for a group call's media, given where the call's
/// participants are hosted.
///
/// Produced by [`decide_call_topology`]; a **pure** decision with no I/O so it
/// is exhaustively unit-testable (mirrors `aero-live-whip`'s `decide_cascade`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallTopology {
    /// Every participant is on this node (or the registry knows nothing) —
    /// the local SFU forwards as it always has. No inter-node traffic.
    ServeLocal,
    /// Other nodes host participants of this call — open one [`CallBridge`]
    /// per listed node URL (deterministically sorted, deduplicated).
    BridgeTo(Vec<String>),
}

/// Decide this node's bridge targets for a call — **pure**, no I/O.
///
/// `nodes` is the registry census (`CallRouteRegistry::nodes_for_call`):
/// `(node_url, participant_count)` per hosting node. The rule: bridge to
/// **every other** node hosting at least one participant; if none exist
/// (empty census, or only this node), serve locally. Targets are normalized
/// (trailing slash stripped), sorted, and deduplicated, so the output is
/// deterministic regardless of census order.
#[must_use]
pub fn decide_call_topology(local_node: &str, nodes: &[(String, u32)]) -> CallTopology {
    let mut targets: Vec<String> = nodes
        .iter()
        .filter(|(node, count)| *count > 0 && !node.is_empty() && !same_node(node, local_node))
        .map(|(node, _)| node.trim_end_matches('/').to_owned())
        .collect();
    targets.sort();
    targets.dedup();
    if targets.is_empty() {
        CallTopology::ServeLocal
    } else {
        CallTopology::BridgeTo(targets)
    }
}

/// Compare two node base URLs ignoring a trailing slash so `http://a` and
/// `http://a/` are the same node (same rule as `stream_route`'s).
fn same_node(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

// ───────────────────────────── test fake ───────────────────────────────────

/// A scripted, in-memory [`CallUpstream`] for tests.
///
/// Emits a pre-built sequence of [`BridgeRtp`]s in order, then returns `None`
/// (end-of-stream) — no socket, no peer node, fully deterministic.
///
/// Exposed (not `#[cfg(test)]`) so integration tests and downstream crates can
/// drive a [`CallBridge`] without a real transport, mirroring
/// `aero-live-whip`'s `FakeUpstream`.
pub struct FakeCallUpstream {
    call_id: CallId,
    node_url: String,
    /// Remaining packets, popped front-to-back.
    queue: VecDeque<BridgeRtp>,
}

impl FakeCallUpstream {
    /// Build a fake upstream for `call_id` (claiming to pull from `node_url`)
    /// that will emit `packets` in order.
    #[must_use]
    pub fn new(
        call_id: CallId,
        node_url: impl Into<String>,
        packets: impl IntoIterator<Item = BridgeRtp>,
    ) -> Self {
        Self {
            call_id,
            node_url: node_url.into(),
            queue: packets.into_iter().collect(),
        }
    }

    /// Number of packets not yet emitted.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.queue.len()
    }
}

#[async_trait]
impl CallUpstream for FakeCallUpstream {
    fn call_id(&self) -> CallId {
        self.call_id
    }

    fn node_url(&self) -> &str {
        &self.node_url
    }

    async fn next_rtp(&mut self) -> Option<BridgeRtp> {
        self.queue.pop_front()
    }
}

#[cfg(test)]
mod feedback_tests;

#[cfg(test)]
mod tests;
