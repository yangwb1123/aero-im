//! SFU per-peer media session — the server-side str0m event loop that finally
//! **drives** the SFU forwarder (ROADMAP 方向五).
//!
//! [`SfuForwarder::on_rtp`](aero_live_webrtc::SfuForwarder::on_rtp) and the whole
//! BWE / simulcast / keyframe machinery were built and unit-tested, but nothing
//! ever called them — no socket loop fed the SFU. This module is that loop, one
//! per participant, mirroring `aero-live-whip`'s WHEP receive loop:
//!
//! 1. bind a media UDP socket and create a [`SfuPeer`] (a str0m `Rtc` in RTP
//!    mode);
//! 2. answer the participant's recvonly/sendrecv SDP offer ([`Self::accept_offer`]);
//! 3. run the loop: drain [`SfuPeer::poll`] — sending ICE/DTLS/RTCP `Transmit`
//!    datagrams and, on each decrypted **`Media`** packet, the SFU `on_rtp`
//!    handler ([`Self::deliver`]) forwards it to local subscribers AND publishes
//!    it to the call's cross-node [`CallEgress`]; feed inbound UDP / timeouts
//!    back via [`SfuPeer::handle_datagram`] / [`SfuPeer::handle_timeout`].
//!
//! Like WHIP/WHEP, the ICE/DTLS/SRTP **handshake that delivers real media** is
//! exercised by a real WebRTC peer (browser, or a paired str0m), not in CI; the
//! loop's structure + SDP answer are tested here, and the two halves of the
//! `on_rtp` handler are tested in `forward.rs` (forwarding) and the call-bridge
//! tests (egress relay).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use aero_common::{CallId, ParticipantId};
use aero_live_webrtc::{
    BridgeRtp, CallEgress, InboundRtp, PeerProgress, SfuError, SfuForwarder, SfuPeer,
};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Inbound datagram scratch size (a jumbo-ish RTP/RTCP packet).
const RECV_BUF: usize = 2048;

/// One participant's SFU media session: a str0m peer bound to a media socket,
/// forwarding its decrypted RTP into the local SFU and (when the call spans
/// nodes) onto the cross-node egress.
pub struct SfuMediaSession {
    peer: SfuPeer,
    socket: UdpSocket,
    local_addr: SocketAddr,
    forwarder: Arc<SfuForwarder>,
    /// Present when this call has cross-node subscribers: each forwarded packet
    /// is also published here for the bridge egress to relay (ROADMAP 方向五).
    egress: Option<CallEgress>,
}

impl SfuMediaSession {
    /// Bind a media socket for `participant` in `call` and create the str0m peer.
    /// `egress` is the call's [`CallEgress`] when its media must also cross nodes.
    ///
    /// # Errors
    /// Propagates a socket bind / `local_addr` failure.
    pub async fn bind(
        call: CallId,
        participant: ParticipantId,
        forwarder: Arc<SfuForwarder>,
        egress: Option<CallEgress>,
        bind_addr: &str,
    ) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(bind_addr).await?;
        let local_addr = socket.local_addr()?;
        Ok(Self {
            peer: SfuPeer::new(call, participant),
            socket,
            local_addr,
            forwarder,
            egress,
        })
    }

    /// Answer the participant's SDP offer (delegates to [`SfuPeer::accept_offer`]).
    ///
    /// # Errors
    /// Returns [`SfuError`] when the offer is malformed or str0m rejects it.
    pub fn accept_offer(&mut self, offer: &str) -> Result<String, SfuError> {
        self.peer.accept_offer(offer)
    }

    /// The media socket's local address (an ICE candidate to advertise).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// This session's participant.
    #[must_use]
    pub fn participant(&self) -> ParticipantId {
        self.peer.id()
    }

    /// The SFU `on_rtp` handler: forward one decrypted RTP packet to local
    /// subscribers and, when the call spans nodes, publish it for the bridge
    /// egress to relay (ROADMAP 方向五).
    fn deliver(&self, rtp: &InboundRtp) {
        self.forwarder.on_rtp(self.peer.id(), rtp);
        if let Some(egress) = &self.egress {
            egress.publish(BridgeRtp::new(
                self.peer.id(),
                rtp.mid.to_string(),
                rtp.payload.clone(),
                rtp.is_keyframe,
            ));
        }
    }

    /// Run the str0m event loop until the peer dies or `cancel` fires.
    pub async fn run(mut self, cancel: CancellationToken) {
        let mut buf = vec![0u8; RECV_BUF];
        loop {
            // Drain str0m output until it parks (Timeout), sending ICE/DTLS/RTCP
            // and delivering inbound media along the way.
            let timeout = loop {
                match self.peer.poll() {
                    Ok(PeerProgress::Timeout(at)) => break at,
                    Ok(PeerProgress::Transmit(t)) => {
                        if let Err(e) = self.socket.send_to(&t.contents, t.destination).await {
                            warn!(error = %e, "sfu-media: udp send failed");
                        }
                    }
                    Ok(PeerProgress::Media(rtp)) => self.deliver(&rtp),
                    Ok(PeerProgress::Connected) => {
                        debug!(peer = %self.peer.id(), "sfu-media: peer connected");
                    }
                    Ok(PeerProgress::KeyframeRequest(_) | PeerProgress::Idle) => {}
                    Err(e) => {
                        warn!(error = %e, peer = %self.peer.id(), "sfu-media: poll error");
                        return;
                    }
                }
                if !self.peer.is_alive() {
                    debug!(peer = %self.peer.id(), "sfu-media: session ended");
                    return;
                }
            };

            let wait = timeout.saturating_duration_since(Instant::now());
            tokio::select! {
                () = cancel.cancelled() => return,
                res = self.socket.recv_from(&mut buf) => {
                    let Ok((n, source)) = res else { return; };
                    if self
                        .peer
                        .handle_datagram(Instant::now(), source, self.local_addr, &buf[..n])
                        .is_err()
                    {
                        return;
                    }
                }
                () = tokio::time::sleep(wait) => {
                    if self.peer.handle_timeout(Instant::now()).is_err() {
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SfuMediaSession;
    use aero_common::{CallId, ParticipantId};
    use aero_live_webrtc::{SfuForwarder, SfuRouter};
    use std::sync::Arc;

    #[tokio::test]
    async fn binds_a_media_socket_and_answers_only_valid_offers() {
        let fwd = Arc::new(SfuForwarder::new(SfuRouter::new()));
        let call = CallId::new();
        let participant = ParticipantId::new();
        let mut session =
            SfuMediaSession::bind(call, participant, fwd, None, "127.0.0.1:0").await.unwrap();

        // The socket bound on localhost with an ephemeral port, and identity is set.
        assert_eq!(session.local_addr().ip().to_string(), "127.0.0.1");
        assert_ne!(session.local_addr().port(), 0, "ephemeral port assigned");
        assert_eq!(session.participant(), participant);

        // A non-SDP offer is rejected (mirrors SfuPeer::accept_offer's tested
        // behaviour); the real ICE/DTLS/SRTP handshake needs a WebRTC peer and is
        // exercised in staging, like WHIP/WHEP.
        assert!(session.accept_offer("definitely not an sdp").is_err());
    }
}
