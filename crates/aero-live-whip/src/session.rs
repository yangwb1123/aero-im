//! str0m-driven WHIP publisher session: the sans-IO WebRTC media plane.
//!
//! [`WhipSession`] owns a str0m [`Rtc`] for one publisher. It exposes:
//!
//! - [`WhipSession::accept`] — parse the browser's SDP **offer** with str0m and
//!   produce a **real** SDP answer (DTLS fingerprint, ICE ufrag/pwd, and a host
//!   ICE candidate that str0m itself generates). This is what replaces the old
//!   hand-rolled answer string.
//! - [`WhipSession::run`] — the standard str0m event loop: drain
//!   [`Rtc::poll_output`] (transmitting datagrams over a UDP socket and handling
//!   timeouts), and pump received UDP datagrams + timeouts back in via
//!   [`Rtc::handle_input`]. The session is built in str0m **RTP mode**, so
//!   incoming media surfaces as [`Event::RtpPacket`]; H.264 RTP payloads are run
//!   through the RFC 6184 [`H264Depacketizer`] into Annex-B access units and
//!   forwarded to a [`MediaSink`].
//!
//! In str0m's default sample mode, `Event::MediaData` hands you an
//! already-depacketized frame, which would make our RFC 6184 depacketizer
//! redundant. Building with `set_rtp_mode(true)` makes str0m emit raw
//! [`RtpPacket`]s instead, so the depacketizer genuinely reassembles NAL units
//! from the publisher's RTP — exactly the boundary this crate owns.
//!
//! Inbound RTP packets are passed through a [`ReorderBuffer`] before reaching
//! the depacketizer. The buffer holds packets that arrived out of order and
//! drains them strictly in sequence-number order. The default jitter window is
//! [`DEFAULT_REORDER_WINDOW`] (64 sequence-number slots). Packets that arrive
//! more than `window` slots behind the drain cursor are silently dropped
//! (duplicate / too-late); a gap wider than `window` is declared lost and the
//! depacketizer is reset (resync on the next keyframe). The buffer is
//! initialized lazily on the first received RTP packet so the first packet's
//! seq is the start of the window — browsers choose a random initial seq, so
//! starting at 0 would stall the drain until the window was exceeded.
//!
//! The transport is covered by an in-process two-peer ICE/DTLS/SRTP test; real
//! browser/device and network behavior remains a staging acceptance boundary.

use std::net::SocketAddr;
use std::time::Instant;

use bytes::BytesMut;
use str0m::change::SdpAnswer;
use str0m::media::Pt;
use str0m::net::{Protocol, Receive};
use str0m::rtp::RtpPacket;
use str0m::{Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tracing::{debug, trace, warn};

use std::sync::Arc;

use crate::depacketize::H264Depacketizer;
use crate::hls_sink::MediaSink;
use crate::metrics;
use crate::relay::MediaRelay;
use crate::reorder::ReorderBuffer;

#[path = "session/hls.rs"]
mod hls;
#[path = "session/support.rs"]
mod support;

use support::{media_time_to_90k, RECV_BUF};
pub use support::{SessionError, DEFAULT_REORDER_WINDOW};

/// A live WHIP publisher session backed by a str0m `Rtc`.
///
/// Construct with [`WhipSession::accept`]; the returned [`SdpAnswer`] is sent
/// back to the browser over HTTP, then [`WhipSession::run`] is spawned on the
/// UDP socket bound to the advertised ingest address.
///
/// Optionally, attach a [`MediaRelay`] with [`with_relay`](Self::with_relay)
/// before calling `run`. When a relay is attached every depacketized H.264
/// access unit is published to it (in addition to the [`MediaSink`]) so that
/// WHEP subscribers can receive the live stream via a relay-backed
/// [`Subscription`](crate::relay::Subscription).
pub struct WhipSession {
    rtc: Rtc,
    /// The address str0m advertised as its host candidate; the UDP socket the
    /// server binds for `run` must be reachable at this address.
    local_addr: SocketAddr,
    /// Payload types negotiated as video. Used to route inbound RTP to the
    /// H.264 depacketizer in RTP mode (where packets carry only a PT, not a
    /// media kind). Captured at `accept` time since negotiation is complete.
    video_pts: Vec<Pt>,
    /// Optional relay hub: when `Some`, each depacketized video access unit is
    /// also published here so WHEP subscribers can receive it. `None` → no
    /// relay tap (existing behaviour unchanged).
    relay: Option<Arc<MediaRelay>>,
}

impl WhipSession {
    /// Parse a WHIP SDP offer and build the session + a real SDP answer.
    ///
    /// `ingest_host`/`ingest_port` are advertised as str0m's host ICE
    /// candidate; the server's UDP socket for [`run`](Self::run) must be bound
    /// to that address so the browser's ICE checks reach us.
    ///
    /// Returns the session (owning the `Rtc`) and the negotiated answer.
    pub fn accept(
        offer_sdp: &str,
        ingest_host: &str,
        ingest_port: u16,
    ) -> Result<(Self, SdpAnswer), SessionError> {
        let support::AcceptedSession {
            rtc,
            local_addr,
            video_pts,
            answer,
        } = support::accept(offer_sdp, ingest_host, ingest_port)?;

        Ok((
            Self {
                rtc,
                local_addr,
                video_pts,
                relay: None,
            },
            answer,
        ))
    }

    /// The host address str0m advertised; bind the UDP socket here before
    /// calling [`run`](Self::run).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// True while the underlying `Rtc` is still alive (ICE/DTLS not torn down).
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.rtc.is_alive()
    }

    /// The payload types negotiated as video (for tests/observability).
    #[must_use]
    pub fn video_payload_types(&self) -> &[Pt] {
        &self.video_pts
    }

    /// Attach a [`MediaRelay`] so that every depacketized H.264 access unit is
    /// also published to it (in addition to the [`MediaSink`]).
    ///
    /// This is purely additive — sessions without a relay behave exactly as
    /// before. The relay is shared via `Arc` so the caller can keep a handle
    /// to add subscribers at any time.
    ///
    /// Call before [`run`](Self::run) or [`run_to_hls`](Self::run_to_hls).
    #[must_use]
    pub fn with_relay(mut self, relay: Arc<MediaRelay>) -> Self {
        self.relay = Some(relay);
        self
    }

    /// Run the str0m event loop until the connection closes or errors.
    ///
    /// This is the canonical sans-IO driver:
    /// 1. Drain [`Rtc::poll_output`]. `Transmit` → `socket.send_to`; `Timeout`
    ///    → arm a sleep; `Event` → handle (RTP media, ICE state, ...).
    /// 2. Either a UDP datagram arrives (feed `Input::Receive`) or the timeout
    ///    fires (feed `Input::Timeout`), then loop.
    ///
    /// Received H.264 RTP is reordered through a [`ReorderBuffer`] (window =
    /// [`DEFAULT_REORDER_WINDOW`]) before being depacketized into Annex-B access
    /// units and pushed to `sink`. The `socket` must be bound to
    /// [`local_addr`](Self::local_addr).
    pub async fn run(
        mut self,
        socket: UdpSocket,
        mut sink: impl MediaSink,
    ) -> Result<(), SessionError> {
        let mut depacketizer = H264Depacketizer::new();
        let mut buf = vec![0u8; RECV_BUF];
        // Accumulates one access unit's worth of Annex-B NAL units.
        let mut au = BytesMut::new();
        // Lazily initialized on the first RTP packet so the buffer starts at the
        // stream's actual initial sequence number (browsers choose a random seq).
        let mut reorder_buf: Option<ReorderBuffer<RtpPacket>> = None;

        loop {
            // 1) Drain everything str0m wants to emit until it asks for input.
            let timeout = loop {
                match self.rtc.poll_output()? {
                    Output::Timeout(t) => break t,
                    Output::Transmit(t) => {
                        // DatagramSend derefs to [u8].
                        if let Err(e) = socket.send_to(&t.contents, t.destination).await {
                            warn!(error = %e, dest = %t.destination, "whip: udp send failed");
                        }
                    }
                    Output::Event(ev) => {
                        match ev {
                            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                                debug!("whip: ice disconnected, ending session");
                                return Ok(());
                            }
                            Event::IceConnectionStateChange(state) => {
                                debug!(?state, "whip: ice state change");
                            }
                            Event::RtpPacket(pkt) => {
                                metrics::record_rtp_packet();
                                let seq = pkt.header.sequence_number;
                                // Lazily initialize the buffer at the first
                                // observed sequence number so we don't stall.
                                let rbuf = reorder_buf.get_or_insert_with(|| {
                                    ReorderBuffer::with_start(seq, DEFAULT_REORDER_WINDOW)
                                });
                                let video_pts = &self.video_pts;
                                let relay_ref = self.relay.as_deref();
                                rbuf.push(seq, pkt, &mut |_s, maybe_pkt| {
                                    if let Some(p) = maybe_pkt {
                                        // Packet delivered in order — depacketize.
                                        Self::depacketize_rtp(
                                            video_pts,
                                            &p,
                                            &mut depacketizer,
                                            &mut au,
                                            &mut sink,
                                            relay_ref,
                                        );
                                    } else {
                                        // Gap declared lost: reset depacketizer so a
                                        // stale FU-A prefix never corrupts the next
                                        // fragment. The next IDR keyframe will resync.
                                        trace!(
                                            "whip: rtp gap declared lost, resyncing depacketizer"
                                        );
                                        depacketizer.reset();
                                        au.clear();
                                    }
                                });
                            }
                            _ => {}
                        }
                        if !self.rtc.is_alive() {
                            debug!("whip: rtc no longer alive, ending session");
                            return Ok(());
                        }
                    }
                }
            };

            // 2) Wait for the next input: a datagram or the str0m timeout.
            let wait = timeout.saturating_duration_since(Instant::now());

            tokio::select! {
                res = socket.recv_from(&mut buf) => {
                    let (n, source) = res?;
                    // Parse the datagram; drop (don't kill the session on) any
                    // unparseable STUN/DTLS/RTP framing.
                    let Ok(contents) = (&buf[..n]).try_into() else {
                        trace!("whip: dropping unparseable datagram");
                        continue;
                    };
                    let receive = Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: self.local_addr,
                        contents,
                    };
                    self.rtc.handle_input(Input::Receive(Instant::now(), receive))?;
                }
                () = tokio::time::sleep(wait) => {
                    self.rtc.handle_input(Input::Timeout(Instant::now()))?;
                }
            }
        }
    }

    /// Route one in-order RTP packet to the depacketizer / sink, and
    /// optionally to a [`MediaRelay`] tap.
    ///
    /// This is extracted as a plain associated function (no `&self` receiver) so
    /// the reorder-buffer drain closure can call it while holding only a
    /// reference to `video_pts` — avoiding a conflicting `self` borrow in the
    /// closure.
    ///
    /// When `relay` is `Some`, every completed video access unit is also
    /// published to it (additive — the sink path is unchanged).
    fn depacketize_rtp(
        video_pts: &[Pt],
        pkt: &RtpPacket,
        depacketizer: &mut H264Depacketizer,
        au: &mut BytesMut,
        sink: &mut impl MediaSink,
        relay: Option<&MediaRelay>,
    ) {
        let pt = pkt.header.payload_type;
        let marker = pkt.header.marker;

        if video_pts.contains(&pt) {
            if let Err(e) = depacketizer.push(&pkt.payload, marker, au) {
                // Reassembly desync (packet loss / reorder): drop the partial
                // AU and resync on the next keyframe.
                metrics::record_depacketize_failure();
                trace!(error = %e, "whip: h264 depacketize error, resyncing");
                depacketizer.reset();
                au.clear();
                return;
            }
            // The RTP marker bit terminates an H.264 access unit (RFC 6184 §5.1).
            if marker && !au.is_empty() {
                let frame = au.split().freeze();
                let pts_90k = media_time_to_90k(pkt.time);
                // Optional relay tap: publish the AU to WHEP subscribers.
                // Done before the sink call so the relay gets the data even if
                // the sink errors.
                if let Some(r) = relay {
                    r.publish(frame.clone(), pts_90k);
                }
                if let Err(e) = sink.on_video_au(frame, pts_90k) {
                    warn!(error = %e, "whip: media sink rejected video AU");
                }
            }
        } else {
            // Audio (Opus) hand-off is not part of the H.264 → HLS path tackled
            // in P5; surface the raw payload for the sink to handle or ignore.
            let pts_90k = media_time_to_90k(pkt.time);
            if let Err(e) = sink.on_audio(pkt.payload.clone().into(), pts_90k) {
                trace!(error = %e, "whip: media sink rejected audio");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use str0m::Candidate;

    /// A minimal but real WHIP-style publisher offer (sendonly audio+video,
    /// bundled, rtcp-mux, with ICE + DTLS attributes) that str0m can parse.
    pub(super) const PUBLISHER_OFFER: &str = "v=0\r\n\
o=- 4611731400430051336 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0 1\r\n\
a=msid-semantic: WMS\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:abcd\r\n\
a=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
a=fingerprint:sha-256 11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=sendonly\r\n\
a=rtcp-mux\r\n\
a=rtpmap:111 opus/48000/2\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:abcd\r\n\
a=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
a=fingerprint:sha-256 11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00\r\n\
a=setup:actpass\r\n\
a=mid:1\r\n\
a=sendonly\r\n\
a=rtcp-mux\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n";

    #[test]
    fn accept_produces_real_answer_from_str0m() {
        let (session, answer) =
            WhipSession::accept(PUBLISHER_OFFER, "127.0.0.1", 7000).expect("accept offer");
        let sdp = answer.to_sdp_string();
        // A str0m-generated answer must carry the WebRTC plumbing the browser
        // needs to start ICE/DTLS.
        assert!(sdp.starts_with("v=0"), "answer must be valid SDP:\n{sdp}");
        assert!(sdp.contains("a=ice-ufrag:"), "missing ice-ufrag:\n{sdp}");
        assert!(sdp.contains("a=ice-pwd:"), "missing ice-pwd:\n{sdp}");
        assert!(
            sdp.contains("a=fingerprint:sha-256"),
            "missing DTLS fingerprint:\n{sdp}"
        );
        // The answer should mirror both m-sections from the offer.
        assert!(sdp.contains("m=audio"), "missing audio m-line:\n{sdp}");
        assert!(sdp.contains("m=video"), "missing video m-line:\n{sdp}");
        // str0m answers the publisher's sendonly with recvonly.
        assert!(
            sdp.contains("a=recvonly"),
            "expected recvonly answer:\n{sdp}"
        );
        // The host candidate we added points at the ingest address.
        assert!(
            sdp.contains("127.0.0.1") && sdp.contains("7000"),
            "expected host candidate for ingest addr:\n{sdp}"
        );
        assert_eq!(session.local_addr(), "127.0.0.1:7000".parse().unwrap());
        assert!(session.is_alive());
    }

    #[test]
    fn accept_classifies_h264_payload_type_as_video() {
        let (session, _answer) =
            WhipSession::accept(PUBLISHER_OFFER, "127.0.0.1", 7001).expect("accept offer");
        // The offer negotiates H.264; at least one video PT must be recorded so
        // the run loop routes inbound video RTP to the depacketizer.
        assert!(
            !session.video_payload_types().is_empty(),
            "expected at least one negotiated video payload type"
        );
        // PT 96 was offered for H.264; str0m should keep it.
        let has_96 = session.video_payload_types().iter().any(|pt| **pt == 96);
        assert!(has_96, "expected H.264 PT 96 among video PTs");
    }

    #[test]
    fn accept_rejects_unparseable_offer() {
        // `accept` returns a non-Debug tuple on success, so match rather than
        // `unwrap_err` (which would require `Debug` on the Ok variant).
        let Err(err) = WhipSession::accept("not an sdp", "127.0.0.1", 7000) else {
            panic!("expected an error for unparseable offer");
        };
        assert!(matches!(err, SessionError::Offer(_)));
    }

    #[test]
    fn accept_rejects_bad_ingest_addr() {
        let Err(err) = WhipSession::accept(PUBLISHER_OFFER, "not-an-ip", 7000) else {
            panic!("expected an error for a bad ingest address");
        };
        assert!(matches!(err, SessionError::Addr(_, _)));
    }

    #[test]
    fn media_time_converts_to_90k() {
        // Sanity on the PTS conversion helper used in the run loop. 1.5s → 135000.
        let mt = str0m::media::MediaTime::from_seconds(1.5);
        assert_eq!(media_time_to_90k(mt), 135_000);
    }

    // =================== Browser-free RTP → HLS E2E ===================
    //
    // Proves the *byte-level* media path the str0m loop drives once an RTP
    // packet has arrived: RTP payload → H264Depacketizer (Annex-B AU) → HlsSink
    // (avcC synthesis + FlvToTsConverter mux + keyframe segment cut) → HlsWriter
    // (.ts + index.m3u8 on disk). No ICE/DTLS/SRTP and no browser — those still
    // need a live publisher (see the crate/session docs); everything *after* a
    // received RTP packet is exercised here.

    use crate::depacketize::H264Depacketizer;
    use crate::hls_sink::{hls_sink, MediaSink};

    /// 4-byte Annex-B start code, for asserting on reassembled bytes.
    const SC: [u8; 4] = [0, 0, 0, 1];

    /// Replicates [`WhipSession::on_rtp`]'s video path against a raw RTP payload:
    /// push it through the *real* depacketizer, and on the marker bit flush the
    /// completed Annex-B access unit to `sink` with a 90 kHz PTS. This is the
    /// exact reassembly + AU-boundary logic the run loop performs (str0m hands us
    /// the payload + marker), minus the network transport.
    fn feed_rtp(
        depack: &mut H264Depacketizer,
        au: &mut BytesMut,
        sink: &mut impl MediaSink,
        payload: &[u8],
        marker: bool,
        pts_90k: u64,
    ) {
        depack.push(payload, marker, au).expect("depacketize");
        if marker && !au.is_empty() {
            let frame = au.split().freeze();
            sink.on_video_au(frame, pts_90k).expect("sink accepts AU");
        }
    }

    /// A STAP-A RTP payload aggregating the given whole NAL units (each = header
    /// byte + body). RFC 6184 §5.7.1: `[STAP-A hdr][ (u16 size)(NAL) ]+`.
    fn stap_a(nals: &[&[u8]]) -> Vec<u8> {
        let mut p = vec![0x78u8]; // F=0, NRI=3, type=24 (STAP-A)
        for nal in nals {
            p.extend_from_slice(&u16::try_from(nal.len()).unwrap().to_be_bytes());
            p.extend_from_slice(nal);
        }
        p
    }

    /// Fragment a whole NAL unit (header byte + body) into `chunks` FU-A packets
    /// (RFC 6184 §5.8), exercising the depacketizer's reassembly. Returns the
    /// per-packet payloads in order (Start … Middle … End).
    fn fu_a(nal_header: u8, body: &[u8], chunks: usize) -> Vec<Vec<u8>> {
        assert!(chunks >= 2, "need at least start+end");
        let fu_indicator = (nal_header & 0xE0) | 28; // keep F|NRI, type=28
        let fu_type = nal_header & 0x1F;
        let per = body.len().div_ceil(chunks).max(1);
        let mut out = Vec::new();
        let mut i = 0;
        let mut idx = 0;
        while i < body.len() {
            let end_byte = (i + per).min(body.len());
            let is_first = idx == 0;
            let is_last = end_byte >= body.len();
            let mut fu_header = fu_type;
            if is_first {
                fu_header |= 0x80; // Start
            }
            if is_last {
                fu_header |= 0x40; // End
            }
            let mut pkt = vec![fu_indicator, fu_header];
            pkt.extend_from_slice(&body[i..end_byte]);
            out.push(pkt);
            i = end_byte;
            idx += 1;
        }
        out
    }

    #[tokio::test]
    async fn rtp_h264_to_hls_writes_real_segments_and_manifest() {
        // A minimal-but-structurally-valid SPS: header 0x67, then
        // profile_idc/constraint/level (0x42,0x00,0x1F) + a couple RBSP bytes.
        let sps: &[u8] = &[0x67, 0x42, 0x00, 0x1F, 0xAC, 0xD9];
        let pps: &[u8] = &[0x68, 0xCE, 0x3C, 0x80];

        let dir = crate::testutil::TempDir::new().unwrap();
        let stream_dir = dir.path().join("01HWHIPSTREAMID");
        let (mut sink, writer) = hls_sink(stream_dir.clone(), 2).await.unwrap();
        let writer_task = tokio::spawn(writer.run());

        let mut depack = H264Depacketizer::new();
        let mut au = BytesMut::new();

        // ---- Access unit 1 (keyframe): STAP-A(SPS,PPS) + FU-A(IDR) ----
        // Parameter sets arrive aggregated (no marker — same AU continues).
        feed_rtp(
            &mut depack,
            &mut au,
            &mut sink,
            &stap_a(&[sps, pps]),
            false,
            0,
        );
        // IDR slice fragmented across 3 FU-A packets; the last carries the marker
        // bit that terminates the access unit.
        let idr_body = [0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99];
        let frags = fu_a(0x65, &idr_body, 3); // 0x65 = IDR slice NAL header
        let n = frags.len();
        for (k, frag) in frags.iter().enumerate() {
            let last = k == n - 1;
            feed_rtp(&mut depack, &mut au, &mut sink, frag, last, 0);
        }
        assert!(!depack.is_reassembling(), "FU-A fully reassembled");
        assert_eq!(sink.video_aus(), 1, "one keyframe AU so far");

        // ---- Access units 2-4 (non-IDR P-frames), single-NAL packets ----
        // 0x41 = non-IDR coded slice (NRI=2, type 1). Each is its own AU (marker
        // set), with increasing PTS so segment durations are positive.
        for (i, pts) in [(1u8, 3_000u64), (2, 6_000), (3, 9_000)] {
            let nal = [0x41u8, 0xA0 | i, 0xB0, 0xC0];
            feed_rtp(&mut depack, &mut au, &mut sink, &nal, true, pts);
        }
        assert_eq!(sink.video_aus(), 4, "1 keyframe + 3 P-frames");

        // End of stream: drop the sink so the writer flushes the trailing
        // (single) GOP and finalizes the manifest.
        drop(sink);
        let segments = writer_task.await.unwrap().unwrap();

        // ---- Assert real HLS output on disk ----
        assert!(
            segments >= 1,
            "at least one .ts segment persisted, got {segments}"
        );
        let seg0 = stream_dir.join("0.ts");
        assert!(seg0.exists(), "0.ts must exist on disk");
        let ts_bytes = std::fs::read(&seg0).unwrap();
        assert!(!ts_bytes.is_empty(), "segment file is non-empty");
        assert_eq!(ts_bytes.len() % 188, 0, "whole 188-byte MPEG-TS packets");
        assert_eq!(ts_bytes[0], 0x47, "TS sync byte at start (PAT)");
        assert_eq!(ts_bytes[188], 0x47, "TS sync byte for PMT packet");

        let manifest_path = stream_dir.join("index.m3u8");
        let manifest = std::fs::read_to_string(&manifest_path).unwrap();
        assert!(!manifest.is_empty(), "manifest is non-empty");
        assert!(
            manifest.starts_with("#EXTM3U"),
            "manifest header:\n{manifest}"
        );
        assert!(
            manifest.contains("#EXT-X-VERSION:3"),
            "manifest:\n{manifest}"
        );
        assert!(
            manifest.contains("#EXT-X-TARGETDURATION:2"),
            "manifest:\n{manifest}"
        );
        assert!(
            manifest.contains("#EXTINF:"),
            "per-segment duration:\n{manifest}"
        );
        assert!(
            manifest.contains("0.ts"),
            "segment name in manifest:\n{manifest}"
        );
        assert!(
            manifest.contains("#EXT-X-ENDLIST"),
            "finalized:\n{manifest}"
        );
    }

    #[tokio::test]
    async fn rtp_multi_gop_cuts_segment_at_second_keyframe() {
        // Two keyframes separated by a P-frame: the depacketizer + sink must cut
        // the first segment at the *second* keyframe (every segment starts on a
        // keyframe), so two .ts files land on disk.
        let sps: &[u8] = &[0x67, 0x42, 0x00, 0x1F, 0xAC, 0xD9];
        let pps: &[u8] = &[0x68, 0xCE, 0x3C, 0x80];

        let dir = crate::testutil::TempDir::new().unwrap();
        let stream_dir = dir.path().join("multi-gop");
        let (mut sink, writer) = hls_sink(stream_dir.clone(), 2).await.unwrap();
        let writer_task = tokio::spawn(writer.run());

        let mut depack = H264Depacketizer::new();
        let mut au = BytesMut::new();

        // GOP 1: SPS+PPS+IDR (single-NAL IDR this time, marker terminates AU).
        feed_rtp(
            &mut depack,
            &mut au,
            &mut sink,
            &stap_a(&[sps, pps]),
            false,
            0,
        );
        feed_rtp(
            &mut depack,
            &mut au,
            &mut sink,
            &[0x65, 0x01, 0x02, 0x03],
            true,
            0,
        );
        // A P-frame.
        feed_rtp(
            &mut depack,
            &mut au,
            &mut sink,
            &[0x41, 0x04, 0x05],
            true,
            3_000,
        );
        // GOP 2: SPS+PPS+IDR → cuts GOP 1 into 0.ts before muxing this keyframe.
        feed_rtp(
            &mut depack,
            &mut au,
            &mut sink,
            &stap_a(&[sps, pps]),
            false,
            6_000,
        );
        feed_rtp(
            &mut depack,
            &mut au,
            &mut sink,
            &[0x65, 0x06, 0x07],
            true,
            6_000,
        );
        assert_eq!(sink.segments_emitted(), 1, "one cut at the 2nd keyframe");

        drop(sink);
        let segments = writer_task.await.unwrap().unwrap();
        assert_eq!(segments, 2, "GOP1 (cut) + GOP2 (flushed on close)");
        assert!(stream_dir.join("0.ts").exists());
        assert!(stream_dir.join("1.ts").exists());
        let manifest = std::fs::read_to_string(stream_dir.join("index.m3u8")).unwrap();
        assert!(
            manifest.contains("0.ts") && manifest.contains("1.ts"),
            "{manifest}"
        );
        assert!(manifest.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn fu_a_helper_roundtrips_through_depacketizer() {
        // Guard the test helper itself: FU-A fragments must reassemble to the
        // original Annex-B NAL via the real depacketizer.
        let mut d = H264Depacketizer::new();
        let mut out = BytesMut::new();
        let body = [0xDEu8, 0xAD, 0xBE, 0xEF, 0x01];
        let frags = fu_a(0x65, &body, 3);
        let n = frags.len();
        for (k, f) in frags.iter().enumerate() {
            d.push(f, k == n - 1, &mut out).unwrap();
        }
        let mut expected = SC.to_vec();
        expected.push(0x65);
        expected.extend_from_slice(&body);
        assert_eq!(&out[..], &expected[..]);
    }

    // =================== Reorder buffer wiring tests ===================
    //
    // Prove that the ReorderBuffer→depacketizer pipeline (i.e. the path wired
    // into WhipSession::run) correctly reorders out-of-order RTP, drops
    // duplicates, drops late packets, and handles seq wraparound — without
    // needing a live str0m session or RtpPacket structs.
    //
    // A packet is represented as `(payload, marker, pts_90k)`. The helper
    // `drain_ingest` mimics depacketize_rtp's video path (no audio routing
    // needed here), collecting each completed Annex-B AU into a Vec<Bytes>.

    use crate::reorder::ReorderBuffer;

    /// A minimal ingest packet: (RTP payload, marker bit, 90 kHz PTS).
    type IngestPkt = (Vec<u8>, bool, u64);

    /// Push `pkt` through `rbuf` and feed any drained packets into
    /// `depack`/`au`. Completed AUs (marker bit set) are appended to `aus`.
    fn push_reorder(
        rbuf: &mut ReorderBuffer<IngestPkt>,
        seq: u16,
        pkt: IngestPkt,
        depack: &mut H264Depacketizer,
        au: &mut BytesMut,
        aus: &mut Vec<(bytes::Bytes, u64)>,
    ) {
        rbuf.push(seq, pkt, &mut |_s, maybe| {
            if let Some((payload, marker, pts)) = maybe {
                let _ = depack.push(&payload, marker, au); // errors → resync
                if marker && !au.is_empty() {
                    aus.push((au.split().freeze(), pts));
                }
            } else {
                // Loss declared: reset depacketizer (mirrors run() behavior).
                depack.reset();
                au.clear();
            }
        });
    }

    #[test]
    fn out_of_order_rtp_yields_same_nal_output_as_in_order() {
        // Feed two single-NAL video packets in the order 0, 2, 1, 3 (seq).
        // The reorder buffer should drain them 0→1→2→3, producing the same
        // two completed AUs (markers on seq 1 and 3) as the in-order path.
        //
        // Packet layout:
        //   seq 0: non-IDR slice, no marker (first half of a two-packet AU)
        //   seq 1: non-IDR slice, marker    (completes AU #1)
        //   seq 2: non-IDR slice, no marker (first half of AU #2)
        //   seq 3: non-IDR slice, marker    (completes AU #2)
        //
        // We use single-NAL packets (header 0x41 = non-IDR, NRI=2, type=1) so
        // the depacketizer emits each one immediately without FU-A state.
        let pkts: [(u16, IngestPkt); 4] = [
            (0, (vec![0x41, 0x01], false, 0)),
            (1, (vec![0x41, 0x02], true, 1_000)),
            (2, (vec![0x41, 0x03], false, 2_000)),
            (3, (vec![0x41, 0x04], true, 3_000)),
        ];

        // Reference: in-order feeding produces the expected access units.
        let mut depack_inorder = H264Depacketizer::new();
        let mut inorder_accum = BytesMut::new();
        let mut inorder_aus: Vec<(bytes::Bytes, u64)> = Vec::new();
        for (_, (payload, marker, pts)) in &pkts {
            let _ = depack_inorder.push(payload, *marker, &mut inorder_accum);
            if *marker && !inorder_accum.is_empty() {
                inorder_aus.push((inorder_accum.split().freeze(), *pts));
            }
        }

        // Test: out-of-order feeding via the reorder buffer must yield the same.
        let mut depack_reordered = H264Depacketizer::new();
        let mut reordered_accum = BytesMut::new();
        let mut reordered_aus: Vec<(bytes::Bytes, u64)> = Vec::new();
        let mut rbuf: ReorderBuffer<IngestPkt> =
            ReorderBuffer::with_start(0, DEFAULT_REORDER_WINDOW);
        // Arrival order: 0, 2, 1, 3 — seq 2 arrives before seq 1.
        for seq in [0u16, 2, 1, 3] {
            let (_, pkt) = pkts.iter().find(|(s, _)| *s == seq).unwrap();
            push_reorder(
                &mut rbuf,
                seq,
                pkt.clone(),
                &mut depack_reordered,
                &mut reordered_accum,
                &mut reordered_aus,
            );
        }

        // Both paths must produce the same AUs in the same order.
        assert_eq!(
            inorder_aus.len(),
            reordered_aus.len(),
            "AU count must match: in-order={}, reordered={}",
            inorder_aus.len(),
            reordered_aus.len()
        );
        for (i, ((ref_au, ref_pts), (ooo_au, ooo_pts))) in
            inorder_aus.iter().zip(reordered_aus.iter()).enumerate()
        {
            assert_eq!(
                ref_au, ooo_au,
                "AU {i} bytes differ between in-order and reordered"
            );
            assert_eq!(ref_pts, ooo_pts, "AU {i} PTS differs");
        }
    }

    #[test]
    fn duplicate_rtp_is_dropped() {
        // Push seq 5 twice. The second push must be silently rejected, so
        // the AU is emitted exactly once (not double-depacketized).
        let mut rbuf: ReorderBuffer<IngestPkt> =
            ReorderBuffer::with_start(5, DEFAULT_REORDER_WINDOW);
        let mut depack = H264Depacketizer::new();
        let mut au = BytesMut::new();
        let mut aus: Vec<(bytes::Bytes, u64)> = Vec::new();

        // First push: seq 5 with a complete single-NAL AU (marker set).
        let pkt = (vec![0x41, 0xAA, 0xBB], true, 9_000u64);
        push_reorder(&mut rbuf, 5, pkt.clone(), &mut depack, &mut au, &mut aus);
        assert_eq!(aus.len(), 1, "AU emitted on first push");

        // Second push of the same seq: must be a no-op.
        push_reorder(&mut rbuf, 5, pkt, &mut depack, &mut au, &mut aus);
        assert_eq!(aus.len(), 1, "duplicate seq must not produce a second AU");
    }

    #[test]
    fn late_rtp_beyond_window_is_dropped_without_stalling() {
        // Window = 4. Push seq 0 (drains immediately), then seq 8 (gap of 8 >
        // window=4). The buffer should declare seqs 1..=7 lost (resetting
        // the depacketizer each time) and drain seq 8, all without stalling.
        let window = 4u16;
        let mut rbuf: ReorderBuffer<IngestPkt> = ReorderBuffer::with_start(0, window);
        let mut depack = H264Depacketizer::new();
        let mut au = BytesMut::new();
        let mut aus: Vec<(bytes::Bytes, u64)> = Vec::new();

        // seq 0: drained immediately.
        push_reorder(
            &mut rbuf,
            0,
            (vec![0x41, 0x01], true, 0),
            &mut depack,
            &mut au,
            &mut aus,
        );
        assert_eq!(aus.len(), 1, "seq 0 AU emitted");

        // seq 8: exceeds window from next_expected=1 → loss markers for 1..=7,
        // then seq 8 drains. The depacketizer is reset for each loss event, but
        // seq 8's NAL should still be emitted as a fresh AU.
        push_reorder(
            &mut rbuf,
            8,
            (vec![0x41, 0x09], true, 8_000),
            &mut depack,
            &mut au,
            &mut aus,
        );
        assert_eq!(aus.len(), 2, "seq 8 AU emitted after loss markers");

        // The buffer must be empty — not stalled waiting for the lost seqs.
        assert_eq!(rbuf.buffered(), 0, "buffer must be empty after window skip");
        assert_eq!(rbuf.next_expected(), 9, "cursor advanced past the gap");
    }

    // =============== Real in-process DTLS-SRTP handshake ===============
    //
    // Proves the media plane's transport — the one piece the byte-level tests
    // above deliberately skip — *without a browser*. We stand up a second str0m
    // `Rtc` as the **publisher** (offerer) and drive a genuine
    // ICE → DTLS-SRTP → RTP exchange against the real `WhipSession` (answerer),
    // pumping str0m's `Output::Transmit` datagrams between the two peers entirely
    // in memory (no UDP socket). str0m is pure Rust (rust-crypto backend), so the
    // DTLS handshake, SRTP keying, and SRTP decryption all run for real here.
    //
    // What this validates end-to-end:
    //   1. `WhipSession::accept` ingests the publisher's *real* str0m-generated
    //      SDP offer and answers it (production code path).
    //   2. ICE connectivity checks (STUN binding req/resp) complete → Connected.
    //   3. The DTLS handshake completes and both sides derive SRTP keys.
    //   4. The publisher SRTP-*encrypts* an H.264 RTP packet; the WhipSession's
    //      str0m SRTP-*decrypts* it and surfaces it as `Event::RtpPacket`.
    //
    // The test lives inside this module so it can touch `WhipSession`'s private
    // `rtc` field directly — no production API is widened for testability.

    use str0m::format::Codec;
    use str0m::media::{Direction, MediaKind};
    use str0m::net::DatagramRecv;
    use str0m::rtp::{ExtensionValues, SeqNo, Ssrc};

    /// An in-memory peer: a str0m `Rtc` plus the host address other peers reach
    /// it at, and the wall-clock the test drives it with.
    struct Pumped {
        rtc: Rtc,
        addr: SocketAddr,
    }

    impl Pumped {
        /// Drain `poll_output`: collect any `Event`s, hand `Transmit`s to the
        /// caller (queued for the peer), and return the next requested timeout.
        fn poll(
            &mut self,
            now: Instant,
            out: &mut Vec<(SocketAddr, SocketAddr, Vec<u8>)>,
            events: &mut Vec<Event>,
        ) -> Instant {
            loop {
                match self.rtc.poll_output().expect("poll_output") {
                    Output::Timeout(t) => return t.max(now),
                    Output::Transmit(t) => {
                        out.push((t.source, t.destination, t.contents.to_vec()));
                    }
                    Output::Event(ev) => events.push(ev),
                }
            }
        }
    }

    /// Deliver one queued datagram to whichever peer owns its destination addr.
    fn deliver(
        peers: &mut [&mut Pumped],
        now: Instant,
        src: SocketAddr,
        dst: SocketAddr,
        data: &[u8],
    ) {
        for p in peers.iter_mut() {
            if p.addr == dst {
                let Ok(contents) = DatagramRecv::try_from(data) else {
                    return; // unparseable framing — drop, mirrors run()
                };
                let recv = Receive {
                    proto: Protocol::Udp,
                    source: src,
                    destination: dst,
                    contents,
                };
                p.rtc
                    .handle_input(Input::Receive(now, recv))
                    .expect("handle_input receive");
                return;
            }
        }
    }

    #[test]
    fn real_dtls_srtp_handshake_and_h264_rtp_forward() {
        // --- Publisher (offerer) addr + the WhipSession (answerer) ingest addr.
        let pub_addr: SocketAddr = "1.1.1.1:5000".parse().unwrap();
        let whip_addr: SocketAddr = "2.2.2.2:6000".parse().unwrap();

        // --- 1) Build the publisher Rtc: RTP mode (matches WhipSession), with a
        // sendonly H.264 video m-line so str0m negotiates H.264 and gives us a
        // sender stream to write_rtp on.
        let now = Instant::now();
        let mut pubrtc = Rtc::builder().set_rtp_mode(true).build(now);
        pubrtc.add_local_candidate(Candidate::host(pub_addr, "udp").unwrap());

        let mut change = pubrtc.sdp_api();
        let mid = change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        let (offer, pending) = change.apply().expect("offer has changes");
        let offer_sdp = offer.to_sdp_string();

        // --- 2) Feed the *real* offer through the production accept() path.
        let (session, answer) =
            WhipSession::accept(&offer_sdp, "2.2.2.2", 6000).expect("whip accepts offer");
        // Sanity: H.264 PT was negotiated as video (so RtpPacket routing works).
        assert!(
            !session.video_payload_types().is_empty(),
            "H.264 must be negotiated as video"
        );

        // The publisher consumes the answer to learn the WhipSession's ICE
        // candidate + DTLS fingerprint (this is what a browser does with the
        // HTTP 201 response body).
        pubrtc
            .sdp_api()
            .accept_answer(pending, answer)
            .expect("publisher accepts answer");

        let mut publisher = Pumped {
            rtc: pubrtc,
            addr: pub_addr,
        };
        let mut whip = Pumped {
            rtc: session.rtc,
            addr: whip_addr,
        };

        // --- 3) Drive ICE + DTLS by pumping datagrams in memory until both
        // peers report Connected (ICE complete + DTLS established + SRTP keyed).
        let mut clock = now;
        let mut events_pub = Vec::new();
        let mut events_whip = Vec::new();
        let mut connected = false;
        for _ in 0..2000 {
            let mut queue: Vec<(SocketAddr, SocketAddr, Vec<u8>)> = Vec::new();
            let t_pub = publisher.poll(clock, &mut queue, &mut events_pub);
            let t_whip = whip.poll(clock, &mut queue, &mut events_whip);

            for (src, dst, data) in queue.drain(..) {
                deliver(&mut [&mut publisher, &mut whip], clock, src, dst, &data);
            }

            if publisher.rtc.is_connected() && whip.rtc.is_connected() {
                connected = true;
                break;
            }

            // Advance the clock to the earliest requested timeout (min 1ms step
            // so a same-instant timeout still makes progress).
            let next = t_pub
                .min(t_whip)
                .max(clock + std::time::Duration::from_millis(1));
            clock = next;
            publisher
                .rtc
                .handle_input(Input::Timeout(clock))
                .expect("pub timeout");
            whip.rtc
                .handle_input(Input::Timeout(clock))
                .expect("whip timeout");
        }

        assert!(
            connected,
            "ICE+DTLS must reach Connected on both peers (pub_connected={}, whip_connected={})",
            publisher.rtc.is_connected(),
            whip.rtc.is_connected()
        );

        // --- 4) Publisher SRTP-encrypts and sends one H.264 RTP packet; the
        // WhipSession must SRTP-decrypt it and surface Event::RtpPacket.
        let params = publisher
            .rtc
            .codec_config()
            .find(|p| p.spec().codec == Codec::H264)
            .copied()
            .expect("publisher negotiated H.264");
        let pt = params.pt();

        // Get the publisher's tx stream for this video mid.
        let ssrc: Ssrc = publisher
            .rtc
            .direct_api()
            .stream_tx_by_mid(mid, None)
            .expect("publisher has a video tx stream")
            .ssrc();

        // A single-NAL H.264 RTP payload (0x41 = non-IDR coded slice header).
        let payload: Vec<u8> = vec![0x41, 0xDE, 0xAD, 0xBE, 0xEF];
        let seq_no: SeqNo = 1_000u64.into();
        let rtp_time: u32 = 90_000;
        let wallclock = clock;
        publisher
            .rtc
            .direct_api()
            .stream_tx(&ssrc)
            .expect("tx stream by ssrc")
            .write_rtp(
                pt,
                seq_no,
                rtp_time,
                wallclock,
                true, // marker: terminates the access unit
                ExtensionValues::default(),
                false,
                payload.clone(),
            )
            .expect("write_rtp on the publisher");

        // Pump until the WhipSession emits the RtpPacket (or we give up).
        // RtpPacket is not Clone, so we capture the decrypted fields we assert on
        // (payload bytes, payload type, marker) into owned values. Same loop
        // shape as the handshake pump: poll both peers, deliver every queued
        // datagram, scan the whip side's new events, then advance the clock.
        let mut got_rtp: Option<(Vec<u8>, Pt, bool)> = None;
        for _ in 0..4000 {
            let mut queue: Vec<(SocketAddr, SocketAddr, Vec<u8>)> = Vec::new();
            let t_pub = publisher.poll(clock, &mut queue, &mut events_pub);
            let before = events_whip.len();
            let t_whip = whip.poll(clock, &mut queue, &mut events_whip);

            for (src, dst, data) in queue.drain(..) {
                deliver(&mut [&mut publisher, &mut whip], clock, src, dst, &data);
            }
            // Drain any events the just-delivered datagrams produced.
            whip.poll(clock, &mut queue, &mut events_whip);
            publisher.poll(clock, &mut queue, &mut events_pub);
            for (src, dst, data) in queue.drain(..) {
                deliver(&mut [&mut publisher, &mut whip], clock, src, dst, &data);
            }

            for ev in &events_whip[before..] {
                if let Event::RtpPacket(p) = ev {
                    got_rtp = Some((p.payload.clone(), p.header.payload_type, p.header.marker));
                }
            }
            if got_rtp.is_some() {
                break;
            }

            let next = t_pub
                .min(t_whip)
                .max(clock + std::time::Duration::from_millis(1));
            clock = next;
            publisher
                .rtc
                .handle_input(Input::Timeout(clock))
                .expect("pub timeout");
            whip.rtc
                .handle_input(Input::Timeout(clock))
                .expect("whip timeout");
        }

        let (got_payload, got_pt, got_marker) = got_rtp.expect(
            "WhipSession must receive the publisher's H.264 RTP packet (SRTP decrypt succeeded)",
        );
        // The decrypted payload must match exactly what the publisher encrypted —
        // proof the SRTP round-trip (encrypt → DTLS-keyed cipher → decrypt) is real.
        assert_eq!(
            &got_payload[..],
            &payload[..],
            "decrypted RTP payload must match the sent H.264 NAL"
        );
        assert_eq!(got_pt, pt, "payload type preserved");
        assert!(got_marker, "marker bit preserved across SRTP");
    }

    #[test]
    fn reorder_buffer_handles_seq_wraparound() {
        // Stream starts near u16::MAX. Push 65534, 0 (wrap), 65535 out of order.
        // Expected drain order: 65534 → 65535 → 0.
        let start: u16 = u16::MAX - 1; // 65534
        let mut rbuf: ReorderBuffer<IngestPkt> = ReorderBuffer::with_start(start, 16);
        let mut depack = H264Depacketizer::new();
        let mut au = BytesMut::new();
        let mut aus: Vec<(bytes::Bytes, u64)> = Vec::new();

        // Arrival order: 65534, 0 (post-wrap), 65535 — so 65535 arrives last
        // even though it is numerically between 65534 and 0.
        let ordered: [(u16, IngestPkt); 3] = [
            (start, (vec![0x41, 0x01], true, 0)),        // 65534
            (0u16, (vec![0x41, 0x03], true, 2_000)),     // 0 (after wrap)
            (u16::MAX, (vec![0x41, 0x02], true, 1_000)), // 65535 — arrives last
        ];
        let arrival_order = [0usize, 2, 1]; // feed 65534, 65535 oop, then 0
        for &i in &arrival_order {
            let (seq, ref pkt) = ordered[i];
            push_reorder(&mut rbuf, seq, pkt.clone(), &mut depack, &mut au, &mut aus);
        }

        // All three should drain in the correct order: 65534, 65535, 0.
        assert_eq!(aus.len(), 3, "all three AUs emitted across the wraparound");
        // PTS 0, 1000, 2000 correspond to seqs 65534, 65535, 0 in drain order.
        assert_eq!(aus[0].1, 0, "first AU pts (seq 65534)");
        assert_eq!(aus[1].1, 1_000, "second AU pts (seq 65535)");
        assert_eq!(aus[2].1, 2_000, "third AU pts (seq 0, post-wrap)");
    }
}
