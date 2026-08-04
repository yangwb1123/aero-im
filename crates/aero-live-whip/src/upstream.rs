//! WHEP upstream relay client: pull H.264 from a peer node via WebRTC.
//!
//! [`WhepUpstreamSource`] implements [`UpstreamSource`] by opening a WHEP
//! `recvonly` session with a remote peer node, receiving H.264 RTP over
//! ICE/DTLS/SRTP (str0m), depacketizing it, and yielding [`UpstreamAu`]s via
//! an async channel.
//!
//! ## Design
//!
//! The constructor (`connect`) does the synchronous part — SDP offer generation,
//! the HTTP WHEP exchange, and ICE candidate setup — then spawns a background
//! task that owns the str0m receive loop. Access units flow from the task to the
//! caller through a bounded `mpsc` channel (capacity 64); the background task
//! blocks on `send` if the consumer is slow, providing natural backpressure.
//!
//! The loop mirrors [`crate::session::WhipSession::run`] but in `RecvOnly` mode:
//! we receive RTP from the remote publisher instead of sending it.
//!
//! ## Runtime-verifiability
//!
//! `connect` error-path tests (bad address, bad SDP URL) can run without a real
//! network. The full loop (`run_receive_loop`) requires a live WHEP peer — it is
//! compiled and reviewed but not unit-tested here, consistent with
//! [`crate::session::WhipSession::run`].

use std::net::SocketAddr;
use std::time::Instant;

use async_trait::async_trait;
use bytes::BytesMut;
use str0m::change::SdpAnswer;
use str0m::media::{Direction, MediaKind, MediaTime, Pt};
use str0m::net::{Protocol, Receive};
use str0m::rtp::RtpPacket;
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tracing::{debug, trace, warn};
use ulid::Ulid;

use crate::cascade::{UpstreamAu, UpstreamSource};
use crate::depacketize::H264Depacketizer;
use crate::reorder::ReorderBuffer;
use crate::session::{SessionError, DEFAULT_REORDER_WINDOW};

/// Channel capacity between the receive loop and the `next_au` consumer.
const AU_CHANNEL_CAP: usize = 64;

/// UDP receive buffer — WebRTC datagrams never exceed path MTU; 2 KiB suffices.
const RECV_BUF: usize = 2048;

/// A WHEP relay client that pulls H.264 from a peer node's WHEP endpoint and
/// yields access units via [`UpstreamSource::next_au`].
///
/// Construct with [`WhepUpstreamSource::connect`]. The background WebRTC receive
/// task starts immediately. Dropping this struct closes the AU receiver; the
/// transport task then exits when the peer/link ends or its next AU send observes
/// the closed channel. A server that enables cascade must additionally own
/// explicit task cancellation and the upstream WHEP resource `DELETE` lifecycle.
#[derive(Debug)]
pub struct WhepUpstreamSource {
    stream_id: Ulid,
    rx: tokio::sync::mpsc::Receiver<UpstreamAu>,
}

impl WhepUpstreamSource {
    /// Connect to a remote WHEP endpoint and begin receiving H.264.
    ///
    /// The caller must supply a `local_host`/`local_port` that is reachable from
    /// the remote peer — this address is advertised as str0m's ICE host candidate
    /// and the relay UDP socket is bound here.
    ///
    /// # Errors
    ///
    /// Returns an error if address parsing, ICE candidate creation, the WHEP
    /// HTTP exchange, SDP answer parsing, or UDP socket binding fails.
    pub async fn connect(
        stream_id: Ulid,
        whep_url: &str,
        local_host: &str,
        local_port: u16,
    ) -> Result<Self, SessionError> {
        let local_addr: SocketAddr = format!("{local_host}:{local_port}")
            .parse()
            .map_err(|_| SessionError::Addr(local_host.to_string(), local_port))?;

        let mut rtc = Rtc::builder().set_rtp_mode(true).build(Instant::now());

        // Advertise a host ICE candidate at our relay socket address.
        let candidate = Candidate::host(local_addr, "udp")
            .map_err(|e| SessionError::Candidate(e.to_string()))?;
        let _ = rtc.add_local_candidate(candidate);

        // Build a RecvOnly video offer and extract the pending negotiation state.
        let mut changes = rtc.sdp_api();
        changes.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        let (offer, pending) = changes
            .apply()
            .ok_or_else(|| SessionError::Offer("no pending SDP changes".to_string()))?;

        // POST the SDP offer to the remote peer's WHEP endpoint.
        let offer_sdp = offer.to_sdp_string();
        let http = reqwest::Client::new();
        let answer_sdp = http
            .post(whep_url)
            .header("Content-Type", "application/sdp")
            .body(offer_sdp)
            .send()
            .await
            .map_err(|e| SessionError::Offer(format!("WHEP HTTP request: {e}")))?
            .text()
            .await
            .map_err(|e| SessionError::Offer(format!("WHEP response body: {e}")))?;

        let answer = SdpAnswer::from_sdp_string(&answer_sdp)
            .map_err(|e| SessionError::Offer(format!("parse WHEP answer: {e}")))?;

        // Finalize codec negotiation.
        rtc.sdp_api().accept_answer(pending, answer)?;

        // Capture the negotiated video payload types for the receive loop.
        let video_pts: Vec<Pt> = rtc
            .codec_config()
            .params()
            .iter()
            .filter(|p| p.spec().codec.is_video())
            .map(str0m::format::PayloadParams::pt)
            .collect();

        // Bind the relay UDP socket and spawn the background receive task.
        let socket = UdpSocket::bind(local_addr).await?;
        let (tx, rx) = tokio::sync::mpsc::channel::<UpstreamAu>(AU_CHANNEL_CAP);
        tokio::spawn(run_receive_loop(
            stream_id, rtc, socket, local_addr, video_pts, tx,
        ));

        Ok(Self { stream_id, rx })
    }
}

#[async_trait]
impl UpstreamSource for WhepUpstreamSource {
    fn stream_id(&self) -> Ulid {
        self.stream_id
    }

    async fn next_au(&mut self) -> Option<UpstreamAu> {
        self.rx.recv().await
    }
}

/// Drive the str0m receive loop, depacketize H.264 RTP, and push access units.
///
/// Mirrors [`crate::session::WhipSession::run`] in `RecvOnly` mode. The loop
/// ends when the RTC connection closes, an unrecoverable error occurs, or the
/// AU channel is dropped (receiver end gone — cascade relay was dropped).
async fn run_receive_loop(
    _stream_id: Ulid,
    mut rtc: Rtc,
    socket: UdpSocket,
    local_addr: SocketAddr,
    video_pts: Vec<Pt>,
    tx: tokio::sync::mpsc::Sender<UpstreamAu>,
) {
    let mut depacketizer = H264Depacketizer::new();
    let mut buf = vec![0u8; RECV_BUF];
    let mut au = BytesMut::new();
    let mut reorder_buf: Option<ReorderBuffer<RtpPacket>> = None;

    loop {
        // Drain str0m output until it requests input (returns a Timeout).
        let timeout = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => {
                    // ICE checks, DTLS handshake, RTCP — send them out.
                    if let Err(e) = socket.send_to(&t.contents, t.destination).await {
                        warn!(error = %e, "whep-upstream: udp send failed");
                    }
                }
                Ok(Output::Event(ev)) => {
                    let keep_running = handle_event(
                        ev,
                        &video_pts,
                        &mut depacketizer,
                        &mut au,
                        &mut reorder_buf,
                        &tx,
                    )
                    .await;
                    if !keep_running || !rtc.is_alive() {
                        debug!("whep-upstream: session ended");
                        return;
                    }
                }
                Err(e) => {
                    warn!(error = %e, "whep-upstream: poll_output error");
                    return;
                }
            }
        };

        let wait = timeout.saturating_duration_since(Instant::now());

        tokio::select! {
            res = socket.recv_from(&mut buf) => {
                let Ok((n, source)) = res else { return; };
                let Ok(contents) = (&buf[..n]).try_into() else { continue; };
                let receive = Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: local_addr,
                    contents,
                };
                if let Err(e) = rtc.handle_input(Input::Receive(Instant::now(), receive)) {
                    warn!(error = %e, "whep-upstream: handle_input error");
                    return;
                }
            }
            () = tokio::time::sleep(wait) => {
                if let Err(e) = rtc.handle_input(Input::Timeout(Instant::now())) {
                    warn!(error = %e, "whep-upstream: timeout error");
                    return;
                }
            }
        }
    }
}

/// Process a single str0m event. Returns `false` if the session should end.
async fn handle_event(
    ev: Event,
    video_pts: &[Pt],
    depacketizer: &mut H264Depacketizer,
    au: &mut BytesMut,
    reorder_buf: &mut Option<ReorderBuffer<RtpPacket>>,
    tx: &tokio::sync::mpsc::Sender<UpstreamAu>,
) -> bool {
    match ev {
        Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
            debug!("whep-upstream: ice disconnected, ending session");
            return false;
        }
        Event::IceConnectionStateChange(state) => {
            debug!(?state, "whep-upstream: ice state change");
        }
        Event::RtpPacket(pkt) => {
            let seq = pkt.header.sequence_number;
            let rbuf = reorder_buf
                .get_or_insert_with(|| ReorderBuffer::with_start(seq, DEFAULT_REORDER_WINDOW));

            // Collect completed AUs synchronously, then send them after.
            let mut completed: Vec<UpstreamAu> = Vec::new();
            rbuf.push(seq, pkt, &mut |_seq, maybe_pkt| {
                if let Some(p) = maybe_pkt {
                    let p_pt = p.header.payload_type;
                    let p_marker = p.header.marker;
                    let pts_90k = media_time_to_90k(p.time);
                    if video_pts.contains(&p_pt) {
                        if let Err(e) = depacketizer.push(&p.payload, p_marker, au) {
                            trace!(error = %e, "whep-upstream: h264 depacketize error, resyncing");
                            depacketizer.reset();
                            au.clear();
                            return;
                        }
                        // RTP marker bit signals end of an H.264 access unit.
                        if p_marker && !au.is_empty() {
                            let frame = au.split().freeze();
                            completed.push(UpstreamAu::new(frame, pts_90k));
                        }
                    }
                } else {
                    // Gap declared lost — reset so a stale FU-A never corrupts
                    // the next fragment; resync on the next IDR keyframe.
                    trace!("whep-upstream: rtp gap declared lost, resyncing depacketizer");
                    depacketizer.reset();
                    au.clear();
                }
            });

            for upstream_au in completed {
                if tx.send(upstream_au).await.is_err() {
                    debug!("whep-upstream: au channel closed, stopping receive loop");
                    return false;
                }
            }
        }
        _ => {}
    }
    true
}

/// Convert a str0m `MediaTime` to 90 kHz ticks (the unit the HLS muxer uses).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn media_time_to_90k(time: MediaTime) -> u64 {
    (time.as_seconds() * 90_000.0).max(0.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_local_addr_returns_addr_error() {
        // A syntactically invalid host triggers SessionError::Addr immediately.
        // We use tokio::runtime::Runtime to drive the async constructor.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(WhepUpstreamSource::connect(
            Ulid::new(),
            "http://192.0.2.1:8080/whep/test",
            "not a valid ip [[[",
            9000,
        ));
        assert!(
            matches!(result, Err(SessionError::Addr(..))),
            "expected Addr error, got {result:?}"
        );
    }

    #[test]
    fn unreachable_whep_url_returns_offer_error() {
        // An HTTP request to a non-existent server returns an Offer error.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(WhepUpstreamSource::connect(
            Ulid::new(),
            "http://192.0.2.1:9999/whep/nonexistent",
            "127.0.0.1",
            0,
        ));
        assert!(
            matches!(result, Err(SessionError::Offer(..))),
            "expected Offer error, got {result:?}"
        );
    }
}
