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
//! ## What is real vs. a seam
//!
//! Everything here is unit-tested against [`FakeCallUpstream`], a scripted
//! in-memory source — there is **no real socket** in this module. The
//! production [`CallUpstream`] (a node-to-node RTP puller) and the wire side
//! of [`CallEgress`] (serving local participants' RTP to remote pullers) are
//! **documented seams**: see the `TODO(real-transport)` notes on each. The
//! bridge *logic* (pump → synthetic publisher → SFU fan-in, keyframe gating,
//! topology policy) is fully exercised without I/O; only the inter-node
//! transport is left to wire.

use std::collections::{HashSet, VecDeque};

use aero_common::{CallId, ParticipantId};
use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::broadcast;
use tracing::{debug, trace};

use crate::{MediaForwarder, PeerRole, SfuRouter};

/// One RTP packet pulled from (or exposed to) a peer node, attributed to the
/// participant that published it and the track (`mid`) it belongs to.
///
/// `Bytes` is reference-counted, so fanning a packet out is zero-copy. The
/// `keyframe` flag marks a decodable random-access point — the puller side
/// learns it from depacketization on the owning node; [`FakeCallUpstream`]
/// scripts it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeRtp {
    /// The remote participant whose media this packet carries.
    pub participant: ParticipantId,
    /// The published track id on the owning node. Used as-is on the pulling
    /// node — allocating non-colliding mids is part of the real transport's
    /// SDP exchange (see [`CallUpstream`]'s seam note).
    pub mid: String,
    /// The RTP packet payload to feed the local forwarder.
    pub payload: Bytes,
    /// `true` if this packet starts a decodable random-access point.
    pub keyframe: bool,
}

impl BridgeRtp {
    /// Build a bridged packet.
    #[must_use]
    pub fn new(
        participant: ParticipantId,
        mid: impl Into<String>,
        payload: impl Into<Bytes>,
        keyframe: bool,
    ) -> Self {
        Self {
            participant,
            mid: mid.into(),
            payload: payload.into(),
            keyframe,
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
/// # TODO(real-transport) — documented seam
///
/// The production implementation is a **node-to-node RTP puller** that:
/// 1. resolves the peer nodes hosting remote participants for `call_id` (the
///    cross-node `CallRouteRegistry` that `join_group_call` already populates,
///    turned into targets by [`decide_call_topology`]),
/// 2. performs a `recvonly` SDP exchange with that node's bridge endpoint
///    (the wire side of [`CallEgress`]),
/// 3. completes ICE/DTLS/SRTP and receives RTP over UDP,
/// 4. attributes each packet to its owning participant + mid (+ keyframe flag
///    from depacketization), yielding it via [`next_rtp`](CallUpstream::next_rtp).
///
/// That leg needs a real socket + a peer node and so is **not unit-testable in
/// this sandbox** — it is left as a seam, mirroring `aero-live-whip`'s
/// `UpstreamSource` (`TODO(real-transport)` there). The bridge *logic*
/// downstream of a received packet ([`CallBridge`]) is fully tested via
/// [`FakeCallUpstream`].
#[async_trait]
pub trait CallUpstream: Send {
    /// The call being bridged.
    fn call_id(&self) -> CallId;

    /// Public base URL of the peer node this upstream pulls from.
    fn node_url(&self) -> &str;

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
/// receive per track are decodable. The gate is **per mid** (tracks from
/// different remote participants start independently). Use
/// [`new_passthrough`](Self::new_passthrough) to forward every packet
/// immediately (no gating).
pub struct CallBridge<S: CallUpstream, F: MediaForwarder> {
    source: S,
    router: SfuRouter,
    forwarder: F,
    /// When `true`, withhold each mid's fan-out until its first keyframe.
    require_keyframe: bool,
    /// Mids whose keyframe gate has opened (unused in passthrough mode).
    started_mids: HashSet<String>,
    /// Synthetic publisher peers this bridge registered, removed on detach.
    registered: HashSet<ParticipantId>,
}

impl<S: CallUpstream, F: MediaForwarder> CallBridge<S, F> {
    /// Create a bridge with **keyframe-gated** startup (per bridged mid).
    pub fn new(source: S, router: SfuRouter, forwarder: F) -> Self {
        Self {
            source,
            router,
            forwarder,
            require_keyframe: true,
            started_mids: HashSet::new(),
            registered: HashSet::new(),
        }
    }

    /// Create a bridge that forwards **every** packet immediately (no
    /// keyframe gating).
    pub fn new_passthrough(source: S, router: SfuRouter, forwarder: F) -> Self {
        Self {
            source,
            router,
            forwarder,
            require_keyframe: false,
            started_mids: HashSet::new(),
            registered: HashSet::new(),
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
        !self.require_keyframe || self.started_mids.contains(mid)
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
    /// - `Some(n)` — a packet was pulled and forwarded; `n` is the number of
    ///   local subscribers currently subscribed to its mid (`0` if none yet,
    ///   which is not an error).
    /// - `Some(0)` is *also* returned when the packet was **dropped** by the
    ///   keyframe gate. Callers that care can disambiguate via
    ///   [`has_started`](Self::has_started).
    /// - `None` — upstream reached end-of-stream.
    pub async fn pump_once(&mut self) -> Option<usize> {
        let rtp = self.source.next_rtp().await?;
        let call = self.source.call_id();

        // Per-track keyframe gate: until a mid's first keyframe, drop its
        // packets so fan-out begins at a decodable random-access point.
        if self.require_keyframe && !self.started_mids.contains(&rtp.mid) {
            if !rtp.keyframe {
                trace!(mid = %rtp.mid, "call-bridge: dropping pre-keyframe RTP");
                return Some(0);
            }
            self.started_mids.insert(rtp.mid.clone());
            debug!(mid = %rtp.mid, "call-bridge: keyframe gate opened, fan-out started");
        }

        // First sighting of a remote participant/track: register it with the
        // local router as a synthetic publisher so subscriptions resolve.
        if self.registered.insert(rtp.participant) {
            self.router.add_peer(call, rtp.participant, PeerRole::Publisher);
        }
        if self.router.owner_of(call, &rtp.mid).is_none() {
            self.router.add_track(call, &rtp.mid, rtp.participant);
        }

        self.forwarder.forward_rtp(call, &rtp.mid, rtp.payload).await;
        Some(self.router.subscribers_for(call, &rtp.mid).len())
    }

    /// Remove every synthetic publisher peer this bridge registered from the
    /// local router (their tracks go with them). Called by [`run`](Self::run)
    /// at end-of-stream; idempotent.
    pub fn detach(&mut self) {
        let call = self.source.call_id();
        for participant in self.registered.drain() {
            self.router.remove_peer(call, participant);
        }
        self.started_mids.clear();
    }

    /// Drive the bridge to completion: repeatedly [`pump_once`](Self::pump_once)
    /// until upstream signals end-of-stream, then [`detach`](Self::detach) the
    /// synthetic peers.
    ///
    /// This is the production driver: the server spawns one per
    /// [`CallTopology::BridgeTo`] target node. It performs no I/O of its own —
    /// all I/O lives behind the [`CallUpstream`] seam.
    pub async fn run(mut self) {
        let call = self.source.call_id();
        debug!(%call, node = self.source.node_url(), "call-bridge: pull loop started");
        while self.pump_once().await.is_some() {}
        self.detach();
        debug!(%call, "call-bridge: upstream ended, synthetic peers detached");
    }
}

// ───────────────────────────── egress seam ─────────────────────────────────

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
/// # TODO(real-transport) — documented seam
///
/// Serving a [`CallEgressTap`] to a remote node — a node-authenticated,
/// `sendonly` SDP exchange + SRTP egress that the remote [`CallUpstream`]
/// pulls from — is the wire half of the same seam documented on
/// [`CallUpstream`]; it needs a real socket + peer node and is left to wire.
/// [`LoopbackUpstream`] is the in-process stand-in that proves the two halves
/// compose.
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
        Self { node_url: node_url.into(), tap }
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
mod tests {
    use super::*;
    use std::sync::Arc;

    // ── fixtures ─────────────────────────────────────────────────────────────

    fn key_pkt(p: ParticipantId, mid: &str, byte: u8) -> BridgeRtp {
        BridgeRtp::new(p, mid, vec![byte], true)
    }

    fn delta_pkt(p: ParticipantId, mid: &str, byte: u8) -> BridgeRtp {
        BridgeRtp::new(p, mid, vec![byte], false)
    }

    /// A [`MediaForwarder`] that records every forwarded packet.
    #[derive(Default, Clone)]
    struct RecordingForwarder {
        log: Arc<parking_lot::Mutex<Vec<(CallId, String, Bytes)>>>,
    }

    impl RecordingForwarder {
        fn forwarded(&self) -> Vec<(CallId, String, Bytes)> {
            self.log.lock().clone()
        }
    }

    #[async_trait]
    impl MediaForwarder for RecordingForwarder {
        async fn forward_rtp(&self, call: CallId, mid: &str, packet: Bytes) {
            self.log.lock().push((call, mid.to_owned(), packet));
        }
    }

    // ── decide_call_topology (exhaustive) ────────────────────────────────────

    #[test]
    fn empty_census_serves_local() {
        assert_eq!(decide_call_topology("http://a.example", &[]), CallTopology::ServeLocal);
    }

    #[test]
    fn self_only_census_serves_local() {
        let nodes = vec![("http://a.example".to_owned(), 3)];
        assert_eq!(decide_call_topology("http://a.example", &nodes), CallTopology::ServeLocal);
        // Trailing-slash variants are still this node.
        let slashed = vec![("http://a.example/".to_owned(), 1)];
        assert_eq!(decide_call_topology("http://a.example", &slashed), CallTopology::ServeLocal);
    }

    #[test]
    fn bridges_to_every_other_hosting_node_sorted() {
        // Census order shuffled on purpose — output must be deterministic.
        let nodes = vec![
            ("http://c.example".to_owned(), 1),
            ("http://a.example".to_owned(), 2),
            ("http://b.example".to_owned(), 4),
        ];
        assert_eq!(
            decide_call_topology("http://a.example", &nodes),
            CallTopology::BridgeTo(vec![
                "http://b.example".to_owned(),
                "http://c.example".to_owned(),
            ]),
        );
    }

    #[test]
    fn bridges_even_when_local_node_not_in_census_yet() {
        // A joiner whose registration hasn't landed still sees remote hosts.
        let nodes = vec![("http://b.example".to_owned(), 1)];
        assert_eq!(
            decide_call_topology("http://a.example", &nodes),
            CallTopology::BridgeTo(vec!["http://b.example".to_owned()]),
        );
    }

    #[test]
    fn zero_count_and_empty_nodes_are_ignored() {
        let nodes = vec![
            ("http://b.example".to_owned(), 0),
            (String::new(), 2),
            ("http://a.example".to_owned(), 1),
        ];
        assert_eq!(
            decide_call_topology("http://a.example", &nodes),
            CallTopology::ServeLocal,
            "a node with no participants (or a malformed entry) is not a bridge target"
        );
    }

    #[test]
    fn duplicate_slash_variants_dedupe_to_one_target() {
        let nodes = vec![
            ("http://b.example".to_owned(), 1),
            ("http://b.example/".to_owned(), 1),
        ];
        assert_eq!(
            decide_call_topology("http://a.example", &nodes),
            CallTopology::BridgeTo(vec!["http://b.example".to_owned()]),
        );
    }

    // ── FakeCallUpstream ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn fake_upstream_emits_scripted_sequence_then_none() {
        let call = CallId::new();
        let p = ParticipantId::new();
        let pkts = vec![key_pkt(p, "v0", 1), delta_pkt(p, "v0", 2)];
        let mut up = FakeCallUpstream::new(call, "http://b.example", pkts.clone());

        assert_eq!(up.call_id(), call);
        assert_eq!(up.node_url(), "http://b.example");
        assert_eq!(up.remaining(), 2);
        for expected in &pkts {
            assert_eq!(up.next_rtp().await.as_ref(), Some(expected));
        }
        assert!(up.next_rtp().await.is_none(), "end-of-stream after the script");
        assert_eq!(up.remaining(), 0);
    }

    // ── CallBridge fan-in ────────────────────────────────────────────────────

    #[tokio::test]
    async fn bridge_registers_synthetic_publisher_and_fans_in_to_local_subscriber() {
        let call = CallId::new();
        let remote = ParticipantId::new();
        let local_sub = ParticipantId::new();
        let router = SfuRouter::new();
        let fwd = RecordingForwarder::default();

        // A local subscriber already wants the bridged track.
        router.add_peer(call, local_sub, PeerRole::Subscriber);
        router.add_subscription(call, "v0", local_sub);

        let up = FakeCallUpstream::new(
            call,
            "http://b.example",
            vec![key_pkt(remote, "v0", 0xA0), delta_pkt(remote, "v0", 0xA1)],
        );
        let mut bridge = CallBridge::new(up, router.clone(), fwd.clone());
        assert_eq!(bridge.call_id(), call);
        assert_eq!(bridge.upstream_node(), "http://b.example");

        assert_eq!(bridge.pump_once().await, Some(1), "keyframe → 1 local subscriber");
        assert_eq!(bridge.pump_once().await, Some(1), "delta → 1 local subscriber");
        assert_eq!(bridge.pump_once().await, None, "end-of-stream");

        // The remote participant became a synthetic publisher owning the mid.
        assert_eq!(router.owner_of(call, "v0"), Some(remote));
        assert!(router.participants(call).contains(&remote));
        assert_eq!(bridge.synthetic_peer_count(), 1);

        // Both packets went through the same forwarder path local RTP takes.
        let log = fwd.forwarded();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0], (call, "v0".to_owned(), Bytes::from(vec![0xA0])));
        assert_eq!(log[1], (call, "v0".to_owned(), Bytes::from(vec![0xA1])));
    }

    #[tokio::test]
    async fn keyframe_gate_drops_pre_keyframe_packets_per_mid() {
        let call = CallId::new();
        let (pa, pb) = (ParticipantId::new(), ParticipantId::new());
        let router = SfuRouter::new();
        let fwd = RecordingForwarder::default();

        let up = FakeCallUpstream::new(
            call,
            "http://b.example",
            vec![
                delta_pkt(pa, "v0", 0x01), // dropped (v0 gate closed)
                key_pkt(pa, "v0", 0x02),   // v0 gate opens
                delta_pkt(pb, "v1", 0x03), // dropped (v1 gate independent, still closed)
                delta_pkt(pa, "v0", 0x04), // forwarded (v0 open)
                key_pkt(pb, "v1", 0x05),   // v1 gate opens
            ],
        );
        let mut bridge = CallBridge::new(up, router.clone(), fwd.clone());

        assert_eq!(bridge.pump_once().await, Some(0), "pre-keyframe v0 dropped");
        assert!(!bridge.has_started("v0"));
        assert_eq!(fwd.forwarded().len(), 0, "dropped packet never reaches the forwarder");

        assert_eq!(bridge.pump_once().await, Some(0), "keyframe forwarded (no subscribers yet)");
        assert!(bridge.has_started("v0"));

        assert_eq!(bridge.pump_once().await, Some(0), "v1 still gated despite v0 being open");
        assert!(!bridge.has_started("v1"));

        bridge.pump_once().await;
        bridge.pump_once().await;
        assert!(bridge.has_started("v1"));

        let mids: Vec<String> = fwd.forwarded().into_iter().map(|(_, mid, _)| mid).collect();
        assert_eq!(mids, vec!["v0", "v0", "v1"], "only post-gate packets forwarded");
        // The gated v1 delta never registered pb's track prematurely under pa.
        assert_eq!(router.owner_of(call, "v1"), Some(pb));
    }

    #[tokio::test]
    async fn passthrough_mode_forwards_pre_keyframe_packets() {
        let call = CallId::new();
        let p = ParticipantId::new();
        let fwd = RecordingForwarder::default();
        let up = FakeCallUpstream::new(
            call,
            "http://b.example",
            vec![delta_pkt(p, "v0", 0x01), key_pkt(p, "v0", 0x02)],
        );
        let mut bridge = CallBridge::new_passthrough(up, SfuRouter::new(), fwd.clone());
        assert!(bridge.has_started("v0"), "passthrough starts immediately");

        assert_eq!(bridge.pump_once().await, Some(0), "delta forwarded (no gate)");
        assert_eq!(bridge.pump_once().await, Some(0), "keyframe forwarded");
        assert_eq!(bridge.pump_once().await, None);
        assert_eq!(fwd.forwarded().len(), 2, "both packets forwarded in passthrough mode");
    }

    #[tokio::test]
    async fn run_drives_to_completion_and_detaches_synthetic_peers() {
        let call = CallId::new();
        let remote = ParticipantId::new();
        let local_sub = ParticipantId::new();
        let router = SfuRouter::new();
        let fwd = RecordingForwarder::default();

        router.add_peer(call, local_sub, PeerRole::Subscriber);
        router.add_subscription(call, "v0", local_sub);

        let up = FakeCallUpstream::new(
            call,
            "http://b.example",
            vec![key_pkt(remote, "v0", 0x01), delta_pkt(remote, "v0", 0x02)],
        );
        CallBridge::new(up, router.clone(), fwd.clone()).run().await;

        assert_eq!(fwd.forwarded().len(), 2, "run() pumped the entire script");
        // Synthetic publisher (and its track) detached; the local peer remains.
        assert!(!router.participants(call).contains(&remote), "synthetic peer removed");
        assert!(router.participants(call).contains(&local_sub), "local peer untouched");
        assert_eq!(router.owner_of(call, "v0"), None, "synthetic track removed");
    }

    #[tokio::test]
    async fn fan_in_reaches_multiple_local_subscribers() {
        let call = CallId::new();
        let remote = ParticipantId::new();
        let router = SfuRouter::new();
        let fwd = RecordingForwarder::default();
        for _ in 0..3 {
            let sub = ParticipantId::new();
            router.add_peer(call, sub, PeerRole::Subscriber);
            router.add_subscription(call, "v0", sub);
        }

        let up = FakeCallUpstream::new(call, "http://b.example", vec![key_pkt(remote, "v0", 0x01)]);
        let mut bridge = CallBridge::new(up, router, fwd);
        assert_eq!(bridge.pump_once().await, Some(3), "keyframe routed to all 3 subscribers");
    }

    // ── egress seam ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn egress_publishes_to_taps_and_closes_on_drop() {
        let call = CallId::new();
        let p = ParticipantId::new();
        let egress = CallEgress::new(call);
        assert_eq!(egress.call_id(), call);
        assert_eq!(egress.publish(key_pkt(p, "v0", 0x01)), 0, "no taps yet → 0, not an error");

        let mut tap = egress.tap();
        assert_eq!(egress.tap_count(), 1);
        assert_eq!(egress.publish(key_pkt(p, "v0", 0x02)), 1);
        assert_eq!(egress.publish(delta_pkt(p, "v0", 0x03)), 1);
        drop(egress);

        // Buffered packets drain, then the closed egress yields None.
        assert_eq!(tap.next_rtp().await, Some(key_pkt(p, "v0", 0x02)));
        assert_eq!(tap.next_rtp().await, Some(delta_pkt(p, "v0", 0x03)));
        assert!(tap.next_rtp().await.is_none(), "egress gone → end-of-stream");
    }

    #[tokio::test]
    async fn loopback_upstream_composes_egress_into_bridge_fan_in() {
        // Node A's egress → (in-process "wire") → node B's bridge → B's router.
        let call = CallId::new();
        let remote = ParticipantId::new(); // publisher on "node A"
        let local_sub = ParticipantId::new(); // subscriber on "node B"

        let egress_a = CallEgress::new(call);
        let tap = egress_a.tap();
        egress_a.publish(key_pkt(remote, "v0", 0x01));
        egress_a.publish(delta_pkt(remote, "v0", 0x02));
        drop(egress_a); // A's publisher leaves → B's upstream ends.

        let router_b = SfuRouter::new();
        let fwd = RecordingForwarder::default();
        router_b.add_peer(call, local_sub, PeerRole::Subscriber);
        router_b.add_subscription(call, "v0", local_sub);

        let up = LoopbackUpstream::new("http://a.example", tap);
        assert_eq!(up.call_id(), call);
        CallBridge::new(up, router_b.clone(), fwd.clone()).run().await;

        let log = fwd.forwarded();
        assert_eq!(log.len(), 2, "both packets crossed the loopback into B's fan-out");
        assert_eq!(log[0].2, Bytes::from(vec![0x01]));
        assert_eq!(log[1].2, Bytes::from(vec![0x02]));
        assert!(
            !router_b.participants(call).contains(&remote),
            "synthetic peer detached after upstream ended"
        );
    }
}
