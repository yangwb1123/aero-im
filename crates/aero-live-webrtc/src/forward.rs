//! Real selective-forwarding: route a publisher's RTP to every subscriber.
//!
//! [`SfuForwarder`] is the runtime counterpart to the pure [`ForwardTable`]
//! (in [`crate::remap`]). It owns the live [`SfuPeer`]s of a call and, for each
//! inbound RTP packet lifted out of a publisher's `str0m` instance, performs the
//! fan-out:
//!
//! 1. ask the [`SfuRouter`] / [`ForwardTable`] which subscriber tracks want it,
//! 2. remap the `(seq, ts)` into each subscriber's own outbound RTP space,
//! 3. write the rewritten RTP onto that subscriber's outbound `str0m` stream.
//!
//! The peer's UDP loop (out of scope) feeds [`SfuForwarder::on_rtp`] from its
//! [`crate::peer::PeerProgress::Media`] branch and flushes each touched peer's
//! `poll()` afterwards to emit the resulting datagrams.

use std::sync::Arc;

use aero_common::{CallId, ParticipantId};
use parking_lot::Mutex;
use tracing::trace;

use crate::peer::{InboundRtp, SfuPeer};
use crate::remap::{ForwardTable, RtpKey};
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
    peers: std::collections::HashMap<ParticipantId, SfuPeer>,
    /// Pure routing + per-subscriber remap bookkeeping.
    table: ForwardTable,
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

    /// Remove a peer and all routing/remap state referencing it.
    pub fn remove_peer(&self, peer: ParticipantId) -> Option<SfuPeer> {
        let mut g = self.inner.lock();
        g.table.unlink_subscriber(peer);
        g.peers.remove(&peer)
    }

    /// Link a published track to a subscriber's outbound transceiver so future
    /// packets on `pub_mid` are forwarded to `subscriber`'s `out_mid` stream.
    pub fn subscribe(&self, pub_mid: &str, subscriber: ParticipantId, out_mid: &str) {
        self.inner.lock().table.link(pub_mid, subscriber, out_mid);
    }

    /// Drop a published track from the routing table (publisher unpublished).
    pub fn unpublish(&self, pub_mid: &str) {
        self.inner.lock().table.unlink_publisher_track(pub_mid);
    }

    /// Run a closure with mutable access to one peer, if present. Lets the UDP
    /// task drive `poll()` / `handle_datagram()` without holding the lock itself.
    pub fn with_peer<R>(&self, id: ParticipantId, f: impl FnOnce(&mut SfuPeer) -> R) -> Option<R> {
        let mut g = self.inner.lock();
        g.peers.get_mut(&id).map(f)
    }

    /// Core fan-out: forward one inbound RTP packet from `_publisher` to every
    /// subscribed peer, rewriting per-subscriber seq/timestamp.
    ///
    /// Returns the number of subscriber tracks the packet was written to (0 if
    /// nobody subscribes, or if subscribers haven't negotiated their outbound
    /// stream yet). Peers whose write reports "no such outbound stream" are
    /// skipped — that's normal before a subscriber finishes negotiation.
    pub fn on_rtp(&self, _publisher: ParticipantId, rtp: &InboundRtp) -> usize {
        let pub_mid = rtp.mid.to_string();
        let mut g = self.inner.lock();
        let ForwardState { peers, table } = &mut *g;

        // Snapshot targets first (cheap clone of small Vec) so we can borrow the
        // remapper and the subscriber peer mutably without aliasing the table.
        let targets: Vec<_> = table.targets(&pub_mid).to_vec();
        if targets.is_empty() {
            return 0;
        }

        let key = RtpKey::new(*rtp.seq_no, u64::from(rtp.rtp_time));
        let mut delivered = 0usize;

        for t in targets {
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
        }
    }

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
}
