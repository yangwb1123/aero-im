//! RTMP ingest server backed by `rml_rtmp`.
//!
//! ## Pipeline
//!
//! 1. Bind TCP on `cfg.rtmp_listen`.
//! 2. Per connection: complete the RTMP handshake, then drive a
//!    [`ServerSession`](rml_rtmp::sessions::ServerSession), accepting `connect`
//!    and `publish` requests.
//! 3. Look up the publish stream key against [`StreamRepo`]. Unknown keys are
//!    rejected with a `NetStream.Publish.Start` error and the TCP connection
//!    is closed.
//! 4. Known keys flip the row to `live`, hand a [`HlsWriter`] the streaming
//!    payload, and tick a wall-clock timer that flushes a segment every
//!    `SEGMENT_DURATION_SECS`.
//! 5. On disconnect or fatal protocol error, mark the row `ended`.
//!
//! ## Muxing strategy: real MPEG-TS (since P9.4)
//!
//! Each video/audio FLV tag body is fed through [`FlvToTsConverter`]
//! (in `aero-live-hls`), which:
//! 1. Extracts SPS/PPS from the first AVCDecoderConfigurationRecord and
//!    AAC AudioSpecificConfig.
//! 2. Converts AVCC NALUs → Annex-B (with AUD + SPS/PPS prepended on each
//!    keyframe), AAC raw → ADTS frames.
//! 3. Wraps access units in PES packets with 90 kHz PTS/DTS.
//! 4. Slices PES into 188-byte TS packets at PID 0x100/0x101.
//! 5. Drains a segment with a fresh PAT + PMT prefix.
//!
//! Browser HLS players (Safari natively, hls.js elsewhere) can decode the
//! output directly. The placeholder warning emitted in earlier versions has
//! been removed.

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aero_live_core::{
    hls_path_for, hls_url_for, LiveError, LiveIngest, LiveResult, LiveStreamConfig,
};
use aero_live_hls::{FlvToTsConverter, HlsWriter, DEFAULT_SEGMENT_EXT};
use aero_storage::{MarkLiveOutcome, StreamRepo};
use async_trait::async_trait;
use bytes::Bytes;
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::sessions::{
    ServerSession, ServerSessionConfig, ServerSessionEvent, ServerSessionResult,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use ulid::Ulid;

/// How long a single HLS segment covers in wall-clock time.
pub const SEGMENT_DURATION_SECS: u64 = 2;

/// Receive-buffer size for each TCP read off the socket.
const SOCKET_READ_BUF: usize = 8 * 1024;

/// Maximum time shutdown waits for publishers to flush their current HLS
/// segment and mark the stream ended before aborting a stuck connection task.
const CONNECTION_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum bytes a single per-connection task will buffer for HLS segmenting
/// before it drops new data with a warning. Acts as a back-pressure safety
/// net: 32 MiB is well above 2s of broadcast video at any sane bitrate.
const MAX_SEGMENT_BUFFER_BYTES: usize = 32 * 1024 * 1024;

/// FLV tag types used by the TS muxer. Matches the values RTMP uses
/// directly so we can mirror them when dropping payloads onto disk.
const FLV_TAG_AUDIO: u8 = 0x08;
const FLV_TAG_VIDEO: u8 = 0x09;

/// Upper bound on the length of a publish stream key we will even look up.
///
/// Legitimate keys are short (the generator emits 32 hex chars); a multi-kilobyte
/// "key" can only be an abusive or malformed publisher. Rejecting oversized keys
/// up front avoids pushing attacker-controlled blobs into a database query
/// parameter and into structured logs.
const MAX_STREAM_KEY_LEN: usize = 256;

/// Why a publish [`stream_key`](ServerSessionEvent::PublishStreamRequested) was
/// rejected before it ever reached [`StreamRepo::get_by_key`].
///
/// RTMP stream keys arrive as arbitrary, attacker-controlled UTF-8 from an
/// unauthenticated publisher. Validating their *shape* before the database
/// round-trip prevents log/path injection and pointless lookups for keys that
/// can never match a legitimately generated one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StreamKeyRejection {
    /// The key was empty or contained only whitespace.
    #[error("stream key is empty")]
    Empty,
    /// The key exceeded [`MAX_STREAM_KEY_LEN`] bytes.
    #[error("stream key exceeds {MAX_STREAM_KEY_LEN} bytes")]
    TooLong,
    /// The key contained an ASCII control character (NUL, newline, tab, …),
    /// which would corrupt logs or downstream protocol framing.
    #[error("stream key contains a control character")]
    ControlChar,
    /// The key contained a path separator or `..` segment. Stream keys index a
    /// database row, never a filesystem path, so these can only be traversal
    /// probes.
    #[error("stream key contains a path-traversal character")]
    PathTraversal,
}

/// Validate the *shape* of an incoming publish stream key without touching the
/// database.
///
/// This is intentionally conservative and self-contained: it rejects keys that
/// can never correspond to a legitimately issued key while accepting any
/// reasonable printable token. A passing key is still authenticated by the
/// `get_by_key` lookup — this only filters out abusive or malformed input.
///
/// # Errors
///
/// Returns the specific [`StreamKeyRejection`] describing the first violation
/// found. Checks run cheapest-first (length before a full character scan).
pub fn validate_stream_key(key: &str) -> Result<(), StreamKeyRejection> {
    if key.len() > MAX_STREAM_KEY_LEN {
        return Err(StreamKeyRejection::TooLong);
    }
    if key.trim().is_empty() {
        return Err(StreamKeyRejection::Empty);
    }
    for ch in key.chars() {
        if ch.is_control() {
            return Err(StreamKeyRejection::ControlChar);
        }
        if matches!(ch, '/' | '\\') {
            return Err(StreamKeyRejection::PathTraversal);
        }
    }
    if key.contains("..") {
        return Err(StreamKeyRejection::PathTraversal);
    }
    Ok(())
}

/// Convenience: spawn the RTMP ingest with default settings on a background task.
pub fn spawn_rtmp_ingest(
    repo: StreamRepo,
    cfg: Arc<LiveStreamConfig>,
) -> JoinHandle<LiveResult<()>> {
    spawn_rtmp_ingest_until_cancelled(repo, cfg, CancellationToken::new())
}

/// Spawn RTMP ingest tied to a process-lifecycle cancellation token.
///
/// On cancellation the listener stops accepting, existing publishers are
/// allowed to flush/finalize for [`CONNECTION_DRAIN_TIMEOUT`], and any task
/// still stuck in external I/O is then aborted.
pub fn spawn_rtmp_ingest_until_cancelled(
    repo: StreamRepo,
    cfg: Arc<LiveStreamConfig>,
    cancel: CancellationToken,
) -> JoinHandle<LiveResult<()>> {
    tokio::spawn(async move {
        let ingest = RtmpIngest::new();
        ingest.run_until_cancelled(repo, cfg, cancel).await
    })
}

/// RTMP ingest implementation. Stateless — all per-connection state lives in
/// the spawned task closure.
#[derive(Debug, Default, Clone)]
pub struct RtmpIngest;

impl RtmpIngest {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Run the listener until `cancel` is triggered.
    ///
    /// Connection tasks are owned by a [`JoinSet`] rather than detached, so
    /// process shutdown has a deterministic drain boundary.
    pub async fn run_until_cancelled(
        &self,
        repo: StreamRepo,
        cfg: Arc<LiveStreamConfig>,
        cancel: CancellationToken,
    ) -> LiveResult<()> {
        let listener = TcpListener::bind(cfg.rtmp_listen)
            .await
            .map_err(LiveError::from)?;
        info!(
            addr = %cfg.rtmp_listen,
            hls_dir = %cfg.hls_dir.display(),
            "RTMP ingest listening"
        );

        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    info!(
                        active_connections = connections.len(),
                        "RTMP ingest shutdown requested; draining publishers"
                    );
                    let drain = async {
                        while let Some(result) = connections.join_next().await {
                            if let Err(error) = result {
                                warn!(%error, "RTMP connection task failed while draining");
                            }
                        }
                    };
                    if tokio::time::timeout(CONNECTION_DRAIN_TIMEOUT, drain).await.is_err() {
                        warn!(
                            active_connections = connections.len(),
                            timeout_secs = CONNECTION_DRAIN_TIMEOUT.as_secs(),
                            "RTMP publisher drain timed out; aborting remaining connections"
                        );
                        connections.abort_all();
                        while connections.join_next().await.is_some() {}
                    }
                    return Ok(());
                }
                joined = connections.join_next(), if !connections.is_empty() => {
                    if let Some(Err(error)) = joined {
                        warn!(%error, "RTMP connection task failed");
                    }
                }
                accepted = listener.accept() => {
                    let (socket, peer) = match accepted {
                        Ok(pair) => pair,
                        Err(error) => {
                            error!(%error, "RTMP accept failed");
                            continue;
                        }
                    };
                    debug!(%peer, "RTMP connection accepted");

                    let repo = repo.clone();
                    let cfg = cfg.clone();
                    let connection_cancel = cancel.clone();
                    connections.spawn(async move {
                        if let Err(error) =
                            handle_connection(socket, repo, cfg, connection_cancel).await
                        {
                            warn!(%error, %peer, "RTMP connection ended with error");
                        }
                    });
                }
            }
        }
    }
}

#[async_trait]
impl LiveIngest for RtmpIngest {
    async fn run(&self, repo: StreamRepo, cfg: Arc<LiveStreamConfig>) -> LiveResult<()> {
        self.run_until_cancelled(repo, cfg, CancellationToken::new())
            .await
    }
}

/// Per-connection driver: handshake → session events → publish loop.
async fn handle_connection(
    mut socket: TcpStream,
    repo: StreamRepo,
    cfg: Arc<LiveStreamConfig>,
    cancel: CancellationToken,
) -> LiveResult<()> {
    let mut buf = [0u8; SOCKET_READ_BUF];

    // 1) Handshake.
    let mut handshake = Handshake::new(PeerType::Server);
    let remaining_after_handshake = loop {
        let Some(n) = read_or_cancelled(&mut socket, &mut buf, &cancel).await? else {
            return Ok(());
        };
        if n == 0 {
            return Err(LiveError::Protocol(
                "peer closed during handshake".to_string(),
            ));
        }
        match handshake
            .process_bytes(&buf[..n])
            .map_err(|e| LiveError::Protocol(format!("handshake: {e:?}")))?
        {
            HandshakeProcessResult::InProgress { response_bytes } => {
                if !response_bytes.is_empty() {
                    socket
                        .write_all(&response_bytes)
                        .await
                        .map_err(LiveError::from)?;
                }
            }
            HandshakeProcessResult::Completed {
                response_bytes,
                remaining_bytes,
            } => {
                if !response_bytes.is_empty() {
                    socket
                        .write_all(&response_bytes)
                        .await
                        .map_err(LiveError::from)?;
                }
                break remaining_bytes;
            }
        }
    };
    debug!("RTMP handshake complete");

    // 2) Session.
    let (mut session, initial_outbound) = ServerSession::new(ServerSessionConfig::new())
        .map_err(|e| LiveError::Protocol(format!("session init: {e:?}")))?;
    for r in initial_outbound {
        if let ServerSessionResult::OutboundResponse(p) = r {
            socket.write_all(&p.bytes).await.map_err(LiveError::from)?;
        }
    }

    // Drain any handshake-overflow bytes through the session before reading more.
    if !remaining_after_handshake.is_empty() {
        let results = session
            .handle_input(&remaining_after_handshake)
            .map_err(|e| LiveError::Protocol(format!("session input: {e:?}")))?;
        if !forward_session_results(&mut socket, &mut session, results, &repo, &cfg).await? {
            return Ok(());
        }
    }

    // 3) Drive the session until the connection ends or we transition into the publish loop.
    let publish = loop {
        let Some(n) = read_or_cancelled(&mut socket, &mut buf, &cancel).await? else {
            return Ok(());
        };
        if n == 0 {
            return Err(LiveError::Protocol(
                "peer closed before publish accepted".to_string(),
            ));
        }
        let results = session
            .handle_input(&buf[..n])
            .map_err(|e| LiveError::Protocol(format!("session input: {e:?}")))?;
        match process_results(&mut socket, &mut session, results, &repo, &cfg).await? {
            ProcessOutcome::Continue => {}
            ProcessOutcome::Disconnect => return Ok(()),
            ProcessOutcome::Publishing(p) => break p,
        }
    };

    // 4) Publish loop.
    let stream_id = publish.stream_id;
    let publish_result = run_publish_loop(socket, session, buf, publish, &cfg, &cancel).await;

    // 5) Mark the row ended regardless of how the publish loop exited.
    if let Err(e) = repo.mark_ended(stream_id).await {
        warn!(error = %e, %stream_id, "failed to mark stream ended");
    }

    publish_result
}

async fn read_or_cancelled(
    socket: &mut TcpStream,
    buf: &mut [u8],
    cancel: &CancellationToken,
) -> LiveResult<Option<usize>> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Ok(None),
        result = socket.read(buf) => result.map(Some).map_err(LiveError::from),
    }
}

enum ProcessOutcome {
    Continue,
    Disconnect,
    Publishing(PublishContext),
}

struct PublishContext {
    stream_id: Ulid,
    hls: HlsWriter,
}

/// Side-effecting wrapper around [`process_results`] used during the pre-publish
/// handshake-overflow drain (we don't yet know whether we'll transition into a
/// publish, so we just need a yes/no on whether to keep talking).
async fn forward_session_results(
    socket: &mut TcpStream,
    session: &mut ServerSession,
    results: Vec<ServerSessionResult>,
    repo: &StreamRepo,
    cfg: &Arc<LiveStreamConfig>,
) -> LiveResult<bool> {
    match process_results(socket, session, results, repo, cfg).await? {
        ProcessOutcome::Continue => Ok(true),
        ProcessOutcome::Disconnect => Ok(false),
        ProcessOutcome::Publishing(_) => Ok(false), // unreachable in practice
    }
}

async fn process_results(
    socket: &mut TcpStream,
    session: &mut ServerSession,
    results: Vec<ServerSessionResult>,
    repo: &StreamRepo,
    cfg: &Arc<LiveStreamConfig>,
) -> LiveResult<ProcessOutcome> {
    let mut outcome = ProcessOutcome::Continue;
    for r in results {
        match r {
            ServerSessionResult::OutboundResponse(p) => {
                socket.write_all(&p.bytes).await.map_err(LiveError::from)?;
            }
            ServerSessionResult::UnhandleableMessageReceived(payload) => {
                debug!(?payload, "RTMP unhandleable message");
            }
            ServerSessionResult::RaisedEvent(event) => match event {
                ServerSessionEvent::ConnectionRequested {
                    request_id,
                    app_name,
                } => {
                    info!(%app_name, "RTMP connect");
                    let more = session
                        .accept_request(request_id)
                        .map_err(|e| LiveError::Protocol(format!("accept connect: {e:?}")))?;
                    for r in more {
                        if let ServerSessionResult::OutboundResponse(p) = r {
                            socket.write_all(&p.bytes).await.map_err(LiveError::from)?;
                        }
                    }
                }
                ServerSessionEvent::PublishStreamRequested {
                    request_id,
                    app_name,
                    stream_key,
                    mode: _,
                } => {
                    info!(
                        %app_name,
                        stream_key_len = stream_key.len(),
                        "RTMP publish requested"
                    );
                    if let Err(reason) = validate_stream_key(&stream_key) {
                        // Malformed/abusive key — reject over RTMP exactly like an
                        // unknown key, but without a database round-trip. We log the
                        // byte length rather than the raw key to avoid log injection
                        // from the (already-rejected) control characters.
                        warn!(
                            %reason,
                            stream_key_len = stream_key.len(),
                            "rejecting publish: malformed stream key"
                        );
                        let more = session
                            .reject_request(
                                request_id,
                                "NetStream.Publish.Start",
                                "Invalid stream key",
                            )
                            .map_err(|e| LiveError::Protocol(format!("reject publish: {e:?}")))?;
                        for r in more {
                            if let ServerSessionResult::OutboundResponse(p) = r {
                                socket.write_all(&p.bytes).await.map_err(LiveError::from)?;
                            }
                        }
                        outcome = ProcessOutcome::Disconnect;
                        continue;
                    }
                    match repo
                        .get_by_key(&stream_key)
                        .await
                        .map_err(LiveError::Database)?
                    {
                        Some(stream) => {
                            let hls_url = hls_url_for(stream.id);
                            let transition = repo
                                .mark_live(stream.id, &hls_url)
                                .await
                                .map_err(LiveError::Database)?;
                            let refusal = match transition {
                                MarkLiveOutcome::Started(_) => None,
                                MarkLiveOutcome::AlreadyLive => {
                                    Some("Stream already has an active publisher")
                                }
                                MarkLiveOutcome::NotFound => Some("Unknown stream key"),
                            };
                            if let Some(description) = refusal {
                                warn!(
                                    stream_id = %stream.id,
                                    %description,
                                    "rejecting competing RTMP publisher"
                                );
                                let more = session
                                    .reject_request(
                                        request_id,
                                        "NetStream.Publish.BadName",
                                        description,
                                    )
                                    .map_err(|e| {
                                        LiveError::Protocol(format!("reject publish: {e:?}"))
                                    })?;
                                for r in more {
                                    if let ServerSessionResult::OutboundResponse(p) = r {
                                        socket
                                            .write_all(&p.bytes)
                                            .await
                                            .map_err(LiveError::from)?;
                                    }
                                }
                                outcome = ProcessOutcome::Disconnect;
                                continue;
                            }
                            let dir = hls_path_for(&cfg.hls_dir, stream.id);
                            let segment_duration_secs = u32::try_from(SEGMENT_DURATION_SECS)
                                .expect("RTMP segment duration must fit in u32");
                            let hls = match HlsWriter::new(dir, segment_duration_secs).await {
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
                                "RTMP publisher accepted; emitting MPEG-TS segments"
                            );

                            let more = session.accept_request(request_id).map_err(|e| {
                                LiveError::Protocol(format!("accept publish: {e:?}"))
                            })?;
                            for r in more {
                                if let ServerSessionResult::OutboundResponse(p) = r {
                                    socket.write_all(&p.bytes).await.map_err(LiveError::from)?;
                                }
                            }
                            outcome = ProcessOutcome::Publishing(PublishContext {
                                stream_id: stream.id,
                                hls,
                            });
                        }
                        None => {
                            warn!(
                                stream_key_len = stream_key.len(),
                                "rejecting publish: unknown stream key"
                            );
                            let more = session
                                .reject_request(
                                    request_id,
                                    "NetStream.Publish.Start",
                                    "Unknown stream key",
                                )
                                .map_err(|e| {
                                    LiveError::Protocol(format!("reject publish: {e:?}"))
                                })?;
                            for r in more {
                                if let ServerSessionResult::OutboundResponse(p) = r {
                                    socket.write_all(&p.bytes).await.map_err(LiveError::from)?;
                                }
                            }
                            outcome = ProcessOutcome::Disconnect;
                        }
                    }
                }
                ServerSessionEvent::PlayStreamRequested { request_id, .. } => {
                    // We're an ingest, not a relay — reject playback.
                    let more = session
                        .reject_request(
                            request_id,
                            "NetStream.Play.Start",
                            "Playback not supported on ingest endpoint",
                        )
                        .map_err(|e| LiveError::Protocol(format!("reject play: {e:?}")))?;
                    for r in more {
                        if let ServerSessionResult::OutboundResponse(p) = r {
                            socket.write_all(&p.bytes).await.map_err(LiveError::from)?;
                        }
                    }
                }
                other => debug!(?other, "RTMP event"),
            },
        }
    }
    Ok(outcome)
}

/// After the publish handshake completes we hand control to this function.
///
/// It owns the socket, the session, and the HLS writer until the publisher
/// disconnects, the connection errors, or it explicitly closes the stream.
async fn run_publish_loop(
    mut socket: TcpStream,
    mut session: ServerSession,
    mut buf: [u8; SOCKET_READ_BUF],
    ctx: PublishContext,
    cfg: &Arc<LiveStreamConfig>,
    cancel: &CancellationToken,
) -> LiveResult<()> {
    let PublishContext { stream_id, mut hls } = ctx;

    // Channel from the socket-reading task to the segmenter task. Bounded to
    // back-pressure pathological encoders without dropping silently.
    let (tx, mut rx) = mpsc::channel::<MediaChunk>(1024);

    let segmenter = tokio::spawn(async move {
        let mut mux = FlvToTsConverter::new();
        let mut buffer_bytes: usize = 0;
        let mut segment_start = Instant::now();
        let mut ticker = interval(Duration::from_millis(250));
        info!(%stream_id, "TS muxer started; segment duration={}s", SEGMENT_DURATION_SECS);
        loop {
            tokio::select! {
                maybe = rx.recv() => {
                    match maybe {
                        Some(chunk) => {
                            buffer_bytes = buffer_bytes.saturating_add(chunk.data.len());
                            if buffer_bytes > MAX_SEGMENT_BUFFER_BYTES {
                                warn!(buffer_bytes, "RTMP segment buffer overflow — dropping current window");
                                mux = FlvToTsConverter::new();
                                buffer_bytes = 0;
                                segment_start = Instant::now();
                                continue;
                            }
                            let res = match chunk.kind {
                                FLV_TAG_VIDEO => mux.push_video_tag(&chunk.data, chunk.timestamp_ms),
                                FLV_TAG_AUDIO => mux.push_audio_tag(&chunk.data, chunk.timestamp_ms),
                                _ => Ok(()),
                            };
                            if let Err(e) = res {
                                warn!(error = ?e, "TS muxer dropped a tag");
                            }
                        }
                        None => {
                            // Sender dropped — flush whatever remains and exit.
                            if mux.has_segment_data() {
                                let elapsed = segment_start.elapsed().as_secs_f32().max(0.001);
                                flush_ts_segment(&mut hls, &mut mux, elapsed).await;
                            }
                            if let Err(e) = hls.finish().await {
                                warn!(error = %e, %stream_id, "hls finish failed");
                            }
                            break;
                        }
                    }
                }
                _ = ticker.tick() => {
                    if segment_start.elapsed() >= Duration::from_secs(SEGMENT_DURATION_SECS)
                        && mux.has_segment_data()
                    {
                        let elapsed = segment_start.elapsed().as_secs_f32();
                        flush_ts_segment(&mut hls, &mut mux, elapsed).await;
                        buffer_bytes = 0;
                        segment_start = Instant::now();
                    }
                }
            }
        }
    });

    let result = drive_publish_io(&mut socket, &mut session, &mut buf, &tx, cfg, cancel).await;
    drop(tx); // Lets the segmenter flush and exit.
    if let Err(e) = segmenter.await {
        warn!(error = %e, %stream_id, "segmenter task join failed");
    }
    result
}

async fn drive_publish_io(
    socket: &mut TcpStream,
    session: &mut ServerSession,
    buf: &mut [u8; SOCKET_READ_BUF],
    tx: &mpsc::Sender<MediaChunk>,
    _cfg: &Arc<LiveStreamConfig>,
    cancel: &CancellationToken,
) -> LiveResult<()> {
    loop {
        let read = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                info!("RTMP publisher stopping for process shutdown");
                return Ok(());
            }
            result = socket.read(buf) => result,
        };
        let n = match read {
            Ok(0) => {
                info!("RTMP publisher disconnected");
                return Ok(());
            }
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                ) =>
            {
                info!(error = %e, "RTMP publisher closed connection");
                return Ok(());
            }
            Err(e) => return Err(LiveError::from(e)),
        };
        let results = session
            .handle_input(&buf[..n])
            .map_err(|e| LiveError::Protocol(format!("session input during publish: {e:?}")))?;
        for r in results {
            match r {
                ServerSessionResult::OutboundResponse(p) => {
                    socket.write_all(&p.bytes).await.map_err(LiveError::from)?;
                }
                ServerSessionResult::UnhandleableMessageReceived(payload) => {
                    debug!(?payload, "RTMP unhandleable message (publish)");
                }
                ServerSessionResult::RaisedEvent(event) => match event {
                    ServerSessionEvent::VideoDataReceived {
                        data, timestamp, ..
                    } => {
                        if tx
                            .send(MediaChunk {
                                kind: FLV_TAG_VIDEO,
                                timestamp_ms: timestamp.value,
                                data,
                            })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                    ServerSessionEvent::AudioDataReceived {
                        data, timestamp, ..
                    } => {
                        if tx
                            .send(MediaChunk {
                                kind: FLV_TAG_AUDIO,
                                timestamp_ms: timestamp.value,
                                data,
                            })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                    ServerSessionEvent::PublishStreamFinished { .. } => {
                        info!("RTMP publish finished");
                        return Ok(());
                    }
                    ServerSessionEvent::StreamMetadataChanged { metadata, .. } => {
                        debug!(?metadata, "RTMP metadata changed");
                    }
                    other => debug!(?other, "RTMP publish event"),
                },
            }
        }
    }
}

#[derive(Debug, Clone)]
struct MediaChunk {
    kind: u8,
    timestamp_ms: u32,
    data: Bytes,
}

/// Drain the converter's pending TS bytes (prepending PAT+PMT) and hand them
/// to the HLS writer. The converter keeps codec config + saw_first_keyframe
/// state across segments so subsequent calls remain valid TS.
async fn flush_ts_segment(hls: &mut HlsWriter, mux: &mut FlvToTsConverter, duration_secs: f32) {
    if !mux.has_segment_data() {
        return;
    }
    let bytes = mux.drain_segment();
    match hls.push_segment(bytes, duration_secs).await {
        Ok(path) => debug!(?path, duration_secs, "wrote TS segment"),
        Err(e) => warn!(error = %e, "failed to write TS segment"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RtmpIngest>();
    }

    #[test]
    fn segment_duration_is_two_seconds() {
        // The README and design doc both reference a ~2s segment cadence; keep
        // them in sync with a test so a future refactor can't silently break it.
        assert_eq!(SEGMENT_DURATION_SECS, 2);
    }

    #[test]
    fn validate_stream_key_accepts_generated_shape() {
        // A 32-char hex token is exactly what `random_key()` emits in
        // aero-storage; the canonical happy path must pass.
        assert!(validate_stream_key("0123456789abcdef0123456789abcdef").is_ok());
        // Custom keys with common URL-safe punctuation are also fine.
        assert!(validate_stream_key("live-room_42.key").is_ok());
        // Exactly at the length limit is allowed.
        let max = "a".repeat(MAX_STREAM_KEY_LEN);
        assert!(validate_stream_key(&max).is_ok());
    }

    #[test]
    fn validate_stream_key_rejects_empty_and_whitespace() {
        assert_eq!(validate_stream_key(""), Err(StreamKeyRejection::Empty));
        assert_eq!(
            validate_stream_key("   \t "),
            Err(StreamKeyRejection::Empty)
        );
    }

    #[test]
    fn validate_stream_key_rejects_too_long_before_scanning() {
        // One byte over the limit. Length is checked first so an oversized blob
        // never gets a full character scan.
        let oversized = "x".repeat(MAX_STREAM_KEY_LEN + 1);
        assert_eq!(
            validate_stream_key(&oversized),
            Err(StreamKeyRejection::TooLong)
        );
        // A huge blob that also contains control chars is still classified by
        // length (cheapest-first ordering), proving the early return.
        let huge = format!("{}\n", "y".repeat(MAX_STREAM_KEY_LEN * 4));
        assert_eq!(validate_stream_key(&huge), Err(StreamKeyRejection::TooLong));
    }

    #[test]
    fn validate_stream_key_rejects_control_characters() {
        assert_eq!(
            validate_stream_key("good\nkey"),
            Err(StreamKeyRejection::ControlChar)
        );
        assert_eq!(
            validate_stream_key("nul\0byte"),
            Err(StreamKeyRejection::ControlChar)
        );
        // A bare carriage return would let a publisher forge log lines.
        assert_eq!(
            validate_stream_key("key\r INFO forged"),
            Err(StreamKeyRejection::ControlChar)
        );
    }

    #[test]
    fn validate_stream_key_rejects_path_traversal() {
        assert_eq!(
            validate_stream_key("../../etc/passwd"),
            Err(StreamKeyRejection::PathTraversal)
        );
        assert_eq!(
            validate_stream_key("a/b"),
            Err(StreamKeyRejection::PathTraversal)
        );
        assert_eq!(
            validate_stream_key("windows\\style"),
            Err(StreamKeyRejection::PathTraversal)
        );
        // `..` without a slash is still suspicious and rejected.
        assert_eq!(
            validate_stream_key("ab..cd"),
            Err(StreamKeyRejection::PathTraversal)
        );
    }

    #[tokio::test]
    async fn idle_listener_stops_promptly_when_cancelled() {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = probe.local_addr().unwrap();
        drop(probe);
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://u:p@localhost/aero")
            .unwrap();
        let cfg = Arc::new(LiveStreamConfig {
            hls_dir: std::env::temp_dir().join("aero-rtmp-cancel-test"),
            rtmp_listen: listen,
        });
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            RtmpIngest::new()
                .run_until_cancelled(StreamRepo::new(pool), cfg, task_cancel)
                .await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("cancelled listener should stop promptly")
            .expect("listener task should not panic");
        assert!(result.is_ok(), "listener should stop cleanly: {result:?}");
    }
}
