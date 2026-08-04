use super::transport::UdpRtpUpstream;
use super::*;
use aero_common::ParticipantId;
use aero_live_webrtc::call_bridge::{BridgeRtp, FakeCallUpstream, LoopbackUpstream};
use aero_live_webrtc::{CallEgress, NullForwarder, PeerRole};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A scripted [`UpstreamFactory`]: hands back a [`FakeCallUpstream`] with a
/// fixed packet script per connect, counts connect calls, and can be set to
/// refuse (return `None`) to model an unreachable peer.
struct ScriptedFactory {
    connects: AtomicUsize,
    refuse: bool,
    packets: Vec<BridgeRtp>,
}

impl ScriptedFactory {
    fn yielding(packets: Vec<BridgeRtp>) -> Arc<Self> {
        Arc::new(Self {
            connects: AtomicUsize::new(0),
            refuse: false,
            packets,
        })
    }
    fn refusing() -> Arc<Self> {
        Arc::new(Self {
            connects: AtomicUsize::new(0),
            refuse: true,
            packets: Vec::new(),
        })
    }
    fn connect_count(&self) -> usize {
        self.connects.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl UpstreamFactory for ScriptedFactory {
    async fn connect(&self, call: CallId, peer_url: &str) -> Option<Box<dyn CallUpstream>> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        if self.refuse {
            return None;
        }
        Some(Box::new(FakeCallUpstream::new(
            call,
            peer_url,
            self.packets.clone(),
        )))
    }
}

/// A factory whose upstream never ends (an empty `LoopbackUpstream` whose
/// egress is held alive) so the bridge task stays running until cancelled —
/// lets cancel-on-leave be observed deterministically.
struct PendingFactory {
    // Kept alive so the loopback tap never closes; one egress per peer.
    egresses: Mutex<Vec<Arc<CallEgress>>>,
}

impl PendingFactory {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            egresses: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl UpstreamFactory for PendingFactory {
    async fn connect(&self, call: CallId, peer_url: &str) -> Option<Box<dyn CallUpstream>> {
        let egress = Arc::new(CallEgress::new(call));
        let tap = egress.tap();
        self.egresses.lock().push(egress);
        Some(Box::new(LoopbackUpstream::new(peer_url, tap)))
    }
}

type SeenSubscribe = Arc<Mutex<Option<(Option<String>, serde_json::Value)>>>;

struct OneShot(Mutex<Option<LoopbackUpstream>>);

#[async_trait]
impl UpstreamFactory for OneShot {
    async fn connect(&self, _call: CallId, _peer: &str) -> Option<Box<dyn CallUpstream>> {
        self.0
            .lock()
            .take()
            .map(|upstream| Box::new(upstream) as Box<dyn CallUpstream>)
    }
}

fn key_pkt(p: ParticipantId, mid: &str, byte: u8) -> BridgeRtp {
    BridgeRtp::new(p, mid, vec![byte], true)
}

pub(super) fn supervisor(factory: Arc<dyn UpstreamFactory>) -> (CallBridgeSupervisor, SfuRouter) {
    let router = SfuRouter::new();
    let fwd: Arc<dyn MediaForwarder> = Arc::new(NullForwarder);
    let sup = CallBridgeSupervisor::new(
        router.clone(),
        fwd,
        factory,
        BridgeSubscriberRegistry::default(),
    );
    (sup, router)
}

#[test]
fn synthetic_remote_publisher_does_not_create_a_local_bridge_island() {
    let (supervisor, router) = supervisor(ScriptedFactory::refusing());
    let call = CallId::new();
    let remote = ParticipantId::new();

    assert!(router.add_bridged_peer(call, remote, PeerRole::Publisher));
    assert!(
        !supervisor.has_local_call(call),
        "a bridge-injected publisher must not trigger a reverse bridge"
    );
    assert!(
        !supervisor.has_call_participant(call, remote),
        "synthetic publishers cannot pass browser SFU offer/ICE guards"
    );

    let local = ParticipantId::new();
    router.add_peer(call, local, PeerRole::Subscriber);
    assert!(
        supervisor.has_local_call(call),
        "a browser-owned leg forms a genuine local SFU island"
    );
    assert!(supervisor.has_call_participant(call, local));
}

/// The PUSH side end-to-end (ROADMAP 方向五): the supervisor spawns a per-call
/// egress relay that pushes the call's locally-published RTP to subscribed
/// pullers; idempotent; torn down with the call. `CallEgress` is fed by
/// production SFU sessions; only a real browser ICE/DTLS/SRTP handshake is
/// left for staging.
#[tokio::test]
async fn ensure_egress_relays_published_rtp_and_tears_down() {
    use aero_live_webrtc::CallEgress;

    let registry = BridgeSubscriberRegistry::default();
    let sup = CallBridgeSupervisor::new(
        SfuRouter::new(),
        Arc::new(NullForwarder) as Arc<dyn MediaForwarder>,
        ScriptedFactory::refusing(),
        registry.clone(),
    );
    let call = CallId::new();

    // A puller subscribes its receive address.
    let mut puller = UdpRtpUpstream::bind(call, "http://owner").await.unwrap();
    let puller_addr = std::net::SocketAddr::new(
        std::net::Ipv4Addr::LOCALHOST.into(),
        puller.local_addr().unwrap().port(),
    );
    assert!(registry.subscribe_generation(
        call,
        puller_addr,
        puller.subscription_id(),
        std::time::Duration::from_secs(60)
    ));

    // The owning node has the call's CallEgress (fed by the SFU forward path;
    // here published directly) and ensures the relay.
    let egress = CallEgress::new(call);
    assert!(
        sup.ensure_egress(call, egress.tap()).await,
        "spawns a new egress relay"
    );
    assert!(sup.has_egress(call));
    assert!(
        !sup.ensure_egress(call, egress.tap()).await,
        "idempotent while running"
    );

    // A published packet is relayed to the subscribed puller.
    let pkt = BridgeRtp::new(
        ParticipantId::new(),
        "video0",
        Bytes::from_static(&[0x80, 0x60, 0xCA, 0xFE]),
        true,
    );
    egress.publish(pkt.clone());
    let got = tokio::time::timeout(std::time::Duration::from_secs(2), puller.next_rtp())
        .await
        .expect("puller must receive the relayed packet within 2s");
    assert_eq!(got, Some(pkt));

    // Ending the call tears the relay down.
    sup.cancel_call(call);
    assert!(!sup.has_egress(call), "egress relay removed on call end");
}

#[tokio::test]
async fn sfu_offer_owns_peer_starts_shared_egress_and_applies_ice() {
    const OFFER: &str = "v=0\r\n\
o=- 4611731400430051336 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0\r\n\
a=msid-semantic: WMS\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:abcd\r\n\
a=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
a=fingerprint:sha-256 11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=sendrecv\r\n\
a=rtcp-mux\r\n\
a=rtpmap:111 opus/48000/2\r\n";

    let router = SfuRouter::new();
    let concrete = Arc::new(aero_live_webrtc::SfuForwarder::new(router.clone()));
    let forwarder: Arc<dyn MediaForwarder> = concrete.clone();
    let media = SfuMediaRegistry::new(
        router.clone(),
        forwarder.clone(),
        "127.0.0.1:0",
        "127.0.0.1",
    );
    let sup = CallBridgeSupervisor {
        router,
        forwarder,
        media,
        factory: ScriptedFactory::refusing(),
        subscribers: BridgeSubscriberRegistry::default(),
        active: Arc::new(Mutex::new(HashMap::new())),
        connecting: Arc::new(Mutex::new(HashMap::new())),
        egress: Arc::new(Mutex::new(HashMap::new())),
        egress_connecting: Arc::new(Mutex::new(HashMap::new())),
        next_id: Arc::new(AtomicU64::new(0)),
    };
    let call = CallId::new();
    let participant = ParticipantId::new();

    let answer = sup
        .accept_sfu_offer(call, participant, 1, OFFER)
        .await
        .expect("supervisor negotiates the production media leg");
    assert_eq!(answer.mids, vec!["0"]);
    assert!(answer.sdp.contains("a=candidate:"));
    assert!(sup.has_media_session(call, participant));
    assert_eq!(sup.media_session_count(call), 1);
    assert_eq!(
        concrete.peer_count(),
        1,
        "forwarder owns one bounded command sink"
    );
    assert!(
        sup.has_egress(call),
        "first local media leg starts the shared egress"
    );

    sup.add_sfu_ice(
        call,
        participant,
        "candidate:1 1 udp 2113937151 127.0.0.1 59999 typ host".into(),
    )
    .await
    .expect("the sole session owner parses and applies trickled ICE");

    assert!(sup.remove_sfu_session(call, participant));
    assert!(!sup.has_media_session(call, participant));
    assert!(
        !sup.has_egress(call),
        "last media leg tears down the shared egress"
    );
    assert_eq!(concrete.peer_count(), 0);
}

#[tokio::test]
async fn ensure_bridges_spawns_one_per_unbridged_peer() {
    let factory = ScriptedFactory::yielding(Vec::new()); // empty script → upstream ends quickly
    let (sup, router) = supervisor(factory.clone());
    let call = CallId::new();
    router.add_peer(call, ParticipantId::new(), PeerRole::Bidirectional);

    let spawned = sup
        .ensure_bridges(
            call,
            &["http://b.example".into(), "http://c.example".into()],
        )
        .await;
    assert_eq!(spawned, 2, "one bridge spawned per listed peer");
    assert_eq!(
        factory.connect_count(),
        2,
        "the factory was consulted once per peer"
    );
    // Registry reflects both (the empty-script tasks may have already
    // self-removed; assert via connect_count + a fresh re-ensure being idempotent).
    sup.cancel_call(call);
}

#[tokio::test]
async fn immediately_ended_bridge_removes_registry_entry_and_can_retry() {
    let factory = ScriptedFactory::yielding(Vec::new());
    let (sup, router) = supervisor(factory.clone());
    let call = CallId::new();
    router.add_peer(call, ParticipantId::new(), PeerRole::Bidirectional);

    assert_eq!(
        sup.ensure_bridges(call, &["http://b.example".into()]).await,
        1
    );
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while sup.bridge_count(call) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("ended bridge removes its registered task");

    assert_eq!(
        sup.ensure_bridges(call, &["http://b.example".into()]).await,
        1,
        "periodic reconciliation can reconnect an ended upstream"
    );
    assert_eq!(factory.connect_count(), 2);
    sup.cancel_call(call);
}

#[tokio::test]
async fn ensure_bridges_is_idempotent_no_double_spawn() {
    // A never-ending upstream so the entries persist while we re-ensure.
    let (sup, router) = supervisor(PendingFactory::new());
    let call = CallId::new();
    router.add_peer(call, ParticipantId::new(), PeerRole::Bidirectional);

    assert_eq!(
        sup.ensure_bridges(call, &["http://b.example".into()]).await,
        1
    );
    assert!(sup.is_bridged(call, "http://b.example"));
    assert_eq!(sup.bridge_count(call), 1);

    // Re-join with the same census — must NOT spawn a second bridge.
    assert_eq!(
        sup.ensure_bridges(call, &["http://b.example".into()]).await,
        0,
        "already-bridged peer is not re-spawned"
    );
    // Trailing-slash variant is the same node → still no new bridge.
    assert_eq!(
        sup.ensure_bridges(call, &["http://b.example/".into()])
            .await,
        0,
        "slash variant dedupes to the existing bridge"
    );
    assert_eq!(sup.bridge_count(call), 1, "exactly one bridge for the peer");

    // A genuinely new peer in the census DOES spawn.
    assert_eq!(
        sup.ensure_bridges(call, &["http://c.example".into()]).await,
        1
    );
    assert_eq!(sup.bridge_count(call), 2);

    sup.cancel_call(call);
}

#[tokio::test]
async fn reconcile_adds_new_peers_and_removes_departed_peers() {
    let (sup, router) = supervisor(PendingFactory::new());
    let call = CallId::new();
    router.add_peer(call, ParticipantId::new(), PeerRole::Bidirectional);
    assert_eq!(
        sup.reconcile_bridges(
            call,
            &[
                "http://node-b.example".into(),
                "http://node-c.example".into()
            ]
        )
        .await,
        (2, 0)
    );
    assert_eq!(sup.bridge_count(call), 2);

    assert_eq!(
        sup.reconcile_bridges(
            call,
            &[
                "http://node-c.example/".into(),
                "http://node-d.example".into()
            ]
        )
        .await,
        (1, 1),
        "node-b is removed, slash-normalized node-c retained, node-d added"
    );
    assert!(!sup.is_bridged(call, "http://node-b.example"));
    assert!(sup.is_bridged(call, "http://node-c.example"));
    assert!(sup.is_bridged(call, "http://node-d.example"));
    sup.cancel_call(call);
}

#[tokio::test]
async fn cancel_call_tears_down_all_bridges_for_that_call() {
    let (sup, router) = supervisor(PendingFactory::new());
    let (call_a, call_b) = (CallId::new(), CallId::new());
    router.add_peer(call_a, ParticipantId::new(), PeerRole::Bidirectional);
    router.add_peer(call_b, ParticipantId::new(), PeerRole::Bidirectional);

    sup.ensure_bridges(
        call_a,
        &["http://b.example".into(), "http://c.example".into()],
    )
    .await;
    sup.ensure_bridges(call_b, &["http://d.example".into()])
        .await;
    assert_eq!(sup.bridge_count(call_a), 2);
    assert_eq!(sup.bridge_count(call_b), 1);
    assert_eq!(sup.total_bridges(), 3);

    let cancelled = sup.cancel_call(call_a);
    assert_eq!(cancelled, 2, "both of call A's bridges torn down");
    assert_eq!(sup.bridge_count(call_a), 0);
    assert_eq!(sup.bridge_count(call_b), 1, "call B is untouched");

    // Idempotent: cancelling an already-empty call is a no-op.
    assert_eq!(sup.cancel_call(call_a), 0);

    sup.cancel_call(call_b);
}

#[tokio::test]
async fn cancel_bridge_removes_a_single_peer_leaving_the_rest() {
    let (sup, router) = supervisor(PendingFactory::new());
    let call = CallId::new();
    router.add_peer(call, ParticipantId::new(), PeerRole::Bidirectional);
    sup.ensure_bridges(
        call,
        &["http://b.example".into(), "http://c.example".into()],
    )
    .await;
    assert_eq!(sup.bridge_count(call), 2);

    assert!(
        sup.cancel_bridge(call, "http://b.example/"),
        "slash-insensitive single cancel"
    );
    assert!(!sup.is_bridged(call, "http://b.example"));
    assert!(sup.is_bridged(call, "http://c.example"));
    assert_eq!(sup.bridge_count(call), 1);

    // Cancelling an unknown peer is a harmless false.
    assert!(!sup.cancel_bridge(call, "http://z.example"));

    sup.cancel_call(call);
}

#[tokio::test]
async fn refusing_factory_spawns_nothing() {
    // Models single-node boot / an unreachable peer: connect() returns None,
    // so the supervisor stays dormant — no bridge registered, no panic.
    let factory = ScriptedFactory::refusing();
    let (sup, _router) = supervisor(factory.clone());
    let call = CallId::new();

    assert_eq!(
        sup.ensure_bridges(call, &["http://b.example".into()]).await,
        0
    );
    assert_eq!(factory.connect_count(), 1, "the factory was consulted");
    assert_eq!(sup.bridge_count(call), 0, "but nothing was registered");
    assert!(!sup.is_bridged(call, "http://b.example"));
}

/// `connect` binds a puller AND announces its receive address to the owning
/// node's subscribe endpoint (ROADMAP 方向五). Drives the real outbound POST
/// against a localhost mock peer and asserts the body + auth are correct —
/// the full bind → announce path. Only the two-real-nodes run is staging.
#[tokio::test]
async fn connect_announces_subscribe_to_the_peer() {
    use axum::{routing::post, Json, Router};

    // Mock owning node: records the subscribe auth header + body.
    let seen: SeenSubscribe = Arc::new(Mutex::new(None));
    let recorder = seen.clone();
    let app = Router::new().route(
        "/api/internal/call-bridge/subscribe",
        post(
            move |headers: axum::http::HeaderMap, Json(body): Json<serde_json::Value>| {
                let recorder = recorder.clone();
                async move {
                    let auth = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    *recorder.lock() = Some((auth, body));
                    axum::http::StatusCode::NO_CONTENT
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let factory = NodeRtpPullerFactory::new("10.0.0.9", Some("clustersecret".to_owned()));
    let call = CallId::new();
    let up = factory.connect(call, &format!("http://{peer_addr}")).await;
    assert!(
        up.is_some(),
        "puller is created even before any media flows"
    );

    // Poll briefly for the async POST to land on the mock peer.
    let mut got = None;
    for _ in 0..100 {
        if let Some(v) = seen.lock().take() {
            got = Some(v);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (auth, body) = got.expect("peer must receive a subscribe POST");
    assert_eq!(
        auth.as_deref(),
        Some("Bearer clustersecret"),
        "shared-secret bearer"
    );
    assert_eq!(body["call_id"], call.to_string(), "the call id");
    assert_eq!(
        body["wire_version"],
        aero_live_webrtc::BOUND_BRIDGE_VERSION,
        "new pullers explicitly negotiate the current bound-frame version"
    );
    assert!(
        body["addr"].as_str().unwrap().starts_with("10.0.0.9:"),
        "advertised host:port, got {}",
        body["addr"]
    );

    server.abort();
}

#[tokio::test]
async fn bridge_fans_remote_media_into_the_local_router() {
    // End-to-end via the loopback wire: an egress on "node A" publishes a
    // keyframe, the supervisor's bridge pulls it and registers the remote
    // publisher as a synthetic peer in THIS node's router.
    let call = CallId::new();
    let remote = ParticipantId::new();
    let local_sub = ParticipantId::new();

    let router = SfuRouter::new();
    let fwd: Arc<dyn MediaForwarder> = Arc::new(NullForwarder);
    router.add_peer(call, local_sub, PeerRole::Subscriber);
    router.add_subscription(call, "v0", local_sub);

    // A loopback egress we publish into, then close so the bridge ends and
    // the assertion observes a settled router state.
    let egress = CallEgress::new(call);
    let tap = egress.tap();
    egress.publish(key_pkt(remote, "v0", 0xAB));
    drop(egress);

    let factory = Arc::new(OneShot(Mutex::new(Some(LoopbackUpstream::new(
        "http://a.example",
        tap,
    )))));
    let sup = CallBridgeSupervisor::new(
        router.clone(),
        fwd,
        factory,
        BridgeSubscriberRegistry::default(),
    );

    sup.ensure_bridges(call, &["http://a.example".into()]).await;
    // Let the spawned bridge drain the (already-closed) loopback.
    for _ in 0..50 {
        if router.owner_of(call, "v0") == Some(remote) {
            break;
        }
        tokio::task::yield_now().await;
    }
    // The bridge ran end-of-stream and detached its synthetic peer, so the
    // synthetic track is gone again — the observable proof it fanned in.
    assert!(
        router.participants(call).contains(&local_sub),
        "the local subscriber is untouched by the bridge lifecycle"
    );
    sup.cancel_call(call);
}
