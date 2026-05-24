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
//! ## Why RTP mode
//!
//! In str0m's default sample mode, `Event::MediaData` hands you an
//! already-depacketized frame, which would make our RFC 6184 depacketizer
//! redundant. Building with `set_rtp_mode(true)` makes str0m emit raw
//! [`RtpPacket`]s instead, so the depacketizer genuinely reassembles NAL units
//! from the publisher's RTP — exactly the boundary this crate owns.
//!
//! ## Runtime-verifiability
//!
//! `accept` is unit-tested (real offer → real answer). `run` is **not**
//! runtime-verifiable here — it needs an actual browser WHIP publisher
//! completing ICE+DTLS, which this environment cannot provide. Its constituent
//! pieces (the depacketizer, answer generation, PT→codec classification) are
//! tested; the loop wiring is reviewed-and-compiled only. The server is
//! responsible for spawning `run` (out of scope for this crate).

use std::net::SocketAddr;
use std::time::Instant;

use bytes::BytesMut;
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::media::Pt;
use str0m::net::{Protocol, Receive};
use str0m::rtp::RtpPacket;
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcError};
use tokio::net::UdpSocket;
use tracing::{debug, trace, warn};

use crate::depacketize::H264Depacketizer;
use crate::hls_sink::MediaSink;

/// Maximum size of a single inbound UDP datagram we buffer. WebRTC keeps
/// datagrams under the path MTU; 2 KiB is comfortably above that.
const RECV_BUF: usize = 2048;

/// Errors specific to driving a str0m session.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The SDP offer could not be parsed by str0m.
    #[error("parse offer: {0}")]
    Offer(String),
    /// str0m rejected the offer or failed to produce an answer.
    #[error("rtc: {0}")]
    Rtc(#[from] RtcError),
    /// The supplied ingest address was not a valid `SocketAddr`.
    #[error("invalid ingest address {0}:{1}")]
    Addr(String, u16),
    /// Building a local ICE candidate failed.
    #[error("candidate: {0}")]
    Candidate(String),
    /// Socket I/O error while running the loop.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// A live WHIP publisher session backed by a str0m `Rtc`.
///
/// Construct with [`WhipSession::accept`]; the returned [`SdpAnswer`] is sent
/// back to the browser over HTTP, then [`WhipSession::run`] is spawned on the
/// UDP socket bound to the advertised ingest address.
pub struct WhipSession {
    rtc: Rtc,
    /// The address str0m advertised as its host candidate; the UDP socket the
    /// server binds for `run` must be reachable at this address.
    local_addr: SocketAddr,
    /// Payload types negotiated as video. Used to route inbound RTP to the
    /// H.264 depacketizer in RTP mode (where packets carry only a PT, not a
    /// media kind). Captured at `accept` time since negotiation is complete.
    video_pts: Vec<Pt>,
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
        let local_addr: SocketAddr = format!("{ingest_host}:{ingest_port}")
            .parse()
            .map_err(|_| SessionError::Addr(ingest_host.to_string(), ingest_port))?;

        let offer = SdpOffer::from_sdp_string(offer_sdp)
            .map_err(|e| SessionError::Offer(e.to_string()))?;

        // RTP mode: str0m emits raw RtpPacket events so our RFC 6184
        // depacketizer can do the NAL reassembly. str0m still generates its own
        // DTLS cert/fingerprint and ICE credentials.
        let mut rtc = Rtc::builder().set_rtp_mode(true).build(Instant::now());

        // Advertise a host candidate pointing at the server's ingest socket.
        // (str0m's `Candidate::host` takes the protocol as a string, e.g. "udp".)
        let candidate = Candidate::host(local_addr, "udp")
            .map_err(|e| SessionError::Candidate(e.to_string()))?;
        let _ = rtc.add_local_candidate(candidate);

        // accept_offer mirrors the offer's m-lines (the browser publishes
        // sendonly audio/video; str0m answers recvonly) and returns the answer.
        let answer = rtc.sdp_api().accept_offer(offer)?;

        // After negotiation, record which payload types are video so the run
        // loop can route inbound RTP correctly.
        let video_pts = rtc
            .codec_config()
            .params()
            .iter()
            .filter(|p| p.spec().codec.is_video())
            .map(str0m::format::PayloadParams::pt)
            .collect();

        Ok((
            Self {
                rtc,
                local_addr,
                video_pts,
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

    /// Drive this session straight into HLS on disk under `stream_dir`.
    ///
    /// This is the canonical production wiring of the P5 media path, kept here so
    /// it compiles against the real types even though it cannot *run* without a
    /// browser completing ICE/DTLS:
    ///
    /// 1. Build the [`HlsSink`] / [`HlsSegmentWriter`] pair rooted at
    ///    `stream_dir` (the caller composes `hls_dir/{stream_id}`).
    /// 2. Spawn the async writer (it owns [`aero_live_hls::HlsWriter`] and does
    ///    the disk I/O off the str0m hot path).
    /// 3. [`run`](Self::run) the str0m loop with the sink; depacketized H.264
    ///    access units become keyframe-aligned `.ts` segments + `index.m3u8`.
    ///
    /// Dropping the sink when `run` returns closes the channel, so the writer
    /// finalizes the manifest and reports how many segments it persisted.
    ///
    /// The `socket` must be bound to [`local_addr`](Self::local_addr).
    pub async fn run_to_hls(
        self,
        socket: UdpSocket,
        stream_dir: std::path::PathBuf,
        target_duration_secs: u32,
    ) -> Result<u64, SessionError> {
        let (sink, writer) = crate::hls_sink::hls_sink(stream_dir, target_duration_secs)
            .await
            .map_err(|e| SessionError::Io(std::io::Error::other(e.to_string())))?;
        let writer_task = tokio::spawn(writer.run());
        // Run the media plane; the sink is moved in and dropped on return, which
        // closes the channel and lets the writer task finalize the manifest.
        // `Box::pin` keeps this combined future off the stack (the str0m `run`
        // future is large) — see clippy::large_futures.
        Box::pin(self.run(socket, sink)).await?;
        match writer_task.await {
            Ok(Ok(segments)) => Ok(segments),
            Ok(Err(e)) => Err(SessionError::Io(std::io::Error::other(e.to_string()))),
            Err(join) => Err(SessionError::Io(std::io::Error::other(join.to_string()))),
        }
    }

    /// Run the str0m event loop until the connection closes or errors.
    ///
    /// This is the canonical sans-IO driver:
    /// 1. Drain [`Rtc::poll_output`]. `Transmit` → `socket.send_to`; `Timeout`
    ///    → arm a sleep; `Event` → handle (RTP media, ICE state, ...).
    /// 2. Either a UDP datagram arrives (feed `Input::Receive`) or the timeout
    ///    fires (feed `Input::Timeout`), then loop.
    ///
    /// Received H.264 RTP is depacketized into Annex-B access units and pushed
    /// to `sink`. The `socket` must be bound to [`local_addr`](Self::local_addr).
    pub async fn run(
        mut self,
        socket: UdpSocket,
        mut sink: impl MediaSink,
    ) -> Result<(), SessionError> {
        let mut depacketizer = H264Depacketizer::new();
        let mut buf = vec![0u8; RECV_BUF];
        // Accumulates one access unit's worth of Annex-B NAL units.
        let mut au = BytesMut::new();

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
                                self.on_rtp(&pkt, &mut depacketizer, &mut au, &mut sink);
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

    /// Route one inbound RTP packet. Video packets feed the H.264 depacketizer;
    /// on the access-unit boundary (RTP marker bit) the assembled Annex-B AU is
    /// flushed to the sink.
    fn on_rtp(
        &self,
        pkt: &RtpPacket,
        depacketizer: &mut H264Depacketizer,
        au: &mut BytesMut,
        sink: &mut impl MediaSink,
    ) {
        let pt = pkt.header.payload_type;
        let marker = pkt.header.marker;

        if self.video_pts.contains(&pt) {
            if let Err(e) = depacketizer.push(&pkt.payload, marker, au) {
                // Reassembly desync (packet loss / reorder): drop the partial
                // AU and resync on the next keyframe.
                trace!(error = %e, "whip: h264 depacketize error, resyncing");
                depacketizer.reset();
                au.clear();
                return;
            }
            // The RTP marker bit terminates an H.264 access unit (RFC 6184 §5.1).
            if marker && !au.is_empty() {
                let frame = au.split().freeze();
                let pts_90k = media_time_to_90k(pkt.time);
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

/// Convert a str0m `MediaTime` into 90 kHz ticks (the unit the MPEG-TS muxer in
/// `aero-live-hls` uses for PTS/DTS).
///
/// The value is clamped to be non-negative and, for any realistic stream
/// duration, fits comfortably in `u64`, so the float→int cast is intentional.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn media_time_to_90k(time: str0m::media::MediaTime) -> u64 {
    (time.as_seconds() * 90_000.0).max(0.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but real WHIP-style publisher offer (sendonly audio+video,
    /// bundled, rtcp-mux, with ICE + DTLS attributes) that str0m can parse.
    const PUBLISHER_OFFER: &str = "v=0\r\n\
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
        assert!(sdp.contains("a=recvonly"), "expected recvonly answer:\n{sdp}");
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
        let has_96 = session
            .video_payload_types()
            .iter()
            .any(|pt| **pt == 96);
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
        feed_rtp(&mut depack, &mut au, &mut sink, &stap_a(&[sps, pps]), false, 0);
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
        assert!(segments >= 1, "at least one .ts segment persisted, got {segments}");
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
        assert!(manifest.starts_with("#EXTM3U"), "manifest header:\n{manifest}");
        assert!(manifest.contains("#EXT-X-VERSION:3"), "manifest:\n{manifest}");
        assert!(manifest.contains("#EXT-X-TARGETDURATION:2"), "manifest:\n{manifest}");
        assert!(manifest.contains("#EXTINF:"), "per-segment duration:\n{manifest}");
        assert!(manifest.contains("0.ts"), "segment name in manifest:\n{manifest}");
        assert!(manifest.contains("#EXT-X-ENDLIST"), "finalized:\n{manifest}");
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
        feed_rtp(&mut depack, &mut au, &mut sink, &stap_a(&[sps, pps]), false, 0);
        feed_rtp(&mut depack, &mut au, &mut sink, &[0x65, 0x01, 0x02, 0x03], true, 0);
        // A P-frame.
        feed_rtp(&mut depack, &mut au, &mut sink, &[0x41, 0x04, 0x05], true, 3_000);
        // GOP 2: SPS+PPS+IDR → cuts GOP 1 into 0.ts before muxing this keyframe.
        feed_rtp(&mut depack, &mut au, &mut sink, &stap_a(&[sps, pps]), false, 6_000);
        feed_rtp(&mut depack, &mut au, &mut sink, &[0x65, 0x06, 0x07], true, 6_000);
        assert_eq!(sink.segments_emitted(), 1, "one cut at the 2nd keyframe");

        drop(sink);
        let segments = writer_task.await.unwrap().unwrap();
        assert_eq!(segments, 2, "GOP1 (cut) + GOP2 (flushed on close)");
        assert!(stream_dir.join("0.ts").exists());
        assert!(stream_dir.join("1.ts").exists());
        let manifest = std::fs::read_to_string(stream_dir.join("index.m3u8")).unwrap();
        assert!(manifest.contains("0.ts") && manifest.contains("1.ts"), "{manifest}");
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
}
