//! RTMP and SRT ingest spawning.
use aero_live_core::LiveStreamConfig;
use aero_live_rtmp::RtmpIngest;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::info;

pub(crate) struct IngestConfig {
    pub(crate) rtmp: std::net::SocketAddr,
    pub(crate) hls_dir: std::path::PathBuf,
    pub(crate) srt: std::net::SocketAddr,
}

impl IngestConfig {
    /// Actual public/listen address used by the SRT socket. Internally the SRT
    /// crate shares `LiveStreamConfig` with RTMP and therefore stores the
    /// one-lower backing port in `self.srt`.
    #[must_use]
    pub(crate) fn srt_listen(&self) -> std::net::SocketAddr {
        std::net::SocketAddr::new(self.srt.ip(), self.srt.port().saturating_add(1))
    }
}

pub(crate) fn from_server_cfg(cfg: &aero_common::config::AppConfig) -> IngestConfig {
    let rtmp_listen = cfg
        .server
        .rtmp_listen
        .parse()
        .unwrap_or_else(|_| "0.0.0.0:1935".parse().expect("default rtmp addr"));
    let hls_dir = std::path::PathBuf::from(&cfg.server.hls_dir);
    let srt = super::srt_backing_rtmp_addr(&rtmp_listen);
    let srt_listen = std::net::SocketAddr::new(srt.ip(), srt.port().saturating_add(1));
    info!(addr = %rtmp_listen, "rtmp ingest listening");
    info!(addr = %srt_listen, "srt ingest configured");
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
    shutdown: &CancellationToken,
) -> Arc<LiveStreamConfig> {
    let live_cfg = Arc::new(LiveStreamConfig {
        hls_dir: cfg.hls_dir.clone(),
        rtmp_listen: cfg.rtmp,
    });

    // RTMP
    {
        let repo = streams.clone();
        let live_cfg = live_cfg.clone();
        let cancel = shutdown.clone();
        tracker.spawn(async move {
            if let Err(error) = RtmpIngest::new()
                .run_until_cancelled(repo, live_cfg, cancel)
                .await
            {
                tracing::warn!(%error, "RTMP ingest task ended");
            }
        });
    }

    // SRT (HSv5 handshake + AES-128-CTR + ACK/NAK reliability → TS→HLS).
    // AERO_SRT_PASSPHRASE enables enforced encryption: the production listener
    // validates KMREQ in the caller's CONCLUSION, installs the unwrapped SEK,
    // and responds with KMRSP before accepting media.
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
        let cancel = shutdown.clone();
        tracker.spawn(async move {
            if let Err(error) = srt.run_until_cancelled(repo, srt_cfg, cancel).await {
                tracing::warn!(%error, "SRT ingest task ended");
            }
        });
    }

    live_cfg
}
