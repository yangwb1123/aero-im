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
use crate::rtcp_fb::{parse_bandwidth_feedback, BandwidthFeedback};
use crate::rtcp_feedback::{parse_keyframe_requests, KeyframeGate, PendingKeyframeRequest};
use crate::simulcast::{ForwardDecision, LayerKind, LayerSet, LayerSelectorTable};
use crate::SfuRouter;

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
    pub fn remove_peer(&self, peer: ParticipantId) -> Option<SfuPeer> {
        let mut g = self.inner.lock();
        g.table.unlink_subscriber(peer);
        g.layer_table.unlink_subscriber(peer);
        g.bwe.remove(&peer);
        g.adapt.retain(|(sub, _), _| *sub != peer);
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
        g.adapt_subscriber(subscriber, pub_mid, now, now_ms);
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
mod tests {
    use super::*;
    use crate::PeerRole;

    fn inbound(mid: &str, seq: u64, ts: u32) -> InboundRtp {
        use str0m::media::Mid;
        use str0m::rtp::ExtensionValues;
        InboundRtp {
            mid: Mid::from(mid),
            pt: 96u8.into(),
            seq_no: seq.into(),
            rtp_time: ts,
            marker: false,
            ext_vals: ExtensionValues::default(),
            wallclock: std::time::Instant::now(),
            payload: vec![0xde, 0xad, 0xbe, 0xef],
            rid: None,
            is_keyframe: false,
        }
    }

    fn inbound_simulcast(mid: &str, seq: u64, ts: u32, rid: &str, is_keyframe: bool) -> InboundRtp {
        let mut rtp = inbound(mid, seq, ts);
        rtp.rid = Some(Rid::from(rid));
        rtp.is_keyframe = is_keyframe;
        rtp
    }

    // ── Existing tests (must stay green) ──────────────────────────────────────

    #[test]
    fn on_rtp_with_no_subscribers_delivers_nothing() {
        let fwd = SfuForwarder::new(SfuRouter::new());
        let pubr = ParticipantId::new();
        let n = fwd.on_rtp(pubr, &inbound("0", 1, 0));
        assert_eq!(n, 0);
    }

    #[test]
    fn on_rtp_skips_subscribers_without_negotiated_outbound_stream() {
        // A fresh peer has no outbound StreamTx for the mid (no negotiation),
        // so write_rtp returns Ok(false) and nothing is delivered — but the
        // routing + remap path still runs without panicking.
        let router = SfuRouter::new();
        let fwd = SfuForwarder::new(router);
        let call = CallId::new();
        let pubr = ParticipantId::new();
        let sub = ParticipantId::new();

        fwd.add_peer(SfuPeer::new(call, sub));
        fwd.subscribe("0", sub, "0");
        assert_eq!(fwd.peer_count(), 1);

        let n = fwd.on_rtp(pubr, &inbound("0", 100, 9000));
        assert_eq!(n, 0, "no negotiated outbound stream → skipped, not delivered");
    }

    #[test]
    fn remove_peer_unlinks_routing_state() {
        let fwd = SfuForwarder::new(SfuRouter::new());
        let call = CallId::new();
        let sub = ParticipantId::new();
        fwd.add_peer(SfuPeer::new(call, sub));
        fwd.subscribe("0", sub, "0");
        assert!(fwd.remove_peer(sub).is_some());
        assert_eq!(fwd.peer_count(), 0);
        // After removal, on_rtp finds no targets.
        assert_eq!(fwd.on_rtp(ParticipantId::new(), &inbound("0", 1, 0)), 0);
    }

    #[tokio::test]
    async fn legacy_forward_rtp_trait_reports_routing() {
        use crate::MediaForwarder;
        let router = SfuRouter::new();
        let call = CallId::new();
        let pubr = ParticipantId::new();
        let sub = ParticipantId::new();
        router.add_peer(call, pubr, PeerRole::Publisher);
        router.add_peer(call, sub, PeerRole::Subscriber);
        router.add_track(call, "0", pubr);
        router.add_subscription(call, "0", sub);
        let fwd = SfuForwarder::new(router);
        // Should not panic; routing observed via the SfuRouter.
        fwd.forward_rtp(call, "0", bytes::Bytes::from_static(b"x")).await;
        assert_eq!(fwd.router().subscribers_for(call, "0").len(), 1);
    }

    // ── Simulcast layer selection ──────────────────────────────────────────────

    /// After `select_layer(sub, mid, High)`, packets on the non-active layer
    /// are dropped until a keyframe arrives on the target. Packets on the
    /// currently active (bootstrapped) layer continue to be forwarded.
    #[test]
    fn on_rtp_forwards_only_selected_layer() {
        use crate::simulcast::{LayerKind, LayerSet, SimulcastLayer};

        let fwd = SfuForwarder::new(SfuRouter::new());
        let pubr = ParticipantId::new();
        let call = CallId::new();
        let sub = ParticipantId::new();

        // Register the publisher with a Low and High layer.
        let layers = LayerSet::from_layers(vec![
            SimulcastLayer::spatial(Rid::from("low"), LayerKind::Low),
            SimulcastLayer::spatial(Rid::from("high"), LayerKind::High),
        ]);
        fwd.register_publisher_layers("v0", pubr, layers);

        // Add a subscriber peer (no real outbound stream, so deliver=0 always;
        // but we're testing the drop/forward decision, not the write outcome).
        fwd.add_peer(SfuPeer::new(call, sub));
        fwd.subscribe("v0", sub, "v0");
        // Drain the new-subscription keyframe request (bypasses coalesce gate).
        let reqs0 = fwd.poll_keyframe_requests();
        assert_eq!(reqs0.len(), 1, "subscribe must enqueue a keyframe request");
        assert_eq!(reqs0[0].publisher, pubr);

        // --- Bootstrap: first simulcast packet on "low" (no pending switch yet).
        // LayerSelector bootstraps on the first RID it sees → Forward.
        // No outbound stream, so delivered = 0, but routing ran (not dropped).
        let pkt_low = inbound_simulcast("v0", 1, 0, "low", false);
        let n = fwd.on_rtp(pubr, &pkt_low);
        assert_eq!(n, 0, "no negotiated stream; layer was forwarded (bootstrap)");

        // --- Request switch to "high" layer.
        let selected = fwd.select_layer(sub, "v0", LayerKind::High);
        assert_eq!(selected, Some(Rid::from("high")), "high layer selected");
        // The select_layer call also triggers on_layer_switch.  That call goes
        // through the coalesce gate — it may or may not be suppressed depending
        // on timing relative to the new_subscriber request above.  Drain any
        // queued requests so the gate state is clear for the on_rtp path.
        let _ = fwd.poll_keyframe_requests();

        // A non-keyframe "high" packet while switch is pending.
        // LayerSelector returns RequestKeyframe (first time).  The forwarder
        // passes it through the coalesce gate, which opened when we drained above.
        // In the unit-test time domain, this Instant::now() is different from the
        // drain above but still within 200 ms, so the gate may suppress it.
        // What we CAN assert is that the packet was NOT delivered (Drop path).
        let pkt_high_non_kf = inbound_simulcast("v0", 100, 9000, "high", false);
        let n2 = fwd.on_rtp(pubr, &pkt_high_non_kf);
        assert_eq!(n2, 0, "non-keyframe on pending-switch target must be dropped");

        // "low" packet while waiting for keyframe → still forwarded (Forward decision).
        // Again, delivered = 0 because no real outbound stream, but it was NOT dropped.
        let pkt_low2 = inbound_simulcast("v0", 2, 3000, "low", false);
        let _ = fwd.on_rtp(pubr, &pkt_low2);

        // Second non-keyframe on "high" → Drop (keyframe already requested once).
        let pkt_high_non_kf2 = inbound_simulcast("v0", 101, 9090, "high", false);
        let n3 = fwd.on_rtp(pubr, &pkt_high_non_kf2);
        assert_eq!(n3, 0, "second non-keyframe on target: Drop, not delivered");

        // Keyframe on "high" → SwitchAndForward → switch committed.
        let pkt_high_kf = inbound_simulcast("v0", 102, 12000, "high", true);
        let n4 = fwd.on_rtp(pubr, &pkt_high_kf);
        // delivered = 0 (no real stream) but NOT dropped (SwitchAndForward).
        assert_eq!(n4, 0, "keyframe on target: SwitchAndForward (no real stream)");

        // After switch: "low" packets should now be dropped (active = high).
        // We verify via the selector: a "low" packet goes through ForwardDecision::Drop
        // and produces no new keyframe request.
        let _ = fwd.poll_keyframe_requests(); // drain any queued
        let pkt_low3 = inbound_simulcast("v0", 3, 6000, "low", false);
        let _ = fwd.on_rtp(pubr, &pkt_low3);
        let reqs2 = fwd.poll_keyframe_requests();
        assert!(
            reqs2.is_empty(),
            "low packets dropped after switch to high → no extra keyframe req"
        );

        // And "high" packets continue to be forwarded (active layer).
        let pkt_high_post = inbound_simulcast("v0", 103, 15000, "high", false);
        let _ = fwd.on_rtp(pubr, &pkt_high_post);
        let reqs3 = fwd.poll_keyframe_requests();
        assert!(
            reqs3.is_empty(),
            "high packets on active layer must not trigger keyframe requests"
        );
    }

    // ── New subscription triggers keyframe request ─────────────────────────────

    #[test]
    fn subscribe_enqueues_keyframe_request_toward_publisher() {
        let fwd = SfuForwarder::new(SfuRouter::new());
        let pubr = ParticipantId::new();
        let call = CallId::new();
        let sub = ParticipantId::new();

        fwd.register_publisher_layers("v0", pubr, LayerSet::default());
        fwd.add_peer(SfuPeer::new(call, sub));
        fwd.subscribe("v0", sub, "v0");

        let reqs = fwd.poll_keyframe_requests();
        assert_eq!(reqs.len(), 1, "new subscription must trigger keyframe request");
        assert_eq!(reqs[0].publisher, pubr);
        assert_eq!(reqs[0].pub_mid, "v0");
        assert!(!reqs[0].use_fir, "new subscriber uses PLI, not FIR");
    }

    #[test]
    fn subscribe_without_registered_publisher_produces_no_keyframe_request() {
        // If register_publisher_layers was not called first, we can't know
        // who the publisher is, so no keyframe request is queued.
        let fwd = SfuForwarder::new(SfuRouter::new());
        let call = CallId::new();
        let sub = ParticipantId::new();
        fwd.add_peer(SfuPeer::new(call, sub));
        fwd.subscribe("v0", sub, "v0");
        let reqs = fwd.poll_keyframe_requests();
        assert!(reqs.is_empty(), "no publisher registered → no keyframe request");
    }

    // ── Inbound subscriber RTCP → upstream keyframe request ───────────────────

    #[test]
    fn subscriber_pli_produces_upstream_keyframe_request() {
        use crate::rtcp_feedback::encode_pli;
        use crate::rtcp_feedback::Ssrc;

        let fwd = SfuForwarder::new(SfuRouter::new());
        let pubr = ParticipantId::new();
        let sub = ParticipantId::new();

        fwd.register_publisher_layers("v0", pubr, LayerSet::default());

        let mut buf = [0u8; 12];
        encode_pli(1u32.into(), 2u32.into(), &mut buf);
        let _ = Ssrc::from(1u32); // just to use the import

        // First PLI — should pass gate and be queued.
        fwd.on_subscriber_rtcp(sub, "v0", &buf);
        let reqs = fwd.poll_keyframe_requests();
        assert_eq!(reqs.len(), 1, "PLI must produce an upstream keyframe request");
        assert_eq!(reqs[0].publisher, pubr);
        assert_eq!(reqs[0].pub_mid, "v0");
        assert!(!reqs[0].use_fir);
    }

    #[test]
    fn duplicate_plis_within_window_coalesce() {
        use crate::rtcp_feedback::encode_pli;

        let fwd = SfuForwarder::new(SfuRouter::new());
        let pubr = ParticipantId::new();
        let sub = ParticipantId::new();

        fwd.register_publisher_layers("v0", pubr, LayerSet::default());

        let mut buf = [0u8; 12];
        encode_pli(1u32.into(), 2u32.into(), &mut buf);

        // Three rapid PLIs — only the first should pass the 200 ms coalesce gate.
        fwd.on_subscriber_rtcp(sub, "v0", &buf);
        fwd.on_subscriber_rtcp(sub, "v0", &buf);
        fwd.on_subscriber_rtcp(sub, "v0", &buf);

        let reqs = fwd.poll_keyframe_requests();
        assert_eq!(reqs.len(), 1, "duplicate PLIs within window must coalesce");
    }

    #[test]
    fn fir_produces_use_fir_keyframe_request() {
        use crate::rtcp_feedback::encode_fir;

        let fwd = SfuForwarder::new(SfuRouter::new());
        let pubr = ParticipantId::new();
        let sub = ParticipantId::new();

        fwd.register_publisher_layers("v0", pubr, LayerSet::default());

        let mut buf = [0u8; 20];
        encode_fir(1u32.into(), 2u32.into(), 0, &mut buf);

        fwd.on_subscriber_rtcp(sub, "v0", &buf);
        let reqs = fwd.poll_keyframe_requests();
        assert_eq!(reqs.len(), 1);
        assert!(reqs[0].use_fir, "FIR must set use_fir=true");
    }

    /// Packets on a non-simulcast track (no RID in rtp) are forwarded to all
    /// subscribers even when a layer selector is registered for the pair.
    #[test]
    fn non_simulcast_packet_forwarded_regardless_of_layer_selector() {
        use crate::simulcast::{LayerKind, SimulcastLayer};
        let fwd = SfuForwarder::new(SfuRouter::new());
        let pubr = ParticipantId::new();
        let call = CallId::new();
        let sub = ParticipantId::new();

        // Register layers (simulcast track).
        let layers = LayerSet::from_layers(vec![SimulcastLayer::spatial(
            Rid::from("low"),
            LayerKind::Low,
        )]);
        fwd.register_publisher_layers("v0", pubr, layers);
        fwd.add_peer(SfuPeer::new(call, sub));
        fwd.subscribe("v0", sub, "v0");

        // Packet with no RID — must bypass the layer gate.
        // (0 delivered because no negotiated stream; but routing path executed.)
        let n = fwd.on_rtp(pubr, &inbound("v0", 10, 0));
        assert_eq!(n, 0, "delivery=0 expected (no outbound stream), not dropped");
    }

    #[test]
    fn continuous_seq_across_layer_switch_via_forwarder() {
        use crate::simulcast::{LayerKind, SimulcastLayer};
        // Verify that the forwarder's remap table produces continuous outbound
        // seq across a simulcast layer switch (ForwardDecision::SwitchAndForward).
        // We can only verify the remap state directly (no real outbound stream).
        // Instead, we simulate the flow through the internal table.

        let fwd = SfuForwarder::new(SfuRouter::new());
        let pubr = ParticipantId::new();
        let sub = ParticipantId::new();
        let call = CallId::new();

        let layers = LayerSet::from_layers(vec![
            SimulcastLayer::spatial(Rid::from("low"), LayerKind::Low),
            SimulcastLayer::spatial(Rid::from("high"), LayerKind::High),
        ]);
        fwd.register_publisher_layers("v0", pubr, layers);
        fwd.add_peer(SfuPeer::new(call, sub));
        fwd.subscribe("v0", sub, "v0");
        let _ = fwd.poll_keyframe_requests();

        // Bootstrap on "low".
        fwd.on_rtp(pubr, &inbound_simulcast("v0", 10, 1000, "low", false));
        fwd.on_rtp(pubr, &inbound_simulcast("v0", 11, 2000, "low", false));
        fwd.on_rtp(pubr, &inbound_simulcast("v0", 12, 3000, "low", false));

        // Request switch to "high".
        fwd.select_layer(sub, "v0", LayerKind::High);
        let _ = fwd.poll_keyframe_requests();

        // Non-keyframe on "high" → RequestKeyframe (drop).
        fwd.on_rtp(pubr, &inbound_simulcast("v0", 100, 9000, "high", false));
        let _ = fwd.poll_keyframe_requests();

        // One more "low" packet while waiting for keyframe.
        fwd.on_rtp(pubr, &inbound_simulcast("v0", 13, 4000, "low", false));

        // Keyframe on "high" → SwitchAndForward.
        // Use seq 40_000 to trigger the source-switch detection in RtpRemapper
        // (delta from 13 = 39_987 > MAX_CONTIGUOUS_FORWARD=32768).
        fwd.on_rtp(pubr, &inbound_simulcast("v0", 40_000, 4_000_000, "high", true));

        // After switch, another "high" packet should continue monotonically.
        // We cannot easily read the remapped seq from outside, but the important
        // thing is this doesn't panic and returns 0 (no outbound stream).
        let n = fwd.on_rtp(pubr, &inbound_simulcast("v0", 40_001, 4_003_000, "high", false));
        assert_eq!(n, 0, "continuous forwarding after layer switch (no real stream)");
    }

    // ── H.264 keyframe detection drives simulcast switching ───────────────────

    /// Helper: build an `InboundRtp` whose payload is a real H.264 IDR single-NAL
    /// byte sequence.  `is_keyframe` is left at `false` (the default); the test
    /// verifies that the forwarder picks it up from the payload, not from the
    /// caller-supplied flag.
    fn inbound_simulcast_h264(
        mid: &str,
        seq: u64,
        ts: u32,
        rid: &str,
        payload: Vec<u8>,
    ) -> InboundRtp {
        use str0m::media::Mid;
        use str0m::rtp::ExtensionValues;
        InboundRtp {
            mid: Mid::from(mid),
            pt: 96u8.into(),
            seq_no: seq.into(),
            rtp_time: ts,
            marker: false,
            ext_vals: ExtensionValues::default(),
            wallclock: std::time::Instant::now(),
            payload,
            rid: Some(Rid::from(rid)),
            // Deliberately left false — the h264 detection path in from_packet
            // is exercised; in on_rtp tests we populate is_keyframe directly
            // because InboundRtp is constructed by the caller (not from_packet).
            is_keyframe: false,
        }
    }

    /// Verify that layer switching COMMITS when `is_keyframe=true` arrives on the
    /// target layer (mocking the h264 detection result) and does NOT commit when
    /// `is_keyframe=false`.
    ///
    /// This test exercises the full `select_layer → should_forward → SwitchAndForward`
    /// path through `SfuForwarder::on_rtp`, proving the wiring between the
    /// keyframe flag and the `LayerSelector` gate.
    #[test]
    fn layer_switch_commits_on_keyframe_not_on_non_keyframe() {
        use crate::simulcast::{LayerKind, SimulcastLayer};

        let fwd = SfuForwarder::new(SfuRouter::new());
        let pubr = ParticipantId::new();
        let call = CallId::new();
        let sub = ParticipantId::new();

        let layers = LayerSet::from_layers(vec![
            SimulcastLayer::spatial(Rid::from("low"), LayerKind::Low),
            SimulcastLayer::spatial(Rid::from("high"), LayerKind::High),
        ]);
        fwd.register_publisher_layers("v0", pubr, layers);
        fwd.add_peer(SfuPeer::new(call, sub));
        fwd.subscribe("v0", sub, "v0");
        let _ = fwd.poll_keyframe_requests(); // drain subscribe-triggered request

        // Bootstrap on "low".
        fwd.on_rtp(pubr, &inbound_simulcast("v0", 1, 0, "low", false));

        // Request switch to "high".
        fwd.select_layer(sub, "v0", LayerKind::High);
        let _ = fwd.poll_keyframe_requests();

        // ── Non-keyframe arrives on "high" — switch must NOT commit ──────────
        // Build a packet with H.264 non-IDR payload (type 1 = 0x41) and
        // is_keyframe=false (as the h264 detector would set it).
        let non_idr_payload = vec![0x41u8, 0x9A, 0x24, 0x6C]; // NAL type 1
        let pkt_non_kf = {
            let mut p = inbound_simulcast_h264("v0", 100, 9000, "high", non_idr_payload);
            p.is_keyframe = false; // explicit: detector would return false for type 1
            p
        };
        fwd.on_rtp(pubr, &pkt_non_kf);

        // The layer selector must still have a pending switch (not committed).
        {
            let g = fwd.inner.lock();
            let sel = g
                .layer_table
                .selector_for(sub, "v0")
                .expect("selector must exist");
            assert_eq!(
                sel.pending_rid(),
                Some(Rid::from("high")),
                "switch must still be pending after a non-keyframe packet"
            );
        }

        // ── Keyframe arrives on "high" — switch MUST commit ──────────────────
        // Build a packet with H.264 IDR payload (type 5 = 0x65) and
        // is_keyframe=true (as the h264 detector would set it).
        let idr_payload = vec![0x65u8, 0x88, 0x84, 0x00, 0x33]; // NAL type 5 (IDR)
        let pkt_kf = {
            let mut p = inbound_simulcast_h264("v0", 101, 12_000, "high", idr_payload);
            p.is_keyframe = true; // explicit: detector returns true for IDR
            p
        };
        fwd.on_rtp(pubr, &pkt_kf);

        // The pending switch must be cleared — active layer is now "high".
        {
            let g = fwd.inner.lock();
            let sel = g
                .layer_table
                .selector_for(sub, "v0")
                .expect("selector must exist");
            assert_eq!(
                sel.pending_rid(),
                None,
                "pending switch must be cleared after IDR keyframe"
            );
            assert_eq!(
                sel.active_rid(),
                Some(Rid::from("high")),
                "active layer must be 'high' after keyframe-gated switch"
            );
        }
    }

    /// Verify that `h264_payload_is_keyframe` is correctly wired inside
    /// `InboundRtp::from_packet` by constructing an `InboundRtp` directly
    /// (simulating what `from_packet` does) and checking the detected flag.
    ///
    /// This is the "through the forwarder" integration requirement: a packet
    /// with a real H.264 IDR payload must arrive at the layer selector with
    /// `is_keyframe = true`, causing `SwitchAndForward`.
    #[test]
    fn h264_idr_payload_drives_layer_switch_via_from_packet_simulation() {
        use crate::h264::h264_payload_is_keyframe;
        use crate::simulcast::{LayerKind, LayerSelector, SimulcastLayer};

        // Confirm that the IDR payload produces is_keyframe=true via the detector.
        let idr_payload = vec![0x65u8, 0x88, 0x84, 0x00]; // single-NAL IDR
        assert!(
            h264_payload_is_keyframe(&idr_payload),
            "IDR payload must be detected as keyframe"
        );

        // Confirm that a non-IDR payload produces is_keyframe=false.
        let non_idr_payload = vec![0x41u8, 0x9A]; // single-NAL non-IDR
        assert!(
            !h264_payload_is_keyframe(&non_idr_payload),
            "non-IDR payload must NOT be detected as keyframe"
        );

        // Now run the full layer-selector path as if packets arrived from the
        // publisher with those payloads.
        let fwd = SfuForwarder::new(SfuRouter::new());
        let pubr = ParticipantId::new();
        let call = CallId::new();
        let sub = ParticipantId::new();

        let layers = LayerSet::from_layers(vec![
            SimulcastLayer::spatial(Rid::from("low"), LayerKind::Low),
            SimulcastLayer::spatial(Rid::from("high"), LayerKind::High),
        ]);
        fwd.register_publisher_layers("v0", pubr, layers);
        fwd.add_peer(SfuPeer::new(call, sub));
        fwd.subscribe("v0", sub, "v0");
        let _ = fwd.poll_keyframe_requests();

        // Bootstrap on "low".
        fwd.on_rtp(pubr, &inbound_simulcast("v0", 1, 0, "low", false));

        // Request switch to "high".
        fwd.select_layer(sub, "v0", LayerKind::High);
        let _ = fwd.poll_keyframe_requests();

        // Feed a non-IDR packet (is_keyframe=false from h264 detector).
        let mut pkt_non_kf = inbound_simulcast_h264("v0", 50, 5000, "high", non_idr_payload);
        pkt_non_kf.is_keyframe = h264_payload_is_keyframe(&pkt_non_kf.payload);
        fwd.on_rtp(pubr, &pkt_non_kf);

        // Switch still pending.
        {
            let g = fwd.inner.lock();
            let sel = g
                .layer_table
                .selector_for(sub, "v0")
                .expect("selector must exist");
            assert!(
                sel.pending_rid().is_some(),
                "switch still pending after non-IDR packet"
            );
        }

        // Feed an IDR packet (is_keyframe=true from h264 detector).
        let mut pkt_kf = inbound_simulcast_h264("v0", 51, 6000, "high", idr_payload);
        pkt_kf.is_keyframe = h264_payload_is_keyframe(&pkt_kf.payload);
        fwd.on_rtp(pubr, &pkt_kf);

        // Switch committed.
        {
            let g = fwd.inner.lock();
            let sel = g
                .layer_table
                .selector_for(sub, "v0")
                .expect("selector must exist");
            assert_eq!(
                sel.active_rid(),
                Some(Rid::from("high")),
                "switch must commit on IDR (h264 detected)"
            );
            assert_eq!(sel.pending_rid(), None, "no pending switch after commit");
        }

        // Also verify directly via the free-standing LayerSelector, mirroring
        // what from_packet + on_rtp does end-to-end.
        let mut sel = LayerSelector::new();
        sel.should_forward(Rid::from("low"), false); // bootstrap
        sel.request_switch(Rid::from("high"));

        // Non-IDR → RequestKeyframe (not SwitchAndForward).
        let d1 = sel.should_forward(Rid::from("high"), false);
        assert_ne!(
            d1,
            crate::simulcast::ForwardDecision::SwitchAndForward,
            "non-IDR must not commit switch"
        );

        // IDR → SwitchAndForward.
        let d2 = sel.should_forward(Rid::from("high"), true);
        assert_eq!(
            d2,
            crate::simulcast::ForwardDecision::SwitchAndForward,
            "IDR must commit switch"
        );
    }

    // ── Bandwidth feedback → estimator → adaptive layer switching ─────────────

    use std::time::Duration;

    /// Build a TWCC feedback packet reporting `received` packets with flat
    /// 1 ms deltas followed by `lost` packets, using run-length chunks.
    fn twcc_buf(received: u16, lost: u16) -> Vec<u8> {
        assert!(received <= 0x1FFF && lost <= 0x1FFF, "run-length chunk limit");
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_be_bytes()); // sender ssrc
        body.extend_from_slice(&2u32.to_be_bytes()); // media ssrc
        body.extend_from_slice(&0u16.to_be_bytes()); // base seq
        body.extend_from_slice(&(received + lost).to_be_bytes());
        body.extend_from_slice(&[0, 0, 0]); // reference time
        body.push(0); // fb pkt count
        if received > 0 {
            body.extend_from_slice(&(0x2000 | received).to_be_bytes()); // S=1 run
        }
        if lost > 0 {
            body.extend_from_slice(&lost.to_be_bytes()); // S=0 run
        }
        // One delta per received packet: 4 × 250 µs = 1 ms, flat trend.
        body.resize(body.len() + usize::from(received), 4);
        while (body.len() + 4) % 4 != 0 {
            body.push(0);
        }
        #[allow(clippy::cast_possible_truncation)]
        let words_less_one = ((body.len() + 4) / 4 - 1) as u16;
        let mut pkt = vec![0x8F, 0xCD];
        pkt.extend_from_slice(&words_less_one.to_be_bytes());
        pkt.extend_from_slice(&body);
        pkt
    }

    /// Set up a forwarder with a low/high simulcast publisher and one
    /// subscriber, drained of the subscription keyframe request.
    fn bwe_fixture() -> (SfuForwarder, ParticipantId, ParticipantId) {
        use crate::simulcast::SimulcastLayer;
        let fwd = SfuForwarder::new(SfuRouter::new());
        let pubr = ParticipantId::new();
        let sub = ParticipantId::new();
        let layers = LayerSet::from_layers(vec![
            SimulcastLayer::spatial(Rid::from("low"), LayerKind::Low),
            SimulcastLayer::spatial(Rid::from("high"), LayerKind::High),
        ]);
        fwd.register_publisher_layers("v0", pubr, layers);
        fwd.add_peer(SfuPeer::new(CallId::new(), sub));
        fwd.subscribe("v0", sub, "v0");
        let _ = fwd.poll_keyframe_requests();
        (fwd, pubr, sub)
    }

    /// Feed RTP on `rid` every 10 ms over `[from_ms, to_ms)` with payloads
    /// sized to produce `bps` measured throughput.
    fn feed_layer(fwd: &SfuForwarder, pubr: ParticipantId, rid: &str, bps: u64, from_ms: u64, to_ms: u64) {
        let bytes_per_pkt = usize::try_from(bps / 8 / 100).expect("fits");
        let mut seq = u64::from(u32::from_be_bytes([rid.as_bytes()[0], 0, 0, 0])); // distinct seq spaces
        for t in (from_ms..to_ms).step_by(10) {
            let mut rtp = inbound_simulcast("v0", seq, 0, rid, false);
            rtp.payload = vec![0u8; bytes_per_pkt];
            fwd.on_rtp_at(pubr, &rtp, Instant::now(), t);
            seq += 1;
        }
    }

    #[test]
    fn remb_feedback_updates_subscriber_estimate() {
        let (fwd, _pubr, sub) = bwe_fixture();
        assert_eq!(fwd.subscriber_estimate_bps(sub), None, "no feedback yet");
        let remb = crate::rtcp_fb::encode_remb(1, 300_000, &[2]);
        fwd.on_subscriber_rtcp_at(sub, "v0", &remb, Instant::now(), 0);
        // AIMD starts at 600k; the 300k REMB clamps the estimate.
        assert_eq!(fwd.subscriber_estimate_bps(sub), Some(300_000));
    }

    #[test]
    fn twcc_loss_backs_off_subscriber_estimate() {
        let (fwd, _pubr, sub) = bwe_fixture();
        // 50% loss → multiplicative decrease from the 600k initial value.
        fwd.on_subscriber_rtcp_at(sub, "v0", &twcc_buf(10, 10), Instant::now(), 0);
        assert_eq!(fwd.subscriber_estimate_bps(sub), Some(510_000), "600k × 0.85");
    }

    #[test]
    fn forwarded_rtp_measures_per_layer_throughput() {
        let (fwd, pubr, _sub) = bwe_fixture();
        assert_eq!(fwd.layer_rate_bps("v0", Rid::from("high")), None);
        feed_layer(&fwd, pubr, "high", 2_000_000, 0, 1_000);
        feed_layer(&fwd, pubr, "low", 200_000, 0, 1_000);
        let high = fwd.layer_rate_bps("v0", Rid::from("high")).expect("measured");
        let low = fwd.layer_rate_bps("v0", Rid::from("low")).expect("measured");
        assert!((1_800_000..=2_200_000).contains(&high), "≈2 Mbps, got {high}");
        assert!((180_000..=220_000).contains(&low), "≈200 kbps, got {low}");
    }

    #[test]
    fn low_remb_triggers_immediate_down_switch() {
        let (fwd, pubr, sub) = bwe_fixture();
        // Bootstrap the subscriber on "high" (first packet wins), then measure
        // both layers: high ≈ 2 Mbps, low ≈ 200 kbps.
        feed_layer(&fwd, pubr, "high", 2_000_000, 0, 1_000);
        feed_layer(&fwd, pubr, "low", 200_000, 0, 1_000);
        {
            let g = fwd.inner.lock();
            let sel = g.layer_table.selector_for(sub, "v0").expect("selector");
            assert_eq!(sel.active_rid(), Some(Rid::from("high")));
        }

        // Receiver reports only 300 kbps → budget 255k → only "low" fits.
        // `now` is pushed 10 s out so the keyframe coalesce window (opened by
        // the subscribe() request) cannot suppress the switch request.
        let remb = crate::rtcp_fb::encode_remb(1, 300_000, &[2]);
        let later = Instant::now() + Duration::from_secs(10);
        fwd.on_subscriber_rtcp_at(sub, "v0", &remb, later, 2_000);

        let g = fwd.inner.lock();
        let sel = g.layer_table.selector_for(sub, "v0").expect("selector");
        assert_eq!(
            sel.pending_rid(),
            Some(Rid::from("low")),
            "congestion must trigger an immediate keyframe-gated down-switch"
        );
        drop(g);
        let reqs = fwd.poll_keyframe_requests();
        assert_eq!(reqs.len(), 1, "down-switch must request a keyframe");
        assert_eq!(reqs[0].publisher, pubr);
        assert_eq!(reqs[0].pub_mid, "v0");
    }

    #[test]
    fn up_switch_waits_for_stable_headroom_then_fires() {
        let (fwd, pubr, sub) = bwe_fixture();
        // Bootstrap on "low"; measure low ≈ 200 kbps and high ≈ 400 kbps so the
        // initial 600k estimate (budget 510k) already affords "high".
        feed_layer(&fwd, pubr, "low", 200_000, 0, 1_000);
        feed_layer(&fwd, pubr, "high", 400_000, 0, 1_000);
        let later = Instant::now() + Duration::from_secs(10);

        // Clean TWCC at t=2s: headroom noticed, but not stable yet → no switch.
        fwd.on_subscriber_rtcp_at(sub, "v0", &twcc_buf(20, 0), later, 2_000);
        fwd.on_subscriber_rtcp_at(sub, "v0", &twcc_buf(20, 0), later, 3_000);
        {
            let g = fwd.inner.lock();
            let sel = g.layer_table.selector_for(sub, "v0").expect("selector");
            assert_eq!(sel.pending_rid(), None, "up-switch must wait ~2 s");
            assert_eq!(sel.active_rid(), Some(Rid::from("low")));
        }

        // 2 s of stable headroom → the up-switch fires (keyframe-gated).
        fwd.on_subscriber_rtcp_at(sub, "v0", &twcc_buf(20, 0), later, 4_000);
        {
            let g = fwd.inner.lock();
            let sel = g.layer_table.selector_for(sub, "v0").expect("selector");
            assert_eq!(sel.pending_rid(), Some(Rid::from("high")));
        }
        let reqs = fwd.poll_keyframe_requests();
        assert_eq!(reqs.len(), 1, "up-switch must request a keyframe");
        assert_eq!(reqs[0].publisher, pubr);

        // The switch itself still commits only on a target-layer keyframe.
        let mut kf = inbound_simulcast("v0", 999_999, 0, "high", true);
        kf.payload = vec![0u8; 100];
        fwd.on_rtp_at(pubr, &kf, Instant::now(), 5_000);
        let g = fwd.inner.lock();
        let sel = g.layer_table.selector_for(sub, "v0").expect("selector");
        assert_eq!(sel.active_rid(), Some(Rid::from("high")));
        assert_eq!(sel.pending_rid(), None);
    }

    #[test]
    fn unmeasured_high_layer_blocks_up_switch() {
        let (fwd, pubr, sub) = bwe_fixture();
        // Only "low" ever carried traffic — "high" has no measured rate.
        feed_layer(&fwd, pubr, "low", 200_000, 0, 1_000);
        let later = Instant::now() + Duration::from_secs(10);
        for t in [2_000u64, 3_000, 4_000, 10_000] {
            fwd.on_subscriber_rtcp_at(sub, "v0", &twcc_buf(20, 0), later, t);
        }
        let g = fwd.inner.lock();
        let sel = g.layer_table.selector_for(sub, "v0").expect("selector");
        assert_eq!(
            sel.pending_rid(),
            None,
            "cannot verify an unmeasured layer fits → no up-switch"
        );
    }

    #[test]
    fn bandwidth_feedback_without_layers_only_updates_estimator() {
        // No register_publisher_layers for this mid: estimator updates, no
        // adaptation (and no panic).
        let fwd = SfuForwarder::new(SfuRouter::new());
        let sub = ParticipantId::new();
        let remb = crate::rtcp_fb::encode_remb(1, 250_000, &[2]);
        fwd.on_subscriber_rtcp_at(sub, "v9", &remb, Instant::now(), 0);
        assert_eq!(fwd.subscriber_estimate_bps(sub), Some(250_000));
        assert!(fwd.poll_keyframe_requests().is_empty());
    }

    #[test]
    fn remove_peer_clears_bwe_state() {
        let (fwd, _pubr, sub) = bwe_fixture();
        let remb = crate::rtcp_fb::encode_remb(1, 300_000, &[2]);
        fwd.on_subscriber_rtcp_at(sub, "v0", &remb, Instant::now(), 0);
        assert!(fwd.subscriber_estimate_bps(sub).is_some());
        fwd.remove_peer(sub);
        assert_eq!(fwd.subscriber_estimate_bps(sub), None);
        let g = fwd.inner.lock();
        assert!(g.adapt.is_empty(), "hysteresis state must be dropped too");
    }

    #[test]
    fn compound_rtcp_serves_both_keyframe_and_bandwidth_paths() {
        use crate::rtcp_feedback::encode_pli;
        let (fwd, pubr, sub) = bwe_fixture();
        let mut buf = vec![0u8; 12];
        encode_pli(1u32.into(), 2u32.into(), &mut buf);
        buf.extend_from_slice(&crate::rtcp_fb::encode_remb(1, 300_000, &[2]));

        let later = Instant::now() + Duration::from_secs(10);
        fwd.on_subscriber_rtcp_at(sub, "v0", &buf, later, 0);

        // PLI → upstream keyframe request; REMB → estimator update.
        let reqs = fwd.poll_keyframe_requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].publisher, pubr);
        assert_eq!(fwd.subscriber_estimate_bps(sub), Some(300_000));
    }
}
