//! Real selective-forwarding: route a publisher's RTP to every subscriber.
//!
//! [`SfuForwarder`] is the runtime counterpart to the pure [`ForwardTable`]
//! (in [`crate::remap`]). It owns the live [`SfuPeer`]s of a call and, for each
//! inbound RTP packet lifted out of a publisher's `str0m` instance, performs the
//! fan-out:
//!
//! 1. ask the [`SfuRouter`] / [`ForwardTable`] which subscriber tracks want it,
//! 2. consult the [`LayerSelectorTable`] to decide whether this subscriber
//!    should receive the packet on the current simulcast layer, honoring the
//!    keyframe-boundary switch rule,
//! 3. remap the `(seq, ts)` into each subscriber's own outbound RTP space,
//! 4. write the rewritten RTP onto that subscriber's outbound `str0m` stream.
//!
//! When a subscriber joins (`subscribe`) or a simulcast layer switch is
//! initiated (`select_layer`), a keyframe request is enqueued via the internal
//! [`KeyframeGate`]. Callers drain those requests via
//! [`SfuForwarder::poll_keyframe_requests`] and relay them to the appropriate
//! publisher peer.
//!
//! Inbound subscriber RTCP is accepted via
//! [`SfuForwarder::on_subscriber_rtcp`], which decodes the buffer twice:
//!
//! - PLI/FIR (via [`crate::rtcp_feedback::parse_keyframe_requests`]) become
//!   coalesced keyframe requests routed upstream to the correct publisher.
//! - REMB/TWCC (via [`crate::rtcp_fb::parse_bandwidth_feedback`]) feed that
//!   subscriber's [`BandwidthEstimator`]; after every bandwidth feedback the
//!   forwarder re-evaluates the subscriber's simulcast layer choice
//!   ([`LayerSwitchPolicy`]: down immediately, up after ~2 s of headroom)
//!   against per-layer throughput measured from forwarded RTP, and any switch
//!   goes through the existing keyframe-gated [`LayerSelectorTable`] mechanics.
//!
//! ## Adaptive-bitrate feedback loop toward the publisher
//!
//! The subscriber side above only *consumes* bandwidth feedback. The other half
//! of the loop pushes an aggregated REMB back to the **publisher** so an
//! OBS/ffmpeg/browser encoder can lower its bitrate when the slowest subscriber
//! is congested. For each published `pub_mid` a [`PublisherRembAggregator`]
//! folds the per-subscriber [`BandwidthEstimator`] estimates into one target
//! (MIN across subscribers, floored, EWMA-smoothed). On each subscriber
//! feedback ([`SfuForwarder::on_subscriber_rtcp_at`]) and on a periodic tick
//! ([`SfuForwarder::tick_remb_at`]) the aggregator decides — under a 10 %
//! hysteresis — whether to emit; emissions are enqueued as
//! [`PendingRemb`]s drained via [`SfuForwarder::poll_remb_requests`].
//!
//! Production sessions route both keyframe requests and aggregated REMB through
//! the publisher's bounded [`SfuPeerSink`]. The publisher's sole UDP/str0m task
//! applies those commands and drains the resulting encrypted RTCP datagrams.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use aero_common::{CallId, ParticipantId, SfuSubscription};
use parking_lot::Mutex;
use str0m::media::{KeyframeRequestKind, Mid, Rid};
use tracing::trace;

use crate::bwe::{BandwidthEstimator, LayerSwitchPolicy, ThroughputEwma};
use crate::call_bridge::BridgeRtp;
use crate::peer::{InboundRtp, SfuPeer};
use crate::remap::{ForwardTable, RtpKey};
use crate::rtcp_fb::{parse_bandwidth_feedback, BandwidthFeedback, PublisherRembAggregator};
use crate::rtcp_feedback::{parse_keyframe_requests, KeyframeGate, PendingKeyframeRequest};
use crate::simulcast::{ForwardDecision, LayerKind, LayerSelectorTable, LayerSet};
use crate::{SfuPeerSink, SfuRouter};

mod adapter;
mod production;
use production::{CallForwardTable, CallTrack};

/// An aggregated REMB the SFU must relay to a publisher so it can cap its
/// encoder to what the slowest subscriber can receive.
///
/// The legacy in-forwarder peer API drains this via
/// [`SfuForwarder::poll_remb_requests`]. Production server sessions instead
/// enqueue the same aggregate directly through the publisher's bounded
/// [`SfuPeerSink`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRemb {
    /// Publisher participant the REMB is addressed to.
    pub publisher: ParticipantId,
    /// Publisher's track `mid` the REMB caps.
    pub pub_mid: String,
    /// Aggregated target bitrate (bps): MIN across subscribers, floored + EWMA.
    pub bitrate_bps: u64,
}

/// Derive a stable, process-local `u64` token from a [`ParticipantId`] for
/// keying [`PublisherRembAggregator`] (which is kept decoupled from
/// `aero-common` id types). Folds the 128-bit ULID into 64 bits; collisions
/// within one call's subscriber set are vanishingly unlikely.
fn subscriber_token(p: ParticipantId) -> u64 {
    let v = p.as_ulid().0;
    #[allow(clippy::cast_possible_truncation)]
    {
        (v as u64) ^ ((v >> 64) as u64)
    }
}

fn call_route_mid(call: CallId, publisher: ParticipantId, mid: &str) -> String {
    format!("{call}:{publisher}:{mid}")
}

/// Live forwarding state for a single call: the per-call peer set plus the
/// publisher→subscriber routing/remap table. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct SfuForwarder {
    router: SfuRouter,
    /// Monotonic epoch used to project wall time into the `now_ms` domain the
    /// pure BWE components ([`crate::bwe`]) consume.
    epoch: Instant,
    inner: Arc<Mutex<ForwardState>>,
}

#[derive(Default)]
struct ForwardState {
    /// Live str0m peers keyed by participant. Boxed behind the call's lock so
    /// the UDP tasks can take `&mut` to one peer at a time.
    peers: HashMap<ParticipantId, SfuPeer>,
    /// Production session command sinks. The corresponding `SfuPeer` stays
    /// exclusively owned and polled by its media-session task.
    peer_sinks: HashMap<(CallId, ParticipantId), Arc<dyn SfuPeerSink>>,
    /// Publisher-feedback sinks owned by remote bridge pulls, keyed again by
    /// a monotonically increasing bridge incarnation. They never replace a
    /// local session sink; publisher control chooses local first and otherwise
    /// the newest live bridge.
    bridge_peer_sinks: HashMap<(CallId, ParticipantId), BTreeMap<String, Arc<dyn SfuPeerSink>>>,
    /// Production routing/remap tables are isolated by call. SDP MIDs are only
    /// unique within one peer connection, so a process-wide `mid -> targets`
    /// table would leak common tracks such as `"0"` between concurrent calls.
    call_tables: HashMap<CallId, CallForwardTable>,
    /// Production publisher-facing REMB aggregation, isolated by call and
    /// publisher because identical MIDs are normal across peer connections.
    call_remb_agg: HashMap<(CallId, CallTrack), PublisherRembAggregator>,
    /// Pure routing + per-subscriber remap bookkeeping.
    table: ForwardTable,
    /// Simulcast layer selection state: one [`LayerSelector`] per
    /// `(subscriber, pub_mid)` pair, plus the [`LayerSet`] for each published
    /// simulcast track.
    layer_table: LayerSelectorTable,
    /// Coalesced keyframe-request coordinator. Accumulates PLI/FIR requests
    /// from subscriber RTCP, new-subscription events, and layer-switch triggers,
    /// and suppresses duplicates within a 200 ms window.
    keyframe_gate: KeyframeGate,
    /// `pub_mid → publisher ParticipantId` — kept here so keyframe requests
    /// can be routed to the right publisher without needing the call ID or the
    /// [`SfuRouter`] lock.
    pub_owners: HashMap<String, ParticipantId>,
    /// `(pub_mid, rid) → measured throughput` — EWMA bytes/sec per simulcast
    /// layer, fed from every inbound publisher RTP packet carrying a RID.
    layer_rates: HashMap<(String, Rid), ThroughputEwma>,
    /// Per-subscriber bandwidth estimators, fed from REMB/TWCC in that
    /// subscriber's RTCP.
    bwe: HashMap<ParticipantId, BandwidthEstimator>,
    /// `(subscriber, pub_mid) → hysteresis state` for bandwidth-driven layer
    /// switching.
    adapt: HashMap<(ParticipantId, String), LayerSwitchPolicy>,
    /// `pub_mid → publisher-facing REMB aggregator` — folds the subscribing
    /// peers' current [`BandwidthEstimator`] estimates into one MIN-bounded,
    /// EWMA-smoothed target for the publisher.
    remb_agg: HashMap<String, PublisherRembAggregator>,
    /// Aggregated REMBs queued for the SFU to relay upstream to publishers,
    /// drained via [`SfuForwarder::poll_remb_requests`].
    remb_queue: std::collections::VecDeque<PendingRemb>,
}

fn publisher_sink(
    inner: &ForwardState,
    call: CallId,
    publisher: ParticipantId,
) -> Option<Arc<dyn SfuPeerSink>> {
    inner
        .peer_sinks
        .get(&(call, publisher))
        .cloned()
        .or_else(|| {
            inner
                .bridge_peer_sinks
                .get(&(call, publisher))
                .and_then(|sinks| sinks.values().next_back().cloned())
        })
}

fn withdraw_call_estimates(
    inner: &mut ForwardState,
    call: CallId,
    subscriber: ParticipantId,
    keep: &HashSet<CallTrack>,
) -> Vec<(Arc<dyn SfuPeerSink>, String, u64)> {
    let token = subscriber_token(subscriber);
    let sources: Vec<_> = inner
        .call_remb_agg
        .keys()
        .filter_map(|(active_call, source)| {
            (*active_call == call && !keep.contains(source)).then_some(source.clone())
        })
        .collect();
    let mut lifted = Vec::new();
    for source in sources {
        let bitrate_bps = inner
            .call_remb_agg
            .get_mut(&(call, source.clone()))
            .and_then(|aggregator| aggregator.remove(token));
        if let (Some(bitrate_bps), Some(sink)) =
            (bitrate_bps, publisher_sink(inner, call, source.publisher))
        {
            lifted.push((sink, source.mid, bitrate_bps));
        }
    }
    inner
        .call_remb_agg
        .retain(|_, aggregator| aggregator.subscriber_count() > 0);
    lifted
}

impl SfuForwarder {
    #[must_use]
    pub fn new(router: SfuRouter) -> Self {
        Self {
            router,
            epoch: Instant::now(),
            inner: Arc::new(Mutex::new(ForwardState::default())),
        }
    }

    /// Milliseconds since this forwarder was created — the time domain fed to
    /// the pure BWE components.
    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Register a live peer with the forwarder (called once its `Rtc` exists).
    pub fn add_peer(&self, peer: SfuPeer) {
        let mut inner = self.inner.lock();
        inner.peer_sinks.remove(&(peer.call(), peer.id()));
        inner.peers.insert(peer.id(), peer);
    }

    /// Register a production session's bounded command sink.
    ///
    /// Any legacy in-forwarder peer under the same participant is removed so a
    /// single `SfuPeer` state machine can never have two owners.
    pub fn attach_peer_sink(
        &self,
        call: CallId,
        participant: ParticipantId,
        sink: Arc<dyn SfuPeerSink>,
    ) -> bool {
        let mut inner = self.inner.lock();
        inner.peers.remove(&participant);
        let pending_tracks = inner
            .call_tables
            .get(&call)
            .map_or_else(Vec::new, |table| table.publisher_tracks(participant));
        let request_sink = Arc::clone(&sink);
        let inserted = inner.peer_sinks.insert((call, participant), sink).is_none();
        drop(inner);
        // A route may have been installed before this publisher reconnected.
        // Request a fresh frame as soon as its unique owner task is attached.
        for track in pending_tracks {
            let _ = request_sink
                .try_request_keyframe(Mid::from(track.mid.as_str()), KeyframeRequestKind::Pli);
        }
        inserted
    }

    /// Register one bridge's publisher-feedback sink without competing with a
    /// local media-session owner. Multiple bridge references are retained
    /// independently so an owner hand-off cannot strand PLI/FIR/REMB control.
    pub fn attach_bridge_peer_sink(
        &self,
        call: CallId,
        participant: ParticipantId,
        bridge: &str,
        sink: Arc<dyn SfuPeerSink>,
    ) -> bool {
        let request_sink = sink.clone();
        let (inserted, request_keyframes) = {
            let mut inner = self.inner.lock();
            let local_exists = inner.peer_sinks.contains_key(&(call, participant));
            let pending_tracks = inner
                .call_tables
                .get(&call)
                .map_or_else(Vec::new, |table| table.publisher_tracks(participant));
            let bridge = bridge.to_owned();
            let sinks = inner
                .bridge_peer_sinks
                .entry((call, participant))
                .or_default();
            let inserted = sinks.insert(bridge.clone(), sink).is_none();
            let request_keyframes =
                !local_exists && sinks.keys().next_back() == Some(&bridge) && inserted;
            (inserted, request_keyframes.then_some(pending_tracks))
        };
        if let Some(tracks) = request_keyframes {
            for track in tracks {
                let _ = request_sink
                    .try_request_keyframe(Mid::from(track.mid.as_str()), KeyframeRequestKind::Pli);
            }
        }
        inserted
    }

    /// Remove exactly one bridge's publisher-feedback sink.
    pub fn detach_bridge_peer_sink(
        &self,
        call: CallId,
        participant: ParticipantId,
        bridge: &str,
    ) -> bool {
        let mut inner = self.inner.lock();
        let key = (call, participant);
        let was_effective = !inner.peer_sinks.contains_key(&key)
            && inner
                .bridge_peer_sinks
                .get(&key)
                .and_then(|sinks| sinks.keys().next_back())
                .is_some_and(|active| active == bridge);
        let removed = inner
            .bridge_peer_sinks
            .get_mut(&key)
            .is_some_and(|sinks| sinks.remove(bridge).is_some());
        if inner
            .bridge_peer_sinks
            .get(&key)
            .is_some_and(BTreeMap::is_empty)
        {
            inner.bridge_peer_sinks.remove(&key);
        }
        let fallback = (removed && was_effective)
            .then(|| publisher_sink(&inner, call, participant))
            .flatten();
        let pending_tracks = fallback.as_ref().map(|_| {
            inner
                .call_tables
                .get(&call)
                .map_or_else(Vec::new, |table| table.publisher_tracks(participant))
        });
        drop(inner);
        if let (Some(sink), Some(tracks)) = (fallback, pending_tracks) {
            for track in tracks {
                let _ = sink
                    .try_request_keyframe(Mid::from(track.mid.as_str()), KeyframeRequestKind::Pli);
            }
        }
        removed
    }

    /// Remove a peer and all routing/remap + simulcast + BWE state
    /// referencing it.
    ///
    /// Dropping a subscriber also withdraws its estimate from every
    /// publisher-facing REMB aggregator; if it was the slowest subscriber the
    /// aggregate lifts and a fresh higher REMB is enqueued for the publisher.
    pub fn remove_peer(&self, peer: ParticipantId) -> Option<SfuPeer> {
        let mut g = self.inner.lock();
        g.table.unlink_subscriber(peer);
        g.layer_table.unlink_subscriber(peer);
        g.bwe.remove(&peer);
        g.adapt.retain(|(sub, _), _| *sub != peer);

        let token = subscriber_token(peer);
        let ForwardState {
            remb_agg,
            remb_queue,
            pub_owners,
            ..
        } = &mut *g;
        for (mid, agg) in remb_agg.iter_mut() {
            if let Some(bitrate_bps) = agg.remove(token) {
                if let Some(&publisher) = pub_owners.get(mid) {
                    remb_queue.push_back(PendingRemb {
                        publisher,
                        pub_mid: mid.clone(),
                        bitrate_bps,
                    });
                }
            }
        }
        g.peers.remove(&peer)
    }

    /// Remove only a production peer sink while applying the same routing-state
    /// cleanup as [`Self::remove_peer`]. Returns whether a sink was present.
    pub fn detach_peer_sink(&self, call: CallId, peer: ParticipantId) -> bool {
        let mut inner = self.inner.lock();
        let existed = inner.peer_sinks.remove(&(call, peer)).is_some();
        let bridge_remains = inner
            .bridge_peer_sinks
            .get(&(call, peer))
            .is_some_and(|sinks| !sinks.is_empty());
        let bridge_fallback = bridge_remains
            .then(|| publisher_sink(&inner, call, peer))
            .flatten();
        let pending_tracks = bridge_fallback.as_ref().map(|_| {
            inner
                .call_tables
                .get(&call)
                .map_or_else(Vec::new, |table| table.publisher_tracks(peer))
        });
        let lifted = withdraw_call_estimates(&mut inner, call, peer, &HashSet::new());
        if let Some(table) = inner.call_tables.get_mut(&call) {
            table.unlink_subscriber(peer);
            if !bridge_remains {
                table.unlink_publisher(peer);
            }
            if table.is_empty() {
                inner.call_tables.remove(&call);
            }
        }
        if !bridge_remains {
            inner.call_remb_agg.retain(|(active_call, source), _| {
                *active_call != call || source.publisher != peer
            });
        }
        drop(inner);
        for (sink, mid, bitrate_bps) in lifted {
            let _ = sink.try_request_remb(Mid::from(mid.as_str()), bitrate_bps);
        }
        if let (Some(sink), Some(tracks)) = (bridge_fallback, pending_tracks) {
            for track in tracks {
                let _ = sink
                    .try_request_keyframe(Mid::from(track.mid.as_str()), KeyframeRequestKind::Pli);
            }
        }
        existed
    }

    /// Clear a reconnecting peer's sink and subscriber-side state while
    /// retaining routes whose source is this peer's unchanged publisher track.
    pub fn reset_peer_sink(&self, call: CallId, peer: ParticipantId) -> bool {
        let mut inner = self.inner.lock();
        let existed = inner.peer_sinks.remove(&(call, peer)).is_some();
        let lifted = withdraw_call_estimates(&mut inner, call, peer, &HashSet::new());
        if let Some(table) = inner.call_tables.get_mut(&call) {
            table.unlink_subscriber(peer);
            if table.is_empty() {
                inner.call_tables.remove(&call);
            }
        }
        drop(inner);
        for (sink, mid, bitrate_bps) in lifted {
            let _ = sink.try_request_remb(Mid::from(mid.as_str()), bitrate_bps);
        }
        existed
    }

    /// Atomically replace one production subscriber's explicit routes.
    ///
    /// Every source is keyed by `(publisher, pub_mid)`, so three participants
    /// may all publish MID `"0"` without interleaving onto one outbound stream.
    /// A PLI is queued to each locally-owned publisher immediately after the
    /// route becomes active.
    pub fn replace_call_subscriptions(
        &self,
        call: CallId,
        subscriber: ParticipantId,
        routes: &[SfuSubscription],
    ) -> usize {
        let mut inner = self.inner.lock();
        let retained_sources: HashSet<_> = routes
            .iter()
            .map(|route| CallTrack::new(route.publisher, &route.pub_mid))
            .collect();
        let lifted = withdraw_call_estimates(&mut inner, call, subscriber, &retained_sources);
        inner
            .call_tables
            .entry(call)
            .or_default()
            .replace_subscriber(subscriber, routes);
        inner.layer_table.unlink_subscriber(subscriber);
        for route in routes {
            inner.layer_table.register(
                subscriber,
                &call_route_mid(call, route.publisher, &route.pub_mid),
            );
        }
        let requests: Vec<_> = routes
            .iter()
            .filter_map(|route| {
                publisher_sink(&inner, call, route.publisher)
                    .map(|sink| (sink, Mid::from(route.pub_mid.as_str())))
            })
            .collect();
        drop(inner);

        for (sink, mid, bitrate_bps) in lifted {
            let _ = sink.try_request_remb(Mid::from(mid.as_str()), bitrate_bps);
        }
        let mut queued = 0;
        for (sink, mid) in requests {
            if sink.try_request_keyframe(mid, KeyframeRequestKind::Pli) {
                queued += 1;
            }
        }
        queued
    }

    /// Compatibility helper for a single explicit route.
    pub fn subscribe_call(
        &self,
        call: CallId,
        publisher: ParticipantId,
        pub_mid: &str,
        subscriber: ParticipantId,
        out_mid: &str,
    ) {
        self.replace_call_subscriptions(
            call,
            subscriber,
            &[SfuSubscription {
                publisher,
                pub_mid: pub_mid.to_owned(),
                out_mid: out_mid.to_owned(),
            }],
        );
    }

    /// Register that publisher `publisher` owns `pub_mid` and advertises the
    /// given set of simulcast layers. Call this whenever a publisher's track is
    /// first seen (before any subscriptions are set up for that track).
    ///
    /// Safe to call with an empty [`LayerSet`] for non-simulcast tracks.
    pub fn register_publisher_layers(
        &self,
        pub_mid: &str,
        publisher: ParticipantId,
        layers: LayerSet,
    ) {
        let mut g = self.inner.lock();
        g.pub_owners.insert(pub_mid.to_owned(), publisher);
        g.layer_table.set_layers(pub_mid, layers);
    }

    /// Link a published track to a subscriber's outbound transceiver so future
    /// packets on `pub_mid` are forwarded to `subscriber`'s `out_mid` stream.
    ///
    /// Also:
    /// - Registers a simulcast layer selector for this `(subscriber, pub_mid)` pair.
    /// - Enqueues a keyframe request toward the publisher so the subscriber can
    ///   start decoding immediately (drained via [`Self::poll_keyframe_requests`]).
    pub fn subscribe(&self, pub_mid: &str, subscriber: ParticipantId, out_mid: &str) {
        let now = Instant::now();
        let mut g = self.inner.lock();
        g.table.link(pub_mid, subscriber, out_mid);
        g.layer_table.register(subscriber, pub_mid);
        // Enqueue a keyframe request so the new subscriber gets an I-frame
        // immediately.  We bypass the coalesce window for new subscriptions.
        if let Some(&publisher) = g.pub_owners.get(pub_mid) {
            g.keyframe_gate.new_subscriber(publisher, pub_mid, now);
        }
    }

    /// Drop a published track from the routing table (publisher unpublished).
    pub fn unpublish(&self, pub_mid: &str) {
        let mut g = self.inner.lock();
        g.table.unlink_publisher_track(pub_mid);
        g.pub_owners.remove(pub_mid);
        g.layer_rates.retain(|(mid, _), _| mid != pub_mid);
        g.adapt.retain(|(_, mid), _| mid != pub_mid);
        g.remb_agg.remove(pub_mid);
        g.remb_queue.retain(|r| r.pub_mid != pub_mid);
    }

    /// Request that `subscriber` receive the best available simulcast layer
    /// ≤ `target` for `pub_mid`.
    ///
    /// - The actual switch is gated on the next keyframe from the target layer
    ///   (via [`LayerSelector::should_forward`]).
    /// - A keyframe request is enqueued toward the publisher (drained via
    ///   [`Self::poll_keyframe_requests`]).
    ///
    /// Returns the RID of the selected layer, or `None` when no layers are
    /// registered for the track (non-simulcast track or publisher not yet
    /// registered).
    pub fn select_layer(
        &self,
        subscriber: ParticipantId,
        pub_mid: &str,
        target: LayerKind,
    ) -> Option<Rid> {
        let now = Instant::now();
        let mut g = self.inner.lock();
        let rid = g.layer_table.select_layer(subscriber, pub_mid, target)?;
        // Enqueue a keyframe request so the target layer switch can complete.
        if let Some(&publisher) = g.pub_owners.get(pub_mid) {
            g.keyframe_gate.on_layer_switch(publisher, pub_mid, now);
        }
        Some(rid)
    }

    /// Accept inbound RTCP from a subscriber: PLI/FIR become coalesced
    /// keyframe requests routed to the correct publisher; REMB/TWCC feed the
    /// subscriber's bandwidth estimator and may trigger a bandwidth-driven
    /// simulcast layer switch (down immediately, up after stable headroom).
    ///
    /// `subscriber` — the participant that sent the RTCP.
    /// `pub_mid` — the published track the subscriber is watching (the SFU
    ///   must resolve this from the subscriber's own outbound mid using the
    ///   routing table before calling here).
    /// `rtcp_buf` — the raw compound RTCP datagram.
    ///
    /// Enqueued keyframe requests (from PLI/FIR *and* from any layer switch
    /// this call decides) are drained via [`Self::poll_keyframe_requests`].
    pub fn on_subscriber_rtcp(&self, subscriber: ParticipantId, pub_mid: &str, rtcp_buf: &[u8]) {
        self.on_subscriber_rtcp_at(subscriber, pub_mid, rtcp_buf, Instant::now(), self.now_ms());
    }

    /// Time-injected body of [`Self::on_subscriber_rtcp`] (`now` drives the
    /// keyframe coalesce gate, `now_ms` the deterministic BWE domain).
    fn on_subscriber_rtcp_at(
        &self,
        subscriber: ParticipantId,
        pub_mid: &str,
        rtcp_buf: &[u8],
        now: Instant,
        now_ms: u64,
    ) {
        let keyframe_fbs = parse_keyframe_requests(rtcp_buf);
        let bandwidth_fbs = parse_bandwidth_feedback(rtcp_buf);
        if keyframe_fbs.is_empty() && bandwidth_fbs.is_empty() {
            return;
        }
        let mut g = self.inner.lock();
        if let Some(&publisher) = g.pub_owners.get(pub_mid) {
            for fb in keyframe_fbs {
                if fb.is_fir() {
                    g.keyframe_gate.on_subscriber_fir(publisher, pub_mid, now);
                } else {
                    g.keyframe_gate.on_subscriber_pli(publisher, pub_mid, now);
                }
            }
        }
        if bandwidth_fbs.is_empty() {
            return;
        }
        let est = g.bwe.entry(subscriber).or_default();
        for fb in &bandwidth_fbs {
            match fb {
                BandwidthFeedback::Remb(r) => est.on_remb(r.bitrate_bps, now_ms),
                BandwidthFeedback::Twcc(t) => {
                    let s = t.summary();
                    est.on_twcc(s.received, s.lost, s.delay_trend_us, now_ms);
                }
            }
        }
        let estimate_bps = est.estimate_bps();
        g.adapt_subscriber(subscriber, pub_mid, now, now_ms);

        // Fold this subscriber's fresh estimate into the publisher-facing REMB
        // aggregate; emit upstream only when it crosses the hysteresis band.
        // Gated on a registered publisher (so we know who to address) — without
        // one we cannot build a routed REMB.
        if let Some(&publisher) = g.pub_owners.get(pub_mid) {
            let token = subscriber_token(subscriber);
            let emitted = g
                .remb_agg
                .entry(pub_mid.to_owned())
                .or_default()
                .update(token, estimate_bps);
            if let Some(bitrate_bps) = emitted {
                g.remb_queue.push_back(PendingRemb {
                    publisher,
                    pub_mid: pub_mid.to_owned(),
                    bitrate_bps,
                });
            }
        }
    }

    /// The subscriber's current bandwidth estimate (bps), once any REMB/TWCC
    /// feedback has been received from them.
    #[must_use]
    pub fn subscriber_estimate_bps(&self, subscriber: ParticipantId) -> Option<u64> {
        self.inner
            .lock()
            .bwe
            .get(&subscriber)
            .map(BandwidthEstimator::estimate_bps)
    }

    /// The measured EWMA throughput (bps) of one simulcast layer, once at
    /// least one measurement window has completed.
    #[must_use]
    pub fn layer_rate_bps(&self, pub_mid: &str, rid: Rid) -> Option<u64> {
        self.inner
            .lock()
            .layer_rates
            .get(&(pub_mid.to_owned(), rid))
            .and_then(ThroughputEwma::rate_bps)
    }

    /// Drain all pending keyframe requests accumulated since the last call.
    ///
    /// The caller should relay each [`PendingKeyframeRequest`] to the named
    /// publisher peer (e.g. via [`SfuPeer::request_keyframe`]).
    pub fn poll_keyframe_requests(&self) -> Vec<PendingKeyframeRequest> {
        let mut g = self.inner.lock();
        g.keyframe_gate.poll_requests().collect()
    }

    /// Periodic REMB tick: for every published track with at least one
    /// subscriber estimate, re-emit the current aggregate target toward the
    /// publisher so its encoder keeps a live REMB even when subscriber feedback
    /// is steady (no hysteresis crossing). Call this from the SFU's housekeeping
    /// timer (e.g. once per second).
    ///
    /// The enqueued [`PendingRemb`]s are drained via
    /// [`Self::poll_remb_requests`]. Tracks with no registered publisher are
    /// skipped (the REMB could not be routed).
    pub fn tick_remb(&self) {
        let mut g = self.inner.lock();
        let ForwardState {
            remb_agg,
            remb_queue,
            pub_owners,
            ..
        } = &mut *g;
        for (mid, agg) in remb_agg.iter_mut() {
            if let Some(bitrate_bps) = agg.tick() {
                if let Some(&publisher) = pub_owners.get(mid) {
                    remb_queue.push_back(PendingRemb {
                        publisher,
                        pub_mid: mid.clone(),
                        bitrate_bps,
                    });
                }
            }
        }
    }

    /// Drain all aggregated REMBs queued for publishers since the last call.
    ///
    /// Compatibility hook for the in-forwarder peer owner. Production
    /// server-owned sessions deliver REMB through [`SfuPeerSink`] instead.
    pub fn poll_remb_requests(&self) -> Vec<PendingRemb> {
        let mut g = self.inner.lock();
        g.remb_queue.drain(..).collect()
    }

    /// Run a closure with mutable access to one peer, if present. Lets the UDP
    /// task drive `poll()` / `handle_datagram()` without holding the lock itself.
    pub fn with_peer<R>(&self, id: ParticipantId, f: impl FnOnce(&mut SfuPeer) -> R) -> Option<R> {
        let mut g = self.inner.lock();
        g.peers.get_mut(&id).map(f)
    }

    /// Core fan-out: forward one inbound RTP packet from `publisher` to every
    /// subscribed peer, applying simulcast layer selection and rewriting
    /// per-subscriber seq/timestamp.
    ///
    /// **Simulcast behaviour** (when `rtp.rid` is `Some`):
    /// - If a [`LayerSelector`] is registered for a `(subscriber, pub_mid)` pair,
    ///   [`ForwardDecision`] gates the packet: only `Forward` and
    ///   `SwitchAndForward` result in delivery; `Drop` and `RequestKeyframe`
    ///   suppress it.  `RequestKeyframe` additionally enqueues a coalesced
    ///   keyframe request via the internal [`KeyframeGate`].
    /// - When `rtp.rid` is `None` (no simulcast, or RID not yet negotiated) the
    ///   layer selector is bypassed and all packets are forwarded.
    ///
    /// Returns the number of subscriber tracks the packet was written to (0 if
    /// nobody subscribes, or if subscribers haven't negotiated their outbound
    /// stream yet). Peers whose write reports "no such outbound stream" are
    /// skipped — that's normal before a subscriber finishes negotiation.
    pub fn on_rtp(&self, publisher: ParticipantId, rtp: &InboundRtp) -> usize {
        self.on_rtp_at(publisher, rtp, Instant::now(), self.now_ms())
    }

    /// Forward one packet received from a peer node through the same
    /// header-aware path as a local publisher packet.
    ///
    /// [`BridgeRtp`] retains PT, extended sequence number, RTP timestamp, SSRC,
    /// marker, MID, RID, payload, and keyframe classification. Hop-local timing
    /// and extension values are rebuilt by [`BridgeRtp::to_inbound`] before the
    /// normal remap/simulcast logic runs.
    pub fn on_bridge_rtp(&self, packet: &BridgeRtp) -> usize {
        let inbound = packet.to_inbound();
        self.on_rtp(packet.participant, &inbound)
    }

    /// Production fan-out scoped to one call.
    ///
    /// SDP MIDs such as `"0"` repeat across calls. Keeping both the route table
    /// and peer-sink lookup under `call` prevents media from one call reaching a
    /// participant session in another call.
    pub fn on_call_rtp(&self, call: CallId, publisher: ParticipantId, rtp: &InboundRtp) -> usize {
        self.on_call_rtp_at(call, publisher, rtp, self.now_ms())
    }

    /// Forward one bridge packet into the matching call-scoped production
    /// sessions.
    pub fn on_call_bridge_rtp(&self, call: CallId, packet: &BridgeRtp) -> usize {
        let inbound = packet.to_inbound();
        self.on_call_rtp(call, packet.participant, &inbound)
    }

    /// Relay PLI/FIR received on a subscriber's `out_mid` to the exact
    /// `(publisher, pub_mid)` source and its unique owner task.
    pub fn on_call_keyframe_request(
        &self,
        call: CallId,
        subscriber: ParticipantId,
        out_mid: &str,
        kind: KeyframeRequestKind,
    ) -> bool {
        let (sink, source) = {
            let inner = self.inner.lock();
            let Some(source) = inner
                .call_tables
                .get(&call)
                .and_then(|table| table.source_for(subscriber, out_mid))
            else {
                return false;
            };
            let Some(sink) = publisher_sink(&inner, call, source.publisher) else {
                return false;
            };
            (sink, source)
        };
        sink.try_request_keyframe(Mid::from(source.mid.as_str()), kind)
    }

    /// Aggregate a subscriber's REMB/TWCC estimate and queue REMB on each exact
    /// publisher owner task. REMB carries an `out_mid`; a connection-wide TWCC
    /// estimate applies to every explicitly mapped source for that subscriber.
    pub fn on_call_bandwidth_estimate(
        &self,
        call: CallId,
        subscriber: ParticipantId,
        out_mid: Option<&str>,
        bitrate_bps: u64,
    ) -> usize {
        let requests = {
            let mut inner = self.inner.lock();
            let Some(table) = inner.call_tables.get(&call) else {
                return 0;
            };
            let sources = out_mid.map_or_else(
                || table.sources_for_subscriber(subscriber),
                |mid| table.source_for(subscriber, mid).into_iter().collect(),
            );
            let token = subscriber_token(subscriber);
            let mut requests = Vec::new();
            for source in sources {
                let emitted = inner
                    .call_remb_agg
                    .entry((call, source.clone()))
                    .or_default()
                    .update(token, bitrate_bps);
                let Some(target_bps) = emitted else {
                    continue;
                };
                if let Some(sink) = publisher_sink(&inner, call, source.publisher) {
                    requests.push((sink, source.mid, target_bps));
                }
            }
            requests
        };

        let mut queued = 0;
        for (sink, mid, bitrate) in requests {
            if sink.try_request_remb(Mid::from(mid.as_str()), bitrate) {
                queued += 1;
            }
        }
        queued
    }

    fn on_call_rtp_at(
        &self,
        call: CallId,
        publisher: ParticipantId,
        rtp: &InboundRtp,
        now_ms: u64,
    ) -> usize {
        let pub_mid = rtp.mid.to_string();
        let route_mid = call_route_mid(call, publisher, &pub_mid);
        let mut g = self.inner.lock();

        if let Some(rid) = rtp.rid {
            g.layer_rates
                .entry((route_mid.clone(), rid))
                .or_default()
                .on_bytes(rtp.payload.len(), now_ms);
        }

        let ForwardState {
            peer_sinks,
            bridge_peer_sinks,
            call_tables,
            layer_table,
            ..
        } = &mut *g;
        let Some(table) = call_tables.get_mut(&call) else {
            return 0;
        };
        let targets: Vec<_> = table.targets(publisher, &pub_mid).to_vec();
        if targets.is_empty() {
            return 0;
        }

        let key = RtpKey::new(*rtp.seq_no, u64::from(rtp.rtp_time));
        let mut delivered = 0usize;
        for target in targets {
            if target.subscriber == publisher {
                continue;
            }
            if let Some(rid) = rtp.rid {
                match layer_table.decide(target.subscriber, &route_mid, rid, rtp.is_keyframe) {
                    Some(ForwardDecision::RequestKeyframe) => {
                        if let Some(sink) = peer_sinks.get(&(call, publisher)).or_else(|| {
                            bridge_peer_sinks
                                .get(&(call, publisher))
                                .and_then(|sinks| sinks.values().next_back())
                        }) {
                            let _ = sink.try_request_keyframe(
                                Mid::from(pub_mid.as_str()),
                                KeyframeRequestKind::Pli,
                            );
                        }
                        continue;
                    }
                    Some(ForwardDecision::Drop) => continue,
                    Some(ForwardDecision::Forward | ForwardDecision::SwitchAndForward) | None => {}
                }
            }

            let Some(remapped) = table.remap_for(target.subscriber, &target.out_mid, key) else {
                continue;
            };
            #[allow(clippy::cast_possible_truncation)]
            let wire_ts = (remapped.ts & 0xFFFF_FFFF) as u32;
            let outbound_mid = str0m::media::Mid::from(target.out_mid.as_str());
            let Some(sink) = peer_sinks.get(&(call, target.subscriber)) else {
                continue;
            };
            let mut outbound = rtp.clone();
            outbound.mid = outbound_mid;
            outbound.ext_vals.mid = Some(outbound_mid);
            outbound.seq_no = remapped.seq.into();
            outbound.rtp_time = wire_ts;
            if sink.try_write_rtp(outbound) {
                delivered += 1;
            } else {
                trace!(
                    %call,
                    subscriber = %target.subscriber,
                    mid = %target.out_mid,
                    "SFU session command queue unavailable; dropping RTP"
                );
            }
        }
        delivered
    }

    /// Time-injected body of [`Self::on_rtp`].
    fn on_rtp_at(
        &self,
        publisher: ParticipantId,
        rtp: &InboundRtp,
        now: Instant,
        now_ms: u64,
    ) -> usize {
        let pub_mid = rtp.mid.to_string();
        let mut g = self.inner.lock();

        // Per-layer throughput measurement runs before the subscriber loop
        // (and before the empty-targets early return) so rates exist by the
        // time the first subscriber's bandwidth feedback consults them.
        if let Some(rid) = rtp.rid {
            g.layer_rates
                .entry((pub_mid.clone(), rid))
                .or_default()
                .on_bytes(rtp.payload.len(), now_ms);
        }

        let ForwardState {
            peers,
            table,
            layer_table,
            keyframe_gate,
            pub_owners,
            ..
        } = &mut *g;

        // Snapshot targets first (cheap clone of small Vec) so we can borrow the
        // remapper and the subscriber peer mutably without aliasing the table.
        let targets: Vec<_> = table.targets(&pub_mid).to_vec();
        if targets.is_empty() {
            return 0;
        }

        let key = RtpKey::new(*rtp.seq_no, u64::from(rtp.rtp_time));
        let mut delivered = 0usize;

        for t in targets {
            // A sendrecv participant subscribes to the common call mids too;
            // never echo its own publisher packet back onto its browser leg.
            if t.subscriber == publisher {
                continue;
            }
            // ── Simulcast layer gate ──────────────────────────────────────────
            // When the inbound packet carries a RID, consult the selector for
            // this (subscriber, pub_mid) pair.  The selector is absent for
            // non-simulcast tracks (decide() returns None), in which case we
            // forward unconditionally.
            if let Some(rid) = rtp.rid {
                match layer_table.decide(t.subscriber, &pub_mid, rid, rtp.is_keyframe) {
                    Some(ForwardDecision::RequestKeyframe) => {
                        // Enqueue a coalesced keyframe request to the publisher.
                        if let Some(&publisher) = pub_owners.get(&pub_mid) {
                            keyframe_gate.on_layer_switch(publisher, &pub_mid, now);
                        }
                        continue; // drop this packet for this subscriber
                    }
                    Some(ForwardDecision::Drop) => continue,
                    // Forward, SwitchAndForward, or no selector → fall through.
                    Some(ForwardDecision::Forward | ForwardDecision::SwitchAndForward) | None => {}
                }
            }

            // ── Seq / timestamp remap ─────────────────────────────────────────
            let Some(remapped) = table.remap_for(t.subscriber, &t.out_mid, key) else {
                continue;
            };
            // Wire RTP timestamps are 32-bit and wrap by design; the low 32 bits
            // of the remapped value are exactly the wire timestamp. str0m's
            // outbound `SeqNo` similarly wraps from the extended `u64`.
            #[allow(clippy::cast_possible_truncation)]
            let wire_ts = (remapped.ts & 0xFFFF_FFFF) as u32;
            let outbound_mid = str0m::media::Mid::from(t.out_mid.as_str());
            let Some(peer) = peers.get_mut(&t.subscriber) else {
                continue;
            };
            match peer.write_rtp(
                outbound_mid,
                rtp.pt,
                remapped.seq.into(),
                wire_ts,
                rtp.wallclock,
                rtp.marker,
                rtp.ext_vals.clone(),
                rtp.payload.clone(),
            ) {
                Ok(true) => delivered += 1,
                Ok(false) => {
                    trace!(subscriber = %t.subscriber, mid = %t.out_mid, "no outbound stream yet");
                }
                Err(e) => trace!(error = %e, "subscriber write_rtp failed"),
            }
        }
        delivered
    }

    /// Access the shared router (call membership / subscription topology).
    #[must_use]
    pub fn router(&self) -> &SfuRouter {
        &self.router
    }

    /// Number of live peers currently registered.
    #[must_use]
    pub fn peer_count(&self) -> usize {
        let inner = self.inner.lock();
        inner.peers.len() + inner.peer_sinks.len()
    }
}

impl ForwardState {
    /// Re-evaluate the bandwidth-driven simulcast layer choice for one
    /// subscriber after fresh REMB/TWCC feedback.
    ///
    /// Builds one candidate per distinct [`LayerKind`] (low→high) — each
    /// represented by the concrete layer
    /// [`LayerSet::select`](crate::simulcast::LayerSet::select) would resolve
    /// that kind to — paired with that layer's measured throughput, then asks
    /// the subscriber's [`LayerSwitchPolicy`] whether to move. A positive
    /// decision goes through the existing keyframe-gated
    /// [`LayerSelectorTable::select_layer`] mechanics and enqueues a coalesced
    /// keyframe request toward the publisher.
    fn adapt_subscriber(
        &mut self,
        subscriber: ParticipantId,
        pub_mid: &str,
        now: Instant,
        now_ms: u64,
    ) {
        let Some(estimate_bps) = self
            .bwe
            .get(&subscriber)
            .map(BandwidthEstimator::estimate_bps)
        else {
            return;
        };
        let Some(layer_set) = self.layer_table.layer_set(pub_mid) else {
            return; // non-simulcast track (or publisher not registered yet)
        };
        if layer_set.is_empty() {
            return;
        }

        // (kind, measured rate) low→high; one entry per distinct kind, keeping
        // the highest temporal sub-layer (what `select(kind)` resolves to).
        let mut rates: Vec<(LayerKind, Option<u64>)> = Vec::new();
        for layer in layer_set.layers() {
            if rates.last().is_some_and(|(k, _)| *k == layer.kind) {
                rates.pop();
            }
            let rate = self
                .layer_rates
                .get(&(pub_mid.to_owned(), layer.rid))
                .and_then(ThroughputEwma::rate_bps);
            rates.push((layer.kind, rate));
        }

        // The layer the subscriber is on (or already heading to mid-switch).
        let current = self
            .layer_table
            .current_rid(subscriber, pub_mid)
            .and_then(|rid| {
                layer_set
                    .layers()
                    .iter()
                    .find(|l| l.rid == rid)
                    .map(|l| l.kind)
            });

        let policy = self
            .adapt
            .entry((subscriber, pub_mid.to_owned()))
            .or_default();
        let Some(target) = policy.decide(current, estimate_bps, &rates, now_ms) else {
            return;
        };

        if self
            .layer_table
            .select_layer(subscriber, pub_mid, target)
            .is_some()
        {
            trace!(
                %subscriber,
                pub_mid,
                ?target,
                estimate_bps,
                "bandwidth-driven simulcast layer switch"
            );
            if let Some(&publisher) = self.pub_owners.get(pub_mid) {
                self.keyframe_gate.on_layer_switch(publisher, pub_mid, now);
            }
        }
    }
}

#[cfg(test)]
mod production_tests;
#[cfg(test)]
pub mod tests;
