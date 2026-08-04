use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use aero_common::{CallId, ParticipantId};
use aero_live_webrtc::{
    BridgeRtp, CallEgress, InboundRtp, KeyframeRequestKind, MediaForwarder, Mid, PeerProgress, Rid,
    SfuError, SfuPeer, SfuPeerSink,
};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::SfuMediaError;

/// Inbound datagram scratch size (a jumbo-ish RTP/RTCP packet).
const RECV_BUF: usize = 2048;
/// A slow subscriber must not back-pressure every publisher in the call.
const SESSION_COMMAND_CAPACITY: usize = 256;

enum SessionCommand {
    WriteRtp(Box<InboundRtp>),
    RequestKeyframe {
        mid: Mid,
        rid: Option<Rid>,
        kind: KeyframeRequestKind,
    },
    RequestRemb {
        mid: Mid,
        bitrate_bps: u64,
    },
    RemoteIce {
        candidate: String,
        result: oneshot::Sender<Result<(), SfuError>>,
    },
}

/// Cloneable command endpoint for one server-owned SFU media session.
///
/// The corresponding [`SfuPeer`] never leaves its session task. Both outbound
/// RTP writes and trickled ICE candidates are serialized through this bounded
/// queue, so no second task can poll or mutate the peer concurrently.
#[derive(Clone)]
pub struct SfuMediaSessionHandle {
    tx: mpsc::Sender<SessionCommand>,
}

impl SfuPeerSink for SfuMediaSessionHandle {
    fn try_write_rtp(&self, packet: InboundRtp) -> bool {
        self.tx
            .try_send(SessionCommand::WriteRtp(Box::new(packet)))
            .is_ok()
    }

    fn try_request_keyframe(&self, mid: Mid, kind: KeyframeRequestKind) -> bool {
        self.request_keyframe(mid, None, kind).is_ok()
    }

    fn try_request_keyframe_for_rid(
        &self,
        mid: Mid,
        rid: Option<Rid>,
        kind: KeyframeRequestKind,
    ) -> bool {
        self.request_keyframe(mid, rid, kind).is_ok()
    }

    fn try_request_remb(&self, mid: Mid, bitrate_bps: u64) -> bool {
        self.request_remb(mid, bitrate_bps).is_ok()
    }
}

impl SfuMediaSessionHandle {
    pub(super) fn request_keyframe(
        &self,
        mid: Mid,
        rid: Option<Rid>,
        kind: KeyframeRequestKind,
    ) -> Result<(), SfuMediaError> {
        self.tx
            .try_send(SessionCommand::RequestKeyframe { mid, rid, kind })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => SfuMediaError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => SfuMediaError::Closed,
            })
    }

    pub(super) fn request_remb(&self, mid: Mid, bitrate_bps: u64) -> Result<(), SfuMediaError> {
        self.tx
            .try_send(SessionCommand::RequestRemb { mid, bitrate_bps })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => SfuMediaError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => SfuMediaError::Closed,
            })
    }

    pub(super) async fn add_remote_candidate(
        &self,
        candidate: String,
    ) -> Result<(), SfuMediaError> {
        let (result_tx, result_rx) = oneshot::channel();
        self.tx
            .try_send(SessionCommand::RemoteIce {
                candidate,
                result: result_tx,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => SfuMediaError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => SfuMediaError::Closed,
            })?;
        result_rx.await.map_err(|_| SfuMediaError::Closed)??;
        Ok(())
    }
}

/// One participant's SFU media session: a str0m peer bound to a media socket,
/// forwarding its decrypted RTP into the local SFU and (when the call spans
/// nodes) onto the cross-node egress.
pub struct SfuMediaSession {
    peer: SfuPeer,
    socket: UdpSocket,
    local_addr: SocketAddr,
    forwarder: Arc<dyn MediaForwarder>,
    /// Present when this call has cross-node subscribers: each forwarded packet
    /// is also published here for the bridge egress to relay (ROADMAP 方向五).
    egress: Option<CallEgress>,
    commands: mpsc::Receiver<SessionCommand>,
    command_handle: SfuMediaSessionHandle,
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
        forwarder: Arc<dyn MediaForwarder>,
        egress: Option<CallEgress>,
        bind_addr: &str,
    ) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(bind_addr).await?;
        let local_addr = socket.local_addr()?;
        Self::from_bound_socket(call, participant, forwarder, egress, socket, local_addr)
    }

    /// Bind a socket while advertising a separately configured host address.
    /// This is the production constructor: the socket normally binds
    /// `0.0.0.0:0`, while browsers must see a reachable pod/node address.
    pub(super) async fn bind_advertised(
        call: CallId,
        participant: ParticipantId,
        forwarder: Arc<dyn MediaForwarder>,
        egress: Option<CallEgress>,
        bind_addr: &str,
        advertise_host: &str,
    ) -> Result<Self, SfuMediaError> {
        let socket = UdpSocket::bind(bind_addr).await?;
        let local_addr = socket.local_addr()?;
        let advertised = resolve_advertised_addr(advertise_host, local_addr).await?;
        Ok(Self::from_bound_socket(
            call,
            participant,
            forwarder,
            egress,
            socket,
            advertised,
        )?)
    }

    fn from_bound_socket(
        call: CallId,
        participant: ParticipantId,
        forwarder: Arc<dyn MediaForwarder>,
        egress: Option<CallEgress>,
        socket: UdpSocket,
        advertised_addr: SocketAddr,
    ) -> std::io::Result<Self> {
        let mut peer = SfuPeer::new(call, participant);
        peer.add_local_candidate(advertised_addr)
            .map_err(std::io::Error::other)?;
        let (command_tx, commands) = mpsc::channel(SESSION_COMMAND_CAPACITY);
        Ok(Self {
            peer,
            socket,
            local_addr: advertised_addr,
            forwarder,
            egress,
            commands,
            command_handle: SfuMediaSessionHandle { tx: command_tx },
        })
    }

    pub(super) fn set_egress(&mut self, egress: CallEgress) {
        self.egress = Some(egress);
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

    #[must_use]
    pub fn command_handle(&self) -> SfuMediaSessionHandle {
        self.command_handle.clone()
    }

    /// The SFU `on_rtp` handler: forward one decrypted RTP packet to local
    /// subscribers and, when the call spans nodes, publish it for the bridge
    /// egress to relay (ROADMAP 方向五).
    pub(super) fn deliver(&self, rtp: &InboundRtp) {
        self.forwarder
            .forward_inbound_rtp(self.peer.call(), self.peer.id(), rtp);
        if let Some(egress) = &self.egress {
            egress.publish(BridgeRtp::from_inbound(self.peer.id(), rtp));
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
                    Ok(PeerProgress::KeyframeRequest(request)) => {
                        let out_mid = request.mid.to_string();
                        let _ = self.forwarder.forward_keyframe_request(
                            self.peer.call(),
                            self.peer.id(),
                            &out_mid,
                            request.kind,
                        );
                    }
                    Ok(PeerProgress::BandwidthEstimate(estimate)) => {
                        let out_mid = estimate.mid.map(|mid| mid.to_string());
                        let _ = self.forwarder.forward_bandwidth_estimate(
                            self.peer.call(),
                            self.peer.id(),
                            out_mid.as_deref(),
                            estimate.bitrate_bps,
                        );
                    }
                    Ok(PeerProgress::Idle) => {}
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
                command = self.commands.recv() => {
                    let Some(command) = command else { return; };
                    match command {
                        SessionCommand::WriteRtp(rtp) => {
                            let rtp = *rtp;
                            if let Err(error) = self.peer.write_rtp(
                                rtp.mid,
                                rtp.pt,
                                rtp.seq_no,
                                rtp.rtp_time,
                                rtp.wallclock,
                                rtp.marker,
                                rtp.ext_vals,
                                rtp.payload,
                            ) {
                                debug!(%error, peer = %self.peer.id(), "sfu-media: outbound RTP write failed");
                            }
                        }
                        SessionCommand::RequestKeyframe { mid, rid, kind } => {
                            if let Err(error) =
                                self.peer.request_keyframe_for_rid(mid, rid, kind)
                            {
                                debug!(%error, peer = %self.peer.id(), "sfu-media: upstream keyframe request failed");
                            }
                        }
                        SessionCommand::RequestRemb { mid, bitrate_bps } => {
                            if let Err(error) = self.peer.request_remb(mid, bitrate_bps) {
                                debug!(%error, peer = %self.peer.id(), "sfu-media: upstream REMB request failed");
                            }
                        }
                        SessionCommand::RemoteIce { candidate, result } => {
                            let _ = result.send(self.peer.add_remote_candidate(&candidate));
                        }
                    }
                }
            }
        }
    }
}

pub(super) async fn resolve_advertised_addr(
    advertise_host: &str,
    bound: SocketAddr,
) -> std::io::Result<SocketAddr> {
    if advertise_host.trim().is_empty() {
        if bound.ip().is_unspecified() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "AERO_SFU_ADVERTISE_HOST is required when binding an unspecified address",
            ));
        }
        return Ok(bound);
    }
    if let Ok(ip) = advertise_host.parse::<IpAddr>() {
        if ip.is_ipv4() != bound.is_ipv4() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "SFU advertise host address family does not match the media socket",
            ));
        }
        return Ok(SocketAddr::new(ip, bound.port()));
    }
    tokio::net::lookup_host((advertise_host, bound.port()))
        .await?
        .find(|candidate| candidate.is_ipv4() == bound.is_ipv4())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "SFU advertise host resolved to no address matching the media socket",
            )
        })
}
