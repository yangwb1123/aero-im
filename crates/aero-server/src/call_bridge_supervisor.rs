//! Cross-node call-bridge spawn orchestration (ROADMAP3 方向二).
//!
//! A group call whose participants land on different nodes is, in the per-process
//! [`SfuRouter`], N disjoint islands of media. [`aero_im_call`]'s
//! `join_group_call` resolves a [`CallTopology`]: `BridgeTo(urls)` lists every
//! *other* node hosting participants, and for **each** such node this node should
//! pull that node's RTP **once** and fan it into the local SFU — exactly the leg
//! [`aero_live_webrtc::CallBridge`] implements.
//!
//! This module owns the *lifecycle* of those pulls: a registry of running bridge
//! tasks keyed by `(call, peer_url)` so that
//!
//! - a join yielding `BridgeTo(urls)` spawns one
//!   [`aero_live_webrtc::CallBridge::run`] task per
//!   *not-already-bridged* url (idempotent — a re-join with the same census
//!   never double-spawns),
//! - the last-local-leave / `end_call` signal cancels and removes every bridge
//!   task for that call.
//!
//! ## What is real vs. staging
//!
//! The production [`NodeRtpPullerFactory`] binds the plain-UDP bridge socket,
//! announces it through the secret-gated subscribe endpoint, decodes framed
//! RTP, and feeds it into the local forwarder. The matching egress loop frames
//! local RTP and sends it to current subscribers. Both directions and lifecycle
//! idempotency are exercised over localhost; a true multi-node deployment with
//! routable advertised addresses remains a staging check.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use aero_common::{CallId, ParticipantId};
use aero_live_webrtc::{
    encode_bound_bridge_frame_version, encode_bridge_frame, BridgeRtp, CallEgressTap, CallUpstream,
    MediaForwarder, SfuPeerSink, SfuRouter,
};
use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::sfu_media::{SfuMediaError, SfuMediaRegistry, SfuSessionAnswer};

#[cfg(test)]
mod egress_tests;
mod reconcile;
#[cfg(test)]
mod reconcile_tests;
mod sfu;
mod subscriber_control;
mod subscribers;
mod transport;
#[cfg(test)]
mod transport_frame_tests;
pub use subscribers::BridgeSubscriberRegistry;
pub use transport::NodeRtpPullerFactory;

/// Newtype letting a `Box<dyn CallUpstream>` satisfy the `S: CallUpstream`
/// generic bound on [`aero_live_webrtc::CallBridge`] (a `Box<dyn Trait>` does
/// not auto-implement
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
    fn feedback_sink(&self, participant: ParticipantId) -> Option<Arc<dyn SfuPeerSink>> {
        self.0.feedback_sink(participant)
    }
    async fn next_rtp(&mut self) -> Option<BridgeRtp> {
        self.0.next_rtp().await
    }
}

/// Newtype letting an `Arc<dyn MediaForwarder>` satisfy the `F: MediaForwarder`
/// generic bound on [`aero_live_webrtc::CallBridge`]. Cloning shares the
/// underlying forwarder, so every bridge fans into the same local SFU forwarder.
#[derive(Clone)]
struct SharedForwarder(Arc<dyn MediaForwarder>);

#[async_trait]
impl MediaForwarder for SharedForwarder {
    async fn forward_rtp(&self, call: CallId, mid: &str, packet: Bytes) {
        self.0.forward_rtp(call, mid, packet).await;
    }

    async fn forward_bridge_rtp(&self, call: CallId, packet: BridgeRtp) -> usize {
        self.0.forward_bridge_rtp(call, packet).await
    }

    fn attach_peer_sink(
        &self,
        call: CallId,
        participant: ParticipantId,
        sink: Arc<dyn SfuPeerSink>,
    ) -> bool {
        self.0.attach_peer_sink(call, participant, sink)
    }

    fn attach_bridge_peer_sink(
        &self,
        call: CallId,
        participant: ParticipantId,
        bridge: &str,
        sink: Arc<dyn SfuPeerSink>,
    ) -> bool {
        self.0
            .attach_bridge_peer_sink(call, participant, bridge, sink)
    }

    fn detach_bridge_peer_sink(
        &self,
        call: CallId,
        participant: ParticipantId,
        bridge: &str,
    ) -> bool {
        self.0.detach_bridge_peer_sink(call, participant, bridge)
    }

    fn reset_peer_sink(&self, call: CallId, participant: ParticipantId) -> bool {
        self.0.reset_peer_sink(call, participant)
    }
}

/// Constructs a [`CallUpstream`] that pulls `call`'s remote RTP from the node at
/// `peer_url`.
///
/// Production uses [`NodeRtpPullerFactory`]; tests inject loopback/scripted
/// factories. Returning `None` (for example when a peer is unreachable) makes
/// the supervisor skip that bridge, and a later join re-attempts it.
#[async_trait]
pub trait UpstreamFactory: Send + Sync {
    /// Build the upstream for `call` pulling from `peer_url`, or `None` when one
    /// cannot be established right now (the bridge is simply not spawned).
    async fn connect(&self, call: CallId, peer_url: &str) -> Option<Box<dyn CallUpstream>>;
}

/// The send side of the node-to-node bridge (ROADMAP 方向五): the node that OWNS
/// a call's local publishers relays their RTP to subscribed pulling nodes.
///
/// Taps a [`CallEgressTap`] (every locally-published packet for the call), binds
/// each upgraded puller's frame to its call + generation, and pushes it over UDP
/// currently-subscribed puller (resolved from a [`BridgeSubscriberRegistry`] per
/// packet, so subscriptions can change mid-call) — the mirror of
/// [`transport::UdpRtpUpstream`]. The tap → encode → send loop is real and exercised
/// end-to-end over localhost.
struct UdpRtpEgress {
    call: CallId,
    socket: tokio::net::UdpSocket,
    registry: BridgeSubscriberRegistry,
}

impl UdpRtpEgress {
    /// Bind an ephemeral send socket for `call`, resolving targets from `registry`.
    async fn bind(call: CallId, registry: BridgeSubscriberRegistry) -> std::io::Result<Self> {
        let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
        Ok(Self {
            call,
            socket,
            registry,
        })
    }

    /// Pump every packet from `tap` to the call's current subscribers until the
    /// egress closes (its last local publisher left) or `cancel` fires. A send
    /// error to one subscriber never stops the others or tears the loop down.
    async fn run(self, mut tap: CallEgressTap, cancel: CancellationToken) {
        let mut announced_targets = HashSet::new();
        loop {
            let pkt = tokio::select! {
                () = cancel.cancelled() => break,
                p = tap.next_rtp() => match p {
                    Some(p) => p,
                    None => break, // egress gone → done
                },
            };
            for target in self.registry.targets(self.call) {
                if announced_targets.insert((target.addr, target.generation, target.wire_version)) {
                    debug!(
                        call = %self.call,
                        addr = %target.addr,
                        generation_bound = target.generation.is_some(),
                        wire_version = ?target.wire_version,
                        "call-bridge: egress started sending to subscriber"
                    );
                }
                let frame = match (target.generation, target.wire_version) {
                    (Some(generation), Some(wire_version)) => {
                        let Some(frame) = encode_bound_bridge_frame_version(
                            self.call,
                            generation,
                            &pkt,
                            wire_version,
                        ) else {
                            debug!(
                                wire_version,
                                addr = %target.addr,
                                "call-bridge: unsupported negotiated wire version"
                            );
                            continue;
                        };
                        frame
                    }
                    (None, None) => encode_bridge_frame(&pkt),
                    _ => {
                        debug!(
                            addr = %target.addr,
                            "call-bridge: inconsistent subscriber wire metadata"
                        );
                        continue;
                    }
                };
                if let Err(e) = self.socket.send_to(&frame, target.addr).await {
                    debug!(?e, addr = %target.addr, "call-bridge: egress send failed");
                }
            }
        }
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
    /// Server-owned browser media sessions. Each peer is polled by exactly one
    /// task; the forwarder reaches it through a bounded command sink.
    media: SfuMediaRegistry,
    factory: Arc<dyn UpstreamFactory>,
    /// Puller subscriptions, shared with the subscribe control endpoint; each
    /// call's egress relay reads its send targets from here (ROADMAP 方向五).
    subscribers: BridgeSubscriberRegistry,
    /// `(call, normalized peer_url) -> running bridge task` (the PULL side).
    active: Arc<Mutex<HashMap<(CallId, String), BridgeTask>>>,
    /// `(call, normalized peer_url) -> reservation id` while the transport
    /// handshake is in flight. The id fences cancellation and late future-drop
    /// cleanup against a replacement reservation for the same key.
    connecting: Arc<Mutex<HashMap<(CallId, String), u64>>>,
    /// `call -> running egress relay task` (the PUSH side; one per call).
    egress: Arc<Mutex<HashMap<CallId, EgressTask>>>,
    /// `call -> source epoch + reservation id` while a UDP sender bind is in
    /// flight. Both values fence a late old bind against a reconnected call.
    egress_connecting: Arc<Mutex<HashMap<CallId, EgressReservation>>>,
    /// Source of per-task ids (see [`BridgeTask::id`]).
    next_id: Arc<AtomicU64>,
}

/// One running egress relay task plus its cancellation handle.
struct EgressTask {
    id: u64,
    source_generation: u64,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EgressReservation {
    source_generation: u64,
    id: u64,
}

/// Exact-match cleanup for a reservation held across an async UDP bind.
///
/// Dropping the future at any await point must make the source epoch
/// immediately retryable; a plain error branch cannot cover cancellation.
struct EgressReservationGuard {
    connecting: Arc<Mutex<HashMap<CallId, EgressReservation>>>,
    call: CallId,
    reservation: EgressReservation,
}

impl Drop for EgressReservationGuard {
    fn drop(&mut self) {
        let mut connecting = self.connecting.lock();
        if connecting.get(&self.call) == Some(&self.reservation) {
            connecting.remove(&self.call);
        }
    }
}

impl CallBridgeSupervisor {
    /// Build a supervisor that fans bridged media into `router` via `forwarder`
    /// and constructs per-peer upstreams via `factory`.
    #[must_use]
    pub fn new(
        router: SfuRouter,
        forwarder: Arc<dyn MediaForwarder>,
        factory: Arc<dyn UpstreamFactory>,
        subscribers: BridgeSubscriberRegistry,
    ) -> Self {
        let media = SfuMediaRegistry::from_env(router.clone(), forwarder.clone());
        Self {
            router,
            forwarder,
            media,
            factory,
            subscribers,
            active: Arc::new(Mutex::new(HashMap::new())),
            connecting: Arc::new(Mutex::new(HashMap::new())),
            egress: Arc::new(Mutex::new(HashMap::new())),
            egress_connecting: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Ensure the relay consuming one exact media-source epoch. A reconnect
    /// replaces an older relay; same-epoch calls are idempotent.
    async fn ensure_egress_generation(
        &self,
        call: CallId,
        source_generation: u64,
        tap: CallEgressTap,
    ) -> bool {
        self.ensure_egress_inner(call, source_generation, tap, true)
            .await
    }

    #[cfg(test)]
    pub async fn ensure_egress(&self, call: CallId, tap: CallEgressTap) -> bool {
        self.ensure_egress_inner(call, 0, tap, false).await
    }

    async fn ensure_egress_inner(
        &self,
        call: CallId,
        source_generation: u64,
        tap: CallEgressTap,
        validate_media_epoch: bool,
    ) -> bool {
        let reserve = || self.reserve_egress(call, source_generation);
        let Some((reservation, replaced)) = (if validate_media_epoch {
            self.media
                .with_current_egress_epoch(call, source_generation, reserve)
                .flatten()
        } else {
            reserve()
        }) else {
            return false;
        };
        if let Some(replaced) = replaced {
            replaced.cancel.cancel();
            replaced.handle.abort();
        }
        let _reservation_guard = EgressReservationGuard {
            connecting: self.egress_connecting.clone(),
            call,
            reservation,
        };
        let sender = match UdpRtpEgress::bind(call, self.subscribers.clone()).await {
            Ok(s) => s,
            Err(e) => {
                warn!(?e, %call, "call-bridge: egress bind failed");
                return false;
            }
        };
        self.spawn_reserved_egress(call, reservation, tap, sender, validate_media_epoch)
    }

    fn reserve_egress(
        &self,
        call: CallId,
        source_generation: u64,
    ) -> Option<(EgressReservation, Option<EgressTask>)> {
        let mut connecting = self.egress_connecting.lock();
        let mut active = self.egress.lock();
        if active
            .get(&call)
            .is_some_and(|task| task.source_generation == source_generation)
        {
            if connecting
                .get(&call)
                .is_some_and(|pending| pending.source_generation != source_generation)
            {
                connecting.remove(&call);
            }
            return None;
        }
        if connecting
            .get(&call)
            .is_some_and(|pending| pending.source_generation == source_generation)
        {
            return None;
        }
        let reservation = EgressReservation {
            source_generation,
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
        };
        connecting.insert(call, reservation);
        Some((reservation, active.remove(&call)))
    }

    fn spawn_reserved_egress(
        &self,
        call: CallId,
        reservation: EgressReservation,
        tap: CallEgressTap,
        sender: UdpRtpEgress,
        validate_media_epoch: bool,
    ) -> bool {
        let spawn = || self.spawn_reserved_egress_locked(call, reservation, tap, sender);
        if validate_media_epoch {
            if let Some(spawned) =
                self.media
                    .with_current_egress_epoch(call, reservation.source_generation, spawn)
            {
                spawned
            } else {
                let mut connecting = self.egress_connecting.lock();
                if connecting.get(&call) == Some(&reservation) {
                    connecting.remove(&call);
                }
                false
            }
        } else {
            spawn()
        }
    }

    fn spawn_reserved_egress_locked(
        &self,
        call: CallId,
        reservation: EgressReservation,
        tap: CallEgressTap,
        sender: UdpRtpEgress,
    ) -> bool {
        let mut connecting = self.egress_connecting.lock();
        if connecting.get(&call) != Some(&reservation) {
            return false;
        }
        connecting.remove(&call);
        let mut active_guard = self.egress.lock();
        if active_guard
            .get(&call)
            .is_some_and(|task| task.source_generation == reservation.source_generation)
        {
            return false;
        }
        let replaced = active_guard.remove(&call);
        let id = reservation.id;
        let source_generation = reservation.source_generation;
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let active = self.egress.clone();
        let subscribers = self.subscribers.clone();
        let media = self.media.clone();
        let (registered_tx, registered_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            if registered_rx.await.is_err() {
                return;
            }
            sender.run(tap, task_cancel).await;
            let mut active = active.lock();
            let removed = active
                .get(&call)
                .is_some_and(|task| task.id == id && task.source_generation == source_generation);
            if removed {
                active.remove(&call);
            }
            drop(active);
            if removed {
                let _ = media.with_ended_egress_epoch(call, source_generation, || {
                    subscribers.clear_call(call);
                });
            }
        });
        active_guard.insert(
            call,
            EgressTask {
                id,
                source_generation,
                cancel,
                handle,
            },
        );
        drop(active_guard);
        drop(connecting);
        if let Some(replaced) = replaced {
            replaced.cancel.cancel();
            replaced.handle.abort();
        }
        let _ = registered_tx.send(());
        true
    }

    /// Retry relay creation from the registry's current source. The internal
    /// subscribe endpoint calls this before admission, so a transient UDP bind
    /// failure is recoverable without waiting for another browser offer.
    pub async fn ensure_current_egress(&self, call: CallId) -> bool {
        let Some((generation, tap)) = self.media.current_egress_tap(call) else {
            return false;
        };
        self.ensure_egress_generation(call, generation, tap).await
    }

    /// Negotiate and start one browser ⇄ server SFU media leg.
    ///
    /// The first session in a call also creates the shared [`CallEgress`] tap
    /// and starts the real UDP egress relay; later publishers clone the same
    /// call bus.
    pub async fn accept_sfu_offer(
        &self,
        call: CallId,
        participant: aero_common::ParticipantId,
        leg_generation: i64,
        offer: &str,
    ) -> Result<SfuSessionAnswer, SfuMediaError> {
        let started = self
            .media
            .start_session(call, participant, leg_generation, offer)
            .await?;
        let _ = self
            .ensure_egress_generation(call, started.egress_generation, started.egress_tap)
            .await;
        Ok(started.answer)
    }

    /// Queue and apply one trickled browser ICE candidate on the session owner.
    pub async fn add_sfu_ice(
        &self,
        call: CallId,
        participant: aero_common::ParticipantId,
        candidate: String,
    ) -> Result<(), SfuMediaError> {
        self.media
            .add_remote_candidate(call, participant, candidate)
            .await
    }

    /// Remove one browser media leg. The call's egress relay is stopped when it
    /// was the last local session.
    pub fn remove_sfu_session(
        &self,
        call: CallId,
        participant: aero_common::ParticipantId,
    ) -> bool {
        let removal = self.media.remove_session_with_topology(call, participant);
        if let Some(generation) = removal.ended_egress_generation {
            self.cancel_ended_media_epoch(call, generation);
        }
        removal.removed
    }

    /// Remove every SFU media leg for a disconnected participant.
    pub fn remove_sfu_participant(&self, participant: aero_common::ParticipantId) -> Vec<CallId> {
        let removed = self.media.remove_participant_with_topology(participant);
        for (call, _, generation) in &removed {
            if let Some(generation) = generation {
                self.cancel_ended_media_epoch(*call, *generation);
            }
        }
        removed.into_iter().map(|(call, _, _)| call).collect()
    }

    #[must_use]
    pub fn media_session_count(&self, call: CallId) -> usize {
        self.media.session_count(call)
    }

    #[must_use]
    pub fn has_media_session(&self, call: CallId, participant: aero_common::ParticipantId) -> bool {
        self.media.has_session(call, participant)
    }

    /// Whether this process owns the participant's live SFU leg.
    #[must_use]
    pub fn has_call_participant(
        &self,
        call: CallId,
        participant: aero_common::ParticipantId,
    ) -> bool {
        self.router.local_participants(call).contains(&participant)
    }

    /// Whether this process currently owns at least one SFU participant for the
    /// call. Remote bus events are consumed by every node, but only nodes with a
    /// local island should create bridge pulls.
    #[must_use]
    pub fn has_local_call(&self, call: CallId) -> bool {
        self.router.has_local_participants(call)
    }

    /// Whether an egress relay is running for `call`.
    #[must_use]
    pub fn has_egress(&self, call: CallId) -> bool {
        self.egress.lock().contains_key(&call)
    }

    fn cancel_egress(&self, call: CallId) -> bool {
        let connecting = self.egress_connecting.lock().remove(&call);
        let mut active = self.egress.lock();
        let egress = active.remove(&call);
        self.subscribers.clear_call(call);
        drop(active);
        let Some(egress) = egress else {
            return connecting.is_some();
        };
        egress.cancel.cancel();
        egress.handle.abort();
        true
    }

    /// Tear down pull/push tasks only if `source_generation` is still the
    /// latest ended local-media epoch. A reconnect commit takes the media lock
    /// first and therefore fences every stale lifecycle/leave cleanup.
    pub fn cancel_ended_media_epoch(&self, call: CallId, source_generation: u64) -> usize {
        self.media
            .consume_ended_egress_epoch(call, source_generation, || {
                self.connecting
                    .lock()
                    .retain(|(candidate, _), _| *candidate != call);
                let tasks: Vec<_> = {
                    let mut active = self.active.lock();
                    let keys: Vec<_> = active
                        .keys()
                        .filter(|(candidate, _)| *candidate == call)
                        .cloned()
                        .collect();
                    keys.into_iter()
                        .filter_map(|key| active.remove(&key))
                        .collect()
                };
                let count = tasks.len();
                for task in tasks {
                    task.cancel.cancel();
                    task.handle.abort();
                }
                // The media lock proves that no current source/session exists
                // and this is the latest ended epoch, so call-wide cleanup is
                // safe and also removes any older orphaned reservation.
                let _ = self.cancel_egress(call);
                count
            })
            .unwrap_or(0)
    }

    pub(crate) async fn lock_sfu_lifecycle(&self, call: CallId) -> tokio::sync::MutexGuard<'_, ()> {
        self.media.lock_lifecycle(call).await
    }

    #[must_use]
    pub(crate) fn is_current_ended_sfu_session(
        &self,
        call: CallId,
        participant: ParticipantId,
        generation: u64,
    ) -> bool {
        self.media
            .is_current_ended_session(call, participant, generation)
    }

    pub(crate) fn invalidate_ended_sfu_session(&self, call: CallId, participant: ParticipantId) {
        if let Some(generation) = self.media.invalidate_ended_session(call, participant) {
            self.cancel_ended_media_epoch(call, generation);
        }
    }

    pub(crate) fn complete_ended_sfu_session(
        &self,
        call: CallId,
        participant: ParticipantId,
        generation: u64,
    ) -> bool {
        self.media
            .complete_ended_session(call, participant, generation)
    }

    /// Number of bridge tasks currently registered for `call`.
    #[must_use]
    pub fn bridge_count(&self, call: CallId) -> usize {
        self.active
            .lock()
            .keys()
            .filter(|(c, _)| *c == call)
            .count()
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

    /// Cancel and remove **every** bridge task for `call` — the
    /// last-local-leave / `end_call` teardown. Idempotent: a call with no
    /// bridges is a no-op. Returns the number of bridges cancelled.
    pub fn cancel_call(&self, call: CallId) -> usize {
        let media_sessions = self.media.remove_call(call);
        self.connecting
            .lock()
            .retain(|(candidate, _), _| *candidate != call);
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
        // Tear down the call's egress relay (the push side) too, if any.
        if self.cancel_egress(call) {
            debug!(%call, "call-bridge: egress relay torn down");
        }
        if n > 0 {
            debug!(%call, count = n, "call-bridge: all bridges for call cancelled");
        }
        if media_sessions > 0 {
            debug!(%call, count = media_sessions, "SFU media sessions cancelled");
        }
        n
    }

    /// Cancel a single peer's bridge for `call` (no-op if none). Returns whether
    /// a bridge was cancelled — used when a census shrinks but the call lives on.
    pub fn cancel_bridge(&self, call: CallId, peer_url: &str) -> bool {
        let key = (call, normalize_node(peer_url));
        let connecting = self.connecting.lock().remove(&key).is_some();
        if let Some(task) = self.active.lock().remove(&key) {
            task.cancel.cancel();
            task.handle.abort();
            true
        } else {
            connecting
        }
    }
}

/// Normalize a node base URL so `http://a` and `http://a/` map to one bridge
/// key (same rule the topology policy and route registry use).
fn normalize_node(url: &str) -> String {
    url.trim_end_matches('/').to_owned()
}

#[cfg(test)]
mod tests;
