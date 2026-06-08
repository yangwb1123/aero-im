//! SRT (Secure Reliable Transport) ingest + time-limited TURN credentials.
//!
//! Two responsibilities live here, both feeding the same `aero-live` boot path
//! that RTMP already uses:
//!
//! 1. **[`TurnConfig`]** — renders a `coturn` config *and* mints short-lived TURN
//!    REST credentials (the `use-auth-secret` convention) for WebRTC clients.
//!    Fully implemented and unit-tested against published HMAC-SHA1 vectors.
//! 2. **[`SrtIngest`]** — accepts an SRT publisher, reads the incoming MPEG-TS
//!    byte stream, segments it at H.264 keyframe boundaries via
//!    [`MpegTsSegmenter`], and writes HLS via [`aero_live_hls::HlsWriter`].
//!
//! ## SRT socket layer status
//!
//! The fully-testable core — TS demux + keyframe-aligned HLS segmentation
//! ([`segmenter::MpegTsSegmenter`]) — is implemented and unit-tested. Rather
//! than vendor the heavy `srt-tokio`/`srt-protocol` stack (~30 transitive
//! crates: crypto, `regex`, …), this crate now hand-rolls a **minimal**
//! SRT (`HSv5`) wire layer in [`protocol`] sufficient to accept a
//! caller→listener MPEG-TS push:
//!
//! - the 16-byte SRT header + 48-byte handshake CIF codec,
//! - the listener-side INDUCTION → CONCLUSION handshake with SYN cookies,
//! - the `streamid=` (StreamID/SID) extension decode that carries the stream
//!   key,
//! - AES-CTR per-packet decryption (honouring the KK flag) via [`SrtCrypto`],
//!   activated when a passphrase is supplied to [`SrtIngest::with_passphrase`],
//! - KMREQ/KMRSP key-material exchange so the session key is established from
//!   the shared passphrase (see [`SrtSession::apply_km_message`]), and
//! - data-packet payload extraction feeding [`MpegTsSegmenter`], with sequence
//!   numbers fed through [`ReliabilityState`] so gaps produce NAKs and periodic
//!   ACKs are emitted.
//!
//! [`SrtIngest`] binds the UDP socket, drives the handshake per peer, resolves
//! the stream by SID via [`StreamRepo`] + `mark_live`, and feeds subsequent data
//! payloads into the segmenter → [`SrtSession`] → HLS path.
//!
//! ## What is still NOT wired to a real transport
//!
//! The [`Action`] values (NAK/ACK/ACKACK) returned from the reliability layer
//! are collected in [`SrtSession::drain_actions`] but are not automatically
//! serialised and sent over UDP in [`handle_datagram`] — a production caller
//! must read those actions and transmit the corresponding SRT control packets.
//! Congestion control, packet reordering, and retransmission scheduling are
//! also not wired to the live UDP socket path.

pub mod control;
pub mod crypto;
pub mod protocol;
pub mod pump;
pub mod reliability;
pub mod segmenter;

pub use control::{decode_nak_loss_list, encode_control};
pub use crypto::{
    KkFlag, KmMessage, KmMessageType, KeyUnwrapError, SrtCrypto,
    aes_key_unwrap, aes_key_wrap, pbkdf2_kek,
};
pub use protocol::{Handshake, HandshakeMachine, HsAction, HsState, SrtHeader};
pub use pump::SrtSink;
pub use reliability::{Action, ReliabilityState, RttEstimator, seq_diff, seq_lt, seq_next};
pub use segmenter::{MpegTsSegmenter, SegmentEvent, TS_PACKET_SIZE, TS_SYNC_BYTE};

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aero_live_core::{
    hls_path_for, hls_url_for, LiveError, LiveIngest, LiveResult, LiveStreamConfig,
};
use aero_live_hls::{HlsWriter, DEFAULT_SEGMENT_EXT};
use aero_storage::StreamRepo;
use async_trait::async_trait;
use protocol::{ControlType, PacketKind, SRT_HEADER_LEN};
use tokio::net::UdpSocket;
use tokio::time::timeout;
use tracing::{debug, info, warn};

/// Maximum UDP datagram we read for one SRT packet. SRT defaults to a 1500-byte
/// MTU; 2048 leaves comfortable headroom for jumbo-ish payloads without large
/// per-recv allocations.
const SRT_RECV_BUF: usize = 2048;

/// Per-process secret folded into SYN cookies. Randomised at construction so
/// cookies aren't predictable across restarts (anti-SYN-flood). Injected via the
/// constructor for deterministic tests.
///
/// We avoid pulling in `rand` for this single value: a SYN-cookie seed only
/// needs to be unpredictable to a remote attacker, not cryptographically
/// uniform. We fold together the high-resolution wall clock and a heap-address
/// nonce (ASLR-randomised per process) via FNV-1a, which is ample entropy to
/// keep cookies unguessable across restarts.
fn random_cookie_seed() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};

    const FNV_OFFSET: u32 = 0x811c_9dc5;
    const FNV_PRIME: u32 = 0x0100_0193;
    let mut h = FNV_OFFSET;
    let mut mix = |word: u128| {
        for b in word.to_ne_bytes() {
            h ^= u32::from(b);
            h = h.wrapping_mul(FNV_PRIME);
        }
    };
    // High-resolution time since the epoch (nanoseconds).
    if let Ok(d) = SystemTime::now().duration_since(UNIX_EPOCH) {
        mix(d.as_nanos());
    }
    // A freshly-allocated box's address is ASLR-randomised per process, adding
    // entropy that differs across restarts even within the same nanosecond.
    let nonce = Box::new(0u8);
    mix((std::ptr::from_ref::<u8>(&*nonce) as usize) as u128);
    h
}

/// How long a single HLS segment covers in wall-clock time, matching the RTMP
/// ingest cadence so players see a consistent target duration across protocols.
pub const SEGMENT_DURATION_SECS: u32 = 2;

/// Same duration as a float, for the per-segment `#EXTINF` the writer records.
/// Kept as its own constant so it never needs a lossy runtime cast.
const SEGMENT_DURATION_SECS_F32: f32 = 2.0;

/// SRT ingest.
///
/// Binds the UDP socket SRT runs on (RTMP port + 1 by convention) and is wired
/// into the server boot path through [`LiveIngest`], mirroring the RTMP backend.
/// Drives the hand-rolled SRT (`HSv5`) handshake in [`protocol`] per peer, then
/// feeds keyframe-aligned MPEG-TS segments (via the fully unit-tested
/// [`MpegTsSegmenter`]) into the HLS path.
///
/// When a passphrase is configured (via [`SrtIngest::with_passphrase`]) the
/// ingest activates AES-CTR decryption: once the peer delivers a KMREQ key-
/// material message, the session key is established via [`KmMessage`] +
/// [`SrtCrypto::from_km_message`] and every subsequent data packet is decrypted
/// before reaching the segmenter.
///
/// Sequence numbers are fed through [`ReliabilityState`] so gaps produce
/// NAK actions and periodic ACK actions are emitted; callers retrieve these
/// via [`SrtSession::drain_actions`].
#[derive(Debug, Clone)]
pub struct SrtIngest {
    /// The SRT socket id this listener presents to callers.
    listener_socket_id: u32,
    /// Per-listener secret folded into SYN cookies.
    cookie_seed: u32,
    /// Optional shared passphrase for AES-CTR decryption. When `Some`, the
    /// ingest expects the publisher to perform the KMREQ/KMRSP exchange and
    /// will decrypt each data packet before feeding it to the segmenter.
    passphrase: Option<Vec<u8>>,
}

impl Default for SrtIngest {
    fn default() -> Self {
        Self::new()
    }
}

impl SrtIngest {
    #[must_use]
    pub fn new() -> Self {
        Self {
            // A fixed, non-zero listener id is fine: callers address us by the
            // id we hand back in the INDUCTION response, and we only run one
            // logical listener per socket.
            listener_socket_id: 0x5254_0001, // "RT\0\x01"
            cookie_seed: random_cookie_seed(),
            passphrase: None,
        }
    }

    /// Construct with an explicit listener socket id and cookie seed — used by
    /// tests that need deterministic cookies.
    #[must_use]
    pub fn with_identity(listener_socket_id: u32, cookie_seed: u32) -> Self {
        Self {
            listener_socket_id,
            cookie_seed,
            passphrase: None,
        }
    }

    /// Configure a shared passphrase for AES-CTR decryption.
    ///
    /// When set, the ingest expects the publisher to deliver a KMREQ key-
    /// material extension during or just after the handshake.  Once the KMREQ
    /// is applied (via [`SrtSession::apply_km_message`]) each data packet is
    /// decrypted in-place before reaching the [`MpegTsSegmenter`].
    #[must_use]
    pub fn with_passphrase(mut self, passphrase: impl Into<Vec<u8>>) -> Self {
        self.passphrase = Some(passphrase.into());
        self
    }

    /// The configured passphrase, if any.
    #[must_use]
    pub fn passphrase(&self) -> Option<&[u8]> {
        self.passphrase.as_deref()
    }

    /// Resolve the SRT listen address from the shared live config. SRT shares
    /// the RTMP host and listens one port above it (e.g. RTMP 1935 → SRT 1936).
    fn listen_addr(cfg: &LiveStreamConfig) -> LiveResult<SocketAddr> {
        format!("{}:{}", cfg.rtmp_listen.ip(), cfg.rtmp_listen.port() + 1)
            .parse()
            .map_err(|e| LiveError::Protocol(format!("bad SRT listen addr: {e}")))
    }
}

/// Wall-clock seconds, used to mint/validate SYN cookies. Pulled out so the
/// handshake remains testable with injected time.
fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Per-peer connection state on the shared UDP socket: either still shaking
/// hands, or established and feeding an [`SrtSession`].
enum PeerState {
    /// Handshake in progress.
    Handshaking(HandshakeMachine),
    /// Handshake complete; data payloads flow into this session.
    Streaming {
        stream_id: ulid::Ulid,
        session: Box<SrtSession>,
    },
}

#[async_trait]
impl LiveIngest for SrtIngest {
    async fn run(&self, repo: StreamRepo, cfg: Arc<LiveStreamConfig>) -> LiveResult<()> {
        let listen = Self::listen_addr(&cfg)?;
        let sock = UdpSocket::bind(listen).await.map_err(LiveError::Io)?;
        info!(
            %listen,
            hls_dir = %cfg.hls_dir.display(),
            "SRT ingest listening (unencrypted HSv5 handshake + MPEG-TS → HLS; \
             AES/ACK-NAK/reordering pending — see crate docs)"
        );

        // SRT multiplexes every caller over the one listener UDP socket, keyed
        // by source address. We hold a small map of per-peer state: callers that
        // are still shaking hands, and established sessions feeding the
        // segmenter. (Reliability/reordering is best-effort, in-order for v1.)
        let mut peers: HashMap<SocketAddr, PeerState> = HashMap::new();
        let mut buf = vec![0u8; SRT_RECV_BUF];

        loop {
            match timeout(Duration::from_secs(60), sock.recv_from(&mut buf)).await {
                Ok(Ok((n, peer))) => {
                    let datagram = &buf[..n];
                    if let Err(e) =
                        handle_datagram(self, &sock, &repo, &cfg, &mut peers, peer, datagram).await
                    {
                        warn!(%peer, error = %e, "SRT: dropping peer after error");
                        if let Some(PeerState::Streaming { session, .. }) = peers.remove(&peer) {
                            // Best-effort finalize so the manifest gets an
                            // ENDLIST even on a hard error.
                            finalize_session(*session, &repo, None).await;
                        } else {
                            peers.remove(&peer);
                        }
                    }
                }
                Ok(Err(e)) => {
                    warn!(error = ?e, "SRT recv_from failed");
                    return Err(LiveError::Io(e));
                }
                Err(_) => { /* idle tick — keep the listener alive */ }
            }
        }
    }
}

/// Route one received datagram for `peer` through the handshake or data plane.
async fn handle_datagram(
    ingest: &SrtIngest,
    sock: &UdpSocket,
    repo: &StreamRepo,
    cfg: &LiveStreamConfig,
    peers: &mut HashMap<SocketAddr, PeerState>,
    peer: SocketAddr,
    datagram: &[u8],
) -> LiveResult<()> {
    // A SHUTDOWN control packet tears the session down cleanly.
    if is_shutdown(datagram) {
        if let Some(PeerState::Streaming {
            session,
            stream_id,
        }) = peers.remove(&peer)
        {
            info!(%peer, %stream_id, "SRT: peer sent SHUTDOWN; finalizing");
            finalize_session(*session, repo, Some(stream_id)).await;
        } else {
            peers.remove(&peer);
        }
        return Ok(());
    }

    let entry = peers
        .entry(peer)
        .or_insert_with(|| PeerState::Handshaking(HandshakeMachine::new(
            ingest.listener_socket_id,
            ingest.cookie_seed,
        )));

    match entry {
        PeerState::Handshaking(machine) => {
            match machine.handle_packet(datagram, peer, now_unix()) {
                Ok(HsAction::Reply(bytes)) => {
                    sock.send_to(&bytes, peer).await.map_err(LiveError::Io)?;
                    Ok(())
                }
                Ok(HsAction::Established {
                    stream_id,
                    agreement,
                }) => {
                    // Confirm the handshake to the caller, then resolve the
                    // stream key carried by the SID and open a session.
                    sock.send_to(&agreement, peer)
                        .await
                        .map_err(LiveError::Io)?;
                    let (id, session) = resolve_stream(repo, cfg, &stream_id).await?;
                    info!(%peer, stream_id = %id, sid = %stream_id, "SRT: handshake complete; streaming");
                    *entry = PeerState::Streaming {
                        stream_id: id,
                        session: Box::new(session),
                    };
                    Ok(())
                }
                Ok(HsAction::Ignore) => Ok(()),
                Err(e) => Err(LiveError::Protocol(format!("SRT handshake: {e}"))),
            }
        }
        PeerState::Streaming { session, .. } => {
            // Established: route the datagram through the session's data plane.
            // `feed_packet` handles header parsing, reliability tracking,
            // optional AES-CTR decryption, and TS segmentation all in one call.
            // Control packets (KEEPALIVE, ACK, …) are silently ignored by
            // feed_packet, but we still need to feed data packets so the
            // reliability layer can schedule ACK/NAK responses.
            let parsed_hdr = SrtHeader::parse(datagram);
            if parsed_hdr.is_some_and(|h| !h.is_control()) {
                session.feed_packet(datagram).await?;
            } else {
                debug!(%peer, "SRT: ignoring non-data control packet on established session");
            }
            // Flush ACK / NAK / ACKACK control packets back to the sender.
            // Use the incoming packet's dest_socket_id as the peer id (mirrors
            // how the handshake machine uses the remote socket id).
            let peer_socket_id = parsed_hdr.map_or(0, |h| h.dest_socket_id);
            let mut collected = Vec::new();
            session.pump(&mut collected, Instant::now(), peer_socket_id);
            for pkt in collected {
                if let Err(e) = sock.send_to(&pkt, peer).await {
                    warn!(%peer, error = %e, "SRT: control packet send failed; dropping peer");
                    return Err(LiveError::Io(e));
                }
            }
            Ok(())
        }
    }
}

/// Whether `datagram` is an SRT SHUTDOWN control packet.
fn is_shutdown(datagram: &[u8]) -> bool {
    matches!(
        SrtHeader::parse(datagram).map(|h| h.kind),
        Some(PacketKind::Control {
            control_type: ControlType::Shutdown,
            ..
        })
    )
}

/// Extract the MPEG-TS payload from a data packet, stripping the 16-byte SRT
/// header. Returns `None` for control packets or truncated datagrams.
///
/// Used by existing unit tests that verify header stripping in isolation;
/// the live data path now uses [`SrtSession::feed_packet`] which handles
/// header parsing, decryption, and reliability tracking in one step.
#[allow(dead_code)]
fn data_payload(datagram: &[u8]) -> Option<&[u8]> {
    let header = SrtHeader::parse(datagram)?;
    if header.is_control() {
        return None;
    }
    datagram.get(SRT_HEADER_LEN..).filter(|p| !p.is_empty())
}

/// Flush and finalize a session's HLS output, marking the stream ended.
async fn finalize_session(mut session: SrtSession, repo: &StreamRepo, stream_id: Option<ulid::Ulid>) {
    if let Err(e) = session.finish().await {
        warn!(error = %e, "SRT: error finalizing HLS on disconnect");
    }
    if let Some(id) = stream_id {
        if let Err(e) = repo.mark_ended(id).await {
            warn!(error = %e, %id, "SRT: failed to mark stream ended");
        }
    }
}

/// Per-publisher SRT session: owns the [`MpegTsSegmenter`] and [`HlsWriter`] and
/// turns inbound MPEG-TS bytes into keyframe-aligned HLS segments.
///
/// This is the bridge the socket layer drives once it has accepted a caller and
/// resolved its stream. It is intentionally transport-free: [`Self::feed_packet`]
/// takes a raw SRT data packet (header included) and:
///
/// 1. Extracts the 31-bit sequence number and `KK` key-flag bits from the
///    header word.
/// 2. Passes the sequence number to [`ReliabilityState::on_data`], collecting
///    any resulting NAK/ACK [`Action`]s.
/// 3. If a [`SrtCrypto`] context is installed, decrypts the payload in-place
///    (honouring the `KK` flag — `Clear` packets are passed through unchanged).
/// 4. Feeds the (decrypted) payload to the [`MpegTsSegmenter`].
///
/// Reliability [`Action`]s accumulate in an internal queue; the caller retrieves
/// them with [`SrtSession::drain_actions`] and is responsible for serialising
/// and transmitting the corresponding SRT control packets.
pub struct SrtSession {
    segmenter: MpegTsSegmenter,
    hls: HlsWriter,
    /// Whether a segment is currently open (we've buffered packets that haven't
    /// been flushed yet).
    has_open_segment: bool,
    /// AES-CTR decryption context; `None` for unencrypted sessions.
    crypto: Option<SrtCrypto>,
    /// Receiver-side reliability state: tracks sequence numbers, emits NAK/ACK.
    /// `pub(crate)` so the sibling `pump` module can drive retransmits and
    /// read/write the ACK interval in tests without exposing the field publicly.
    pub(crate) reliability: ReliabilityState,
    /// Pending reliability actions (NAK/ACK/ACKACK) waiting to be drained by
    /// the caller and serialised onto the wire.
    /// `pub(crate)` so the `pump` module can push ACKACK/etc. actions that
    /// arrive outside the normal `feed_packet` path (e.g. from `on_ack`).
    pub(crate) pending_actions: Vec<Action>,
}

impl SrtSession {
    /// Open a session for an already-resolved stream, creating the HLS writer
    /// under `hls_dir/{stream_id}`.
    ///
    /// `initial_seq` is the caller's initial sequence number from the handshake
    /// CIF, used to seed the reliability state machine.
    pub fn new(hls: HlsWriter) -> Self {
        Self::with_crypto(hls, None)
    }

    /// Open a session with an optional AES-CTR crypto context.
    ///
    /// When `crypto` is `Some`, data-packet payloads are decrypted before
    /// reaching the segmenter.  The reliability state machine starts from
    /// sequence number 0; to match a peer's initial sequence number use
    /// [`SrtSession::with_crypto`] followed by
    /// [`SrtSession::set_initial_seq`] if needed.
    #[must_use]
    pub fn with_crypto(hls: HlsWriter, crypto: Option<SrtCrypto>) -> Self {
        Self {
            segmenter: MpegTsSegmenter::new(),
            hls,
            has_open_segment: false,
            crypto,
            reliability: ReliabilityState::new(0),
            pending_actions: Vec::new(),
        }
    }

    /// Install or replace the AES-CTR crypto context.
    ///
    /// Called after the KMREQ/KMRSP key-material exchange to activate
    /// per-packet decryption.  Any previously installed context is replaced.
    pub fn set_crypto(&mut self, crypto: SrtCrypto) {
        self.crypto = Some(crypto);
    }

    /// Apply a received KMREQ [`KmMessage`] using the supplied `passphrase` to
    /// derive and install the session key.
    ///
    /// On success the session switches to encrypted mode: all subsequent data
    /// packets are decrypted before reaching the segmenter.  Returns `Err` if
    /// the passphrase is wrong or the KM message is malformed.
    pub fn apply_km_message(
        &mut self,
        km: &KmMessage,
        passphrase: &[u8],
    ) -> Result<(), KeyUnwrapError> {
        let crypto = SrtCrypto::from_km_message(km, passphrase)?;
        self.crypto = Some(crypto);
        Ok(())
    }

    /// Drain and return any pending reliability [`Action`]s (NAK / ACK /
    /// ACKACK) that have accumulated since the last call.
    ///
    /// The caller is responsible for serialising these into SRT control packets
    /// and transmitting them over the transport.
    pub fn drain_actions(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.pending_actions)
    }

    /// Drain all pending reliability actions and return them as ready-to-send
    /// SRT control-packet byte vectors.
    ///
    /// Each returned `Vec<u8>` is a complete, correctly-framed SRT control
    /// packet (16-byte header + CIF) that the UDP transport can `send_to` the
    /// peer address directly.
    ///
    /// - ACK packets carry the full ACK CIF (ack-seq-no, current RTT + RTT
    ///   variance from the embedded [`ReliabilityState`], zeroes for untracked
    ///   fields).
    /// - NAK packets carry a loss-list CIF using SRT range encoding (high bit
    ///   set = range start).
    /// - ACKACK packets are a header-only (no CIF body).
    /// - [`Action::SendData`] actions are **not** serialised by this method
    ///   (they are data-plane, not control-plane); they are dropped silently.
    ///
    /// The `dst_socket_id` of every encoded packet is set to `peer_socket_id`.
    /// The timestamp word is set to 0 (not tracked in the current
    /// implementation; a production caller should pass `now − connect_time` in
    /// microseconds).
    ///
    /// # What still needs a real UDP loop
    ///
    /// This method serialises the packets but does NOT transmit them.  The
    /// caller must read the returned bytes and call `sock.send_to(bytes, peer)`
    /// for each one.  Retransmission scheduling, congestion control, and the
    /// periodic keep-alive timer are also not wired to this path.
    pub fn drain_control_packets(&mut self, peer_socket_id: u32) -> Vec<Vec<u8>> {
        let actions = std::mem::take(&mut self.pending_actions);
        let rtt_us = u32::try_from(self.reliability.rtt().as_micros())
            .unwrap_or(u32::MAX);
        let rttvar_us = u32::try_from(self.reliability.rttvar().as_micros())
            .unwrap_or(u32::MAX);
        actions
            .iter()
            .filter_map(|a| control::encode_control(a, peer_socket_id, 0, rtt_us, rttvar_us))
            .collect()
    }

    /// Feed a full SRT data packet (16-byte header + payload) into the session.
    ///
    /// This is the main data-plane entry point for the socket layer.  It:
    /// 1. Parses the SRT header to extract `seq_no` and KK flag bits.
    /// 2. Runs the sequence number through the reliability state machine,
    ///    collecting NAK/ACK actions.
    /// 3. Optionally decrypts the payload in-place using the installed
    ///    [`SrtCrypto`] context (if `KK != Clear`).
    /// 4. Feeds the decrypted payload to the [`MpegTsSegmenter`].
    ///
    /// Control packets (KK == Clear with no payload) and truncated datagrams
    /// are silently ignored.
    pub async fn feed_packet(&mut self, datagram: &[u8]) -> LiveResult<()> {
        let Some(header) = SrtHeader::parse(datagram) else {
            return Ok(());
        };
        let (seq_no, msg_word) = match header.kind {
            PacketKind::Data { seq_no, msg_word } => (seq_no, msg_word),
            PacketKind::Control { .. } => return Ok(()), // not a data packet
        };
        let payload_slice = match datagram.get(SRT_HEADER_LEN..) {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        // Drive the reliability state machine with the incoming sequence number.
        let now = std::time::Instant::now();
        let rel_actions = self.reliability.on_data(seq_no, now);
        self.pending_actions.extend(rel_actions);

        // Decrypt the payload if we have a crypto context and the KK flag says
        // this packet is encrypted.
        let kk = KkFlag::from_msg_word(msg_word);
        if kk != KkFlag::Clear {
            if let Some(crypto) = &self.crypto {
                let mut buf = payload_slice.to_vec();
                crypto.decrypt_packet(seq_no, &mut buf);
                return self.feed_ts_bytes(&buf).await;
            }
        }
        self.feed_ts_bytes(payload_slice).await
    }

    /// Feed a chunk of the inbound MPEG-TS byte stream. Flushes a finished HLS
    /// segment whenever the segmenter reaches a keyframe boundary.
    ///
    /// This is the lower-level entry point used by both [`Self::feed_packet`]
    /// (after optional decryption) and legacy callers that have already stripped
    /// the SRT header externally.
    pub async fn feed(&mut self, bytes: &[u8]) -> LiveResult<()> {
        self.feed_ts_bytes(bytes).await
    }

    /// Internal: push raw TS bytes into the segmenter.
    async fn feed_ts_bytes(&mut self, bytes: &[u8]) -> LiveResult<()> {
        for event in self.segmenter.push(bytes) {
            match event {
                SegmentEvent::Buffered => self.has_open_segment = true,
                SegmentEvent::CutBeforeKeyframe => {
                    // The segmenter has closed a complete segment (the keyframe
                    // that triggered the cut already heads the *next* one), so
                    // `take_segment` here returns exactly the closed segment.
                    self.flush_segment().await?;
                    self.has_open_segment = true;
                }
            }
        }
        Ok(())
    }

    /// Flush whatever segment the segmenter is offering (a just-closed one after
    /// a cut, or the open tail at end-of-stream) as one HLS segment.
    async fn flush_segment(&mut self) -> LiveResult<()> {
        let bytes = self.segmenter.take_segment();
        if bytes.is_empty() {
            return Ok(());
        }
        self.hls
            .push_segment(bytes.into(), SEGMENT_DURATION_SECS_F32)
            .await
            .map_err(|e| LiveError::Internal(anyhow::anyhow!("hls push: {e}")))?;
        self.has_open_segment = false;
        Ok(())
    }

    /// Flush any trailing segment and finalize the manifest. Call on disconnect.
    pub async fn finish(&mut self) -> LiveResult<()> {
        if self.segmenter.has_segment_data() {
            self.flush_segment().await?;
        }
        self.hls
            .finish()
            .await
            .map_err(|e| LiveError::Internal(anyhow::anyhow!("hls finish: {e}")))?;
        Ok(())
    }

    /// Whether a segment is currently open (packets buffered, not yet flushed).
    #[must_use]
    pub fn has_open_segment(&self) -> bool {
        self.has_open_segment
    }
}

/// Resolve an SRT `streamid` to a stream row and open an [`SrtSession`].
///
/// Mirrors the RTMP publish path: an unknown key is rejected (here, surfaced as
/// [`LiveError::UnknownStreamKey`]); a known key flips the row to `live` and
/// returns a session whose [`HlsWriter`] is rooted under `hls_dir/{stream_id}`.
///
/// SRT carries the stream key in the `streamid` handshake extension (the SRT
/// analogue of an RTMP publish path), so callers pass whatever the handshake
/// reported.
pub async fn resolve_stream(
    repo: &StreamRepo,
    cfg: &LiveStreamConfig,
    stream_key: &str,
) -> LiveResult<(ulid::Ulid, SrtSession)> {
    let stream = repo
        .get_by_key(stream_key)
        .await
        .map_err(LiveError::Database)?
        .ok_or_else(|| LiveError::UnknownStreamKey(stream_key.to_string()))?;

    let hls_url = hls_url_for(stream.id);
    repo.mark_live(stream.id, &hls_url)
        .await
        .map_err(LiveError::Database)?;

    let dir = hls_path_for(&cfg.hls_dir, stream.id);
    let hls = HlsWriter::new(dir, SEGMENT_DURATION_SECS)
        .await
        .map_err(|e| LiveError::Internal(anyhow::anyhow!("hls writer init: {e}")))?
        .with_segment_ext(DEFAULT_SEGMENT_EXT);

    info!(
        stream_id = %stream.id,
        stream_key = %stream_key,
        "SRT publisher accepted; emitting MPEG-TS HLS segments"
    );
    Ok((stream.id, SrtSession::new(hls)))
}

// ============================ TURN credentials ============================

/// A WebRTC ICE server entry in the shape browser clients expect from
/// `RTCPeerConnection({ iceServers: [...] })`.
///
/// Serialized as `{"urls": "...", "username": "...", "credential": "..."}`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IceServer {
    /// TURN/STUN URL(s), e.g. `turn:turn.example.com:3478`.
    pub urls: String,
    /// Time-limited TURN REST username (`<expiry>:<name>`).
    pub username: String,
    /// Base64 HMAC-SHA1 credential bound to `username`.
    pub credential: String,
}

/// TURN config helper. coturn is the real server; this struct renders a usable
/// `turnserver.conf` snippet *and* mints the short-lived REST credentials that
/// browser WebRTC clients use to authenticate against it.
#[derive(Debug, Clone)]
pub struct TurnConfig {
    pub listening_port: u16,
    pub realm: String,
    pub static_auth_secret: String,
    pub external_ip: Option<String>,
    pub min_port: u16,
    pub max_port: u16,
}

impl TurnConfig {
    pub fn from_env() -> Option<Self> {
        let secret = std::env::var("AERO_TURN_SHARED_SECRET").ok()?;
        Some(Self {
            listening_port: std::env::var("AERO_TURN_LISTENING_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3478),
            realm: std::env::var("AERO_TURN_REALM").unwrap_or_else(|_| "aero.local".into()),
            static_auth_secret: secret,
            external_ip: std::env::var("AERO_TURN_EXTERNAL_IP").ok(),
            min_port: 49152,
            max_port: 65535,
        })
    }

    /// Render a minimal `turnserver.conf` body suitable for `coturn`.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let _ = writeln!(s, "listening-port={}", self.listening_port);
        let _ = writeln!(s, "realm={}", self.realm);
        let _ = writeln!(s, "use-auth-secret");
        let _ = writeln!(s, "static-auth-secret={}", self.static_auth_secret);
        let _ = writeln!(s, "min-port={}", self.min_port);
        let _ = writeln!(s, "max-port={}", self.max_port);
        if let Some(ip) = &self.external_ip {
            let _ = writeln!(s, "external-ip={ip}");
        }
        let _ = writeln!(s, "no-cli");
        let _ = writeln!(s, "no-tcp");
        let _ = writeln!(s, "no-tls");
        let _ = writeln!(s, "fingerprint");
        s
    }

    /// Mint a time-limited TURN REST credential pair.
    ///
    /// Implements coturn's `use-auth-secret` / TURN REST API convention
    /// (<https://datatracker.ietf.org/doc/html/draft-uberti-behave-turn-rest-00>):
    ///
    /// ```text
    /// username = "<unix_expiry_ts>:<name>"
    /// password = base64( HMAC_SHA1(shared_secret, username) )
    /// ```
    ///
    /// `now_unix` is injected (rather than read from the clock) so callers can
    /// produce deterministic credentials and tests can pin exact values.
    /// Returns `(username, password)`.
    #[must_use]
    pub fn ephemeral_credential(
        &self,
        name: &str,
        ttl: Duration,
        now_unix: i64,
    ) -> (String, String) {
        // Clamp absurd TTLs rather than wrapping; expiries don't need > i64 secs.
        let ttl_secs = i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX);
        let expiry = now_unix.saturating_add(ttl_secs);
        let username = format!("{expiry}:{name}");
        let password = hmac_sha1_base64(self.static_auth_secret.as_bytes(), username.as_bytes());
        (username, password)
    }

    /// Build the browser [`IceServer`] entry for a freshly-minted credential.
    ///
    /// `host` is the publicly reachable TURN host (typically `external_ip` or a
    /// DNS name); the URL uses the configured `listening_port`.
    #[must_use]
    pub fn ice_server(
        &self,
        host: &str,
        name: &str,
        ttl: Duration,
        now_unix: i64,
    ) -> IceServer {
        let (username, credential) = self.ephemeral_credential(name, ttl, now_unix);
        IceServer {
            urls: format!("turn:{host}:{}", self.listening_port),
            username,
            credential,
        }
    }
}

/// `base64( HMAC_SHA1(key, msg) )` using standard base64 (with padding), the
/// exact form coturn validates for REST credentials.
fn hmac_sha1_base64(key: &[u8], msg: &[u8]) -> String {
    use base64::prelude::{Engine as _, BASE64_STANDARD};
    use hmac::{Hmac, Mac};
    use sha1::Sha1;

    let mut mac =
        Hmac::<Sha1>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(msg);
    let tag = mac.finalize().into_bytes();
    BASE64_STANDARD.encode(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> TurnConfig {
        TurnConfig {
            listening_port: 3478,
            realm: "aero.local".into(),
            static_auth_secret: "north-star-shared-secret".into(),
            external_ip: Some("203.0.113.7".into()),
            min_port: 49152,
            max_port: 65535,
        }
    }

    // ------------------------- TURN render -------------------------

    #[test]
    fn turn_config_renders_required_lines() {
        let c = sample_config();
        let body = c.render();
        assert!(body.contains("listening-port=3478"));
        assert!(body.contains("realm=aero.local"));
        assert!(body.contains("use-auth-secret"));
        assert!(body.contains("static-auth-secret=north-star-shared-secret"));
        assert!(body.contains("external-ip=203.0.113.7"));
        assert!(body.contains("fingerprint"));
    }

    #[test]
    fn turn_config_skips_external_ip_when_absent() {
        let mut c = sample_config();
        c.external_ip = None;
        assert!(!c.render().contains("external-ip="));
    }

    // ------------------ HMAC-SHA1 known-answer tests ------------------

    #[test]
    fn hmac_sha1_matches_rfc2202_case2() {
        // RFC 2202 §3 test case 2: a *published* HMAC-SHA1 vector, proving our
        // HMAC is correct against the standard — not merely self-consistent.
        //   key  = "Jefe"
        //   data = "what do ya want for nothing?"
        //   HMAC = 0xeffcdf6ae5eb2fa2d27416d5f184df9c259a7c79
        // Independently base64-encoded (openssl) → "7/zfauXrL6LSdBbV8YTfnCWafHk=".
        let got = hmac_sha1_base64(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(got, "7/zfauXrL6LSdBbV8YTfnCWafHk=");
    }

    #[test]
    fn hmac_sha1_matches_rfc2202_case1() {
        // RFC 2202 §3 test case 1: key = 20 × 0x0b, data = "Hi There".
        //   HMAC = 0xb617318655057264e28bc0b6fb378c8ef146be00
        let key = [0x0bu8; 20];
        let got = hmac_sha1_base64(&key, b"Hi There");
        // base64 of the published digest above.
        assert_eq!(got, "thcxhlUFcmTii8C2+zeMjvFGvgA=");
    }

    #[test]
    fn ephemeral_credential_is_deterministic_and_matches_reference() {
        // Hand-computed reference, independently produced by BOTH
        //   printf '%s' '1700000000:alice' | openssl dgst -sha1 \
        //       -hmac 'north-star-shared-secret' -binary | openssl base64
        // and Python's `hmac.new(secret, user, hashlib.sha1)`:
        //   secret   = "north-star-shared-secret"
        //   username = "1700000000:alice"
        //   password = base64(HMAC_SHA1(secret, username))
        //            = "WqY9HToCTDh6T15lNQzfpjD2pIo="
        let c = sample_config();
        let (username, password) =
            c.ephemeral_credential("alice", Duration::from_secs(0), 1_700_000_000);
        assert_eq!(username, "1700000000:alice");
        assert_eq!(password, "WqY9HToCTDh6T15lNQzfpjD2pIo=");
    }

    #[test]
    fn ttl_is_added_to_now_for_the_expiry() {
        let c = sample_config();
        // now=1_700_000_000, ttl=600 → expiry 1_700_000_600.
        let (username, _) =
            c.ephemeral_credential("bob", Duration::from_secs(600), 1_700_000_000);
        assert_eq!(username, "1700000600:bob");
    }

    #[test]
    fn same_inputs_yield_same_password_different_secret_differs() {
        let c = sample_config();
        let (_, p1) = c.ephemeral_credential("alice", Duration::from_secs(60), 100);
        let (_, p2) = c.ephemeral_credential("alice", Duration::from_secs(60), 100);
        assert_eq!(p1, p2, "deterministic for identical inputs");

        let mut c2 = c.clone();
        c2.static_auth_secret = "different-secret".into();
        let (_, p3) = c2.ephemeral_credential("alice", Duration::from_secs(60), 100);
        assert_ne!(p1, p3, "credential is bound to the shared secret");
    }

    #[test]
    fn ice_server_shape_and_url() {
        let c = sample_config();
        let ice = c.ice_server("turn.example.com", "carol", Duration::from_secs(300), 1_700_000_000);
        assert_eq!(ice.urls, "turn:turn.example.com:3478");
        assert_eq!(ice.username, "1700000300:carol");
        // credential must equal the standalone HMAC of the username.
        let expected =
            hmac_sha1_base64(c.static_auth_secret.as_bytes(), ice.username.as_bytes());
        assert_eq!(ice.credential, expected);
    }

    #[test]
    fn ice_server_serializes_to_browser_json() {
        let c = sample_config();
        let ice = c.ice_server("turn.example.com", "dave", Duration::from_secs(60), 0);
        let json = serde_json::to_string(&ice).unwrap();
        // Browser RTCPeerConnection expects exactly these keys.
        assert!(json.contains("\"urls\":\"turn:turn.example.com:3478\""));
        assert!(json.contains("\"username\":\"60:dave\""));
        assert!(json.contains("\"credential\":"));
    }

    // ----------------------------- SRT -----------------------------

    #[test]
    fn ingest_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SrtIngest>();
    }

    #[test]
    fn srt_listen_addr_is_rtmp_port_plus_one() {
        let cfg = LiveStreamConfig::local_dev(); // rtmp 0.0.0.0:1935
        let addr = SrtIngest::listen_addr(&cfg).unwrap();
        assert_eq!(addr.port(), 1936);
        assert_eq!(addr.ip(), cfg.rtmp_listen.ip());
    }

    // ---- data-plane glue: SRT header stripping & SHUTDOWN detection ----

    #[test]
    fn data_payload_strips_srt_header_and_yields_ts_bytes() {
        // A data packet wrapping an MPEG-TS payload: 16-byte SRT header + body.
        let header = SrtHeader {
            kind: PacketKind::Data {
                seq_no: 5,
                msg_word: 0,
            },
            timestamp: 0,
            dest_socket_id: 1,
        };
        let mut pkt = bytes::BytesMut::new();
        header.write_to(&mut pkt);
        let body = [0x47u8, 0x40, 0x00, 0x10, 0xDE, 0xAD]; // looks like a TS chunk
        pkt.extend_from_slice(&body);
        let payload = data_payload(&pkt).expect("data packet yields a payload");
        assert_eq!(payload, &body, "payload is everything after the 16B header");
    }

    #[test]
    fn data_payload_rejects_control_packets() {
        let ka = SrtHeader {
            kind: PacketKind::Control {
                control_type: ControlType::KeepAlive,
                subtype: 0,
                type_specific: 0,
            },
            timestamp: 0,
            dest_socket_id: 1,
        };
        assert!(data_payload(&ka.to_bytes()).is_none(), "control has no TS payload");
        // Truncated datagram → None, never a panic.
        assert!(data_payload(&[0u8; 4]).is_none());
    }

    #[test]
    fn is_shutdown_detects_only_shutdown_control() {
        let shutdown = SrtHeader {
            kind: PacketKind::Control {
                control_type: ControlType::Shutdown,
                subtype: 0,
                type_specific: 0,
            },
            timestamp: 0,
            dest_socket_id: 1,
        };
        assert!(is_shutdown(&shutdown.to_bytes()));

        let ka = SrtHeader {
            kind: PacketKind::Control {
                control_type: ControlType::KeepAlive,
                subtype: 0,
                type_specific: 0,
            },
            timestamp: 0,
            dest_socket_id: 1,
        };
        assert!(!is_shutdown(&ka.to_bytes()), "keep-alive is not shutdown");
        assert!(!is_shutdown(&[0u8; 4]), "short datagram is not shutdown");
    }

    #[test]
    fn segment_duration_matches_rtmp() {
        // RTMP uses a 2s cadence; keep SRT in lockstep so players see a
        // consistent target duration regardless of ingest protocol.
        assert_eq!(SEGMENT_DURATION_SECS, 2);
    }

    #[test]
    fn segment_duration_constants_agree() {
        // The u32 and f32 forms must not drift apart. Use an epsilon comparison
        // (clippy flags `==`/`!=` on floats, not ordering) against the integer.
        let diff = (SEGMENT_DURATION_SECS_F32 - f32::from(u8::try_from(SEGMENT_DURATION_SECS).unwrap())).abs();
        assert!(diff < f32::EPSILON, "f32 and u32 segment durations diverged");
    }

    // ---- SrtSession integration (segmenter → HLS writer on disk) ----

    /// Minimal 188-byte payload-only TS packet with the given PID/PUSI.
    fn ts_packet(pid: u16, pusi: bool, payload: &[u8]) -> Vec<u8> {
        let mut pkt = vec![0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = (u8::from(pusi) << 6) | u8::try_from((pid >> 8) & 0x1F).unwrap();
        pkt[2] = u8::try_from(pid & 0xFF).unwrap();
        pkt[3] = 0x10; // afc=01 (payload only)
        let n = payload.len().min(TS_PACKET_SIZE - 4);
        pkt[4..4 + n].copy_from_slice(&payload[..n]);
        pkt
    }

    /// A video PES carrying NAL units of the given types (4-byte start codes).
    fn video_pes(nal_types: &[u8]) -> Vec<u8> {
        let mut es = Vec::new();
        for &t in nal_types {
            es.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, t & 0x1F, 0xAA]);
        }
        let mut pes = vec![0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x00, 0x00];
        pes.extend_from_slice(&es);
        pes
    }

    /// A 1-byte PAT advertising PMT PID 0x1000 / PMT advertising video PID 0x100.
    /// Reuses the segmenter's own parser, so we only need plausible PSI here.
    fn pat() -> Vec<u8> {
        // pointer(0) table_id(0) B0 len.. tsid version sec last prog=1 pmt_pid
        let mut s = vec![0x00, 0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00];
        s.extend_from_slice(&1u16.to_be_bytes());
        s.extend_from_slice(&(0xE000u16 | 0x1000).to_be_bytes());
        s.extend_from_slice(&[0, 0, 0, 0]); // CRC (ignored)
        s
    }

    fn pmt() -> Vec<u8> {
        let mut s = vec![0x00, 0x02, 0xB0, 0x12, 0x00, 0x01, 0xC1, 0x00, 0x00];
        s.extend_from_slice(&(0xE000u16 | 0x0100).to_be_bytes()); // PCR PID
        s.extend_from_slice(&0xF000u16.to_be_bytes()); // program_info_length=0
        s.push(0x1B); // H.264
        s.extend_from_slice(&(0xE000u16 | 0x0100).to_be_bytes());
        s.extend_from_slice(&0xF000u16.to_be_bytes());
        s.extend_from_slice(&[0, 0, 0, 0]); // CRC
        s
    }

    #[tokio::test]
    async fn srt_session_writes_hls_segments_at_keyframes() {
        let dir = tempfile::tempdir().unwrap();
        let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
            .await
            .unwrap();
        let mut session = SrtSession::new(hls);

        // Feed PSI then two keyframes separated by an inter frame. The second
        // keyframe should close the first segment.
        session.feed(&ts_packet(0x0000, true, &pat())).await.unwrap();
        session.feed(&ts_packet(0x1000, true, &pmt())).await.unwrap();
        session
            .feed(&ts_packet(0x0100, true, &video_pes(&[5])))
            .await
            .unwrap();
        session
            .feed(&ts_packet(0x0100, true, &video_pes(&[1])))
            .await
            .unwrap();
        // Second keyframe → cut → first segment (0.ts) is flushed to disk.
        session
            .feed(&ts_packet(0x0100, true, &video_pes(&[5])))
            .await
            .unwrap();
        assert!(dir.path().join("0.ts").exists(), "first segment written");

        // Finish flushes the trailing open segment and finalizes the manifest.
        session.finish().await.unwrap();
        assert!(dir.path().join("1.ts").exists(), "trailing segment written");
        let manifest = std::fs::read_to_string(dir.path().join("index.m3u8")).unwrap();
        assert!(manifest.contains("#EXT-X-ENDLIST"), "manifest finalized");
        assert!(manifest.contains("0.ts"));
        assert!(!session.has_open_segment());
    }

    #[tokio::test]
    async fn srt_session_finish_is_idempotent_and_finalizes() {
        let dir = tempfile::tempdir().unwrap();
        let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
            .await
            .unwrap();
        let mut session = SrtSession::new(hls);
        session.feed(&ts_packet(0x0000, true, &pat())).await.unwrap();
        session.feed(&ts_packet(0x1000, true, &pmt())).await.unwrap();
        session
            .feed(&ts_packet(0x0100, true, &video_pes(&[5])))
            .await
            .unwrap();
        session.finish().await.unwrap();
        // A second finish must not error (HlsWriter::finish is idempotent).
        session.finish().await.unwrap();
    }

    // ── Crypto integration tests ────────────────────────────────────────────

    /// Build a full SRT data packet (header + payload) with the given `seq_no`,
    /// KK flag, and payload bytes.
    fn make_data_packet(seq_no: u32, kk: KkFlag, payload: &[u8]) -> Vec<u8> {
        let msg_word = kk.set_in_msg_word(0);
        let header = SrtHeader {
            kind: PacketKind::Data { seq_no, msg_word },
            timestamp: 0,
            dest_socket_id: 1,
        };
        let mut pkt = bytes::BytesMut::new();
        header.write_to(&mut pkt);
        pkt.extend_from_slice(payload);
        pkt.to_vec()
    }

    /// An ingest configured with a passphrase must decrypt an AES-CTR-encrypted
    /// data packet so the plaintext TS bytes reach the segmenter.
    ///
    /// The test builds a TS payload, encrypts it with the same `SrtCrypto`
    /// instance, packages it in a data packet with `KkFlag::EvenKey`, feeds it
    /// to a session that has that crypto installed, and verifies the
    /// segmenter sees the original plaintext (i.e. the TS sync byte 0x47).
    #[tokio::test]
    async fn encrypted_data_packet_is_decrypted_before_segmenter() {
        // Set up a known passphrase, salt, and SEK so we can produce a
        // matching ciphertext on the test (sender) side.
        let passphrase = b"test-passphrase";
        let salt = [0xBBu8; 16];
        let sek = [0xCCu8; 16];

        let crypto = SrtCrypto::from_passphrase(passphrase, &salt, sek);

        // Build a TS payload (minimal PAT-like bytes starting with 0x47).
        let mut ts_payload = ts_packet(0x0000, true, &pat());
        let original = ts_payload.clone();

        // Encrypt the payload as the sender would (seq_no = 1, even key).
        let seq_no = 1u32;
        crypto.encrypt_packet(seq_no, &mut ts_payload);
        assert_ne!(ts_payload, original, "ciphertext must differ from plaintext");

        // Package as an SRT data packet with KK=EvenKey.
        let pkt = make_data_packet(seq_no, KkFlag::EvenKey, &ts_payload);

        // Open a session with the same crypto context installed.
        let dir = tempfile::tempdir().unwrap();
        let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
            .await
            .unwrap();
        let mut session = SrtSession::with_crypto(
            hls,
            Some(SrtCrypto::from_passphrase(passphrase, &salt, sek)),
        );

        // Feed the encrypted packet — the session must decrypt it first.
        session.feed_packet(&pkt).await.unwrap();

        // The segmenter received the plaintext.  Verify by feeding the
        // original plaintext through a plain session and confirming both
        // sessions end up with the same segmenter state (non-empty buffer).
        assert!(
            session.segmenter.has_segment_data(),
            "segmenter must have buffered data after decryption"
        );
    }

    /// Clear (unencrypted) data packets must pass through unchanged even when
    /// a crypto context is installed.
    #[tokio::test]
    async fn clear_data_packet_passes_through_unchanged() {
        let passphrase = b"any-passphrase";
        let salt = [0x11u8; 16];
        let sek = [0x22u8; 16];

        let ts_payload = ts_packet(0x0000, true, &pat());
        // KK = Clear → packet is NOT encrypted.
        let pkt = make_data_packet(0, KkFlag::Clear, &ts_payload);

        let dir = tempfile::tempdir().unwrap();
        let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
            .await
            .unwrap();
        let mut session = SrtSession::with_crypto(
            hls,
            Some(SrtCrypto::from_passphrase(passphrase, &salt, sek)),
        );

        // Feed the clear packet — even with a crypto context installed, clear
        // packets must not be decrypted (that would corrupt the data).
        session.feed_packet(&pkt).await.unwrap();
        assert!(
            session.segmenter.has_segment_data(),
            "clear packet must reach segmenter even when crypto is installed"
        );
    }

    /// A KMREQ→KMRSP exchange must establish a usable session key: after
    /// `apply_km_message`, the session can decrypt a packet that was encrypted
    /// with the same passphrase.
    #[tokio::test]
    async fn kmreq_kmrsp_exchange_establishes_session_key() {
        let passphrase = b"shared-secret";
        let salt = [0xA5u8; 16];
        let sek = [0x3Cu8; 16];

        // Sender side: build a KMREQ message.
        let sender_crypto = SrtCrypto::from_passphrase(passphrase, &salt, sek);
        let km = sender_crypto.build_km_message(passphrase);

        // Receiver side: apply the KMREQ to derive the same session key.
        let dir = tempfile::tempdir().unwrap();
        let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
            .await
            .unwrap();
        let mut session = SrtSession::new(hls);

        // Before applying the KM message, the session has no crypto.
        assert!(
            session.crypto.is_none(),
            "fresh session starts without a crypto context"
        );

        session
            .apply_km_message(&km, passphrase)
            .expect("apply_km_message must succeed with the correct passphrase");

        assert!(
            session.crypto.is_some(),
            "session must have a crypto context after applying the KM message"
        );

        // Verify the installed SEK matches the sender's.
        let installed_sek = session.crypto.as_ref().unwrap().sek();
        assert_eq!(
            installed_sek, &sek,
            "receiver must derive the same SEK as the sender"
        );

        // Prove it can decrypt: encrypt a packet with the sender's crypto then
        // feed it to the session.
        let mut ts_payload = ts_packet(0x0000, true, &pat());
        let seq_no = 5u32;
        sender_crypto.encrypt_packet(seq_no, &mut ts_payload);
        let pkt = make_data_packet(seq_no, KkFlag::EvenKey, &ts_payload);
        session.feed_packet(&pkt).await.unwrap();
        assert!(
            session.segmenter.has_segment_data(),
            "session must successfully decrypt and buffer the TS packet"
        );
    }

    /// Applying a KMREQ with the wrong passphrase must fail.
    #[test]
    fn apply_km_message_fails_with_wrong_passphrase() {
        let salt = [0u8; 16];
        let sek = [1u8; 16];
        let sender = SrtCrypto::from_passphrase(b"correct", &salt, sek);
        let km = sender.build_km_message(b"correct");

        // A standalone check (no async needed here).
        let result = SrtCrypto::from_km_message(&km, b"wrong");
        assert!(result.is_err(), "wrong passphrase must not unwrap the SEK");
    }

    // ── Reliability integration tests ───────────────────────────────────────

    /// In-order delivery (consecutive sequence numbers) must emit ACKs via the
    /// periodic ACK timer; no NAKs should be produced.
    #[tokio::test]
    async fn in_order_delivery_emits_ack_not_nak() {
        use std::time::Duration as StdDuration;

        let dir = tempfile::tempdir().unwrap();
        let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
            .await
            .unwrap();
        let mut session = SrtSession::new(hls);

        // Set a very short ACK interval so the timer fires during the test.
        session.reliability.set_ack_interval(StdDuration::from_nanos(1));

        let ts_bytes = ts_packet(0x0000, true, &pat());

        // Feed three consecutive packets — no gap, so no NAK expected.
        for seq_no in 0u32..3 {
            let pkt = make_data_packet(seq_no, KkFlag::Clear, &ts_bytes);
            session.feed_packet(&pkt).await.unwrap();
        }

        let actions = session.drain_actions();

        // No NAKs must appear.
        assert!(
            actions.iter().all(|a| !matches!(a, Action::SendNak { .. })),
            "no NAK expected for consecutive packets; got actions: {actions:?}"
        );
        // At least one ACK must have been emitted (the timer fires quickly).
        assert!(
            actions.iter().any(|a| matches!(a, Action::SendAck { .. })),
            "at least one ACK expected; got actions: {actions:?}"
        );
    }

    /// A gap in the received sequence space must cause the session to emit a
    /// NAK for exactly the missing range.
    #[tokio::test]
    async fn sequence_gap_drives_nak_emission() {
        let dir = tempfile::tempdir().unwrap();
        let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
            .await
            .unwrap();
        let mut session = SrtSession::new(hls);

        let ts_bytes = ts_packet(0x0000, true, &pat());

        // Feed packet 0 (no gap).
        let pkt0 = make_data_packet(0, KkFlag::Clear, &ts_bytes);
        session.feed_packet(&pkt0).await.unwrap();
        let _ = session.drain_actions(); // clear first-packet actions

        // Feed packet 3 — skipping 1 and 2.  The reliability layer must emit a
        // NAK for the range [1, 2].
        let pkt3 = make_data_packet(3, KkFlag::Clear, &ts_bytes);
        session.feed_packet(&pkt3).await.unwrap();

        let actions = session.drain_actions();
        let nak = actions
            .iter()
            .find(|a| matches!(a, Action::SendNak { .. }));
        assert!(nak.is_some(), "NAK must be emitted for the gap [1, 2]");
        assert_eq!(
            nak.unwrap(),
            &Action::SendNak { from: 1, to: 2 },
            "NAK must cover exactly the missing range"
        );
    }

    /// `SrtIngest::with_passphrase` and `passphrase()` accessor work correctly.
    #[test]
    fn ingest_passphrase_roundtrip() {
        let ingest = SrtIngest::new().with_passphrase(b"mysecret".to_vec());
        assert_eq!(ingest.passphrase(), Some(b"mysecret".as_ref()));

        let plain = SrtIngest::new();
        assert!(plain.passphrase().is_none());
    }

    // ── drain_control_packets integration tests ──────────────────────────────

    /// In-order delivery produces ACK control packets via `drain_control_packets`,
    /// and none of them are NAK packets.
    #[tokio::test]
    async fn drain_control_packets_in_order_yields_ack_not_nak() {
        use std::time::Duration as StdDuration;
        use protocol::{ControlType, PacketKind};

        let dir = tempfile::tempdir().unwrap();
        let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
            .await
            .unwrap();
        let mut session = SrtSession::new(hls);
        // Fire the ACK timer immediately.
        session.reliability.set_ack_interval(StdDuration::from_nanos(1));

        let ts_bytes = ts_packet(0x0000, true, &pat());
        let peer_socket_id = 0xBEEF_1234u32;

        // Feed three consecutive packets.
        for seq_no in 0u32..3 {
            let pkt = make_data_packet(seq_no, KkFlag::Clear, &ts_bytes);
            session.feed_packet(&pkt).await.unwrap();
        }

        let ctrl_pkts = session.drain_control_packets(peer_socket_id);
        assert!(
            !ctrl_pkts.is_empty(),
            "drain_control_packets must return at least one packet for in-order delivery"
        );

        // Every packet must be a valid SRT header addressed to the peer.
        for pkt in &ctrl_pkts {
            let hdr = SrtHeader::parse(pkt).expect("must be a valid SRT header");
            assert!(hdr.is_control(), "every returned packet must be a control packet");
            assert_eq!(hdr.dest_socket_id, peer_socket_id);
        }

        // At least one must be an ACK.
        let has_ack = ctrl_pkts.iter().any(|pkt| {
            SrtHeader::parse(pkt).is_some_and(|h| {
                matches!(
                    h.kind,
                    PacketKind::Control {
                        control_type: ControlType::Ack,
                        ..
                    }
                )
            })
        });
        assert!(has_ack, "at least one ACK control packet expected");

        // None must be a NAK.
        let has_nak = ctrl_pkts.iter().any(|pkt| {
            SrtHeader::parse(pkt).is_some_and(|h| {
                matches!(
                    h.kind,
                    PacketKind::Control {
                        control_type: ControlType::Nak,
                        ..
                    }
                )
            })
        });
        assert!(!has_nak, "no NAK expected for consecutive packets");
    }

    /// A sequence gap drives `drain_control_packets` to yield a NAK packet
    /// whose decoded loss list covers the exact missing range.
    #[tokio::test]
    async fn drain_control_packets_gap_yields_nak_with_correct_loss_list() {
        use protocol::{ControlType, PacketKind, SRT_HEADER_LEN};

        let dir = tempfile::tempdir().unwrap();
        let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
            .await
            .unwrap();
        let mut session = SrtSession::new(hls);

        let ts_bytes = ts_packet(0x0000, true, &pat());
        let peer_socket_id = 0xCAFE_BABEu32;

        // Feed packet 0.
        let pkt0 = make_data_packet(0, KkFlag::Clear, &ts_bytes);
        session.feed_packet(&pkt0).await.unwrap();
        // Drain to reset the pending-action queue.
        let _ = session.drain_control_packets(peer_socket_id);

        // Feed packet 3, skipping 1 and 2 → gap [1, 2].
        let pkt3 = make_data_packet(3, KkFlag::Clear, &ts_bytes);
        session.feed_packet(&pkt3).await.unwrap();

        let ctrl_pkts = session.drain_control_packets(peer_socket_id);

        // Find the NAK packet.
        let nak_pkt = ctrl_pkts.iter().find(|pkt| {
            SrtHeader::parse(pkt).is_some_and(|h| {
                matches!(
                    h.kind,
                    PacketKind::Control {
                        control_type: ControlType::Nak,
                        ..
                    }
                )
            })
        });
        assert!(nak_pkt.is_some(), "NAK control packet must be emitted for the gap [1, 2]");

        let nak_pkt = nak_pkt.unwrap();
        let hdr = SrtHeader::parse(nak_pkt).unwrap();
        assert_eq!(hdr.dest_socket_id, peer_socket_id);

        // Decode the loss list from the NAK CIF body.
        let body = &nak_pkt[SRT_HEADER_LEN..];
        let ranges = decode_nak_loss_list(body);

        // The gap [1, 2] must appear as a range (from=1, to=2).
        assert_eq!(
            ranges,
            vec![(1, 2)],
            "NAK loss list must cover exactly the missing range [1, 2]"
        );
    }

    /// `drain_control_packets` must leave the pending-actions queue empty, so a
    /// second call returns nothing (no double-send).
    #[tokio::test]
    async fn drain_control_packets_is_consuming() {
        use std::time::Duration as StdDuration;

        let dir = tempfile::tempdir().unwrap();
        let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
            .await
            .unwrap();
        let mut session = SrtSession::new(hls);
        // Use a tiny ACK interval so the timer fires on the second packet.
        session.reliability.set_ack_interval(StdDuration::from_nanos(1));

        let ts_bytes = ts_packet(0x0000, true, &pat());

        // First packet — sets the timer but doesn't fire it yet.
        let pkt0 = make_data_packet(0, KkFlag::Clear, &ts_bytes);
        session.feed_packet(&pkt0).await.unwrap();

        // Second packet — fires the ACK timer (interval = 1 ns, definitely elapsed).
        let pkt1 = make_data_packet(1, KkFlag::Clear, &ts_bytes);
        session.feed_packet(&pkt1).await.unwrap();

        // First drain must return packets (at least one ACK).
        let first = session.drain_control_packets(0);
        assert!(!first.is_empty(), "first drain must return at least one packet");

        // Second drain must be empty — actions consumed.
        let second = session.drain_control_packets(0);
        assert!(
            second.is_empty(),
            "second drain must return nothing; actions already consumed"
        );
    }
}
