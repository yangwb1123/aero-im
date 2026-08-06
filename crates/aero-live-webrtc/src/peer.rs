//! A single SFU participant backed by a `str0m` [`Rtc`] instance.
//!
//! `str0m` is **sans-IO**: it never touches a socket or a clock itself. The
//! caller drives it by (a) feeding inbound UDP datagrams + the current time via
//! [`Rtc::handle_input`], and (b) draining [`Rtc::poll_output`] for packets to
//! send, the next wake-up deadline, and media/connection events. [`SfuPeer`]
//! wraps that contract behind a small, server-friendly API so the owning UDP
//! task can stay a thin loop:
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
use str0m::bwe::{Bitrate, BweKind};
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::media::{KeyframeRequestKind, MediaKind, Mid, Pt, Rid};
use str0m::net::{Protocol, Receive};
use str0m::rtp::{ExtensionValues, RtpPacket, SeqNo, Ssrc};
use str0m::{Candidate, Event, Input, Output, Rtc};

use crate::codec::{payload_is_keyframe, Codec};
use crate::SfuError;

/// Canonical form str0m uses for an SDP media id.
///
/// Route keys must use this form too: str0m normalizes some token characters
/// (for example `publisher-mid` becomes `publisher_mid`) before surfacing RTP.
#[must_use]
pub fn canonical_mid(value: &str) -> String {
    Mid::from(value).to_string()
}

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
    /// Synchronization source from the publisher-facing RTP header.
    ///
    /// Local forwarding rewrites packets onto each subscriber's declared SSRC,
    /// but the source value must survive a node-to-node bridge so RTCP,
    /// diagnostics, and future SSRC-aware routing can still attribute the packet.
    pub ssrc: str0m::rtp::Ssrc,
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
    /// Whether a newly attached cross-node bridge must wait for
    /// [`Self::is_keyframe`] before forwarding this track.
    ///
    /// This is `true` for the video codecs understood by the SFU and `false`
    /// for audio/unknown codecs, which have no intra-frame concept.
    pub requires_keyframe: bool,
}

impl InboundRtp {
    fn from_packet(p: &RtpPacket, codec: Codec, mid: Mid, rid: Option<Rid>) -> Self {
        // Codec-aware keyframe detection from the raw payload; unknown codecs
        // are never keyframes (see `payload_is_keyframe` docs).
        let is_keyframe = payload_is_keyframe(codec, &p.payload);
        let mut ext_vals = p.header.ext_vals.clone();
        ext_vals.mid = Some(mid);
        ext_vals.rid = rid;
        Self {
            mid,
            pt: p.header.payload_type,
            seq_no: p.seq_no,
            rtp_time: p.header.timestamp,
            ssrc: p.header.ssrc,
            marker: p.header.marker,
            rid,
            is_keyframe,
            requires_keyframe: codec.requires_keyframe(),
            ext_vals,
            wallclock: p.timestamp,
            payload: p.payload.clone(),
        }
    }
}

/// Last-resort MID for a packet that neither carries a MID extension nor maps
/// to one of str0m's negotiated receive streams.
fn mid_fallback() -> Mid {
    Mid::from("0")
}

/// A keyframe (PLI/FIR) request bubbled up from a subscriber — the forwarder
/// relays it to the upstream publisher so a fresh I-frame is produced.
#[derive(Debug, Clone, Copy)]
pub struct KeyframeReq {
    pub mid: Mid,
    pub kind: KeyframeRequestKind,
}

/// Receiver bandwidth estimate bubbled up from a subscriber.
#[derive(Debug, Clone, Copy)]
pub struct BandwidthEstimate {
    /// REMB is scoped to one outbound MID; TWCC estimates the whole peer
    /// connection and therefore has no MID.
    pub mid: Option<Mid>,
    pub bitrate_bps: u64,
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
    /// REMB/TWCC from a subscriber; aggregate and relay it upstream.
    BandwidthEstimate(BandwidthEstimate),
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
        let offer = SdpOffer::from_sdp_string(offer).map_err(|e| SfuError::Sdp(e.to_string()))?;
        let answer: SdpAnswer = self
            .rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|e| SfuError::Sdp(e.to_string()))?;
        Ok(answer.to_sdp_string())
    }

    /// Add the UDP host candidate advertised in this peer's SDP answer.
    ///
    /// This must run before [`Self::accept_offer`], because str0m snapshots
    /// already-known local candidates into the generated answer.
    pub fn add_local_candidate(&mut self, addr: SocketAddr) -> Result<(), SfuError> {
        let candidate = Candidate::host(addr, "udp").map_err(|e| SfuError::Net(e.to_string()))?;
        self.rtc.add_local_candidate(candidate);
        Ok(())
    }

    /// Add one browser trickle-ICE candidate.
    ///
    /// Browsers normally send the `RTCIceCandidate.candidate` attribute value
    /// (`candidate:...`); a few clients include the leading `a=`, which is
    /// accepted for parity with `aero-signaling`'s validation.
    pub fn add_remote_candidate(&mut self, candidate: &str) -> Result<(), SfuError> {
        let candidate = candidate
            .trim()
            .strip_prefix("a=")
            .unwrap_or(candidate.trim());
        let candidate =
            Candidate::from_sdp_string(candidate).map_err(|e| SfuError::Net(e.to_string()))?;
        self.rtc.add_remote_candidate(candidate);
        Ok(())
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
        match self
            .rtc
            .poll_output()
            .map_err(|e| SfuError::Rtc(e.to_string()))?
        {
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

    fn classify_event(&mut self, event: Event) -> PeerProgress {
        match event {
            Event::Connected => PeerProgress::Connected,
            Event::RtpPacket(p) => {
                let codec = self.codec_for_pt(p.header.payload_type);
                let (mid, rid) = self.resolve_packet_route(
                    p.header.ssrc,
                    p.header.ext_vals.mid,
                    p.header.ext_vals.rid,
                );
                PeerProgress::Media(Box::new(InboundRtp::from_packet(&p, codec, mid, rid)))
            }
            Event::KeyframeRequest(kf) => PeerProgress::KeyframeRequest(KeyframeReq {
                mid: kf.mid,
                kind: kf.kind,
            }),
            Event::EgressBitrateEstimate(estimate) => match estimate {
                BweKind::Remb(mid, bitrate) => PeerProgress::BandwidthEstimate(BandwidthEstimate {
                    mid: Some(mid),
                    bitrate_bps: bitrate.as_u64(),
                }),
                BweKind::Twcc(bitrate) => PeerProgress::BandwidthEstimate(BandwidthEstimate {
                    mid: None,
                    bitrate_bps: bitrate.as_u64(),
                }),
                _ => PeerProgress::Idle,
            },
            // Other events (stats, channel data, media-added, ICE state, …) are
            // not part of the forwarding hot path; the server can subscribe to
            // them separately if needed.
            _ => PeerProgress::Idle,
        }
    }

    fn resolve_packet_route(
        &mut self,
        ssrc: Ssrc,
        media_mid: Option<Mid>,
        stream_rid: Option<Rid>,
    ) -> (Mid, Option<Rid>) {
        // str0m already owns the bounded SSRC lifecycle and normalizes RTX to
        // its primary stream. Reuse that authoritative mapping when browsers
        // stop repeating MID/RID extensions instead of growing a parallel map.
        let negotiated = self
            .rtc
            .direct_api()
            .stream_rx(&ssrc)
            .map(|stream| (stream.mid(), stream.rid()));
        (
            media_mid
                .or_else(|| negotiated.map(|(mid, _)| mid))
                .unwrap_or_else(mid_fallback),
            stream_rid.or_else(|| negotiated.and_then(|(_, rid)| rid)),
        )
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
            .write_rtp(
                pt, seq_no, rtp_time, wallclock, marker, ext_vals, true, payload,
            )
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
        self.request_keyframe_for_rid(mid, None, kind)
    }

    /// Ask one exact simulcast receive stream to emit a keyframe. `rid=None`
    /// retains the historical MID-scoped behavior for non-simulcast callers.
    pub fn request_keyframe_for_rid(
        &mut self,
        mid: Mid,
        rid: Option<Rid>,
        kind: KeyframeRequestKind,
    ) -> Result<bool, SfuError> {
        let mut api = self.rtc.direct_api();
        let Some(stream) = api.stream_rx_by_mid(mid, rid) else {
            return Ok(false);
        };
        stream.request_keyframe(kind);
        Ok(true)
    }

    /// Ask this publisher to cap one inbound stream at `bitrate_bps` by
    /// emitting REMB toward the browser. Returns `Ok(false)` when `mid` is not
    /// an active receive stream on this peer.
    pub fn request_remb(&mut self, mid: Mid, bitrate_bps: u64) -> Result<bool, SfuError> {
        let mut api = self.rtc.direct_api();
        let Some(stream) = api.stream_rx_by_mid(mid, None) else {
            return Ok(false);
        };
        stream.request_remb(Bitrate::bps(bitrate_bps));
        Ok(true)
    }

    /// Declare an outbound stream so the SFU can forward a publisher's track to
    /// this (subscriber) peer. Mirrors the publisher's codec/`mid`.
    ///
    /// `ssrc`/`rtx` are the SFU-chosen synchronization sources for the
    /// subscriber-facing stream (the remapper rewrites seq/ts into this space).
    pub fn declare_outbound(&mut self, mid: Mid, kind: MediaKind, ssrc: u32, rtx: Option<u32>) {
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
    fn accepts_local_and_trickled_remote_candidates() {
        let mut peer = SfuPeer::new(CallId::new(), ParticipantId::new());
        peer.add_local_candidate("127.0.0.1:5000".parse().unwrap())
            .expect("valid local host candidate");
        peer.add_remote_candidate("a=candidate:1 1 udp 2113937151 127.0.0.1 5001 typ host")
            .expect("valid remote trickle candidate");
        assert!(peer
            .add_remote_candidate("definitely-not-a-candidate")
            .is_err());
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
    fn negotiated_stream_restores_mid_and_rid_for_extensionless_packets() {
        let mut peer = SfuPeer::new(CallId::new(), ParticipantId::new());
        let video_ssrc = Ssrc::from(0x0102_0304);
        let track_mid = Mid::from("video-track");
        let payload_rid = Rid::from("high");
        {
            let mut api = peer.rtc.direct_api();
            api.declare_media(track_mid, MediaKind::Video);
            api.expect_stream_rx(video_ssrc, None, track_mid, Some(payload_rid));
        }

        assert_eq!(
            peer.resolve_packet_route(video_ssrc, None, None),
            (track_mid, Some(payload_rid)),
            "str0m's bounded stream mapping restores omitted routing extensions"
        );
        assert_eq!(
            peer.resolve_packet_route(
                video_ssrc,
                Some(Mid::from("explicit")),
                Some(Rid::from("low"))
            ),
            (Mid::from("explicit"), Some(Rid::from("low"))),
            "packet extensions remain authoritative when present"
        );
        assert_eq!(
            peer.resolve_packet_route(Ssrc::from(0x0506_0708), None, None),
            (Mid::from("0"), None),
            "an unseen malformed SSRC still uses the total fallback"
        );
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

    #[test]
    fn negotiated_publisher_stream_accepts_pli_and_remb_requests() {
        const OFFER: &str = "v=0\r\n\
o=- 1 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
c=IN IP4 0.0.0.0\r\n\
a=ice-ufrag:abcd\r\n\
a=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
a=fingerprint:sha-256 11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=sendonly\r\n\
a=rtcp-mux\r\n\
a=rtpmap:96 VP8/90000\r\n\
a=rtcp-fb:96 nack pli\r\n\
a=rtcp-fb:96 goog-remb\r\n";
        let mut peer = SfuPeer::new(CallId::new(), ParticipantId::new());
        peer.add_local_candidate("127.0.0.1:5000".parse().unwrap())
            .unwrap();
        peer.accept_offer(OFFER).unwrap();
        peer.rtc
            .direct_api()
            .expect_stream_rx(123_u32.into(), None, Mid::from("0"), None);
        peer.rtc.direct_api().expect_stream_rx(
            124_u32.into(),
            None,
            Mid::from("0"),
            Some(Rid::from("high")),
        );

        assert!(peer
            .request_keyframe(Mid::from("0"), KeyframeRequestKind::Pli)
            .unwrap());
        assert!(peer
            .request_keyframe_for_rid(
                Mid::from("0"),
                Some(Rid::from("high")),
                KeyframeRequestKind::Pli
            )
            .unwrap());
        assert!(!peer
            .request_keyframe_for_rid(
                Mid::from("0"),
                Some(Rid::from("missing")),
                KeyframeRequestKind::Pli
            )
            .unwrap());
        assert!(peer.request_remb(Mid::from("0"), 750_000).unwrap());
        assert!(!peer
            .request_keyframe(Mid::from("missing"), KeyframeRequestKind::Pli)
            .unwrap());
        assert!(!peer.request_remb(Mid::from("missing"), 750_000).unwrap());
    }
}
