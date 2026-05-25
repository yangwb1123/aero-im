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
//! crates: crypto, `regex`, …), this crate now hand-rolls a **minimal,
//! unencrypted** SRT (`HSv5`) wire layer in [`protocol`] sufficient to accept a
//! caller→listener MPEG-TS push:
//!
//! - the 16-byte SRT header + 48-byte handshake CIF codec,
//! - the listener-side INDUCTION → CONCLUSION handshake with SYN cookies,
//! - the `streamid=` (StreamID/SID) extension decode that carries the stream
//!   key,
//! - and the data-packet payload extraction feeding [`MpegTsSegmenter`].
//!
//! [`SrtIngest`] binds the UDP socket, drives the handshake per peer, resolves
//! the stream by SID via [`StreamRepo`] + `mark_live`, and feeds subsequent data
//! payloads into the segmenter → [`SrtSession`] → HLS path. **Pending** (and
//! documented as such): AES encryption (`KMREQ`/`KMRSP`), ACK/NAK
//! retransmission, congestion control, and packet reordering — the v1 data
//! plane is best-effort, in-order. See [`protocol`] for the full pending list.

pub mod crypto;
pub mod protocol;
pub mod reliability;
pub mod segmenter;

pub use crypto::{
    KkFlag, KmMessage, KmMessageType, KeyUnwrapError, SrtCrypto,
    aes_key_unwrap, aes_key_wrap, pbkdf2_kek,
};
pub use protocol::{Handshake, HandshakeMachine, HsAction, HsState, SrtHeader};
pub use reliability::{Action, ReliabilityState, RttEstimator, seq_diff, seq_lt, seq_next};
pub use segmenter::{MpegTsSegmenter, SegmentEvent, TS_PACKET_SIZE, TS_SYNC_BYTE};

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

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
/// Drives the hand-rolled, unencrypted SRT (`HSv5`) handshake in [`protocol`] per
/// peer, then feeds keyframe-aligned MPEG-TS segments (via the fully unit-tested
/// [`MpegTsSegmenter`]) into the HLS path. AES, ACK/NAK retransmission and
/// reordering are pending (see the module docs).
#[derive(Debug, Clone)]
pub struct SrtIngest {
    /// The SRT socket id this listener presents to callers.
    listener_socket_id: u32,
    /// Per-listener secret folded into SYN cookies.
    cookie_seed: u32,
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
        }
    }

    /// Construct with an explicit listener socket id and cookie seed — used by
    /// tests that need deterministic cookies.
    #[must_use]
    pub fn with_identity(listener_socket_id: u32, cookie_seed: u32) -> Self {
        Self {
            listener_socket_id,
            cookie_seed,
        }
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
            // Established: strip the 16-byte SRT data header and feed the
            // MPEG-TS payload to the segmenter. Control packets (KEEPALIVE,
            // ACK, …) are acknowledged-by-ignoring for now.
            if let Some(payload) = data_payload(datagram) {
                session.feed(payload).await?;
            } else {
                debug!(%peer, "SRT: ignoring non-data packet on established session");
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
/// resolved its stream. It is intentionally transport-free: [`Self::feed`] takes
/// raw TS bytes (however they arrived) so it can be unit-tested and reused
/// regardless of which SRT implementation ultimately delivers them.
pub struct SrtSession {
    segmenter: MpegTsSegmenter,
    hls: HlsWriter,
    /// Whether a segment is currently open (we've buffered packets that haven't
    /// been flushed yet).
    has_open_segment: bool,
}

impl SrtSession {
    /// Open a session for an already-resolved stream, creating the HLS writer
    /// under `hls_dir/{stream_id}`.
    pub fn new(hls: HlsWriter) -> Self {
        Self {
            segmenter: MpegTsSegmenter::new(),
            hls,
            has_open_segment: false,
        }
    }

    /// Feed a chunk of the inbound MPEG-TS byte stream. Flushes a finished HLS
    /// segment whenever the segmenter reaches a keyframe boundary.
    pub async fn feed(&mut self, bytes: &[u8]) -> LiveResult<()> {
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
}
