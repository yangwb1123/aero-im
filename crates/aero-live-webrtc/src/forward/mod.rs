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
//! **Infra-seam boundary**: the aggregation + [`crate::rtcp_fb::encode_remb`]
//! ENCODE + ENQUEUE built here is fully unit-tested. The drained
//! [`PendingRemb`] still has to be written onto the publisher peer's outbound
//! RTCP stream over real DTLS-SRTP to reach an OBS/browser encoder — that wire
//! egress is the documented infra seam (mirrors how [`PendingKeyframeRequest`]s
//! from [`SfuForwarder::poll_keyframe_requests`] are relayed upstream).
//!
//! The peer's UDP loop (out of scope) feeds [`SfuForwarder::on_rtp`] from its
//! [`crate::peer::PeerProgress::Media`] branch and flushes each touched peer's
//! `poll()` afterwards to emit the resulting datagrams.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use aero_common::{CallId, ParticipantId};
use parking_lot::Mutex;
use str0m::media::Rid;
use tracing::trace;

use crate::bwe::{BandwidthEstimator, LayerSwitchPolicy, ThroughputEwma};
use crate::peer::{InboundRtp, SfuPeer};
use crate::remap::{ForwardTable, RtpKey};
use crate::rtcp_fb::{parse_bandwidth_feedback, BandwidthFeedback, PublisherRembAggregator};
use crate::rtcp_feedback::{parse_keyframe_requests, KeyframeGate, PendingKeyframeRequest};
use crate::simulcast::{ForwardDecision, LayerKind, LayerSet, LayerSelectorTable};
use crate::SfuRouter;

/// An aggregated REMB the SFU must relay to a publisher so it can cap its
/// encoder to what the slowest subscriber can receive.
///
/// Drained via [`SfuForwarder::poll_remb_requests`]; the server's UDP loop
/// encodes it with [`crate::rtcp_fb::encode_remb`] (sender SSRC = the SFU's,
/// `ssrcs` = the publisher's media SSRC) and writes it onto the publisher
/// peer's outbound RTCP stream — the documented DTLS-SRTP wire seam.
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
        self.inner.lock().peers.insert(peer.id(), peer);
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
    /// The caller encodes each [`PendingRemb`] with
    /// [`crate::rtcp_fb::encode_remb`] and writes it onto the publisher peer's
    /// outbound RTCP stream (the DTLS-SRTP wire egress — the documented infra
    /// seam), mirroring how [`Self::poll_keyframe_requests`] results are relayed.
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

    /// Core fan-out: forward one inbound RTP packet from `_publisher` to every
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

    /// Time-injected body of [`Self::on_rtp`].
    fn on_rtp_at(
        &self,
        _publisher: ParticipantId,
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
            let Some(peer) = peers.get_mut(&t.subscriber) else {
                continue;
            };
            // Wire RTP timestamps are 32-bit and wrap by design; the low 32 bits
            // of the remapped value are exactly the wire timestamp. str0m's
            // outbound `SeqNo` similarly wraps from the extended `u64`.
            #[allow(clippy::cast_possible_truncation)]
            let wire_ts = (remapped.ts & 0xFFFF_FFFF) as u32;
            match peer.write_rtp(
                rtp.mid,
                rtp.pt,
                remapped.seq.into(),
                wire_ts,
                rtp.wallclock,
                rtp.marker,
                rtp.ext_vals.clone(),
                rtp.payload.clone(),
            ) {
                Ok(true) => delivered += 1,
                Ok(false) => trace!(subscriber = %t.subscriber, mid = %t.out_mid, "no outbound stream yet"),
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
        self.inner.lock().peers.len()
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

/// Bridge the synchronous fan-out to a hypothetical higher-level call that only
/// has the (legacy) `forward_rtp(call, mid, Bytes)` shape. This adapter is kept
/// minimal: the real entry point is [`SfuForwarder::on_rtp`], which carries the
/// parsed header fields the legacy `Bytes`-only signature lacks.
#[async_trait::async_trait]
impl crate::MediaForwarder for SfuForwarder {
    async fn forward_rtp(&self, call: CallId, mid: &str, _packet: bytes::Bytes) {
        // The legacy signature lacks parsed RTP header fields (pt/seq/ts), which
        // real forwarding needs. We can still surface the routing decision so a
        // caller wired to this trait observes the fan-out fanout count.
        let subs = self.router.subscribers_for(call, mid);
        trace!(
            %call,
            mid,
            subscribers = subs.len(),
            "forward_rtp(legacy): use SfuForwarder::on_rtp for real RTP fan-out"
        );
    }
}


#[cfg(test)]
pub mod tests;
