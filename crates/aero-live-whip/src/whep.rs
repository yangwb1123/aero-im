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

use std::net::{IpAddr, SocketAddr};
use std::time::Instant;

use str0m::change::{SdpAnswer, SdpOffer};
use str0m::format::Codec;
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
    /// Payload type negotiated for packetization-mode 1 H.264 video.
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
        aero_signaling::signaling::validate_sdp(offer_sdp)
            .map_err(|error| SessionError::Offer(error.to_string()))?;
        let egress_ip: IpAddr = egress_host
            .parse()
            .map_err(|_| SessionError::Addr(egress_host.to_string(), egress_port))?;
        let local_addr = SocketAddr::new(egress_ip, egress_port);

        let offer =
            SdpOffer::from_sdp_string(offer_sdp).map_err(|e| SessionError::Offer(e.to_string()))?;

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

        // Resolve the PT against this media's negotiated remote PT list. Looking
        // only at codec_config is insufficient: str0m retains multiple H.264
        // profiles and their default PTs alongside the entry remapped by the
        // browser's offer.
        let video_mid = first_video_mid(offer_sdp)
            .ok_or_else(|| SessionError::Offer("WHEP offer has no video mid".to_string()))?;
        let video_pt = negotiated_h264_payload_type(&rtc, video_mid).ok_or_else(|| {
            SessionError::Offer(
                "WHEP offer has no negotiated H.264 packetization-mode=1 payload".to_string(),
            )
        })?;

        Ok((
            Self {
                rtc,
                local_addr,
                video_pt: Some(video_pt),
                // SSRC is assigned later via Event::MediaAdded.
                video_ssrc: None,
                video_mid: Some(video_mid),
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
                                    && self.video_mid == Some(ma.mid)
                                {
                                    // Re-resolve at the event boundary so the
                                    // packetizer and write_rtp cannot drift from
                                    // the media's final negotiated PT.
                                    self.video_pt = negotiated_h264_payload_type(&self.rtc, ma.mid);
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
            if let Err(e) = stream.write_rtp(pt, seq, ts, now, marker, ext_vals, true, payload_vec)
            {
                warn!(error = %e, "whep: write_rtp failed");
            }
        }
    }
}

/// Find the first video media identifier in an SDP offer.
///
/// str0m exposes media by `Mid`, but not an iterator over all negotiated media.
/// The validated SDP supplies the stable mapping from the video m-line to that
/// identifier.
fn first_video_mid(sdp: &str) -> Option<Mid> {
    let mut in_video = false;
    for raw_line in sdp.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.starts_with("m=") {
            in_video = line.starts_with("m=video ");
            continue;
        }
        if in_video {
            if let Some(mid) = line.strip_prefix("a=mid:").map(str::trim) {
                if !mid.is_empty() {
                    return Some(Mid::from(mid));
                }
            }
        }
    }
    None
}

/// Select the exact H.264 PT the remote negotiated for a media section.
///
/// The remote PT order is authoritative for an answerer. Packetization mode 1
/// is required because [`WhepPacketizer`] emits FU-A fragments for large NALs.
fn negotiated_h264_payload_type(rtc: &Rtc, mid: Mid) -> Option<Pt> {
    let remote_pts = rtc.media(mid)?.remote_pts();
    remote_pts.iter().copied().find(|remote_pt| {
        rtc.codec_config().params().iter().any(|params| {
            params.pt() == *remote_pt
                && params.spec().codec == Codec::H264
                && params.spec().format.packetization_mode == Some(1)
        })
    })
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
mod tests;
