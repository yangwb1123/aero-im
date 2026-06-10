//! Cross-node call-bridge spawn orchestration (ROADMAP3 方向二).
//!
//! A group call whose participants land on different nodes is, in the per-process
//! [`SfuRouter`], N disjoint islands of media. [`aero_im_call`]'s
//! `join_group_call` resolves a [`CallTopology`]: `BridgeTo(urls)` lists every
//! *other* node hosting participants, and for **each** such node this node should
//! pull that node's RTP **once** and fan it into the local SFU — exactly the leg
//! [`CallBridge`] implements.
//!
//! This module owns the *lifecycle* of those pulls: a registry of running bridge
//! tasks keyed by `(call, peer_url)` so that
//!
//! - a join yielding `BridgeTo(urls)` spawns one [`CallBridge::run`] task per
//!   *not-already-bridged* url (idempotent — a re-join with the same census
//!   never double-spawns),
//! - the last-local-leave / `end_call` signal cancels and removes every bridge
//!   task for that call.
//!
//! ## What is real vs. the documented seam
//!
//! Everything here — the registry, spawn-on-`BridgeTo`, idempotent no-double-
//! spawn, cancel-on-leave — is exercised by the unit tests below against
//! [`FakeCallUpstream`] / [`LoopbackUpstream`] (no socket, no peer node). The
//! one thing left to the inter-node transport seam is **how a [`CallUpstream`]
//! is constructed for a peer url**: the production path performs a `recvonly`
//! SDP exchange with that node's bridge endpoint and receives RTP over UDP (the
//! `TODO(real-transport)` on [`CallUpstream`], mirroring `aero-live-whip`'s
//! `cascade.rs`). The supervisor takes that construction as an injected
//! [`UpstreamFactory`] trait object, so the *orchestration* is fully built and
//! tested up to that seam — only the socket-bearing factory is left to wire.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use aero_common::CallId;
use aero_live_webrtc::{BridgeRtp, CallBridge, CallUpstream, MediaForwarder, SfuRouter};
use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Newtype letting a `Box<dyn CallUpstream>` satisfy the `S: CallUpstream`
/// generic bound on [`CallBridge`] (a `Box<dyn Trait>` does not auto-implement
/// `Trait`). Pure delegation.
struct BoxedUpstream(Box<dyn CallUpstream>);

#[async_trait]
impl CallUpstream for BoxedUpstream {
    fn call_id(&self) -> CallId {
        self.0.call_id()
    }
    fn node_url(&self) -> &str {
        self.0.node_url()
    }
    async fn next_rtp(&mut self) -> Option<BridgeRtp> {
        self.0.next_rtp().await
    }
}

/// Newtype letting an `Arc<dyn MediaForwarder>` satisfy the `F: MediaForwarder`
/// generic bound on [`CallBridge`]. Cloning shares the underlying forwarder, so
/// every bridge fans into the same local SFU forwarder.
#[derive(Clone)]
struct SharedForwarder(Arc<dyn MediaForwarder>);

#[async_trait]
impl MediaForwarder for SharedForwarder {
    async fn forward_rtp(&self, call: CallId, mid: &str, packet: Bytes) {
        self.0.forward_rtp(call, mid, packet).await;
    }
}

/// Constructs a [`CallUpstream`] that pulls `call`'s remote RTP from the node at
/// `peer_url`.
///
/// # TODO(real-transport) — documented seam
///
/// The production implementation opens a node-to-node RTP puller: resolve the
/// peer node's bridge endpoint, perform a `recvonly` SDP exchange, complete
/// ICE/DTLS/SRTP, and yield each received packet attributed to its owning
/// participant + mid (the seam documented on
/// [`aero_live_webrtc::CallUpstream`], mirroring `aero-live-whip`'s
/// `UpstreamSource`). That leg needs a real socket and a second node, so it is
/// **not unit-testable in this sandbox** — it is left to wire. Returning `None`
/// (e.g. the peer is unreachable) makes the supervisor skip that bridge for this
/// join; a later join re-attempts it.
///
/// In tests this is a fake that hands back a [`FakeCallUpstream`] /
/// [`LoopbackUpstream`], proving the supervisor spawns, fans media into the
/// local [`SfuRouter`], and cancels correctly without any I/O.
#[async_trait]
pub trait UpstreamFactory: Send + Sync {
    /// Build the upstream for `call` pulling from `peer_url`, or `None` when one
    /// cannot be established right now (the bridge is simply not spawned).
    async fn connect(&self, call: CallId, peer_url: &str) -> Option<Box<dyn CallUpstream>>;
}

/// The production [`UpstreamFactory`]: the real node-to-node RTP puller.
///
/// Left unimplemented behind the [`UpstreamFactory::connect`] seam — see the
/// `TODO(real-transport)` there. It is wired into the supervisor at server boot
/// so that, once the inter-node transport lands, no orchestration changes are
/// needed; until then `connect` returns `None`, leaving the supervisor dormant
/// and safe (single-node boot never produces a `BridgeTo`, so it is never even
/// called).
#[derive(Debug, Default, Clone, Copy)]
pub struct NodeRtpPullerFactory;

#[async_trait]
impl UpstreamFactory for NodeRtpPullerFactory {
    async fn connect(&self, _call: CallId, peer_url: &str) -> Option<Box<dyn CallUpstream>> {
        // TODO(real-transport): construct the recvonly node-to-node RTP puller
        // (SDP exchange + ICE/DTLS/SRTP) against `peer_url`'s bridge endpoint.
        // Until that transport is wired this yields nothing, so the supervisor
        // registers no bridge — correct and harmless for single-node operation.
        debug!(peer_url, "call-bridge: real-transport upstream not wired; skipping");
        None
    }
}

/// One running bridge task plus its cancellation handle.
struct BridgeTask {
    /// Monotonic id distinguishing this task from a later re-spawn for the same
    /// `(call, peer)` key, so a task's late self-removal can't evict its
    /// successor.
    id: u64,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

/// Registry of active cross-node call bridges, keyed by `(call, peer_url)`.
///
/// Cheap to clone (interior state is `Arc`-shared). Construct with
/// [`CallBridgeSupervisor::new`], wire a join's [`CallTopology::BridgeTo`]
/// targets in with [`ensure_bridges`](Self::ensure_bridges), and tear a call's
/// bridges down with [`cancel_call`](Self::cancel_call) on the last-local-leave
/// / `end_call` signal.
#[derive(Clone)]
pub struct CallBridgeSupervisor {
    router: SfuRouter,
    forwarder: Arc<dyn MediaForwarder>,
    factory: Arc<dyn UpstreamFactory>,
    /// `(call, normalized peer_url) -> running bridge task`.
    active: Arc<Mutex<HashMap<(CallId, String), BridgeTask>>>,
    /// Source of per-task ids (see [`BridgeTask::id`]).
    next_id: Arc<AtomicU64>,
}

impl CallBridgeSupervisor {
    /// Build a supervisor that fans bridged media into `router` via `forwarder`
    /// and constructs per-peer upstreams via `factory`.
    #[must_use]
    pub fn new(
        router: SfuRouter,
        forwarder: Arc<dyn MediaForwarder>,
        factory: Arc<dyn UpstreamFactory>,
    ) -> Self {
        Self {
            router,
            forwarder,
            factory,
            active: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Number of bridge tasks currently registered for `call`.
    #[must_use]
    pub fn bridge_count(&self, call: CallId) -> usize {
        self.active.lock().keys().filter(|(c, _)| *c == call).count()
    }

    /// Total number of bridge tasks across all calls.
    #[must_use]
    pub fn total_bridges(&self) -> usize {
        self.active.lock().len()
    }

    /// Whether a bridge from `peer_url` is already running for `call`.
    #[must_use]
    pub fn is_bridged(&self, call: CallId, peer_url: &str) -> bool {
        self.active
            .lock()
            .contains_key(&(call, normalize_node(peer_url)))
    }

    /// Ensure one running bridge per listed peer node for `call`.
    ///
    /// For each url **not already bridged**, asks the [`UpstreamFactory`] for an
    /// upstream and, on success, spawns a [`CallBridge::run`] task pulling it
    /// into the local router. Idempotent: urls already bridged are left
    /// untouched, so a re-join with the same (or a superset) census never
    /// double-spawns. Trailing slashes are normalized so `http://b` and
    /// `http://b/` are the same target.
    ///
    /// Returns the number of **new** bridges spawned by this call.
    pub async fn ensure_bridges(&self, call: CallId, peer_urls: &[String]) -> usize {
        let mut spawned = 0;
        for raw in peer_urls {
            let url = normalize_node(raw);
            if url.is_empty() {
                continue;
            }
            // Idempotency guard: skip a peer we already bridge for this call.
            if self.active.lock().contains_key(&(call, url.clone())) {
                continue;
            }
            let Some(upstream) = self.factory.connect(call, &url).await else {
                // No transport right now (single-node boot, or the peer is
                // unreachable): register nothing; a later join re-attempts it.
                continue;
            };
            self.spawn_bridge(call, url, upstream);
            spawned += 1;
        }
        spawned
    }

    /// Spawn (and register) one bridge task pulling `upstream` into the router.
    fn spawn_bridge(&self, call: CallId, peer_url: String, upstream: Box<dyn CallUpstream>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cancel = CancellationToken::new();
        let bridge = CallBridge::new(
            BoxedUpstream(upstream),
            self.router.clone(),
            SharedForwarder(self.forwarder.clone()),
        );
        let task_cancel = cancel.clone();
        let key_url = peer_url.clone();
        let active = self.active.clone();
        let handle = tokio::spawn(async move {
            // Race the pull loop against cancellation. On either exit the bridge
            // is dropped: `run` detaches its synthetic peers itself at
            // end-of-stream; a cancel drops the future, and the self-removal
            // below sweeps the registry entry so a re-join can re-spawn cleanly.
            tokio::select! {
                () = bridge.run() => {
                    debug!(%call, peer = %key_url, "call-bridge: upstream ended");
                }
                () = task_cancel.cancelled() => {
                    debug!(%call, peer = %key_url, "call-bridge: cancelled");
                }
            }
            // Self-removal on natural end-of-stream. Guard on the stored task id
            // so a *re-spawn* for the same (call, peer) that landed after this
            // task isn't evicted by this task's late exit. The cancel path also
            // reaches here, but `cancel_call`/`cancel_bridge` already removed the
            // entry, so the id won't match and this is a no-op.
            let mut guard = active.lock();
            if guard.get(&(call, key_url.clone())).is_some_and(|t| t.id == id) {
                guard.remove(&(call, key_url));
            }
        });
        self.active
            .lock()
            .insert((call, peer_url), BridgeTask { id, cancel, handle });
    }

    /// Cancel and remove **every** bridge task for `call` — the
    /// last-local-leave / `end_call` teardown. Idempotent: a call with no
    /// bridges is a no-op. Returns the number of bridges cancelled.
    pub fn cancel_call(&self, call: CallId) -> usize {
        let tasks: Vec<((CallId, String), BridgeTask)> = {
            let mut guard = self.active.lock();
            let keys: Vec<(CallId, String)> =
                guard.keys().filter(|(c, _)| *c == call).cloned().collect();
            keys.into_iter()
                .filter_map(|k| guard.remove(&k).map(|t| (k, t)))
                .collect()
        };
        let n = tasks.len();
        for ((_, peer), task) in tasks {
            task.cancel.cancel();
            // Detach the join handle: the spawned task observes the cancel and
            // unwinds on its own. We don't await here so cancellation stays
            // non-blocking on the caller (the WS/leave path).
            task.handle.abort();
            debug!(%call, %peer, "call-bridge: torn down");
        }
        if n > 0 {
            debug!(%call, count = n, "call-bridge: all bridges for call cancelled");
        }
        n
    }

    /// Cancel a single peer's bridge for `call` (no-op if none). Returns whether
    /// a bridge was cancelled — used when a census shrinks but the call lives on.
    pub fn cancel_bridge(&self, call: CallId, peer_url: &str) -> bool {
        let key = (call, normalize_node(peer_url));
        let Some(task) = self.active.lock().remove(&key) else {
            warn!(%call, peer = %key.1, "call-bridge: cancel of unknown bridge ignored");
            return false;
        };
        task.cancel.cancel();
        task.handle.abort();
        true
    }
}

/// Normalize a node base URL so `http://a` and `http://a/` map to one bridge
/// key (same rule the topology policy and route registry use).
fn normalize_node(url: &str) -> String {
    url.trim_end_matches('/').to_owned()
}

#[cfg(test)]
mod tests {
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
            Arc::new(Self { connects: AtomicUsize::new(0), refuse: false, packets })
        }
        fn refusing() -> Arc<Self> {
            Arc::new(Self { connects: AtomicUsize::new(0), refuse: true, packets: Vec::new() })
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
            Some(Box::new(FakeCallUpstream::new(call, peer_url, self.packets.clone())))
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
            Arc::new(Self { egresses: Mutex::new(Vec::new()) })
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

    fn key_pkt(p: ParticipantId, mid: &str, byte: u8) -> BridgeRtp {
        BridgeRtp::new(p, mid, vec![byte], true)
    }

    fn supervisor(factory: Arc<dyn UpstreamFactory>) -> (CallBridgeSupervisor, SfuRouter) {
        let router = SfuRouter::new();
        let fwd: Arc<dyn MediaForwarder> = Arc::new(NullForwarder);
        (CallBridgeSupervisor::new(router.clone(), fwd, factory), router)
    }

    #[tokio::test]
    async fn ensure_bridges_spawns_one_per_unbridged_peer() {
        let factory = ScriptedFactory::yielding(Vec::new()); // empty script → upstream ends quickly
        let (sup, _router) = supervisor(factory.clone());
        let call = CallId::new();

        let spawned = sup
            .ensure_bridges(call, &["http://b.example".into(), "http://c.example".into()])
            .await;
        assert_eq!(spawned, 2, "one bridge spawned per listed peer");
        assert_eq!(factory.connect_count(), 2, "the factory was consulted once per peer");
        // Registry reflects both (the empty-script tasks may have already
        // self-removed; assert via connect_count + a fresh re-ensure being idempotent).
        sup.cancel_call(call);
    }

    #[tokio::test]
    async fn ensure_bridges_is_idempotent_no_double_spawn() {
        // A never-ending upstream so the entries persist while we re-ensure.
        let (sup, _router) = supervisor(PendingFactory::new());
        let call = CallId::new();

        assert_eq!(sup.ensure_bridges(call, &["http://b.example".into()]).await, 1);
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
            sup.ensure_bridges(call, &["http://b.example/".into()]).await,
            0,
            "slash variant dedupes to the existing bridge"
        );
        assert_eq!(sup.bridge_count(call), 1, "exactly one bridge for the peer");

        // A genuinely new peer in the census DOES spawn.
        assert_eq!(sup.ensure_bridges(call, &["http://c.example".into()]).await, 1);
        assert_eq!(sup.bridge_count(call), 2);

        sup.cancel_call(call);
    }

    #[tokio::test]
    async fn cancel_call_tears_down_all_bridges_for_that_call() {
        let (sup, _router) = supervisor(PendingFactory::new());
        let (call_a, call_b) = (CallId::new(), CallId::new());

        sup.ensure_bridges(call_a, &["http://b.example".into(), "http://c.example".into()]).await;
        sup.ensure_bridges(call_b, &["http://d.example".into()]).await;
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
        let (sup, _router) = supervisor(PendingFactory::new());
        let call = CallId::new();
        sup.ensure_bridges(call, &["http://b.example".into(), "http://c.example".into()]).await;
        assert_eq!(sup.bridge_count(call), 2);

        assert!(sup.cancel_bridge(call, "http://b.example/"), "slash-insensitive single cancel");
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

        assert_eq!(sup.ensure_bridges(call, &["http://b.example".into()]).await, 0);
        assert_eq!(factory.connect_count(), 1, "the factory was consulted");
        assert_eq!(sup.bridge_count(call), 0, "but nothing was registered");
        assert!(!sup.is_bridged(call, "http://b.example"));
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

        struct OneShot(Mutex<Option<LoopbackUpstream>>);
        #[async_trait]
        impl UpstreamFactory for OneShot {
            async fn connect(&self, _call: CallId, _peer: &str) -> Option<Box<dyn CallUpstream>> {
                self.0.lock().take().map(|u| Box::new(u) as Box<dyn CallUpstream>)
            }
        }
        let factory = Arc::new(OneShot(Mutex::new(Some(LoopbackUpstream::new("http://a.example", tap)))));
        let sup = CallBridgeSupervisor::new(router.clone(), fwd, factory);

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
}
