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
//! ## Muxing strategy: passthrough placeholder
//!
//! `rml_rtmp` exposes audio and video payloads as raw FLV tag bodies (H.264
//! AVCC / AAC raw, prefixed with the FLV codec/keyframe byte and a 3-byte
//! composition-time offset). Re-multiplexing those into proper MPEG-TS would
//! require a full TS muxer plus an Annex-B converter — both are out of scope
//! for the P4 spike.
//!
//! Per the task instructions this crate ships the **passthrough placeholder**
//! path: each ~2-second window is concatenated as `[u32be tag_size][tag bytes]`
//! pairs and written verbatim into `{stream_id}/{index}.ts`. A browser HLS
//! player **will not** play these segments back as-is — that's by design. The
//! purpose is to verify the end-to-end ingest plumbing (handshake → DB flip →
//! HLS manifest → file rotation) without depending on a heavy mux library.
//! Real TS muxing is a follow-up.
//!
//! Operators see this warning loudly at every publish start:
//! `"emitting passthrough placeholder segments — browsers cannot play these"`.

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aero_live_core::{hls_path_for, hls_url_for, LiveError, LiveIngest, LiveResult, LiveStreamConfig};
use aero_live_hls::{HlsWriter, DEFAULT_SEGMENT_EXT};
use aero_storage::StreamRepo;
use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::sessions::{
    ServerSession, ServerSessionConfig, ServerSessionEvent, ServerSessionResult,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::interval;
use tracing::{debug, error, info, warn};
use ulid::Ulid;

/// How long a single HLS segment covers in wall-clock time.
pub const SEGMENT_DURATION_SECS: u64 = 2;

/// Receive-buffer size for each TCP read off the socket.
const SOCKET_READ_BUF: usize = 8 * 1024;

/// Maximum bytes a single per-connection task will buffer for HLS segmenting
/// before it drops new data with a warning. Acts as a back-pressure safety
/// net: 32 MiB is well above 2s of broadcast video at any sane bitrate.
const MAX_SEGMENT_BUFFER_BYTES: usize = 32 * 1024 * 1024;

/// FLV tag types used by the placeholder muxer. Matches the values RTMP uses
/// directly so we can mirror them when dropping payloads onto disk.
const FLV_TAG_AUDIO: u8 = 0x08;
const FLV_TAG_VIDEO: u8 = 0x09;

/// Convenience: spawn the RTMP ingest with default settings on a background task.
pub fn spawn_rtmp_ingest(
    repo: StreamRepo,
    cfg: Arc<LiveStreamConfig>,
) -> JoinHandle<LiveResult<()>> {
    tokio::spawn(async move {
        let ingest = RtmpIngest::new();
        ingest.run(repo, cfg).await
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
}

#[async_trait]
impl LiveIngest for RtmpIngest {
    async fn run(&self, repo: StreamRepo, cfg: Arc<LiveStreamConfig>) -> LiveResult<()> {
        let listener = TcpListener::bind(cfg.rtmp_listen)
            .await
            .map_err(LiveError::from)?;
        info!(
            addr = %cfg.rtmp_listen,
            hls_dir = %cfg.hls_dir.display(),
            "RTMP ingest listening"
        );

        loop {
            let (socket, peer) = match listener.accept().await {
                Ok(p) => p,
                Err(e) => {
                    error!(error = %e, "RTMP accept failed");
                    continue;
                }
            };
            debug!(%peer, "RTMP connection accepted");

            let repo = repo.clone();
            let cfg = cfg.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(socket, repo, cfg).await {
                    warn!(error = %e, %peer, "RTMP connection ended with error");
                }
            });
        }
    }
}

/// Per-connection driver: handshake → session events → publish loop.
async fn handle_connection(
    mut socket: TcpStream,
    repo: StreamRepo,
    cfg: Arc<LiveStreamConfig>,
) -> LiveResult<()> {
    let mut buf = [0u8; SOCKET_READ_BUF];

    // 1) Handshake.
    let mut handshake = Handshake::new(PeerType::Server);
    let remaining_after_handshake = loop {
        let n = socket.read(&mut buf).await.map_err(LiveError::from)?;
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
        let n = socket.read(&mut buf).await.map_err(LiveError::from)?;
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
    let publish_result = run_publish_loop(socket, session, buf, publish, &cfg).await;

    // 5) Mark the row ended regardless of how the publish loop exited.
    if let Err(e) = repo.mark_ended(stream_id).await {
        warn!(error = %e, %stream_id, "failed to mark stream ended");
    }

    publish_result
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
                ServerSessionEvent::ConnectionRequested { request_id, app_name } => {
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
                    info!(%app_name, %stream_key, "RTMP publish requested");
                    match repo
                        .get_by_key(&stream_key)
                        .await
                        .map_err(LiveError::Database)?
                    {
                        Some(stream) => {
                            let hls_url = hls_url_for(stream.id);
                            repo.mark_live(stream.id, &hls_url)
                                .await
                                .map_err(LiveError::Database)?;

                            let dir = hls_path_for(&cfg.hls_dir, stream.id);
                            let hls = HlsWriter::new(dir, SEGMENT_DURATION_SECS as u32)
                                .await
                                .map_err(|e| {
                                    LiveError::Internal(anyhow::anyhow!(
                                        "hls writer init: {e}"
                                    ))
                                })?
                                // Keep `.ts` extension even in placeholder mode so the manifest
                                // entries look right; players still won't play these, but
                                // operators expect the file extension regardless.
                                .with_segment_ext(DEFAULT_SEGMENT_EXT);

                            warn!(
                                stream_id = %stream.id,
                                stream_key = %stream_key,
                                "emitting passthrough placeholder segments — browsers cannot play these"
                            );

                            let more = session
                                .accept_request(request_id)
                                .map_err(|e| {
                                    LiveError::Protocol(format!("accept publish: {e:?}"))
                                })?;
                            for r in more {
                                if let ServerSessionResult::OutboundResponse(p) = r {
                                    socket
                                        .write_all(&p.bytes)
                                        .await
                                        .map_err(LiveError::from)?;
                                }
                            }
                            outcome = ProcessOutcome::Publishing(PublishContext {
                                stream_id: stream.id,
                                hls,
                            });
                        }
                        None => {
                            warn!(%stream_key, "rejecting publish: unknown stream key");
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
                                    socket
                                        .write_all(&p.bytes)
                                        .await
                                        .map_err(LiveError::from)?;
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
) -> LiveResult<()> {
    let PublishContext {
        stream_id,
        mut hls,
    } = ctx;

    // Channel from the socket-reading task to the segmenter task. Bounded to
    // back-pressure pathological encoders without dropping silently.
    let (tx, mut rx) = mpsc::channel::<MediaChunk>(1024);

    let segmenter = tokio::spawn(async move {
        let mut buffer: VecDeque<MediaChunk> = VecDeque::new();
        let mut buffer_bytes: usize = 0;
        let mut segment_start = Instant::now();
        let mut ticker = interval(Duration::from_millis(250));
        loop {
            tokio::select! {
                maybe = rx.recv() => {
                    match maybe {
                        Some(chunk) => {
                            buffer_bytes = buffer_bytes.saturating_add(chunk.data.len());
                            if buffer_bytes > MAX_SEGMENT_BUFFER_BYTES {
                                warn!(buffer_bytes, "RTMP segment buffer overflow — dropping current window");
                                buffer.clear();
                                buffer_bytes = 0;
                                segment_start = Instant::now();
                                continue;
                            }
                            buffer.push_back(chunk);
                        }
                        None => {
                            // Sender dropped — flush whatever remains and exit.
                            if !buffer.is_empty() {
                                let elapsed = segment_start.elapsed().as_secs_f32().max(0.001);
                                flush_segment(&mut hls, &mut buffer, elapsed).await;
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
                        && !buffer.is_empty()
                    {
                        let elapsed = segment_start.elapsed().as_secs_f32();
                        flush_segment(&mut hls, &mut buffer, elapsed).await;
                        buffer_bytes = 0;
                        segment_start = Instant::now();
                    }
                }
            }
        }
    });

    let result = drive_publish_io(&mut socket, &mut session, &mut buf, &tx, cfg).await;
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
) -> LiveResult<()> {
    loop {
        let n = match socket.read(buf).await {
            Ok(0) => {
                info!("RTMP publisher disconnected");
                return Ok(());
            }
            Ok(n) => n,
            Err(e) if matches!(e.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset) => {
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
                    ServerSessionEvent::VideoDataReceived { data, timestamp, .. } => {
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
                    ServerSessionEvent::AudioDataReceived { data, timestamp, .. } => {
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

/// Concatenate all queued chunks into a single "segment" blob and hand it to
/// the HLS writer. In placeholder mode we prefix each chunk with a small
/// header so a future debugging tool can pick the stream apart.
async fn flush_segment(
    hls: &mut HlsWriter,
    buffer: &mut VecDeque<MediaChunk>,
    duration_secs: f32,
) {
    if buffer.is_empty() {
        return;
    }
    let approx_size: usize = buffer.iter().map(|c| c.data.len() + 9).sum();
    let mut blob = BytesMut::with_capacity(approx_size);
    for chunk in buffer.drain(..) {
        // [tag_kind:u8][timestamp:u32][payload_len:u32][payload...]
        blob.put_u8(chunk.kind);
        blob.put_u32(chunk.timestamp_ms);
        // Length-prefix so a downstream tool can split chunks back out.
        let len = u32::try_from(chunk.data.len()).unwrap_or(u32::MAX);
        blob.put_u32(len);
        blob.put_slice(&chunk.data);
    }
    let frozen = blob.freeze();
    match hls.push_segment(frozen, duration_secs).await {
        Ok(path) => debug!(?path, duration_secs, "wrote placeholder segment"),
        Err(e) => warn!(error = %e, "failed to write placeholder segment"),
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
}
