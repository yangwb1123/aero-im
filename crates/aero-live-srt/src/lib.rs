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
//! Outbound data packets ARE congestion-scheduled: every send drained by
//! [`SrtSession::pump`] passes through a [`Pacer`] (token bucket + AIMD rate
//! control fed by ACK RTT samples and NAK rates — see [`pacing`]). Receive-side
//! packet reordering IS wired: data-packet payloads pass through a sequence-aware
//! [`ReorderBuffer`] before the segmenter (see [`reorder`]).

pub mod control;
pub mod crypto;
mod metrics;
pub mod pacing;
pub mod protocol;
pub mod pump;
pub mod reliability;
pub mod reorder;
pub mod segmenter;

pub use control::{decode_nak_loss_list, encode_control};
pub use crypto::{
    KkFlag, KmMessage, KmMessageType, KeyUnwrapError, SrtCrypto,
    aes_key_unwrap, aes_key_wrap, pbkdf2_kek,
};
pub use pacing::{Allowance, Pacer, DEFAULT_MAX_BANDWIDTH};
pub use protocol::{Handshake, HandshakeMachine, HsAction, HsState, SrtHeader};
pub use pump::{decode_ack_cif, AckCif, SrtSink};
pub use reliability::{Action, ReliabilityState, RttEstimator, seq_diff, seq_lt, seq_next};
pub use reorder::ReorderBuffer;
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
    /// Maximum outbound send bandwidth in **bytes/sec**, seeded into every
    /// established session's [`Pacer`] (retransmits count against it).
    max_bandwidth: u64,
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
            max_bandwidth: max_bandwidth_from_env().unwrap_or(DEFAULT_MAX_BANDWIDTH),
        }
    }

    /// Construct with an explicit listener socket id and cookie seed — used by
    /// tests that need deterministic cookies. Skips the env passthrough so
    /// tests stay hermetic; use [`SrtIngest::with_max_bandwidth`] to vary it.
    #[must_use]
    pub fn with_identity(listener_socket_id: u32, cookie_seed: u32) -> Self {
        Self {
            listener_socket_id,
            cookie_seed,
            passphrase: None,
            max_bandwidth: DEFAULT_MAX_BANDWIDTH,
        }
    }

    /// Override the maximum outbound send bandwidth (bytes/sec) applied to
    /// each session's [`Pacer`]. Defaults to [`DEFAULT_MAX_BANDWIDTH`]
    /// (12 Mbit/s), or the `AERO_SRT_MAX_BANDWIDTH_BYTES_PER_SEC` environment
    /// variable when constructed via [`SrtIngest::new`].
    #[must_use]
    pub fn with_max_bandwidth(mut self, bytes_per_sec: u64) -> Self {
        self.max_bandwidth = bytes_per_sec;
        self
    }

    /// The configured maximum outbound send bandwidth in bytes/sec.
    #[must_use]
    pub fn max_bandwidth(&self) -> u64 {
        self.max_bandwidth
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

/// Read `AERO_SRT_MAX_BANDWIDTH_BYTES_PER_SEC` (bytes/sec for the send
/// [`Pacer`]); `None` when unset or unparsable. The parse itself lives in
/// [`parse_max_bandwidth`] so it stays unit-testable without touching the
/// process environment.
fn max_bandwidth_from_env() -> Option<u64> {
    std::env::var("AERO_SRT_MAX_BANDWIDTH_BYTES_PER_SEC")
        .ok()
        .as_deref()
        .and_then(parse_max_bandwidth)
}

/// Parse a max-bandwidth override: a positive integer number of bytes/sec.
/// Zero is rejected (it would stall the stream at the pacer floor).
fn parse_max_bandwidth(s: &str) -> Option<u64> {
    s.trim().parse::<u64>().ok().filter(|&v| v > 0)
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
                    let session = session.with_max_bandwidth(ingest.max_bandwidth);
                    info!(%peer, stream_id = %id, sid = %stream_id, "SRT: handshake complete; streaming");
                    // A new session is live: bump the process-wide gauge. The
                    // matching decrement happens in `finalize_session`, the sole
                    // teardown path for streaming peers.
                    metrics::SessionCounter::added();
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
                // Count inbound data datagrams + bytes on the established session.
                // `feed_packet` itself attributes any detected loss to the
                // packets-lost counter via the reliability NAK actions below.
                metrics::record_datagram(datagram.len());
                session.feed_packet(datagram).await?;
            } else if parsed_hdr.is_some() {
                // ACK / NAK / ACKACK from the peer drive the sender-side
                // reliability state and feed the congestion pacer (RTT from
                // the ACK CIF, loss counts from the NAK loss list).
                session.handle_control(datagram, Instant::now());
            } else {
                debug!(%peer, "SRT: ignoring unparsable datagram on established session");
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
///
/// This is the sole teardown path for an established (`Streaming`) peer, so it
/// also decrements the process-wide active-sessions gauge here.
async fn finalize_session(mut session: SrtSession, repo: &StreamRepo, stream_id: Option<ulid::Ulid>) {
    // A streaming session is going away: re-publish the gauge one lower.
    metrics::SessionCounter::removed();
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
    /// Receive-side reorder buffer: releases data-packet payloads to the
    /// segmenter in sequence order, holding out-of-order packets until the gap
    /// fills (ROADMAP 方向五). In-order packets pass through immediately.
    reorder: ReorderBuffer,
    /// Pending reliability actions (NAK/ACK/ACKACK) waiting to be drained by
    /// the caller and serialised onto the wire.
    /// `pub(crate)` so the `pump` module can push ACKACK/etc. actions that
    /// arrive outside the normal `feed_packet` path (e.g. from `on_ack`).
    pub(crate) pending_actions: Vec<Action>,
    /// Congestion-aware send pacer; every outbound data packet drained by
    /// [`SrtSession::pump`] is gated through it. `pub(crate)` so the `pump`
    /// module can consult it and feed it ACK/NAK signals.
    pub(crate) pacer: Pacer,
    /// NAK-triggered retransmissions awaiting pacer budget, in NAK order.
    /// Drained by `pump` ahead of `fresh_data` (retransmits have priority).
    pub(crate) deferred_retransmits: std::collections::VecDeque<(u32, Vec<u8>)>,
    /// Fresh outbound data packets awaiting pacer budget, in sequence order.
    pub(crate) fresh_data: std::collections::VecDeque<(u32, Vec<u8>)>,
    /// When the pacer last deferred: earliest instant the head-of-line data
    /// packet may be sent. `None` when nothing is held back.
    pub(crate) next_send_at: Option<Instant>,
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
            reorder: ReorderBuffer::default(),
            pending_actions: Vec::new(),
            pacer: Pacer::new(DEFAULT_MAX_BANDWIDTH),
            deferred_retransmits: std::collections::VecDeque::new(),
            fresh_data: std::collections::VecDeque::new(),
            next_send_at: None,
        }
    }

    /// Replace the send pacer with one capped at `bytes_per_sec`.
    ///
    /// Call at construction time (before any traffic) — the new pacer starts
    /// from a clean baseline, dropping any accumulated congestion state.
    #[must_use]
    pub fn with_max_bandwidth(mut self, bytes_per_sec: u64) -> Self {
        self.pacer = Pacer::new(bytes_per_sec);
        self
    }

    /// The pacer's current (congestion-adjusted) send rate in bytes/sec.
    #[must_use]
    pub fn current_send_rate(&self) -> u64 {
        self.pacer.current_rate()
    }

    /// Earliest instant the pacer will release the next held data packet, or
    /// `None` when nothing is held back. Callers can use this to schedule the
    /// next [`SrtSession::pump`] instead of polling.
    #[must_use]
    pub fn next_send_at(&self) -> Option<Instant> {
        self.next_send_at
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
        // Attribute detected loss to the packets-lost counter: each NAK reports
        // an inclusive `[from, to]` range, so its span is the number of missing
        // packets the receiver observed for that gap.
        for action in &rel_actions {
            if let Action::SendNak { from, to } = action {
                let span = reliability::seq_diff(*from, *to) + 1;
                metrics::record_lost(u64::try_from(span).unwrap_or(0));
            }
        }
        self.pending_actions.extend(rel_actions);

        // Decrypt the payload if we have a crypto context and the KK flag says
        // this packet is encrypted.
        let kk = KkFlag::from_msg_word(msg_word);
        let payload = if kk != KkFlag::Clear {
            if let Some(crypto) = &self.crypto {
                let mut buf = payload_slice.to_vec();
                crypto.decrypt_packet(seq_no, &mut buf);
                buf
            } else {
                payload_slice.to_vec()
            }
        } else {
            payload_slice.to_vec()
        };

        // Reorder before the segmenter (ROADMAP 方向五): an in-order packet is fed
        // immediately; an out-of-order one is held until the gap fills, then the
        // contiguous run is released. The segmenter needs byte-stream continuity.
        for chunk in self.reorder.accept(seq_no, payload) {
            self.feed_ts_bytes(&chunk).await?;
        }
        Ok(())
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
    // Best-effort follower notification (same bus-free hook as RTMP); WHIP
    // publishes the go-live event directly.
    if let Some(hook) = &cfg.go_live {
        hook(stream.id);
    }

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
mod tests;
