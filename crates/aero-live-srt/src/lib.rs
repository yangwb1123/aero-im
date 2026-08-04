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
//! - AES-128-CTR per-packet decryption (honouring the KK flag) via [`SrtCrypto`],
//!   activated when a passphrase is supplied to [`SrtIngest::with_passphrase`],
//! - standards-framed KMREQ/KMRSP handshake extensions: the listener unwraps
//!   the caller's SEK before resolving the stream, confirms it with KMRSP, and
//!   enforces encrypted media for the resulting session, and
//! - data-packet payload extraction feeding [`MpegTsSegmenter`], with sequence
//!   numbers fed through [`ReliabilityState`] so gaps produce NAKs and periodic
//!   ACKs are emitted.
//!
//! [`SrtIngest`] binds the UDP socket, drives the handshake per peer, resolves
//! the stream by SID via [`StreamRepo`] + `mark_live`, and feeds subsequent data
//! payloads into the segmenter → [`SrtSession`] → HLS path.
//!
//! ACK/NAK/ACKACK actions and congestion-scheduled retransmits are drained by
//! [`SrtSession::pump`] and transmitted on the production UDP socket after every
//! inbound packet. Receive-side data also passes through a sequence-aware
//! [`ReorderBuffer`] before the segmenter. An independent control tick sends an
//! SRT KEEPALIVE after one second without outbound traffic.

pub mod control;
pub mod crypto;
mod metrics;
pub mod pacing;
pub mod protocol;
pub mod pump;
pub mod reliability;
pub mod reorder;
pub mod segmenter;
mod turn;

pub use control::{decode_nak_loss_list, encode_control};
pub use crypto::{
    aes_key_unwrap, aes_key_wrap, pbkdf2_kek, KeyUnwrapError, KkFlag, KmKeyFlags, KmMessage,
    KmMessageType, SrtCrypto,
};
pub use pacing::{Allowance, Pacer, DEFAULT_MAX_BANDWIDTH};
pub use protocol::{Handshake, HandshakeMachine, HsAction, HsState, SrtHeader};
pub use pump::{decode_ack_cif, AckCif, SrtSink};
pub use reliability::{seq_diff, seq_lt, seq_next, Action, ReliabilityState, RttEstimator};
pub use reorder::ReorderBuffer;
pub use segmenter::{MpegTsSegmenter, SegmentEvent, TS_PACKET_SIZE, TS_SYNC_BYTE};
pub use turn::{IceServer, TurnConfig};

#[cfg(test)]
use turn::hmac_sha1_base64;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aero_live_core::{
    hls_path_for, hls_url_for, LiveError, LiveIngest, LiveResult, LiveStreamConfig,
};
use aero_live_hls::{HlsWriter, DEFAULT_SEGMENT_EXT};
use aero_storage::{MarkLiveOutcome, StreamRepo};
use async_trait::async_trait;
use crypto::SrtKeyRotation;
use protocol::{ControlType, PacketKind, SRT_HEADER_LEN};
use tokio::net::UdpSocket;
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Maximum UDP datagram we read for one SRT packet. SRT defaults to a 1500-byte
/// MTU; 2048 leaves comfortable headroom for jumbo-ish payloads without large
/// per-recv allocations.
const SRT_RECV_BUF: usize = 2048;

/// SRT peers independently send a keep-alive after one second without any
/// outbound data or control packet.
const SRT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);

/// Resolution used to notice idle established peers and send keep-alives.
const SRT_CONTROL_TICK_INTERVAL: Duration = Duration::from_millis(250);

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
/// ingest activates enforced AES-128-CTR decryption: the peer must deliver a
/// valid KMREQ key-material handshake extension, the SEK is unwrapped via
/// [`KmMessage`] + [`SrtCrypto::from_km_message`], and every subsequent data
/// packet must be encrypted before reaching the segmenter.
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
    /// When set, the ingest requires the publisher to deliver a valid AES-128
    /// KMREQ extension in its `HSv5` CONCLUSION. The listener confirms it with
    /// KMRSP and rejects clear media or key material that does not unwrap with
    /// this passphrase.
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

    /// Run the UDP listener until `cancel` is triggered, then finalize every
    /// active HLS writer and mark its stream ended before returning.
    pub async fn run_until_cancelled(
        &self,
        repo: StreamRepo,
        cfg: Arc<LiveStreamConfig>,
        cancel: CancellationToken,
    ) -> LiveResult<()> {
        run_listener(self, repo, cfg, cancel).await
    }

    /// Resolve the SRT listen address from the shared live config. SRT shares
    /// the RTMP host and listens one port above it (e.g. RTMP 1935 → SRT 1936).
    fn listen_addr(cfg: &LiveStreamConfig) -> LiveResult<SocketAddr> {
        let port = cfg
            .rtmp_listen
            .port()
            .checked_add(1)
            .ok_or_else(|| LiveError::Protocol("SRT listen port overflows u16".into()))?;
        Ok(SocketAddr::new(cfg.rtmp_listen.ip(), port))
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
        /// Caller's socket id, used as the destination on ACK/NAK replies.
        peer_socket_id: u32,
        /// Monotonic origin for SRT's wrapping connection-relative timestamp.
        connected_at: Instant,
        /// Last successful outbound packet, for the independent keep-alive.
        last_sent_at: Instant,
        session: Box<SrtSession>,
    },
}

#[async_trait]
impl LiveIngest for SrtIngest {
    async fn run(&self, repo: StreamRepo, cfg: Arc<LiveStreamConfig>) -> LiveResult<()> {
        self.run_until_cancelled(repo, cfg, CancellationToken::new())
            .await
    }
}

/// Stream-side effects behind the UDP packet router.
///
/// Production resolves a SID through Postgres and opens HLS. Tests provide an
/// in-memory backend so they can exercise the exact listener datagram path,
/// including UDP replies, without a database.
#[async_trait]
trait SessionBackend: Sync {
    async fn resolve(&self, stream_key: &str) -> LiveResult<(ulid::Ulid, SrtSession)>;

    async fn finalize(&self, session: SrtSession, stream_id: Option<ulid::Ulid>);
}

struct RepoSessionBackend<'a> {
    repo: &'a StreamRepo,
    cfg: &'a LiveStreamConfig,
}

#[async_trait]
impl SessionBackend for RepoSessionBackend<'_> {
    async fn resolve(&self, stream_key: &str) -> LiveResult<(ulid::Ulid, SrtSession)> {
        resolve_stream(self.repo, self.cfg, stream_key).await
    }

    async fn finalize(&self, session: SrtSession, stream_id: Option<ulid::Ulid>) {
        finalize_session(session, self.repo, stream_id).await;
    }
}

async fn run_listener(
    ingest: &SrtIngest,
    repo: StreamRepo,
    cfg: Arc<LiveStreamConfig>,
    cancel: CancellationToken,
) -> LiveResult<()> {
    let listen = SrtIngest::listen_addr(&cfg)?;
    let sock = UdpSocket::bind(listen).await.map_err(LiveError::Io)?;
    info!(
        %listen,
        hls_dir = %cfg.hls_dir.display(),
        encryption_required = ingest.passphrase.is_some(),
        "SRT ingest listening (HSv5 + AES-128-CTR + ACK/NAK + MPEG-TS → HLS)"
    );

    // SRT multiplexes every caller over the one listener UDP socket, keyed
    // by source address.
    let backend = RepoSessionBackend {
        repo: &repo,
        cfg: cfg.as_ref(),
    };
    let mut peers: HashMap<SocketAddr, PeerState> = HashMap::new();
    let mut buf = vec![0u8; SRT_RECV_BUF];
    let mut control_tick = interval(SRT_CONTROL_TICK_INTERVAL);
    control_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // `interval` ticks immediately once; consume that tick so the first
    // keep-alive scan happens after a real control interval.
    control_tick.tick().await;

    loop {
        let received = tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            _ = control_tick.tick() => {
                send_due_keepalives(&sock, &mut peers, Instant::now()).await;
                continue;
            }
            result = sock.recv_from(&mut buf) => result,
        };
        match received {
            Ok((n, peer)) => {
                let datagram = &buf[..n];
                if let Err(e) =
                    handle_datagram(ingest, &sock, &backend, &mut peers, peer, datagram).await
                {
                    warn!(%peer, error = %e, "SRT: dropping peer after error");
                    if let Some(PeerState::Streaming {
                        stream_id, session, ..
                    }) = peers.remove(&peer)
                    {
                        // Best-effort finalize so the manifest gets an
                        // ENDLIST even on a hard error.
                        backend.finalize(*session, Some(stream_id)).await;
                    } else {
                        peers.remove(&peer);
                    }
                }
            }
            Err(e) => {
                warn!(error = ?e, "SRT recv_from failed");
                return Err(LiveError::Io(e));
            }
        }
    }

    info!(
        active_peers = peers.len(),
        "SRT ingest shutdown requested; finalizing publishers"
    );
    for (_, peer) in peers {
        if let PeerState::Streaming {
            stream_id, session, ..
        } = peer
        {
            backend.finalize(*session, Some(stream_id)).await;
        }
    }
    Ok(())
}

/// Send a header-only KEEPALIVE to every established peer that has seen no
/// outbound packet for the protocol's one-second interval.
async fn send_due_keepalives(
    sock: &UdpSocket,
    peers: &mut HashMap<SocketAddr, PeerState>,
    now: Instant,
) -> usize {
    let mut sent = 0;
    for (peer, state) in peers {
        let PeerState::Streaming {
            peer_socket_id,
            connected_at,
            last_sent_at,
            ..
        } = state
        else {
            continue;
        };
        if now.duration_since(*last_sent_at) < SRT_KEEPALIVE_INTERVAL {
            continue;
        }

        let packet = keepalive_packet(
            *peer_socket_id,
            connection_timestamp_micros(*connected_at, now),
        );
        match sock.send_to(&packet, *peer).await {
            Ok(_) => {
                *last_sent_at = now;
                sent += 1;
            }
            Err(error) => {
                warn!(%peer, %error, "SRT: keep-alive send failed; will retry");
            }
        }
    }
    sent
}

/// Encode a header-only SRT KEEPALIVE addressed to the publisher.
fn keepalive_packet(peer_socket_id: u32, timestamp: u32) -> bytes::Bytes {
    SrtHeader {
        kind: PacketKind::Control {
            control_type: ControlType::KeepAlive,
            subtype: 0,
            type_specific: 0,
        },
        timestamp,
        dest_socket_id: peer_socket_id,
    }
    .to_bytes()
}

/// Convert monotonic connection age to SRT's wrapping 32-bit microsecond clock.
fn connection_timestamp_micros(connected_at: Instant, now: Instant) -> u32 {
    const TIMESTAMP_MODULUS: u128 = 1_u128 << 32;
    let wrapped = now.duration_since(connected_at).as_micros() % TIMESTAMP_MODULUS;
    u32::try_from(wrapped).unwrap_or(0)
}

/// Route one received datagram for `peer` through the handshake or data plane.
async fn handle_datagram(
    ingest: &SrtIngest,
    sock: &UdpSocket,
    backend: &impl SessionBackend,
    peers: &mut HashMap<SocketAddr, PeerState>,
    peer: SocketAddr,
    datagram: &[u8],
) -> LiveResult<()> {
    // A SHUTDOWN control packet tears the session down cleanly.
    if is_shutdown(datagram) {
        if let Some(PeerState::Streaming {
            session, stream_id, ..
        }) = peers.remove(&peer)
        {
            info!(%peer, %stream_id, "SRT: peer sent SHUTDOWN; finalizing");
            backend.finalize(*session, Some(stream_id)).await;
        } else {
            peers.remove(&peer);
        }
        return Ok(());
    }

    let entry = peers.entry(peer).or_insert_with(|| {
        let machine = HandshakeMachine::new(ingest.listener_socket_id, ingest.cookie_seed);
        let machine = if let Some(passphrase) = ingest.passphrase.clone() {
            machine.with_passphrase(passphrase)
        } else {
            machine
        };
        PeerState::Handshaking(machine)
    });

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
                    flow_window,
                    crypto,
                }) => {
                    let peer_socket_id = machine.peer_socket_id();
                    // Confirm the handshake to the caller, then resolve the
                    // stream key carried by the SID and open a session.
                    sock.send_to(&agreement, peer)
                        .await
                        .map_err(LiveError::Io)?;
                    let (id, mut session) = backend.resolve(&stream_id).await?;
                    if let Some(crypto) = crypto {
                        session.set_crypto(crypto);
                        if let Some(passphrase) = ingest.passphrase.clone() {
                            session.enable_key_rotation(passphrase);
                        }
                    }
                    let session = session
                        .with_receive_buffer_capacity(flow_window)
                        .with_max_bandwidth(ingest.max_bandwidth);
                    info!(
                        %peer,
                        stream_id = %id,
                        "SRT: handshake complete; streaming"
                    );
                    // A new session is live: bump the process-wide gauge. The
                    // matching decrement happens in `finalize_session`, the sole
                    // teardown path for streaming peers.
                    metrics::SessionCounter::added();
                    let connected_at = Instant::now();
                    *entry = PeerState::Streaming {
                        stream_id: id,
                        peer_socket_id,
                        connected_at,
                        last_sent_at: connected_at,
                        session: Box::new(session),
                    };
                    Ok(())
                }
                Ok(HsAction::Ignore) => Ok(()),
                Err(e) => Err(LiveError::Protocol(format!("SRT handshake: {e}"))),
            }
        }
        PeerState::Streaming {
            peer_socket_id,
            connected_at,
            last_sent_at,
            session,
            ..
        } => {
            // Established: route the datagram through the session's data plane.
            // `feed_packet` handles header parsing, reliability tracking,
            // enforced clear/AES-CTR policy, and TS segmentation all in one call.
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
                let now = Instant::now();
                let timestamp = connection_timestamp_micros(*connected_at, now);
                if let Some(response) =
                    session.handle_key_material_control(datagram, *peer_socket_id, timestamp)?
                {
                    sock.send_to(&response, peer).await.map_err(LiveError::Io)?;
                    *last_sent_at = now;
                } else {
                    // ACK / NAK / ACKACK drive reliability and congestion.
                    session.handle_control(datagram, now);
                }
            } else {
                debug!(%peer, "SRT: ignoring unparsable datagram on established session");
            }
            // Flush ACK / NAK / ACKACK control packets back to the sender.
            let mut collected = Vec::new();
            let now = Instant::now();
            session.pump(&mut collected, now, *peer_socket_id);
            for pkt in collected {
                if let Err(e) = sock.send_to(&pkt, peer).await {
                    warn!(%peer, error = %e, "SRT: control packet send failed; dropping peer");
                    return Err(LiveError::Io(e));
                }
                *last_sent_at = now;
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
async fn finalize_session(
    mut session: SrtSession,
    repo: &StreamRepo,
    stream_id: Option<ulid::Ulid>,
) {
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
/// 3. Enforces the negotiated clear/encrypted mode and decrypts even-key
///    payloads in-place with the installed [`SrtCrypto`] context.
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
    /// Even AES-CTR key slot; `None` for unencrypted sessions.
    crypto: Option<SrtCrypto>,
    /// Odd slot plus the fail-closed even/odd transition state.
    key_rotation: Option<SrtKeyRotation>,
    /// Receiver-side reliability state: tracks sequence numbers, emits NAK/ACK.
    /// `pub(crate)` so the sibling `pump` module can drive retransmits and
    /// read/write the ACK interval in tests without exposing the field publicly.
    pub(crate) reliability: ReliabilityState,
    /// Receive-side reorder buffer: releases data-packet payloads to the
    /// segmenter in sequence order, holding out-of-order packets until the gap
    /// fills (ROADMAP 方向五). In-order packets pass through immediately.
    reorder: ReorderBuffer,
    /// Maximum receive capacity accepted during the handshake, in packets.
    /// Full ACKs advertise this capacity minus packets held for reordering.
    receive_buffer_capacity: u32,
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
        let key_rotation = crypto.as_ref().map(|_| SrtKeyRotation::initial(None));
        Self {
            segmenter: MpegTsSegmenter::new(),
            hls,
            has_open_segment: false,
            crypto,
            key_rotation,
            reliability: ReliabilityState::new(0),
            reorder: ReorderBuffer::default(),
            receive_buffer_capacity: protocol::SRT_DEFAULT_FLOW_WINDOW,
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

    /// Set the packet capacity accepted during the handshake.
    ///
    /// This value drives the `Available Buffer Size` field of every full ACK.
    /// Advertising a value greater than the handshake agreement is invalid;
    /// advertising zero while the ingest path has room stalls a libsrt sender.
    #[must_use]
    pub fn with_receive_buffer_capacity(mut self, packets: u32) -> Self {
        self.receive_buffer_capacity = packets;
        self
    }

    /// Current receive capacity available to the peer, in packet slots.
    fn available_receive_buffer_packets(&self) -> u32 {
        let occupied = u32::try_from(self.reorder.pending_len()).unwrap_or(u32::MAX);
        self.receive_buffer_capacity.saturating_sub(occupied)
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
        self.key_rotation = Some(SrtKeyRotation::initial(None));
    }

    fn enable_key_rotation(&mut self, passphrase: Vec<u8>) {
        if let Some(rotation) = &mut self.key_rotation {
            rotation.set_passphrase(passphrase);
        }
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
        if km.key_flags != KmKeyFlags::Even {
            return Err(KeyUnwrapError::BadLength);
        }
        let crypto = SrtCrypto::from_km_message(km, passphrase)?;
        self.set_crypto(crypto);
        self.enable_key_rotation(passphrase.to_vec());
        Ok(())
    }

    fn handle_key_material_control(
        &mut self,
        datagram: &[u8],
        peer_socket_id: u32,
        timestamp: u32,
    ) -> LiveResult<Option<bytes::Bytes>> {
        let Some(message) = protocol::decode_key_material_control(datagram)
            .map_err(|error| LiveError::Protocol(format!("SRT key refresh: {error}")))?
        else {
            return Ok(None);
        };
        if message.msg_type != KmMessageType::Request {
            return Err(LiveError::Protocol(
                "unsolicited SRT KMRSP on receive-only ingest session".into(),
            ));
        }
        self.key_rotation
            .as_mut()
            .ok_or_else(|| {
                LiveError::Protocol(
                    "post-handshake KMREQ received on an unencrypted session".into(),
                )
            })?
            .install_request(&mut self.crypto, &message)
            .map_err(|error| LiveError::Protocol(format!("SRT key refresh: {error}")))?;
        Ok(Some(protocol::encode_key_material_control(
            &message.as_response(),
            peer_socket_id,
            timestamp,
        )))
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
    ///   variance from the embedded [`ReliabilityState`], current available
    ///   receive capacity, and zeroes for untracked rate fields).
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
        self.drain_control_packets_at(peer_socket_id, Instant::now())
    }

    /// Timestamp-aware form used by [`SrtSession::pump`] so a sent full ACK is
    /// registered at the exact fake/real clock instant that produced it.
    pub(crate) fn drain_control_packets_at(
        &mut self,
        peer_socket_id: u32,
        sent_at: Instant,
    ) -> Vec<Vec<u8>> {
        let actions = std::mem::take(&mut self.pending_actions);
        for action in &actions {
            if let Action::SendAck { ack_id, .. } = action {
                self.reliability.on_ack_sent(*ack_id, sent_at);
            }
        }
        let rtt_us = u32::try_from(self.reliability.rtt().as_micros()).unwrap_or(u32::MAX);
        let rttvar_us = u32::try_from(self.reliability.rttvar().as_micros()).unwrap_or(u32::MAX);
        let available_buffer_packets = self.available_receive_buffer_packets();
        actions
            .iter()
            .filter_map(|a| {
                control::encode_control(
                    a,
                    peer_socket_id,
                    0,
                    rtt_us,
                    rttvar_us,
                    available_buffer_packets,
                )
            })
            .collect()
    }

    /// Feed a full SRT data packet (16-byte header + payload) into the session.
    ///
    /// This is the main data-plane entry point for the socket layer.  It:
    /// 1. Parses the SRT header to extract `seq_no` and KK flag bits.
    /// 2. Enforces the negotiated clear/encrypted mode and even/odd lifecycle.
    /// 3. Runs an accepted sequence through the reliability state machine.
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

        // Reject an unannounced/stale slot before it can mutate reliability
        // bookkeeping. A valid pending slot is promoted only after decryption.
        let kk = KkFlag::from_msg_word(msg_word);
        let payload = match (kk, self.key_rotation.as_mut()) {
            (KkFlag::Clear, None) => payload_slice.to_vec(),
            (KkFlag::EvenKey | KkFlag::OddKey, Some(rotation)) => {
                let mut buf = payload_slice.to_vec();
                let rotated = rotation
                    .decrypt(&self.crypto, kk, seq_no, &mut buf)
                    .map_err(|error| LiveError::Protocol(format!("SRT key lifecycle: {error}")))?;
                if rotated {
                    metrics::record_key_rotation();
                }
                buf
            }
            (KkFlag::Clear, Some(_)) => {
                return Err(LiveError::Protocol(
                    "unencrypted SRT data received after encrypted KM negotiation".into(),
                ));
            }
            (KkFlag::EvenKey | KkFlag::OddKey, None) => {
                return Err(LiveError::Protocol(
                    "encrypted SRT data received without completed KM negotiation".into(),
                ));
            }
            (KkFlag::Invalid, _) => {
                return Err(LiveError::Protocol(
                    "reserved KK=11 flag on SRT data packet".into(),
                ));
            }
        };

        let rel_actions = self.reliability.on_data(seq_no, std::time::Instant::now());
        for action in &rel_actions {
            if let Action::SendNak { from, to } = action {
                let span = reliability::seq_diff(*from, *to) + 1;
                metrics::record_lost(u64::try_from(span).unwrap_or(0));
            }
        }
        self.pending_actions.extend(rel_actions);

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
    match repo
        .mark_live(stream.id, &hls_url)
        .await
        .map_err(LiveError::Database)?
    {
        MarkLiveOutcome::Started(_) => {}
        MarkLiveOutcome::AlreadyLive => {
            return Err(LiveError::Protocol(format!(
                "stream {} already has an active publisher",
                stream.id
            )));
        }
        MarkLiveOutcome::NotFound => {
            return Err(LiveError::UnknownStreamKey(stream_key.to_string()));
        }
    }
    let dir = hls_path_for(&cfg.hls_dir, stream.id);
    let hls = match HlsWriter::new(dir, SEGMENT_DURATION_SECS).await {
        Ok(hls) => hls.with_segment_ext(DEFAULT_SEGMENT_EXT),
        Err(error) => {
            if let Err(mark_error) = repo.mark_ended(stream.id).await {
                warn!(
                    %mark_error,
                    stream_id = %stream.id,
                    "failed to roll back live state after HLS init error"
                );
            }
            return Err(LiveError::Internal(anyhow::anyhow!(
                "hls writer init: {error}"
            )));
        }
    };

    info!(
        stream_id = %stream.id,
        "SRT publisher accepted; emitting MPEG-TS HLS segments"
    );
    Ok((stream.id, SrtSession::new(hls)))
}

#[cfg(test)]
mod rotation_tests;
#[cfg(test)]
mod tests;
