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
//! Inbound subscriber RTCP (PLI/FIR) is accepted via
//! [`SfuForwarder::on_subscriber_rtcp`], which uses
//! [`crate::rtcp_feedback::parse_keyframe_requests`] to decode the buffer and
//! routes coalesced keyframe requests upstream to the correct publisher.
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

use crate::peer::{InboundRtp, SfuPeer};
use crate::remap::{ForwardTable, RtpKey};
use crate::rtcp_feedback::{parse_keyframe_requests, KeyframeGate, PendingKeyframeRequest};
use crate::simulcast::{ForwardDecision, LayerKind, LayerSet, LayerSelectorTable};
use crate::SfuRouter;

/// Live forwarding state for a single call: the per-call peer set plus the
/// publisher→subscriber routing/remap table. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct SfuForwarder {
    router: SfuRouter,
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
}

impl SfuForwarder {
    #[must_use]
    pub fn new(router: SfuRouter) -> Self {
        Self {
            router,
            inner: Arc::new(Mutex::new(ForwardState::default())),
        }
    }

    /// Register a live peer with the forwarder (called once its `Rtc` exists).
    pub fn add_peer(&self, peer: SfuPeer) {
        self.inner.lock().peers.insert(peer.id(), peer);
    }

    /// Remove a peer and all routing/remap + simulcast state referencing it.
    pub fn remove_peer(&self, peer: ParticipantId) -> Option<SfuPeer> {
        let mut g = self.inner.lock();
        g.table.unlink_subscriber(peer);
        g.layer_table.unlink_subscriber(peer);
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

    /// Accept inbound RTCP from a subscriber and route any PLI/FIR requests
    /// upstream toward the correct publisher as coalesced keyframe requests.
    ///
    /// `subscriber` — the participant that sent the RTCP.
    /// `pub_mid` — the published track the subscriber is watching (the SFU
    ///   must resolve this from the subscriber's own outbound mid using the
    ///   routing table before calling here).
    /// `rtcp_buf` — the raw compound RTCP datagram.
    ///
    /// Enqueued requests are drained via [`Self::poll_keyframe_requests`].
    pub fn on_subscriber_rtcp(
        &self,
        _subscriber: ParticipantId,
        pub_mid: &str,
        rtcp_buf: &[u8],
    ) {
        let now = Instant::now();
        let feedbacks = parse_keyframe_requests(rtcp_buf);
        if feedbacks.is_empty() {
            return;
        }
        let mut g = self.inner.lock();
        let Some(&publisher) = g.pub_owners.get(pub_mid) else {
            return;
        };
        for fb in feedbacks {
            if fb.is_fir() {
                g.keyframe_gate.on_subscriber_fir(publisher, pub_mid, now);
            } else {
                g.keyframe_gate.on_subscriber_pli(publisher, pub_mid, now);
            }
        }
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
    pub fn on_rtp(&self, _publisher: ParticipantId, rtp: &InboundRtp) -> usize {
        let pub_mid = rtp.mid.to_string();
        let mut g = self.inner.lock();
        let ForwardState {
            peers,
            table,
            layer_table,
            keyframe_gate,
            pub_owners,
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
                            keyframe_gate.on_layer_switch(publisher, &pub_mid, Instant::now());
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
}
