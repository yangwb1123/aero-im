//! SFU **cascade / relay**: pull a remote stream **once** and fan it out to
//! local subscribers.
//!
//! ## Why cascade
//!
//! When a stream is published on node A but viewers connect to node B, node B
//! has two cheap options and one expensive-but-scalable one:
//!
//! - **Serve locally** — the stream lives here; just [`MediaRelay::subscribe`].
//! - **Redirect to owner** — 307 the viewer to node A (sticky routing). Cheap
//!   when there is barely any local demand, but every viewer crosses the WAN.
//! - **Cascade from owner** — node B opens **one** upstream pull to node A and
//!   fans the media out to all of its *local* subscribers. One inter-node flow
//!   amortized across many local viewers; node A's egress fan-out stays bounded.
//!
//! This module owns the cascade leg. An [`UpstreamSource`] abstracts "a remote
//! stream this node relays from" (yielding decoded H.264 access units); a
//! [`CascadeRelay`] pumps that source into the existing [`MediaRelay`] so the
//! pulled media reaches local relay-backed [`Subscription`](crate::relay::Subscription)s
//! — exactly the same fan-out path a local WHIP publisher uses.
//!
//! ## What is real vs. a seam
//!
//! The cascade policy and pump are unit-tested against [`FakeUpstream`]. The
//! concrete [`WhepUpstreamSource`](crate::upstream::WhepUpstreamSource) is the
//! real transport implementation: it performs a WHEP SDP exchange, drives
//! str0m ICE/DTLS/SRTP over UDP, reorders/depacketizes H.264 RTP, and yields
//! access units through this trait.
//!
//! `aero-server` currently serves a remote stream with its production sticky
//! routing path (a 307 to the node recorded in `StreamRouteRegistry`); it does
//! not instantiate a local [`CascadeRelay`]. Selecting cascade based on
//! node-local demand and owning that extra upstream WHEP resource is the
//! remaining optional scaling-policy integration. It is not required for
//! remote WHEP playback, which already works through the sticky redirect.
//!
//! ## Relationship to [`MediaRelay`]
//!
//! [`CascadeRelay`] does **not** change [`MediaRelay`]'s contract. It owns a
//! relay, feeds it via [`MediaRelay::publish`], and hands out subscriptions via
//! [`CascadeRelay::subscribe`]. Late-joiner / keyframe behaviour at the relay
//! layer is unchanged (a `broadcast` fan-out does not replay history). What the
//! cascade *does* add is **keyframe-gated startup**: it can withhold fan-out
//! until the first IDR access unit arrives from upstream, so the very first
//! bytes a fresh cascade publishes are decodable rather than a mid-GOP P-frame.

use async_trait::async_trait;
use bytes::Bytes;
use tracing::{debug, trace};
use ulid::Ulid;

use crate::hls_sink::{classify_nal, split_annex_b, NalClass};
use crate::relay::{MediaRelay, Subscription};

/// One access unit pulled from an upstream node.
///
/// Mirrors the relay's internal access-unit shape: an **Annex-B** encoded run
/// of one or more NAL units plus a 90 kHz presentation timestamp. `Bytes` is
/// reference-counted, so handing it to [`MediaRelay::publish`] is zero-copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamAu {
    /// Annex-B encoded access unit (NAL units prefixed with `00 00 00 01`).
    pub annex_b: Bytes,
    /// Presentation timestamp in 90 kHz ticks.
    pub ts_90k: u64,
}

impl UpstreamAu {
    /// Build an access unit from raw Annex-B bytes and a 90 kHz timestamp.
    #[must_use]
    pub fn new(annex_b: impl Into<Bytes>, ts_90k: u64) -> Self {
        Self {
            annex_b: annex_b.into(),
            ts_90k,
        }
    }

    /// True if this access unit contains an IDR (keyframe) slice — i.e. it is a
    /// decodable random-access point. Used for keyframe-gated cascade startup.
    #[must_use]
    pub fn is_keyframe(&self) -> bool {
        split_annex_b(&self.annex_b)
            .iter()
            .filter_map(|nal| nal.first())
            .any(|&hdr| classify_nal(hdr) == NalClass::IdrSlice)
    }
}

/// A remote stream this node relays from.
///
/// An `UpstreamSource` yields decoded H.264 access units (Annex-B) pulled from
/// the node that owns the stream. [`next_au`](UpstreamSource::next_au) is
/// asynchronous: it awaits the next access unit and returns `None` at
/// end-of-stream (the upstream publisher stopped or the link dropped).
///
/// The production implementation is
/// [`WhepUpstreamSource`](crate::upstream::WhepUpstreamSource). Its SDP/error
/// paths compile and run in unit tests; a successful two-node ICE/DTLS/SRTP
/// exchange still needs a routable staging peer, just like
/// [`crate::session::WhipSession::run`] and [`crate::whep::WhepSession::run`].
/// The cascade logic downstream of received access units ([`CascadeRelay`]) is
/// fully deterministic and tested via [`FakeUpstream`].
#[async_trait]
pub trait UpstreamSource: Send {
    /// The id of the remote stream being relayed.
    fn stream_id(&self) -> Ulid;

    /// Await and return the next access unit, or `None` at end-of-stream.
    async fn next_au(&mut self) -> Option<UpstreamAu>;
}

/// Wires an [`UpstreamSource`] into a [`MediaRelay`] so access units pulled from
/// a remote node fan out to this node's **local** subscribers.
///
/// A `CascadeRelay` owns a [`MediaRelay`]; call [`subscribe`](Self::subscribe)
/// to hand out local relay-backed subscriptions, then drive the cascade with
/// [`run`](Self::run) (production) or step it deterministically with
/// [`pump_once`](Self::pump_once) (tests).
///
/// ## Keyframe-gated startup
///
/// Constructed with [`new`](Self::new), the relay withholds fan-out until the
/// first keyframe (IDR) access unit arrives from upstream — pre-keyframe access
/// units are dropped so that the first bytes local subscribers receive form a
/// decodable random-access point. Use [`new_passthrough`](Self::new_passthrough)
/// to publish every access unit immediately (no gating), matching a plain local
/// publisher.
pub struct CascadeRelay<S: UpstreamSource> {
    source: S,
    relay: MediaRelay,
    /// When `true`, withhold fan-out until the first keyframe has been seen.
    require_keyframe: bool,
    /// Becomes `true` once fan-out has started (keyframe seen, or gating off).
    started: bool,
}

impl<S: UpstreamSource> CascadeRelay<S> {
    /// Create a cascade relay with **keyframe-gated** startup and the relay's
    /// default fan-out capacity.
    ///
    /// Fan-out is withheld until the first IDR access unit arrives from
    /// `source`; earlier access units are dropped.
    pub fn new(source: S) -> Self {
        Self {
            source,
            relay: MediaRelay::new(),
            require_keyframe: true,
            started: false,
        }
    }

    /// Create a cascade relay that fans out **every** access unit immediately
    /// (no keyframe gating), with the relay's default fan-out capacity.
    pub fn new_passthrough(source: S) -> Self {
        Self {
            source,
            relay: MediaRelay::new(),
            require_keyframe: false,
            started: true,
        }
    }

    /// Wrap an explicit [`MediaRelay`] (e.g. one with a custom buffer capacity)
    /// with keyframe-gated startup.
    pub fn with_relay(source: S, relay: MediaRelay) -> Self {
        Self {
            source,
            relay,
            require_keyframe: true,
            started: false,
        }
    }

    /// Open a new **local** subscription to the cascaded media.
    ///
    /// Identical to subscribing to a local publisher — the returned
    /// [`Subscription`] is a [`NalSource`](crate::whep::NalSource) that can drive
    /// a [`WhepSession::run`](crate::whep::WhepSession::run). Subscribers receive
    /// access units published *after* they subscribe (no history replay).
    #[must_use]
    pub fn subscribe(&self) -> Subscription {
        self.relay.subscribe()
    }

    /// The remote stream id this relay is cascading.
    #[must_use]
    pub fn stream_id(&self) -> Ulid {
        self.source.stream_id()
    }

    /// Number of local subscribers currently attached.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.relay.subscriber_count()
    }

    /// Borrow the underlying [`MediaRelay`] (e.g. to publish a locally-sourced
    /// access unit alongside the cascaded ones, or to inspect it in tests).
    #[must_use]
    pub fn relay(&self) -> &MediaRelay {
        &self.relay
    }

    /// True once fan-out has started (the keyframe gate has opened, or gating is
    /// disabled).
    #[must_use]
    pub fn has_started(&self) -> bool {
        self.started
    }

    /// Pull **one** access unit from upstream and, if the keyframe gate allows,
    /// fan it out to local subscribers.
    ///
    /// Returns:
    /// - `Some(n)` — an access unit was pulled and published to `n` subscribers
    ///   (`n == 0` if there are none right now, which is not an error).
    /// - `Some(0)` is *also* returned when the access unit was **dropped** by the
    ///   keyframe gate (pre-first-keyframe). Callers that care can disambiguate
    ///   via [`has_started`](Self::has_started).
    /// - `None` — upstream reached end-of-stream.
    ///
    /// This is the single, synchronous-to-reason-about step that
    /// [`run`](Self::run) loops over; tests drive it directly with a
    /// [`FakeUpstream`].
    pub async fn pump_once(&mut self) -> Option<usize> {
        let au = self.source.next_au().await?;

        // Keyframe gate: until the first IDR, drop access units so the cascade
        // begins fan-out at a decodable random-access point.
        if !self.started {
            if self.require_keyframe && !au.is_keyframe() {
                trace!(ts_90k = au.ts_90k, "cascade: dropping pre-keyframe AU");
                return Some(0);
            }
            self.started = true;
            debug!(
                ts_90k = au.ts_90k,
                "cascade: keyframe gate opened, fan-out started"
            );
        }

        let n = self.relay.publish(au.annex_b, au.ts_90k);
        Some(n)
    }

    /// Drive the cascade to completion: repeatedly [`pump_once`](Self::pump_once)
    /// until upstream signals end-of-stream.
    ///
    /// This is the production driver, analogous to
    /// [`WhepSession::run`](crate::whep::WhepSession::run): it owns the pull loop
    /// and runs until the upstream link ends. A caller that enables the optional
    /// cascade policy spawns it once per cascaded stream. It performs no I/O of
    /// its own — all I/O lives behind the [`UpstreamSource`] implementation.
    pub async fn run(mut self) {
        let stream_id = self.source.stream_id();
        debug!(%stream_id, "cascade: relay loop started");
        while self.pump_once().await.is_some() {}
        debug!(%stream_id, "cascade: upstream ended, relay loop finished");
    }
}

// ───────────────────────────── decision policy ─────────────────────────────

/// What a node should do when a viewer asks for a stream.
///
/// Produced by [`decide_cascade`]; a **pure** decision with no I/O so it is
/// exhaustively unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CascadeDecision {
    /// The stream is owned by *this* node — serve the viewer from the local
    /// relay directly. No inter-node traffic.
    ServeLocally,
    /// The stream is remote and local demand is low — 307-redirect the viewer to
    /// the owning node (sticky routing). Cheapest when only one or two viewers
    /// want a remote stream.
    RedirectToOwner,
    /// The stream is remote and local demand has reached the threshold — open
    /// **one** [`CascadeRelay`] from the owner and fan it out locally,
    /// amortizing a single inter-node flow across many local viewers.
    CascadeFromOwner,
}

/// Inputs to the cascade-vs-redirect decision.
///
/// Kept as a small struct (rather than positional args) so call sites read
/// clearly and the policy can grow fields without churning every caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CascadeInputs {
    /// `true` if *this* node owns (ingests) the stream.
    pub stream_is_local: bool,
    /// Number of local viewers that want this stream, **including** the one
    /// currently being routed. A brand-new first viewer is `1`.
    pub local_subscribers: u32,
    /// Local-demand threshold at which cascading begins to pay off. When
    /// `local_subscribers >= cascade_threshold`, prefer cascade over redirect.
    /// A value of `1` means "always cascade a remote stream"; `0` is treated
    /// as `1`.
    pub cascade_threshold: u32,
}

/// Decide how to serve a viewer for a stream — **pure**, no I/O.
///
/// Rules (in order):
/// 1. If the stream is local → [`ServeLocally`](CascadeDecision::ServeLocally).
///    Local ingest always wins; there is nothing to fetch.
/// 2. Otherwise the stream is remote. If local demand has reached the threshold
///    (`local_subscribers >= cascade_threshold`) →
///    [`CascadeFromOwner`](CascadeDecision::CascadeFromOwner): one upstream pull
///    amortized across the local viewers.
/// 3. Otherwise (sub-threshold demand) →
///    [`RedirectToOwner`](CascadeDecision::RedirectToOwner): cheaper to send the
///    one/few viewers straight to the owner than to stand up a cascade leg.
///
/// A `cascade_threshold` of `0` is clamped to `1`, so a remote stream with no
/// viewers always redirects (there is nothing to amortize).
#[must_use]
pub fn decide_cascade(inputs: CascadeInputs) -> CascadeDecision {
    if inputs.stream_is_local {
        return CascadeDecision::ServeLocally;
    }
    let threshold = inputs.cascade_threshold.max(1);
    if inputs.local_subscribers >= threshold {
        CascadeDecision::CascadeFromOwner
    } else {
        CascadeDecision::RedirectToOwner
    }
}

// ───────────────────────────── test fake ───────────────────────────────────

/// A scripted, in-memory [`UpstreamSource`] for tests.
///
/// Emits a pre-built sequence of [`UpstreamAu`]s in order, then returns `None`
/// (end-of-stream). This is the cascade analogue of feeding synthetic NAL/RTP
/// to a fake sink — no socket, no peer node, fully deterministic.
///
/// Exposed (not `#[cfg(test)]`) so integration tests and the production crate's
/// own tests can both drive a [`CascadeRelay`] without a real transport. It is a
/// trivial scripted source with no real I/O.
pub struct FakeUpstream {
    stream_id: Ulid,
    /// Remaining access units, popped front-to-back.
    queue: std::collections::VecDeque<UpstreamAu>,
}

impl FakeUpstream {
    /// Build a fake upstream for `stream_id` that will emit `aus` in order.
    #[must_use]
    pub fn new(stream_id: Ulid, aus: impl IntoIterator<Item = UpstreamAu>) -> Self {
        Self {
            stream_id,
            queue: aus.into_iter().collect(),
        }
    }

    /// Number of access units not yet emitted.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.queue.len()
    }
}

#[async_trait]
impl UpstreamSource for FakeUpstream {
    fn stream_id(&self) -> Ulid {
        self.stream_id
    }

    async fn next_au(&mut self) -> Option<UpstreamAu> {
        self.queue.pop_front()
    }
}

/// Build a minimal Annex-B access unit from `(nal_header, body)` pairs.
///
/// Test/fixture helper shared by this crate's cascade tests; mirrors the
/// `annex_b` helper used in [`crate::relay`]'s tests so the cascade fan-out
/// assertions line up byte-for-byte with the relay's.
#[must_use]
pub fn annex_b_au(nals: &[(u8, &[u8])]) -> Bytes {
    let mut out = Vec::new();
    for (header, body) in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.push(*header);
        out.extend_from_slice(body);
    }
    Bytes::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whep::NalSource;

    // ── fixtures ─────────────────────────────────────────────────────────────

    /// An IDR (keyframe) access unit: SPS + PPS + IDR slice.
    fn keyframe_au(ts: u64) -> UpstreamAu {
        UpstreamAu::new(
            annex_b_au(&[
                (0x67, &[0x42, 0x00, 0x1F]), // SPS
                (0x68, &[0xCE]),             // PPS
                (0x65, &[0xAA, 0xBB]),       // IDR slice (type 5)
            ]),
            ts,
        )
    }

    /// A delta (P-frame) access unit: a single non-IDR slice (type 1).
    fn delta_au(body: &[u8], ts: u64) -> UpstreamAu {
        UpstreamAu::new(annex_b_au(&[(0x41, body)]), ts)
    }

    // ── UpstreamAu::is_keyframe ───────────────────────────────────────────────

    #[test]
    fn keyframe_au_is_classified_as_keyframe() {
        assert!(
            keyframe_au(0).is_keyframe(),
            "SPS+PPS+IDR must be a keyframe"
        );
    }

    #[test]
    fn delta_au_is_not_a_keyframe() {
        assert!(
            !delta_au(&[0x01], 3_000).is_keyframe(),
            "a lone P-slice is not a keyframe"
        );
    }

    #[test]
    fn idr_anywhere_in_au_counts_as_keyframe() {
        // Even if the IDR is not the first NAL, the AU is a random-access point.
        let au = UpstreamAu::new(annex_b_au(&[(0x06, &[0x00]), (0x65, &[0x01])]), 0);
        assert!(au.is_keyframe(), "SEI + IDR is still a keyframe AU");
    }

    #[test]
    fn empty_annex_b_is_not_a_keyframe() {
        let au = UpstreamAu::new(Bytes::new(), 0);
        assert!(!au.is_keyframe());
    }

    // ── FakeUpstream ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn fake_upstream_emits_scripted_sequence_then_none() {
        let id = Ulid::new();
        let aus = vec![
            keyframe_au(0),
            delta_au(&[0x01], 3_000),
            delta_au(&[0x02], 6_000),
        ];
        let mut up = FakeUpstream::new(id, aus.clone());

        assert_eq!(up.stream_id(), id);
        assert_eq!(up.remaining(), 3);

        for expected in &aus {
            let got = up.next_au().await.expect("scripted AU must be emitted");
            assert_eq!(&got, expected, "AU must match the scripted one");
        }
        assert!(
            up.next_au().await.is_none(),
            "end-of-stream after the script"
        );
        assert_eq!(up.remaining(), 0);
    }

    // ── CascadeRelay fan-out (mirrors relay fan-out tests) ────────────────────

    /// Drain every access unit currently buffered in a subscription into a vec
    /// of `(nals, rtp_ts)`.
    fn drain(sub: &mut Subscription) -> Vec<(Vec<Vec<u8>>, u32)> {
        let mut out = Vec::new();
        while let Some(au) = sub.next_access_unit() {
            out.push(au);
        }
        out
    }

    #[tokio::test]
    async fn cascade_fans_scripted_upstream_to_one_local_subscriber_in_order() {
        let id = Ulid::new();
        let up = FakeUpstream::new(
            id,
            vec![
                keyframe_au(0),
                delta_au(&[0x10], 3_000),
                delta_au(&[0x20], 6_000),
            ],
        );
        let mut cascade = CascadeRelay::new(up);

        // Subscribe BEFORE pumping so the subscriber sees every fanned-out AU.
        let mut sub = cascade.subscribe();
        assert_eq!(cascade.subscriber_count(), 1);
        assert_eq!(cascade.stream_id(), id);

        // Pump the whole script.
        assert_eq!(
            cascade.pump_once().await,
            Some(1),
            "keyframe → 1 subscriber"
        );
        assert_eq!(cascade.pump_once().await, Some(1), "delta 1 → 1 subscriber");
        assert_eq!(cascade.pump_once().await, Some(1), "delta 2 → 1 subscriber");
        assert_eq!(cascade.pump_once().await, None, "end-of-stream");

        let received = drain(&mut sub);
        assert_eq!(received.len(), 3, "all three AUs fanned out");

        // AU0: keyframe = SPS + PPS + IDR (3 NALs), ts 0.
        assert_eq!(received[0].0.len(), 3, "keyframe has 3 NALs");
        assert_eq!(received[0].0[0], &[0x67, 0x42, 0x00, 0x1F]);
        assert_eq!(received[0].0[1], &[0x68, 0xCE]);
        assert_eq!(received[0].0[2], &[0x65, 0xAA, 0xBB]);
        assert_eq!(received[0].1, 0);
        // AU1: delta, ts 3000.
        assert_eq!(received[1].0, vec![vec![0x41, 0x10]]);
        assert_eq!(received[1].1, 3_000);
        // AU2: delta, ts 6000.
        assert_eq!(received[2].0, vec![vec![0x41, 0x20]]);
        assert_eq!(received[2].1, 6_000);
    }

    #[tokio::test]
    async fn cascade_fans_out_to_multiple_local_subscribers() {
        let id = Ulid::new();
        let up = FakeUpstream::new(id, vec![keyframe_au(0), delta_au(&[0x42], 3_000)]);
        let mut cascade = CascadeRelay::new(up);

        let mut subs: Vec<Subscription> = (0..4).map(|_| cascade.subscribe()).collect();
        assert_eq!(cascade.subscriber_count(), 4);

        assert_eq!(cascade.pump_once().await, Some(4), "keyframe → all 4");
        assert_eq!(cascade.pump_once().await, Some(4), "delta → all 4");
        assert_eq!(cascade.pump_once().await, None);

        for (i, sub) in subs.iter_mut().enumerate() {
            let received = drain(sub);
            assert_eq!(received.len(), 2, "subscriber {i}: two AUs");
            assert_eq!(received[0].0.len(), 3, "subscriber {i}: keyframe 3 NALs");
            assert_eq!(
                received[1].0,
                vec![vec![0x41, 0x42]],
                "subscriber {i}: delta"
            );
            assert_eq!(received[1].1, 3_000, "subscriber {i}: delta ts");
        }
    }

    #[tokio::test]
    async fn run_drives_cascade_to_completion() {
        let id = Ulid::new();
        let up = FakeUpstream::new(
            id,
            vec![
                keyframe_au(0),
                delta_au(&[0x01], 3_000),
                delta_au(&[0x02], 6_000),
            ],
        );
        let cascade = CascadeRelay::new(up);
        let mut sub = cascade.subscribe();

        // run() loops pump_once until end-of-stream, then returns.
        cascade.run().await;

        let received = drain(&mut sub);
        assert_eq!(received.len(), 3, "run() fanned out the entire script");
        assert_eq!(received[0].1, 0);
        assert_eq!(received[1].1, 3_000);
        assert_eq!(received[2].1, 6_000);
    }

    // ── keyframe-gated startup ────────────────────────────────────────────────

    #[tokio::test]
    async fn keyframe_gate_drops_pre_keyframe_aus_then_starts_at_idr() {
        let id = Ulid::new();
        // Two delta AUs BEFORE the first keyframe — these must be dropped.
        let up = FakeUpstream::new(
            id,
            vec![
                delta_au(&[0x01], 0),     // dropped (pre-keyframe)
                delta_au(&[0x02], 3_000), // dropped (pre-keyframe)
                keyframe_au(6_000),       // gate opens here
                delta_au(&[0x03], 9_000), // fanned out
            ],
        );
        let mut cascade = CascadeRelay::new(up);
        let mut sub = cascade.subscribe();

        // First two pumps drop their AUs (gate closed) → 0 subscribers reached.
        assert_eq!(
            cascade.pump_once().await,
            Some(0),
            "pre-keyframe AU dropped"
        );
        assert!(!cascade.has_started(), "gate still closed");
        assert_eq!(
            cascade.pump_once().await,
            Some(0),
            "pre-keyframe AU dropped"
        );
        assert!(!cascade.has_started(), "gate still closed");

        // Keyframe opens the gate and is published.
        assert_eq!(cascade.pump_once().await, Some(1), "keyframe fans out");
        assert!(cascade.has_started(), "gate open after keyframe");

        // Subsequent delta is fanned out.
        assert_eq!(
            cascade.pump_once().await,
            Some(1),
            "post-keyframe delta fans out"
        );
        assert_eq!(cascade.pump_once().await, None);

        let received = drain(&mut sub);
        assert_eq!(
            received.len(),
            2,
            "only the keyframe and the AU after it reach the subscriber"
        );
        // First delivered AU must be the keyframe (decodable random-access point).
        assert_eq!(received[0].0.len(), 3, "first delivered AU is the keyframe");
        assert_eq!(received[0].1, 6_000, "keyframe ts");
        assert_eq!(received[1].0, vec![vec![0x41, 0x03]], "then the delta");
        assert_eq!(received[1].1, 9_000);
    }

    #[tokio::test]
    async fn passthrough_mode_fans_out_pre_keyframe_aus_immediately() {
        let id = Ulid::new();
        let up = FakeUpstream::new(id, vec![delta_au(&[0x01], 0), keyframe_au(3_000)]);
        let mut cascade = CascadeRelay::new_passthrough(up);
        assert!(cascade.has_started(), "passthrough starts immediately");
        let mut sub = cascade.subscribe();

        // No gating: the leading delta is published too.
        assert_eq!(
            cascade.pump_once().await,
            Some(1),
            "delta published (no gate)"
        );
        assert_eq!(cascade.pump_once().await, Some(1), "keyframe published");
        assert_eq!(cascade.pump_once().await, None);

        let received = drain(&mut sub);
        assert_eq!(received.len(), 2, "both AUs fanned out in passthrough mode");
        assert_eq!(
            received[0].0,
            vec![vec![0x41, 0x01]],
            "leading delta delivered"
        );
        assert_eq!(received[0].1, 0);
    }

    #[tokio::test]
    async fn late_joiner_misses_pre_subscription_aus() {
        // Mirrors the relay's `subscriber_joining_after_publish_misses_earlier_au`:
        // the broadcast fan-out does not replay history, so a viewer that joins
        // mid-cascade only sees AUs published after it subscribed. (Documented
        // limitation — keyframe re-priming for late joiners is not in scope.)
        let id = Ulid::new();
        let up = FakeUpstream::new(
            id,
            vec![
                keyframe_au(0),
                delta_au(&[0x01], 3_000),
                delta_au(&[0x02], 6_000),
            ],
        );
        let mut cascade = CascadeRelay::new(up);

        // Pump the keyframe + first delta BEFORE anyone subscribes.
        assert_eq!(
            cascade.pump_once().await,
            Some(0),
            "keyframe, no subscribers yet"
        );
        assert_eq!(
            cascade.pump_once().await,
            Some(0),
            "delta 1, no subscribers yet"
        );

        // Now a late viewer joins.
        let mut sub = cascade.subscribe();
        assert_eq!(
            cascade.pump_once().await,
            Some(1),
            "delta 2 reaches the late joiner"
        );
        assert_eq!(cascade.pump_once().await, None);

        let received = drain(&mut sub);
        assert_eq!(
            received.len(),
            1,
            "late joiner only sees the AU after it joined"
        );
        assert_eq!(received[0].0, vec![vec![0x41, 0x02]]);
        assert_eq!(received[0].1, 6_000);
    }

    #[tokio::test]
    async fn pump_with_no_subscribers_returns_zero_not_error() {
        let id = Ulid::new();
        let up = FakeUpstream::new(id, vec![keyframe_au(0)]);
        let mut cascade = CascadeRelay::new(up);
        // No subscribers: publish reaches 0, but it is not end-of-stream.
        assert_eq!(cascade.pump_once().await, Some(0));
        assert_eq!(cascade.pump_once().await, None, "then end-of-stream");
    }

    #[tokio::test]
    async fn empty_upstream_yields_no_aus() {
        let id = Ulid::new();
        let up = FakeUpstream::new(id, Vec::new());
        let mut cascade = CascadeRelay::new(up);
        let mut sub = cascade.subscribe();
        assert_eq!(
            cascade.pump_once().await,
            None,
            "empty upstream → immediate EOS"
        );
        assert!(drain(&mut sub).is_empty(), "nothing fanned out");
    }

    #[tokio::test]
    async fn with_relay_uses_supplied_relay_capacity() {
        // A relay with capacity 1: after pumping many AUs before the subscriber
        // reads, the lagged subscriber still receives at least the latest AU
        // without panicking (mirrors the relay's lagged-subscriber test).
        let id = Ulid::new();
        let aus: Vec<UpstreamAu> = std::iter::once(keyframe_au(0))
            .chain((1u8..6).map(|i| delta_au(&[i], u64::from(i) * 3_000)))
            .collect();
        let up = FakeUpstream::new(id, aus);
        let mut cascade = CascadeRelay::with_relay(up, MediaRelay::with_capacity(1));
        let mut sub = cascade.subscribe();

        // Pump everything before the subscriber reads a single AU.
        while cascade.pump_once().await.is_some() {}

        // The lagged subscriber must still get at least one AU (no panic).
        assert!(
            sub.next_access_unit().is_some(),
            "lagged subscriber recovers"
        );
    }

    // ── decision policy (exhaustive) ──────────────────────────────────────────

    #[test]
    fn local_stream_always_served_locally() {
        for subs in [0u32, 1, 5, 1_000] {
            for threshold in [0u32, 1, 3, 100] {
                let d = decide_cascade(CascadeInputs {
                    stream_is_local: true,
                    local_subscribers: subs,
                    cascade_threshold: threshold,
                });
                assert_eq!(
                    d,
                    CascadeDecision::ServeLocally,
                    "local stream must always serve locally (subs={subs}, thr={threshold})"
                );
            }
        }
    }

    #[test]
    fn remote_stream_below_threshold_redirects() {
        // threshold 3 → 0,1,2 viewers redirect.
        for subs in [0u32, 1, 2] {
            let d = decide_cascade(CascadeInputs {
                stream_is_local: false,
                local_subscribers: subs,
                cascade_threshold: 3,
            });
            assert_eq!(
                d,
                CascadeDecision::RedirectToOwner,
                "remote, sub-threshold demand ({subs} < 3) → redirect"
            );
        }
    }

    #[test]
    fn remote_stream_at_or_above_threshold_cascades() {
        for subs in [3u32, 4, 50] {
            let d = decide_cascade(CascadeInputs {
                stream_is_local: false,
                local_subscribers: subs,
                cascade_threshold: 3,
            });
            assert_eq!(
                d,
                CascadeDecision::CascadeFromOwner,
                "remote, demand ({subs} >= 3) → cascade"
            );
        }
    }

    #[test]
    fn threshold_one_always_cascades_a_remote_stream_with_viewers() {
        for subs in [1u32, 2, 10] {
            let d = decide_cascade(CascadeInputs {
                stream_is_local: false,
                local_subscribers: subs,
                cascade_threshold: 1,
            });
            assert_eq!(
                d,
                CascadeDecision::CascadeFromOwner,
                "thr=1, {subs} viewer(s) → cascade"
            );
        }
    }

    #[test]
    fn threshold_zero_is_clamped_to_one() {
        // thr=0 must behave like thr=1: a single remote viewer cascades, zero redirects.
        let one_viewer = decide_cascade(CascadeInputs {
            stream_is_local: false,
            local_subscribers: 1,
            cascade_threshold: 0,
        });
        assert_eq!(
            one_viewer,
            CascadeDecision::CascadeFromOwner,
            "thr=0 clamps to 1"
        );

        let zero_viewers = decide_cascade(CascadeInputs {
            stream_is_local: false,
            local_subscribers: 0,
            cascade_threshold: 0,
        });
        assert_eq!(
            zero_viewers,
            CascadeDecision::RedirectToOwner,
            "thr=0, no viewers → redirect (nothing to amortize)"
        );
    }

    #[test]
    fn remote_stream_zero_viewers_redirects_regardless_of_threshold() {
        for threshold in [1u32, 3, 100] {
            let d = decide_cascade(CascadeInputs {
                stream_is_local: false,
                local_subscribers: 0,
                cascade_threshold: threshold,
            });
            assert_eq!(
                d,
                CascadeDecision::RedirectToOwner,
                "no local demand → redirect (thr={threshold})"
            );
        }
    }

    #[test]
    fn boundary_exactly_at_threshold_cascades_one_below_redirects() {
        let thr = 5;
        let at = decide_cascade(CascadeInputs {
            stream_is_local: false,
            local_subscribers: thr,
            cascade_threshold: thr,
        });
        let below = decide_cascade(CascadeInputs {
            stream_is_local: false,
            local_subscribers: thr - 1,
            cascade_threshold: thr,
        });
        assert_eq!(
            at,
            CascadeDecision::CascadeFromOwner,
            "exactly at threshold → cascade"
        );
        assert_eq!(
            below,
            CascadeDecision::RedirectToOwner,
            "one below → redirect"
        );
    }
}
