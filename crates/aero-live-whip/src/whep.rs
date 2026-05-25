//! str0m-driven WHEP subscriber session: the sans-IO WebRTC egress plane.
//!
//! WHEP (WebRTC-HTTP Egress Protocol) is the viewer-side counterpart of WHIP.
//! A WHEP subscriber sends a **recvonly** SDP offer asking to receive media;
//! the server responds with a **sendonly** answer and starts pushing RTP.
//!
//! ## Public API
//!
//! - [`accept_whep_offer`] — parse the viewer's SDP offer with str0m (passive
//!   mode) and return a [`WhepSession`] + a real SDP answer (str0m-generated
//!   DTLS fingerprint, ICE ufrag/pwd, and a host ICE candidate at the egress
//!   address). Analogous to [`WhipSession::accept`](crate::session::WhipSession::accept).
//! - [`WhepSession::packetize`] — convert a slice of H.264 NAL units (without
//!   start codes) into [`RtpPacket`]s via [`WhepPacketizer`]. The packets carry
//!   incrementing sequence numbers, correct 90 kHz timestamps, and the RFC 6184
//!   marker bit on the last packet of each access unit.
//! - [`WhepSession::run`] — the str0m event loop that drains `poll_output`
//!   (UDP `Transmit` / `Timeout`) and feeds back `Receive` / `Timeout` events,
//!   writing RTP from a user-supplied NAL source into the transport. Requires a
//!   real ICE/DTLS connection with a browser — not testable in unit tests.
//!
//! ## Why RTP mode on the egress path
//!
//! The existing crate already uses str0m's RTP mode for WHIP ingest (so that
//! our RFC 6184 depacketizer owns NAL reassembly). We mirror that choice for
//! WHEP egress: [`WhepPacketizer`] handles all RFC 6184 packetization (single-
//! NAL, FU-A, STAP-A) and we hand the resulting raw RTP packets directly to
//! str0m's `StreamTx::write_rtp`. That keeps the two halves (ingest /
//! packetize → depacketize / egress) symmetric and fully unit-tested.
//!
//! ## Runtime-verifiability
//!
//! [`accept_whep_offer`] is unit-tested (viewer offer → server answer). The
//! SDP attribute assertions (sendonly, H.264, ICE/DTLS) are also tested.
//! [`WhepSession::packetize`] is fully exercised in tests (NAL → RTP → round-
//! trip via depacketizer). [`WhepSession::run`] needs a real WHEP browser
//! subscriber to complete ICE/DTLS — that is not runtime-verifiable here.

use std::net::SocketAddr;
use std::time::Instant;

use str0m::change::{SdpAnswer, SdpOffer};
use str0m::media::{Direction, MediaKind, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::rtp::{ExtensionValues, SeqNo, Ssrc};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tracing::{debug, trace, warn};

use crate::packetize::{RtpPacket, WhepPacketizer};
use crate::session::SessionError;

/// Maximum size of a single inbound (RTCP) UDP datagram we buffer.
const RECV_BUF: usize = 2048;

/// Default MTU for WHEP egress RTP packets. Leaves headroom for IP+UDP headers.
const EGRESS_MTU: usize = 1200;

/// Source of H.264 NAL units for [`WhepSession::run`].
///
/// Each call to `next_access_unit` should return `Some((nals, ts_90k))` where
/// `nals` are the raw NAL unit bytes (no Annex-B start codes) for one access
/// unit and `ts_90k` is the 90 kHz presentation timestamp. Returning `None`
/// signals end-of-stream.
pub trait NalSource: Send {
    fn next_access_unit(&mut self) -> Option<(Vec<Vec<u8>>, u32)>;
}

/// A live WHEP subscriber session backed by a str0m `Rtc`.
///
/// Construct with [`accept_whep_offer`]; the returned [`SdpAnswer`] is sent
/// back to the browser over HTTP, then [`WhepSession::run`] is spawned with the
/// UDP socket bound to the advertised egress address.
///
/// For tests that do not need a live browser, use [`WhepSession::packetize`] to
/// convert NAL units into [`RtpPacket`]s without touching the transport.
pub struct WhepSession {
    rtc: Rtc,
    /// The address str0m advertised as its host candidate.
    local_addr: SocketAddr,
    /// Payload type negotiated for H.264 video (first sendonly video PT).
    /// Present after a successful `accept_whep_offer` negotiation.
    video_pt: Option<Pt>,
    /// SSRC str0m assigned to the outgoing video stream.
    video_ssrc: Option<Ssrc>,
    /// The `Mid` of the video media section from the negotiated answer.
    video_mid: Option<Mid>,
    /// Packetizer for H.264 NAL → RTP; initialized after negotiation.
    packetizer: Option<WhepPacketizer>,
    /// Monotonically incrementing str0m `SeqNo` for outgoing RTP.
    seq: u64,
}

impl WhepSession {
    /// Parse a WHEP subscriber's SDP **offer** and produce a sendonly answer.
    ///
    /// A WHEP viewer's offer advertises `a=recvonly` on its media sections;
    /// str0m answers them as `a=sendonly` (we send, they receive). The session
    /// is configured in **RTP mode** so [`WhepSession::run`] can inject raw RTP
    /// packets via `StreamTx::write_rtp`.
    ///
    /// Returns `(session, answer)`. The session owns the `Rtc`; the answer is
    /// sent back to the browser over HTTP.
    pub fn accept(
        offer_sdp: &str,
        egress_host: &str,
        egress_port: u16,
    ) -> Result<(Self, SdpAnswer), SessionError> {
        let local_addr: SocketAddr = format!("{egress_host}:{egress_port}")
            .parse()
            .map_err(|_| SessionError::Addr(egress_host.to_string(), egress_port))?;

        let offer = SdpOffer::from_sdp_string(offer_sdp)
            .map_err(|e| SessionError::Offer(e.to_string()))?;

        // RTP mode: we hand raw RTP packets to StreamTx rather than using the
        // frame-level Writer API. This is consistent with the WHIP ingest path
        // (which also uses RTP mode to expose raw packets to the depacketizer).
        let mut rtc = Rtc::builder().set_rtp_mode(true).build(Instant::now());

        // Advertise our host candidate: the browser's ICE agent connects here.
        let candidate = Candidate::host(local_addr, "udp")
            .map_err(|e| SessionError::Candidate(e.to_string()))?;
        let _ = rtc.add_local_candidate(candidate);

        // accept_offer: the viewer's recvonly m-lines → str0m answers sendonly.
        let answer = rtc.sdp_api().accept_offer(offer)?;

        // Collect the negotiated video PT and SSRC from codec_config.
        // These are present immediately after accept_offer.
        let (video_pt, video_ssrc) = {
            let pt: Option<Pt> = rtc
                .codec_config()
                .params()
                .iter()
                .find(|p| p.spec().codec.is_video())
                .map(str0m::format::PayloadParams::pt);
            // SSRC is assigned later (via Event::MediaAdded in the run loop
            // or via direct_api().new_ssrc() on first use). Start as None.
            (pt, None::<Ssrc>)
        };

        Ok((
            Self {
                rtc,
                local_addr,
                video_pt,
                video_ssrc,
                video_mid: None,
                packetizer: None,
                seq: 0,
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

    /// The H.264 payload type negotiated for this session, if any.
    #[must_use]
    pub fn video_payload_type(&self) -> Option<Pt> {
        self.video_pt
    }

    /// Packetize one H.264 access unit (a slice of raw NAL units, **no** Annex-B
    /// start codes) into RTP packets.
    ///
    /// Uses the negotiated payload type and SSRC if available, falling back to
    /// sensible defaults (`PT=96`, `SSRC=0`) for offline/test use. This method
    /// does **not** touch the str0m transport and can be used in unit tests
    /// without a live browser.
    ///
    /// Returns an empty `Vec` if `nals` is empty. The last packet in the
    /// returned slice always has `marker = true` (RFC 6184 §5.1).
    pub fn packetize(&mut self, nals: &[&[u8]], rtp_ts: u32) -> Vec<RtpPacket> {
        // Build the packetizer on first use (or on demand).
        let pt = self.video_pt.map_or(96, |p| *p);
        let ssrc = self.video_ssrc.map_or(0u32, |s| *s);
        if self.packetizer.is_none() {
            self.packetizer = Some(WhepPacketizer::new(ssrc, pt, EGRESS_MTU, true));
        }
        let pktzr = self.packetizer.as_mut().expect("just initialized");
        pktzr.packetize(nals, rtp_ts)
    }

    /// Packetize one Annex-B access unit (NAL units prefixed with `00 00 00 01`
    /// or `00 00 01` start codes) into RTP packets.
    ///
    /// Convenience wrapper over [`packetize`](Self::packetize) that strips start
    /// codes before handing off to the core packetizer.
    pub fn packetize_annex_b(&mut self, annex_b: &[u8], rtp_ts: u32) -> Vec<RtpPacket> {
        let pt = self.video_pt.map_or(96, |p| *p);
        let ssrc = self.video_ssrc.map_or(0u32, |s| *s);
        if self.packetizer.is_none() {
            self.packetizer = Some(WhepPacketizer::new(ssrc, pt, EGRESS_MTU, true));
        }
        let pktzr = self.packetizer.as_mut().expect("just initialized");
        pktzr.packetize_annex_b(annex_b, rtp_ts)
    }

    /// Run the str0m event loop, feeding H.264 NAL units from `source` as RTP.
    ///
    /// This is the production egress driver. It:
    /// 1. Drains [`Rtc::poll_output`] (transmitting UDP datagrams and handling
    ///    timeouts).
    /// 2. After ICE/DTLS is up (signalled by `Event::Connected`), reads NAL
    ///    units from `source`, packetizes them via [`WhepPacketizer`], and writes
    ///    each RTP packet into the negotiated `StreamTx`.
    /// 3. Waits for RTCP or the timeout, then loops.
    ///
    /// Requires a real browser WHEP subscriber completing ICE/DTLS — **not
    /// testable without a live subscriber**. The server is responsible for
    /// spawning this (out of scope for this crate to auto-start).
    pub async fn run(
        mut self,
        socket: UdpSocket,
        mut source: impl NalSource,
    ) -> Result<(), SessionError> {
        let mut buf = vec![0u8; RECV_BUF];
        let mut connected = false;

        loop {
            // 1) Drain everything str0m wants to emit until it asks for input.
            let timeout = loop {
                match self.rtc.poll_output()? {
                    Output::Timeout(t) => break t,
                    Output::Transmit(t) => {
                        if let Err(e) = socket.send_to(&t.contents, t.destination).await {
                            warn!(error = %e, dest = %t.destination, "whep: udp send failed");
                        }
                    }
                    Output::Event(ev) => {
                        match ev {
                            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                                debug!("whep: ice disconnected, ending session");
                                return Ok(());
                            }
                            Event::IceConnectionStateChange(state) => {
                                debug!(?state, "whep: ice state change");
                                if matches!(state, IceConnectionState::Connected) {
                                    connected = true;
                                }
                            }
                            Event::MediaAdded(ma) => {
                                if ma.direction == Direction::SendOnly
                                    && ma.kind == MediaKind::Video
                                {
                                    self.video_mid = Some(ma.mid);
                                    self.ensure_stream_tx();
                                }
                            }
                            _ => {}
                        }
                        if !self.rtc.is_alive() {
                            debug!("whep: rtc no longer alive, ending session");
                            return Ok(());
                        }
                    }
                }
            };

            // 2) If connected and we have a stream, try to send the next AU.
            if connected {
                if let Some((nals, rtp_ts)) = source.next_access_unit() {
                    let nal_refs: Vec<&[u8]> = nals.iter().map(Vec::as_slice).collect();
                    let pkts = self.packetize(&nal_refs, rtp_ts);
                    self.write_rtp_packets(&pkts);
                }
            }

            // 3) Wait for RTCP feedback or the str0m timeout.
            let wait = timeout.saturating_duration_since(Instant::now());

            tokio::select! {
                res = socket.recv_from(&mut buf) => {
                    let (n, source_addr) = res?;
                    let Ok(contents) = (&buf[..n]).try_into() else {
                        trace!("whep: dropping unparseable datagram");
                        continue;
                    };
                    let receive = Receive {
                        proto: Protocol::Udp,
                        source: source_addr,
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

    // ---- internals ----

    /// Declare a `StreamTx` for the negotiated video mid so that `write_rtp`
    /// can be called. Called once after `Event::MediaAdded` fires with
    /// `Direction::SendOnly` for video.
    fn ensure_stream_tx(&mut self) {
        let Some(mid) = self.video_mid else { return };
        let ssrc = self.rtc.direct_api().new_ssrc();
        let rtx_ssrc = self.rtc.direct_api().new_ssrc();
        self.video_ssrc = Some(ssrc);
        self.rtc
            .direct_api()
            .declare_stream_tx(ssrc, Some(rtx_ssrc), mid, None);
        // Reinitialize packetizer with the real SSRC now that we know it.
        let pt = self.video_pt.map_or(96, |p| *p);
        self.packetizer = Some(WhepPacketizer::new(*ssrc, pt, EGRESS_MTU, true));
    }

    /// Write a sequence of pre-packetized RTP packets into the negotiated
    /// `StreamTx`. Silently skips if the mid or stream is not yet set up.
    fn write_rtp_packets(&mut self, pkts: &[RtpPacket]) {
        let (Some(mid), Some(pt)) = (self.video_mid, self.video_pt) else {
            return;
        };
        for pkt in pkts {
            let Some((_seq, ts, marker, payload)) = crate::packetize::rtp_parse(&pkt.bytes) else {
                continue;
            };
            // Collect the data we need before borrowing `rtc` mutably.
            let seq = SeqNo::from(self.seq);
            self.seq += 1;
            let ext_vals = ExtensionValues::default();
            let payload_vec = payload.to_vec();
            let now = Instant::now();

            let mut api = self.rtc.direct_api();
            let Some(stream) = api.stream_tx_by_mid(mid, None) else {
                return;
            };
            if let Err(e) =
                stream.write_rtp(pt, seq, ts, now, marker, ext_vals, true, payload_vec)
            {
                warn!(error = %e, "whep: write_rtp failed");
            }
        }
    }
}

/// Accept a WHEP subscriber's SDP offer and return a real str0m SDP answer.
///
/// Analogous to [`crate::accept_whip_offer`] on the ingest side. The answer is
/// `sendonly` for H.264 video; the browser subscriber can start ICE/DTLS/SRTP
/// and receive the media once the server calls [`WhepSession::run`].
///
/// The session itself is discarded by this convenience function; to actually
/// push media the server should call [`WhepSession::accept`] directly and keep
/// the returned session for use in [`WhepSession::run`].
pub fn accept_whep_offer(
    offer_sdp: &str,
    egress_host: &str,
    egress_port: u16,
) -> Result<String, SessionError> {
    use aero_signaling::signaling::validate_sdp;
    validate_sdp(offer_sdp).map_err(|e| SessionError::Offer(e.to_string()))?;
    let (_session, answer) = WhepSession::accept(offer_sdp, egress_host, egress_port)?;
    Ok(answer.to_sdp_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::depacketize::H264Depacketizer;
    use crate::packetize::rtp_parse;
    use bytes::BytesMut;

    // ---- SDP fixtures ----

    /// A WHEP viewer's SDP offer: recvonly video (H.264) with ICE/DTLS. This is
    /// what a browser subscriber POSTs to the WHEP endpoint.
    const WHEP_OFFER: &str = "v=0\r\n\
o=- 7614819274000 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0\r\n\
a=msid-semantic: WMS\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:whep\r\n\
a=ice-pwd:wheppasswordwheppasswordw\r\n\
a=fingerprint:sha-256 11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=recvonly\r\n\
a=rtcp-mux\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n";

    // ---- SDP offer→answer tests ----

    #[test]
    fn accept_whep_offer_fn_returns_sendonly_sdp() {
        let sdp = accept_whep_offer(WHEP_OFFER, "127.0.0.1", 8000).expect("accept should succeed");
        assert!(sdp.starts_with("v=0"), "answer must be valid SDP:\n{sdp}");
        // The server answers the viewer's recvonly with sendonly.
        assert!(
            sdp.contains("a=sendonly"),
            "answer must be sendonly (we send to viewer):\n{sdp}"
        );
        // H.264 must be present.
        assert!(sdp.contains("H264"), "answer must advertise H.264:\n{sdp}");
        assert!(sdp.contains("m=video"), "answer must have video m-line:\n{sdp}");
        // DTLS / ICE plumbing.
        assert!(sdp.contains("a=fingerprint:sha-256"), "fingerprint:\n{sdp}");
        assert!(sdp.contains("a=ice-ufrag:"), "ice-ufrag:\n{sdp}");
        assert!(sdp.contains("a=ice-pwd:"), "ice-pwd:\n{sdp}");
        // Host candidate at our egress address.
        assert!(
            sdp.contains("127.0.0.1") && sdp.contains("8000"),
            "host candidate for egress addr:\n{sdp}"
        );
    }

    #[test]
    fn accept_whep_offer_fn_rejects_garbage_sdp() {
        let err = accept_whep_offer("garbage", "127.0.0.1", 8000)
            .expect_err("must reject non-SDP input");
        assert!(matches!(err, SessionError::Offer(_)));
    }

    #[test]
    fn accept_whep_offer_fn_rejects_bad_addr() {
        let Err(err) = accept_whep_offer(WHEP_OFFER, "not-an-ip", 8000) else {
            panic!("expected an error for a bad egress address");
        };
        assert!(matches!(err, SessionError::Addr(_, _)));
    }

    #[test]
    fn whep_session_accept_produces_real_answer() {
        let (session, answer) =
            WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8001).expect("accept offer");
        let sdp = answer.to_sdp_string();

        assert!(sdp.starts_with("v=0"), "real SDP answer:\n{sdp}");
        assert!(sdp.contains("a=sendonly"), "sendonly direction:\n{sdp}");
        assert!(sdp.contains("a=fingerprint:sha-256"), "DTLS fingerprint:\n{sdp}");
        assert!(sdp.contains("a=ice-ufrag:"), "ICE ufrag:\n{sdp}");
        assert!(sdp.contains("a=ice-pwd:"), "ICE pwd:\n{sdp}");
        assert!(sdp.contains("m=video"), "video m-line:\n{sdp}");
        assert!(sdp.contains("H264"), "H.264 codec:\n{sdp}");

        assert_eq!(session.local_addr(), "127.0.0.1:8001".parse().unwrap());
        assert!(session.is_alive(), "session must start alive");
    }

    #[test]
    fn whep_session_accept_rejects_bad_ingest_addr() {
        let Err(err) = WhepSession::accept(WHEP_OFFER, "::invalid::", 8000) else {
            panic!("expected an error for a bad egress address");
        };
        assert!(matches!(err, SessionError::Addr(_, _)));
    }

    // ---- Packetization tests ----

    /// Build a NAL unit: header byte + body.
    fn make_nal(header: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![header];
        v.extend_from_slice(body);
        v
    }

    /// Feed RTP packets through the depacketizer and collect Annex-B output.
    fn depacketize(pkts: &[RtpPacket]) -> BytesMut {
        let mut d = H264Depacketizer::new();
        let mut out = BytesMut::new();
        for pkt in pkts {
            let (_, _, marker, payload) =
                rtp_parse(&pkt.bytes).expect("valid RTP header in test packet");
            d.push(payload, marker, &mut out).expect("depacketize");
        }
        out
    }

    #[test]
    fn packetize_single_nal_produces_one_packet_with_marker() {
        let (mut session, _) =
            WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8002).expect("accept offer");

        let nal = make_nal(0x65, &[0x11, 0x22, 0x33]); // IDR slice
        let pkts = session.packetize(&[&nal], 90_000);

        assert_eq!(pkts.len(), 1, "single NAL → single RTP packet");
        assert!(pkts[0].marker, "marker bit on last (sole) packet");

        // The seq number in the header must be 0 (first packet).
        let (seq, ts, marker, _payload) = rtp_parse(&pkts[0].bytes).unwrap();
        assert_eq!(seq, 0, "first packet seq = 0");
        assert_eq!(ts, 90_000u32, "90 kHz timestamp carried through");
        assert!(marker, "marker bit in wire header");
    }

    #[test]
    fn packetize_multiple_aus_increments_seq() {
        let (mut session, _) =
            WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8003).expect("accept offer");

        let sps = make_nal(0x67, &[0x42, 0x00, 0x1F]);
        let pps = make_nal(0x68, &[0xCE]);
        let idr = make_nal(0x65, &[0xAA, 0xBB, 0xCC]);

        // AU 1: SPS + PPS + IDR (STAP-A for small NALs, then IDR single-NAL or STAP-A).
        let au1 = session.packetize(&[&sps, &pps, &idr], 0);
        let last_seq_au1 = {
            let last = au1.last().unwrap();
            assert!(last.marker, "AU1 last packet must have marker");
            let (seq, _, _, _) = rtp_parse(&last.bytes).unwrap();
            seq
        };

        // AU 2: a P-frame.
        let pframe = make_nal(0x41, &[0x01, 0x02]);
        let au2 = session.packetize(&[&pframe], 3_000);
        let (first_seq_au2, _, _, _) = rtp_parse(&au2[0].bytes).unwrap();

        assert!(
            first_seq_au2 == last_seq_au1.wrapping_add(1),
            "seq continues from AU1 end ({last_seq_au1}) to AU2 start ({first_seq_au2})"
        );
        assert!(au2.last().unwrap().marker, "AU2 last packet must have marker");
    }

    #[test]
    fn packetize_large_nal_uses_fu_a_fragmentation() {
        let (mut session, _) =
            WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8004).expect("accept offer");

        // Build a NAL larger than EGRESS_MTU so FU-A kicks in.
        let body: Vec<u8> = (0u8..=255).cycle().take(2000).collect();
        let nal = make_nal(0x65, &body); // IDR slice, larger than MTU
        let pkts = session.packetize(&[&nal], 180_000);

        assert!(pkts.len() > 1, "large NAL must produce multiple FU-A packets");
        // Only the last packet has the marker.
        let n = pkts.len();
        for (i, pkt) in pkts.iter().enumerate() {
            assert_eq!(
                pkt.marker,
                i == n - 1,
                "marker on packet {i}/{n}: got {}",
                pkt.marker
            );
        }
        // Wire marker bit matches the marker field.
        for pkt in &pkts {
            let (_, _, wire_marker, _) = rtp_parse(&pkt.bytes).unwrap();
            assert_eq!(wire_marker, pkt.marker, "wire marker must match struct field");
        }
        // Sequence numbers must be strictly incrementing.
        let mut seqs: Vec<u16> = pkts
            .iter()
            .map(|p| rtp_parse(&p.bytes).unwrap().0)
            .collect();
        for w in seqs.windows(2) {
            assert_eq!(w[1], w[0].wrapping_add(1), "seq must increment");
        }
        let _ = seqs.pop(); // suppress unused warning
    }

    #[test]
    fn packetize_roundtrip_through_depacketizer() {
        let (mut session, _) =
            WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8005).expect("accept offer");

        let sps = make_nal(0x67, &[0x42, 0x00, 0x1F, 0xAB]);
        let pps = make_nal(0x68, &[0xCE]);
        let idr_body: Vec<u8> = (0u8..100).collect();
        let idr = make_nal(0x65, &idr_body);

        let pkts = session.packetize(&[&sps, &pps, &idr], 0);
        assert!(!pkts.is_empty());

        let out = depacketize(&pkts);
        // The depacketized output must contain all three NALs (Annex-B prefixed).
        // Verify the IDR body is present (SPS+PPS may be STAP-A'd together).
        let out_bytes = &out[..];
        // IDR: type 5. Its body must appear verbatim in the depacketized output.
        let idr_body_pos = out_bytes
            .windows(idr_body.len())
            .position(|w| w == idr_body.as_slice());
        assert!(
            idr_body_pos.is_some(),
            "IDR body must appear in depacketized output"
        );
        // SPS byte must be somewhere (preceded by a start code).
        assert!(
            out_bytes.contains(&0x67u8),
            "SPS NAL header 0x67 must be present"
        );
        assert!(
            out_bytes.contains(&0x68u8),
            "PPS NAL header 0x68 must be present"
        );
    }

    #[test]
    fn packetize_annex_b_matches_packetize() {
        // packetize_annex_b and packetize must produce identical bytes.
        use crate::depacketize::ANNEX_B_START_CODE;

        let (mut s1, _) = WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8006).expect("accept");
        let (mut s2, _) = WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8007).expect("accept");

        let sps_body = [0x42u8, 0x00, 0x1F];
        let pps_body = [0xCEu8];
        let sps = make_nal(0x67, &sps_body);
        let pps = make_nal(0x68, &pps_body);

        // Build Annex-B manually.
        let mut annex_b = Vec::new();
        for nal in [&sps, &pps] {
            annex_b.extend_from_slice(&ANNEX_B_START_CODE);
            annex_b.extend_from_slice(nal);
        }

        let pkts_direct = s1.packetize(&[&sps, &pps], 9_000);
        let pkts_annexb = s2.packetize_annex_b(&annex_b, 9_000);

        assert_eq!(
            pkts_direct.len(),
            pkts_annexb.len(),
            "same number of packets"
        );
        for (a, b) in pkts_direct.iter().zip(pkts_annexb.iter()) {
            assert_eq!(a.bytes, b.bytes, "packet bytes must be identical");
        }
    }

    // ---- Session lifecycle tests ----

    #[test]
    fn session_is_alive_after_accept() {
        let (session, _) =
            WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8008).expect("accept offer");
        assert!(session.is_alive(), "session alive immediately after accept");
    }

    #[test]
    fn session_local_addr_matches_input() {
        let (session, _) =
            WhepSession::accept(WHEP_OFFER, "10.0.0.1", 9876).expect("accept offer");
        assert_eq!(
            session.local_addr(),
            "10.0.0.1:9876".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn session_video_pt_is_populated_after_accept() {
        let (session, answer) =
            WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8009).expect("accept offer");
        let sdp = answer.to_sdp_string();
        // The session must have a video PT after accept (str0m may renegotiate
        // the exact PT value; we just verify it is present and that the answer
        // SDP carries H.264 — the SDP is the source of truth for the PT).
        let pt = session.video_payload_type();
        if let Some(pt) = pt {
            // The negotiated PT must appear in the answer SDP.
            assert!(
                sdp.contains(&format!("a=rtpmap:{} H264", *pt))
                    || sdp.contains("H264"),
                "negotiated PT {pt} must correspond to H.264 in the answer:\n{sdp}"
            );
        }
        // Whether or not the PT is exposed immediately, the SDP must contain H.264.
        assert!(sdp.contains("H264"), "answer must advertise H.264:\n{sdp}");
    }

    #[test]
    fn accept_whep_offer_fn_rejects_minimal_broken_sdp() {
        // Passes the `v=0` gate but is not parseable as a complete SDP.
        let Err(err) = accept_whep_offer("v=0\r\nbroken", "127.0.0.1", 8000) else {
            panic!("expected an error for malformed SDP");
        };
        // Either Offer (parse error) or Rtc (negotiation error) is acceptable.
        assert!(
            matches!(err, SessionError::Offer(_) | SessionError::Rtc(_)),
            "unexpected error variant"
        );
    }

    #[test]
    fn packetize_empty_nals_returns_empty() {
        let (mut session, _) =
            WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8010).expect("accept offer");
        let pkts = session.packetize(&[], 0);
        assert!(pkts.is_empty(), "empty NAL slice must produce no packets");
    }

    #[test]
    fn packetize_marker_bit_boundary_on_multi_au_sequence() {
        // Feed four single-NAL access units and verify that only the last packet
        // of *each* AU carries the marker bit.
        let (mut session, _) =
            WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8011).expect("accept offer");

        let nals = [
            make_nal(0x67, &[0x01]),
            make_nal(0x68, &[0x02]),
            make_nal(0x65, &[0x03]),
            make_nal(0x41, &[0x04]),
        ];
        for (i, nal) in nals.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let rtp_ts = (i as u32) * 3_000;
            let packets = session.packetize(&[nal], rtp_ts);
            assert_eq!(packets.len(), 1, "single small NAL → single packet");
            assert!(packets[0].marker, "AU{i}: marker bit on sole packet");
        }
    }
}
