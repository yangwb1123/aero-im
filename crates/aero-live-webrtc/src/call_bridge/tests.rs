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
    router: SfuRouter,
    log: Arc<parking_lot::Mutex<Vec<(CallId, BridgeRtp)>>>,
}

impl RecordingForwarder {
    fn new(router: SfuRouter) -> Self {
        Self {
            router,
            log: Arc::default(),
        }
    }

    fn forwarded(&self) -> Vec<(CallId, BridgeRtp)> {
        self.log.lock().clone()
    }
}

#[async_trait]
impl MediaForwarder for RecordingForwarder {
    async fn forward_rtp(&self, call: CallId, mid: &str, packet: Bytes) {
        let decoded =
            crate::decode_bridge_frame(&packet).expect("compat path carries a bridge frame");
        assert_eq!(decoded.mid.to_string(), mid);
        self.log.lock().push((call, decoded));
    }

    async fn forward_bridge_rtp(&self, call: CallId, packet: BridgeRtp) -> usize {
        let mid = packet.mid.to_string();
        let delivered = self.router.subscribers_for(call, &mid).len();
        self.log.lock().push((call, packet));
        delivered
    }
}

// ── decide_call_topology (exhaustive) ────────────────────────────────────

#[test]
fn empty_census_serves_local() {
    assert_eq!(
        decide_call_topology("http://a.example", &[]),
        CallTopology::ServeLocal
    );
}

#[test]
fn self_only_census_serves_local() {
    let nodes = vec![("http://a.example".to_owned(), 3)];
    assert_eq!(
        decide_call_topology("http://a.example", &nodes),
        CallTopology::ServeLocal
    );
    // Trailing-slash variants are still this node.
    let slashed = vec![("http://a.example/".to_owned(), 1)];
    assert_eq!(
        decide_call_topology("http://a.example", &slashed),
        CallTopology::ServeLocal
    );
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

#[test]
fn inbound_conversion_preserves_forwarding_metadata() {
    let participant = ParticipantId::new();
    let ext_vals = ExtensionValues {
        mid: Some(Mid::from("video0")),
        rid: Some(Rid::from("high")),
        ..ExtensionValues::default()
    };
    let inbound = InboundRtp {
        mid: Mid::from("video0"),
        rid: Some(Rid::from("high")),
        pt: Pt::from(127),
        seq_no: SeqNo::from(0x1_0002),
        rtp_time: 0xAABB_CCDD,
        ssrc: Ssrc::from(0x1122_3344),
        marker: true,
        ext_vals,
        wallclock: std::time::Instant::now(),
        payload: vec![0x65, 0xAA, 0xBB],
        is_keyframe: true,
        requires_keyframe: true,
    };

    let bridged = BridgeRtp::from_inbound(participant, &inbound);
    let restored = bridged.to_inbound();
    assert_eq!(bridged.participant, participant);
    assert_eq!(restored.mid, inbound.mid);
    assert_eq!(restored.rid, inbound.rid);
    assert_eq!(restored.pt, inbound.pt);
    assert_eq!(restored.seq_no, inbound.seq_no);
    assert_eq!(restored.rtp_time, inbound.rtp_time);
    assert_eq!(restored.ssrc, inbound.ssrc);
    assert_eq!(restored.marker, inbound.marker);
    assert_eq!(restored.payload, inbound.payload);
    assert_eq!(restored.is_keyframe, inbound.is_keyframe);
    assert_eq!(restored.requires_keyframe, inbound.requires_keyframe);
    assert_eq!(restored.ext_vals.mid, Some(inbound.mid));
    assert_eq!(restored.ext_vals.rid, inbound.rid);
}

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
    assert!(
        up.next_rtp().await.is_none(),
        "end-of-stream after the script"
    );
    assert_eq!(up.remaining(), 0);
}

// ── CallBridge fan-in ────────────────────────────────────────────────────

#[tokio::test]
async fn bridge_registers_synthetic_publisher_and_fans_in_to_local_subscriber() {
    let call = CallId::new();
    let remote = ParticipantId::new();
    let local_sub = ParticipantId::new();
    let router = SfuRouter::new();
    let fwd = RecordingForwarder::new(router.clone());

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

    assert_eq!(
        bridge.pump_once().await,
        Some(1),
        "keyframe → 1 local subscriber"
    );
    assert_eq!(
        bridge.pump_once().await,
        Some(1),
        "delta → 1 local subscriber"
    );
    assert_eq!(bridge.pump_once().await, None, "end-of-stream");

    // The remote participant became a synthetic publisher owning the mid.
    assert_eq!(router.owner_of(call, "v0"), Some(remote));
    assert!(router.participants(call).contains(&remote));
    assert_eq!(bridge.synthetic_peer_count(), 1);
    assert_eq!(
        router.roster_snapshot(),
        vec![(call, local_sub)],
        "the call-route heartbeat must advertise only node-B's local leg"
    );
    assert!(
        !router.local_participants(call).contains(&remote),
        "node-A's bridged publisher cannot become node-B route ownership"
    );

    // Both packets went through the same forwarder path local RTP takes.
    let log = fwd.forwarded();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0], (call, key_pkt(remote, "v0", 0xA0)));
    assert_eq!(log[1], (call, delta_pkt(remote, "v0", 0xA1)));
}

#[tokio::test]
async fn bridge_enters_real_header_aware_sfu_forwarder() {
    use crate::SfuPeer;
    use str0m::media::{MediaKind, Rid};

    let call = CallId::new();
    let remote = ParticipantId::new();
    let local_sub = ParticipantId::new();
    let router = SfuRouter::new();
    router.add_peer(call, local_sub, PeerRole::Subscriber);
    router.add_subscription(call, "v0", local_sub);

    // Give the real forwarder a concrete subscriber peer with an outbound
    // stream. No browser handshake is needed to prove write_rtp is reached:
    // `declare_outbound` is the same direct-API setup negotiation performs.
    let fwd = crate::SfuForwarder::new(router.clone());
    let mut peer = SfuPeer::new(call, local_sub);
    peer.declare_outbound(Mid::from("v0"), MediaKind::Video, 0xCAFE_BABE, None);
    fwd.add_peer(peer);
    fwd.subscribe("v0", local_sub, "v0");

    let mut packet = key_pkt(remote, "v0", 0xA5);
    packet.rid = Some(Rid::from("high"));
    packet.pt = Pt::from(96);
    packet.seq_no = SeqNo::from(65_537);
    packet.rtp_time = 0x1020_3040;
    packet.ssrc = Ssrc::from(0x1122_3344);
    packet.marker = true;

    let up = FakeCallUpstream::new(call, "http://remote.example", vec![packet]);
    let mut bridge = CallBridge::new(up, router, fwd);
    assert_eq!(
        bridge.pump_once().await,
        Some(1),
        "the bridge packet must reach SfuForwarder::on_rtp and write one subscriber stream"
    );
}

#[tokio::test]
async fn keyframe_gate_drops_pre_keyframe_packets_per_mid() {
    let call = CallId::new();
    let (pa, pb) = (ParticipantId::new(), ParticipantId::new());
    let router = SfuRouter::new();
    let fwd = RecordingForwarder::new(router.clone());

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
    assert_eq!(
        fwd.forwarded().len(),
        0,
        "dropped packet never reaches the forwarder"
    );

    assert_eq!(
        bridge.pump_once().await,
        Some(0),
        "keyframe forwarded (no subscribers yet)"
    );
    assert!(bridge.has_started("v0"));

    assert_eq!(
        bridge.pump_once().await,
        Some(0),
        "v1 still gated despite v0 being open"
    );
    assert!(!bridge.has_started("v1"));

    bridge.pump_once().await;
    bridge.pump_once().await;
    assert!(bridge.has_started("v1"));

    let mids: Vec<String> = fwd
        .forwarded()
        .into_iter()
        .map(|(_, packet)| packet.mid.to_string())
        .collect();
    assert_eq!(
        mids,
        vec!["v0", "v0", "v1"],
        "only post-gate packets forwarded"
    );
    // The gated v1 delta never registered pb's track prematurely under pa.
    assert_eq!(router.owner_of(call, "v1"), Some(pb));
}

#[tokio::test]
async fn audio_without_a_keyframe_concept_bypasses_startup_gate() {
    let call = CallId::new();
    let publisher = ParticipantId::new();
    let router = SfuRouter::new();
    let forwarder = RecordingForwarder::new(router.clone());
    let audio = delta_pkt(publisher, "audio0", 0xA0).with_keyframe_requirement(false);
    let upstream = FakeCallUpstream::new(call, "http://b.example", vec![audio.clone()]);
    let mut bridge = CallBridge::new(upstream, router, forwarder.clone());

    assert_eq!(bridge.pump_once().await, Some(0));
    assert!(bridge.has_started_for(publisher, "audio0"));
    assert_eq!(
        forwarder.forwarded(),
        vec![(call, audio)],
        "Opus/unknown-codec media must not wait for an impossible keyframe"
    );
}

#[tokio::test]
async fn keyframe_gate_is_scoped_by_publisher_when_mids_collide() {
    let call = CallId::new();
    let (publisher_a, publisher_b) = (ParticipantId::new(), ParticipantId::new());
    let router = SfuRouter::new();
    let forwarder = RecordingForwarder::new(router.clone());
    let upstream = FakeCallUpstream::new(
        call,
        "http://b.example",
        vec![
            key_pkt(publisher_a, "0", 0xA0),
            delta_pkt(publisher_b, "0", 0xB0),
            key_pkt(publisher_b, "0", 0xB1),
        ],
    );
    let mut bridge = CallBridge::new(upstream, router, forwarder.clone());

    bridge.pump_once().await;
    assert!(bridge.has_started_for(publisher_a, "0"));
    assert!(!bridge.has_started_for(publisher_b, "0"));

    bridge.pump_once().await;
    assert_eq!(
        forwarder.forwarded().len(),
        1,
        "publisher A's keyframe cannot open publisher B's same-MID gate"
    );
    assert!(!bridge.has_started_for(publisher_b, "0"));

    bridge.pump_once().await;
    assert!(bridge.has_started_for(publisher_b, "0"));
    let packets = forwarder.forwarded();
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0].1.participant, publisher_a);
    assert_eq!(packets[1].1.participant, publisher_b);
    assert_eq!(packets[1].1.payload, Bytes::from(vec![0xB1]));
}

#[tokio::test]
async fn passthrough_mode_forwards_pre_keyframe_packets() {
    let call = CallId::new();
    let p = ParticipantId::new();
    let router = SfuRouter::new();
    let fwd = RecordingForwarder::new(router.clone());
    let up = FakeCallUpstream::new(
        call,
        "http://b.example",
        vec![delta_pkt(p, "v0", 0x01), key_pkt(p, "v0", 0x02)],
    );
    let mut bridge = CallBridge::new_passthrough(up, router, fwd.clone());
    assert!(bridge.has_started("v0"), "passthrough starts immediately");

    assert_eq!(
        bridge.pump_once().await,
        Some(0),
        "delta forwarded (no gate)"
    );
    assert_eq!(bridge.pump_once().await, Some(0), "keyframe forwarded");
    assert_eq!(bridge.pump_once().await, None);
    assert_eq!(
        fwd.forwarded().len(),
        2,
        "both packets forwarded in passthrough mode"
    );
}

#[tokio::test]
async fn run_drives_to_completion_and_detaches_synthetic_peers() {
    let call = CallId::new();
    let remote = ParticipantId::new();
    let local_sub = ParticipantId::new();
    let router = SfuRouter::new();
    let fwd = RecordingForwarder::new(router.clone());

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
    assert!(
        !router.participants(call).contains(&remote),
        "synthetic peer removed"
    );
    assert!(
        router.participants(call).contains(&local_sub),
        "local peer untouched"
    );
    assert_eq!(router.owner_of(call, "v0"), None, "synthetic track removed");
}

#[tokio::test]
async fn fan_in_reaches_multiple_local_subscribers() {
    let call = CallId::new();
    let remote = ParticipantId::new();
    let router = SfuRouter::new();
    let fwd = RecordingForwarder::new(router.clone());
    for _ in 0..3 {
        let sub = ParticipantId::new();
        router.add_peer(call, sub, PeerRole::Subscriber);
        router.add_subscription(call, "v0", sub);
    }

    let up = FakeCallUpstream::new(call, "http://b.example", vec![key_pkt(remote, "v0", 0x01)]);
    let mut bridge = CallBridge::new(up, router, fwd);
    assert_eq!(
        bridge.pump_once().await,
        Some(3),
        "keyframe routed to all 3 subscribers"
    );
}

// ── egress bus ───────────────────────────────────────────────────────────

#[tokio::test]
async fn egress_publishes_to_taps_and_closes_on_drop() {
    let call = CallId::new();
    let p = ParticipantId::new();
    let egress = CallEgress::new(call);
    assert_eq!(egress.call_id(), call);
    assert_eq!(
        egress.publish(key_pkt(p, "v0", 0x01)),
        0,
        "no taps yet → 0, not an error"
    );

    let mut tap = egress.tap();
    assert_eq!(egress.tap_count(), 1);
    assert_eq!(egress.publish(key_pkt(p, "v0", 0x02)), 1);
    assert_eq!(egress.publish(delta_pkt(p, "v0", 0x03)), 1);
    drop(egress);

    // Buffered packets drain, then the closed egress yields None.
    assert_eq!(tap.next_rtp().await, Some(key_pkt(p, "v0", 0x02)));
    assert_eq!(tap.next_rtp().await, Some(delta_pkt(p, "v0", 0x03)));
    assert!(
        tap.next_rtp().await.is_none(),
        "egress gone → end-of-stream"
    );
}

#[tokio::test]
async fn cloned_egress_is_one_shared_call_bus_for_multiple_publishers() {
    let call = CallId::new();
    let publisher_a = ParticipantId::new();
    let publisher_b = ParticipantId::new();
    let egress_a = CallEgress::new(call);
    let egress_b = egress_a.clone();
    let mut tap = egress_a.tap();

    let a = key_pkt(publisher_a, "a0", 0xA0);
    let b = key_pkt(publisher_b, "b0", 0xB0);
    assert_eq!(egress_a.publish(a.clone()), 1);
    assert_eq!(egress_b.publish(b.clone()), 1);
    drop(egress_a);

    assert_eq!(tap.next_rtp().await, Some(a));
    assert_eq!(tap.next_rtp().await, Some(b));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), tap.next_rtp())
            .await
            .is_err(),
        "dropping one publisher clone must not close the shared call egress"
    );

    drop(egress_b);
    assert!(
        tap.next_rtp().await.is_none(),
        "last publisher clone closes the call egress"
    );
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
    let fwd = RecordingForwarder::new(router_b.clone());
    router_b.add_peer(call, local_sub, PeerRole::Subscriber);
    router_b.add_subscription(call, "v0", local_sub);

    let up = LoopbackUpstream::new("http://a.example", tap);
    assert_eq!(up.call_id(), call);
    CallBridge::new(up, router_b.clone(), fwd.clone())
        .run()
        .await;

    let log = fwd.forwarded();
    assert_eq!(
        log.len(),
        2,
        "both packets crossed the loopback into B's fan-out"
    );
    assert_eq!(log[0].1.payload, Bytes::from(vec![0x01]));
    assert_eq!(log[1].1.payload, Bytes::from(vec![0x02]));
    assert!(
        !router_b.participants(call).contains(&remote),
        "synthetic peer detached after upstream ended"
    );
}
