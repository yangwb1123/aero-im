//! RTMP and SRT ingest spawning.
use std::sync::Arc;
use tokio_util::task::TaskTracker;
use aero_live_core::{LiveIngest, LiveStreamConfig};
use aero_live_rtmp::spawn_rtmp_ingest;
use tracing::info;

pub(crate) struct IngestConfig {
    pub(crate) rtmp: std::net::SocketAddr,
    pub(crate) hls_dir: std::path::PathBuf,
    pub(crate) srt: std::net::SocketAddr,
}

pub(crate) fn from_server_cfg(cfg: &aero_common::config::AppConfig) -> IngestConfig {
    let rtmp_listen = cfg
        .server
        .rtmp_listen
        .parse()
        .unwrap_or_else(|_| "0.0.0.0:1935".parse().expect("default rtmp addr"));
    let hls_dir = std::path::PathBuf::from(&cfg.server.hls_dir);
    let srt = super::srt_backing_rtmp_addr(&rtmp_listen);
    let srt_listen = srt.ip().to_string();
    let srt_port = srt.port().saturating_add(1);
    info!(addr = %rtmp_listen, "rtmp ingest listening");
    info!(addr = %format!("{srt_listen}:{srt_port}"), "srt ingest configured");
    IngestConfig {
        rtmp: rtmp_listen,
        hls_dir,
        srt,
    }
}

pub(crate) fn spawn(
    tracker: &TaskTracker,
    streams: aero_storage::StreamRepo,
    cfg: IngestConfig,
) -> Arc<LiveStreamConfig> {
    let live_cfg = Arc::new(LiveStreamConfig {
        hls_dir: cfg.hls_dir.clone(),
        rtmp_listen: cfg.rtmp,
    });

    // RTMP
    {
        let repo = streams.clone();
        let live_cfg = live_cfg.clone();
        tracker.spawn(async move {
            let handle = spawn_rtmp_ingest(repo, live_cfg);
            if let Err(e) = handle.await {
                tracing::warn!(error = ?e, "rtmp ingest task ended");
            }
        });
    }

    // SRT (HSv5 handshake + AES-CTR + ACK/NAK reliability → TS→HLS). `SrtIngest`
    // implements the `LiveIngest::run` trait method (aero-live-srt/src/lib.rs:282),
    // so it wires exactly like RTMP — the prior "method not available" note was
    // stale. AES-CTR encryption activates when AERO_SRT_PASSPHRASE is set.
    // E2E push needs a real ffmpeg/OBS SRT source (staging seam), same as RTMP.
    {
        let repo = streams.clone();
        let srt_cfg = Arc::new(LiveStreamConfig {
            hls_dir: cfg.hls_dir,
            rtmp_listen: cfg.srt,
        });
        let mut srt = aero_live_srt::SrtIngest::new();
        if let Ok(pass) = std::env::var("AERO_SRT_PASSPHRASE") {
            if !pass.is_empty() {
                srt = srt.with_passphrase(pass.into_bytes());
            }
        }
        tracker.spawn(async move {
            if let Err(e) = srt.run(repo, srt_cfg).await {
                tracing::warn!(error = ?e, "SRT ingest task ended");
            }
        });
    }

    live_cfg
}
