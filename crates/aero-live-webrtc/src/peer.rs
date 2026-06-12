//! A single SFU participant backed by a `str0m` [`Rtc`] instance.
//!
//! `str0m` is **sans-IO**: it never touches a socket or a clock itself. The
//! caller drives it by (a) feeding inbound UDP datagrams + the current time via
//! [`Rtc::handle_input`], and (b) draining [`Rtc::poll_output`] for packets to
//! send, the next wake-up deadline, and media/connection events. [`SfuPeer`]
//! wraps that contract behind a small, server-friendly API so the (out-of-scope)
//! UDP task can stay a thin loop:
//!
//! ```ignore
//! loop {
//!     match peer.poll()? {
//!         PeerProgress::Transmit(t) => socket.send_to(&t.contents, t.destination).await?,
//!         PeerProgress::Timeout(at) => { /* sleep until `at`, or until a datagram arrives */ }
//!         PeerProgress::Media(rtp)  => forwarder.on_rtp(peer.id(), rtp),
//!         PeerProgress::KeyframeRequest(kf) => forwarder.on_keyframe_request(peer.id(), kf),
//!         PeerProgress::Idle => {}
//!     }
//! }
//! ```
//!
//! Built in **RTP mode** ([`RtcConfig::set_rtp_mode`]) so the SFU forwards raw
//! RTP packets between peers without depacketizing/re-encoding media.

use std::net::SocketAddr;
use std::time::Instant;

use aero_common::{CallId, ParticipantId};
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::media::{KeyframeRequestKind, MediaKind, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::rtp::{ExtensionValues, RtpPacket, SeqNo};
use str0m::{Event, Input, Output, Rtc};

use crate::codec::{payload_is_keyframe, Codec};
use crate::SfuError;

/// One forwarded RTP packet lifted out of `str0m`'s [`RtpPacket`] into the
/// minimal set of fields the forwarder needs to rewrite + re-emit it.
#[derive(Debug, Clone)]
pub struct InboundRtp {
    /// The publisher transceiver this packet arrived on.
    pub mid: Mid,
    /// Payload type (codec) — copied verbatim to the subscriber stream.
    pub pt: Pt,
    /// Extended sequence number (roll-over counted).
    pub seq_no: SeqNo,
    /// Wire RTP timestamp (codec clock rate).
    pub rtp_time: u32,
    /// Marker bit (frame boundary for many codecs).
    pub marker: bool,
    /// Header extension values (audio level, video orientation, …).
    pub ext_vals: ExtensionValues,
    /// `str0m` server-side arrival time, reused as the write wallclock.
    pub wallclock: Instant,
    /// Payload bytes (no RTP header).
    pub payload: Vec<u8>,
    /// Simulcast RID (Restriction Identifier) if present in the RTP header
    /// extension. `None` for non-simulcast tracks.
    pub rid: Option<str0m::media::Rid>,
    /// Whether this packet begins a keyframe (intra frame).
    ///
    /// Set via [`crate::codec::payload_is_keyframe`] with the [`Codec`]
    /// resolved from this peer's negotiated payload-type mapping
    /// ([`SfuPeer::codec_for_pt`]). Detected codecs:
    ///
    /// - **H.264** (RFC 6184) — single-NAL IDR (type 5), SPS (type 7),
    ///   PPS (type 8), FU-A START fragments of an IDR, and STAP-A aggregates
    ///   containing any of those NAL types.
    /// - **VP8** (RFC 7741) — first packet of the first partition whose
    ///   payload header has the inverse-key-frame `P` bit clear.
    /// - **VP9** (draft-ietf-payload-vp9) — frame-begin packets (`B=1`) that
    ///   are not inter-picture predicted (`P=0`).
    /// - **AV1** (draft-ietf-payload-av1) — aggregation-header `N` bit set
    ///   (first packet of a new coded video sequence).
    /// - **H.265** (RFC 7798) — IRAP NAL types 16–21 (single NAL or FU START).
    ///
    /// **Limitation**: audio / RTX / any other codec yields `false` — there is
    /// no intra-frame concept to gate on for those tracks.
    pub is_keyframe: bool,
}

impl InboundRtp {
    fn from_packet(p: &RtpPacket, codec: Codec) -> Self {
        // Codec-aware keyframe detection from the raw payload; unknown codecs
        // are never keyframes (see `payload_is_keyframe` docs).
        let is_keyframe = payload_is_keyframe(codec, &p.payload);
        Self {
            mid: p.header.ext_vals.mid.unwrap_or_else(|| mid_fallback(p)),
            pt: p.header.payload_type,
            seq_no: p.seq_no,
            rtp_time: p.header.timestamp,
            marker: p.header.marker,
            rid: p.header.ext_vals.rid,
            is_keyframe,
            ext_vals: p.header.ext_vals.clone(),
            wallclock: p.timestamp,
            payload: p.payload.clone(),
        }
    }
}

/// `str0m`'s `RtpPacket` carries the `mid` in the header extension only when the
/// MID extension was negotiated and sent. When it's absent the SFU resolves the
/// owning track from the packet's SSRC (the publisher's `Rtc` knows the
/// SSRC→`mid` mapping); this synthetic default keeps header parsing total and
/// panic-free until that lookup is wired by the server. With BUNDLE + a single
/// m-section the first mid is `"0"`, which is also the common single-track case.
fn mid_fallback(_p: &RtpPacket) -> Mid {
    Mid::from("0")
}

/// A keyframe (PLI/FIR) request bubbled up from a subscriber — the forwarder
/// relays it to the upstream publisher so a fresh I-frame is produced.
#[derive(Debug, Clone, Copy)]
pub struct KeyframeReq {
    pub mid: Mid,
    pub kind: KeyframeRequestKind,
}

/// Outcome of a single [`SfuPeer::poll`] step.
#[derive(Debug)]
pub enum PeerProgress {
    /// A datagram to write to the socket.
    Transmit(Box<str0m::net::Transmit>),
    /// No work until this instant (or until a datagram arrives).
    Timeout(Instant),
    /// Inbound media to forward to subscribers (boxed — it carries the payload).
    Media(Box<InboundRtp>),
    /// A subscriber asked for a keyframe; relay upstream.
    KeyframeRequest(KeyframeReq),
    /// ICE/DTLS finished — the peer is now sending/receiving.
    Connected,
    /// Nothing actionable this step (an event we don't route).
    Idle,
}

/// SFU participant: a `str0m` [`Rtc`] plus its identity within a call.
pub struct SfuPeer {
    call: CallId,
    id: ParticipantId,
    rtc: Rtc,
}

impl SfuPeer {
    /// Create a peer in **RTP mode** (the SFU forwards raw RTP, no transcode),
    /// anchored to the current monotonic clock.
    #[must_use]
    pub fn new(call: CallId, id: ParticipantId) -> Self {
        Self::with_start(call, id, Instant::now())
    }

    /// Like [`Self::new`] but with an explicit `str0m` start instant (lets the
    /// caller share one clock anchor across all peers; also keeps tests
    /// deterministic). The crypto provider is resolved from the enabled crate
    /// feature (`rust-crypto`) — no global registration required.
    #[must_use]
    pub fn with_start(call: CallId, id: ParticipantId, start: Instant) -> Self {
        let rtc = Rtc::builder().set_rtp_mode(true).build(start);
        Self { call, id, rtc }
    }

    /// Build from a pre-configured [`Rtc`] (tests / custom crypto providers).
    #[must_use]
    pub fn from_rtc(call: CallId, id: ParticipantId, rtc: Rtc) -> Self {
        Self { call, id, rtc }
    }

    #[must_use]
    pub fn id(&self) -> ParticipantId {
        self.id
    }

    #[must_use]
    pub fn call(&self) -> CallId {
        self.call
    }

    /// Whether the underlying `Rtc` is still usable (false after fatal error /
    /// disconnect — the server should drop the peer).
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.rtc.is_alive()
    }

    /// Accept a remote SDP **offer** and produce the SFU's **answer**.
    ///
    /// Used for the standard browser ⇄ SFU negotiation. The offered
    /// transceivers become this peer's inbound/outbound streams.
    pub fn accept_offer(&mut self, offer: &str) -> Result<String, SfuError> {
        let offer = SdpOffer::from_sdp_string(offer)
            .map_err(|e| SfuError::Sdp(e.to_string()))?;
        let answer: SdpAnswer = self
            .rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|e| SfuError::Sdp(e.to_string()))?;
        Ok(answer.to_sdp_string())
    }

    /// Feed an inbound UDP datagram received at `now` from `source` on `dest`.
    pub fn handle_datagram(
        &mut self,
        now: Instant,
        source: SocketAddr,
        dest: SocketAddr,
        data: &[u8],
    ) -> Result<(), SfuError> {
        let recv = Receive::new(Protocol::Udp, source, dest, data)
            .map_err(|e| SfuError::Net(e.to_string()))?;
        self.rtc
            .handle_input(Input::Receive(now, recv))
            .map_err(|e| SfuError::Rtc(e.to_string()))?;
        Ok(())
    }

    /// Advance time without a datagram (fired when a [`PeerProgress::Timeout`]
    /// deadline elapses).
    pub fn handle_timeout(&mut self, now: Instant) -> Result<(), SfuError> {
        self.rtc
            .handle_input(Input::Timeout(now))
            .map_err(|e| SfuError::Rtc(e.to_string()))?;
        Ok(())
    }

    /// Drain one unit of output from the `Rtc` state machine.
    ///
    /// Call in a loop until you get [`PeerProgress::Timeout`], which means the
    /// machine is parked until that instant or the next inbound datagram.
    pub fn poll(&mut self) -> Result<PeerProgress, SfuError> {
        match self.rtc.poll_output().map_err(|e| SfuError::Rtc(e.to_string()))? {
            Output::Timeout(at) => Ok(PeerProgress::Timeout(at)),
            Output::Transmit(t) => Ok(PeerProgress::Transmit(Box::new(t))),
            Output::Event(e) => Ok(self.classify_event(e)),
        }
    }

    /// Resolve the [`Codec`] negotiated for payload type `pt` on this peer.
    ///
    /// Looks up the `Rtc`'s live codec config — `str0m` rewrites it to the
    /// remote's payload-type numbering during SDP negotiation, so the mapping
    /// is correct per-peer even when publishers offer differing pt↔codec
    /// assignments. Unmatched pts (RTX, unnegotiated, …) resolve to
    /// [`Codec::Unknown`], which never reports keyframes.
    #[must_use]
    pub fn codec_for_pt(&self, pt: Pt) -> Codec {
        self.rtc
            .codec_config()
            .find(|p| p.pt() == pt)
            .map_or(Codec::Unknown, |p| Codec::from(p.spec().codec))
    }

    fn classify_event(&self, event: Event) -> PeerProgress {
        match event {
            Event::Connected => PeerProgress::Connected,
            Event::RtpPacket(p) => {
                let codec = self.codec_for_pt(p.header.payload_type);
                PeerProgress::Media(Box::new(InboundRtp::from_packet(&p, codec)))
            }
            Event::KeyframeRequest(kf) => PeerProgress::KeyframeRequest(KeyframeReq {
                mid: kf.mid,
                kind: kf.kind,
            }),
            // Other events (stats, channel data, media-added, ICE state, …) are
            // not part of the forwarding hot path; the server can subscribe to
            // them separately if needed.
            _ => PeerProgress::Idle,
        }
    }

    /// Write a (remapped) RTP packet onto this peer's **outbound** stream for
    /// `mid`. Returns `Ok(false)` if no such outbound stream exists yet (e.g.
    /// the subscriber hasn't negotiated that transceiver) so the caller can skip
    /// without treating it as a hard error.
    #[allow(clippy::too_many_arguments)]
    pub fn write_rtp(
        &mut self,
        mid: Mid,
        pt: Pt,
        seq_no: SeqNo,
        rtp_time: u32,
        wallclock: Instant,
        marker: bool,
        ext_vals: ExtensionValues,
        payload: Vec<u8>,
    ) -> Result<bool, SfuError> {
        let mut api = self.rtc.direct_api();
        let Some(stream) = api.stream_tx_by_mid(mid, None) else {
            return Ok(false);
        };
        stream
            .write_rtp(pt, seq_no, rtp_time, wallclock, marker, ext_vals, true, payload)
            .map_err(|e| SfuError::Rtc(e.to_string()))?;
        Ok(true)
    }

    /// Ask this peer (a publisher) to emit a keyframe on `mid` — relayed from a
    /// downstream subscriber's PLI/FIR. No-op (`Ok(false)`) if the inbound
    /// stream isn't present.
    pub fn request_keyframe(
        &mut self,
        mid: Mid,
        kind: KeyframeRequestKind,
    ) -> Result<bool, SfuError> {
        let mut api = self.rtc.direct_api();
        let Some(stream) = api.stream_rx_by_mid(mid, None) else {
            return Ok(false);
        };
        stream.request_keyframe(kind);
        Ok(true)
    }

    /// Declare an outbound stream so the SFU can forward a publisher's track to
    /// this (subscriber) peer. Mirrors the publisher's codec/`mid`.
    ///
    /// `ssrc`/`rtx` are the SFU-chosen synchronization sources for the
    /// subscriber-facing stream (the remapper rewrites seq/ts into this space).
    pub fn declare_outbound(
        &mut self,
        mid: Mid,
        kind: MediaKind,
        ssrc: u32,
        rtx: Option<u32>,
    ) {
        let mut api = self.rtc.direct_api();
        api.declare_media(mid, kind);
        api.declare_stream_tx(ssrc.into(), rtx.map(Into::into), mid, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_peer_is_alive_and_carries_identity() {
        let call = CallId::new();
        let id = ParticipantId::new();
        let peer = SfuPeer::new(call, id);
        assert!(peer.is_alive());
        assert_eq!(peer.id(), id);
        assert_eq!(peer.call(), call);
    }

    #[test]
    fn rejects_garbage_sdp_offer() {
        let mut peer = SfuPeer::new(CallId::new(), ParticipantId::new());
        let err = peer.accept_offer("not an sdp").unwrap_err();
        assert!(matches!(err, SfuError::Sdp(_)));
    }

    #[test]
    fn codec_for_pt_resolves_default_payload_types() {
        // str0m 0.19 pre-negotiation defaults: VP8=96 (RTX 97), VP9=98,
        // Opus=111, first H.264 config=127. Negotiation rewrites these to the
        // remote's numbering; the lookup path is identical either way.
        let peer = SfuPeer::new(CallId::new(), ParticipantId::new());
        assert_eq!(peer.codec_for_pt(Pt::from(96)), Codec::Vp8);
        assert_eq!(peer.codec_for_pt(Pt::from(98)), Codec::Vp9);
        assert_eq!(peer.codec_for_pt(Pt::from(127)), Codec::H264);
        // Audio is not keyframe-detectable.
        assert_eq!(peer.codec_for_pt(Pt::from(111)), Codec::Unknown);
        // RTX pts live in `resend`, not as primary params.
        assert_eq!(peer.codec_for_pt(Pt::from(97)), Codec::Unknown);
    }

    #[test]
    fn poll_returns_timeout_before_any_input() {
        // A fresh Rtc with no negotiated session parks on a timeout.
        let mut peer = SfuPeer::new(CallId::new(), ParticipantId::new());
        let p = peer.poll().expect("poll ok");
        assert!(
            matches!(p, PeerProgress::Timeout(_) | PeerProgress::Idle),
            "fresh peer should be idle/parked, got {p:?}"
        );
    }
}
