//! Live-streaming domain abstractions: configuration, ingest trait, and ingest events.
//!
//! Implemented from P4 onwards. Shared shape that lets the RTMP/SRT/WHIP backends
//! plug into the same server boot path.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aero_storage::StreamRepo;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Best-effort callback fired right after a stream is marked live. Decoupled from
/// the event bus so this crate needn't depend on `aero-bus`: the server wires a
/// closure that publishes the go-live event the follower-notification bot fans
/// out. Without it, RTMP/SRT go-lives would mark the stream live but never notify
/// followers (only WHIP, which has the bus, did).
pub type GoLiveHook = Arc<dyn Fn(Ulid) + Send + Sync>;

/// Runtime configuration shared between all live-ingest backends.
///
/// `hls_dir` is the root under which `{stream_id}/index.m3u8` directories live;
/// `rtmp_listen` is what the RTMP backend binds to.
#[derive(Clone)]
pub struct LiveStreamConfig {
    pub hls_dir: PathBuf,
    pub rtmp_listen: SocketAddr,
    /// Fired (best-effort) when a stream goes live, after `mark_live`. `None` in
    /// tests / standalone use ⇒ no follower notification.
    pub go_live: Option<GoLiveHook>,
}

impl std::fmt::Debug for LiveStreamConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveStreamConfig")
            .field("hls_dir", &self.hls_dir)
            .field("rtmp_listen", &self.rtmp_listen)
            .field("go_live", &self.go_live.as_ref().map(|_| "<hook>"))
            .finish()
    }
}

impl LiveStreamConfig {
    /// Sensible local-dev defaults: `./data/hls` and `0.0.0.0:1935`.
    #[must_use]
    pub fn local_dev() -> Self {
        Self {
            hls_dir: PathBuf::from("./data/hls"),
            rtmp_listen: "0.0.0.0:1935"
                .parse()
                .expect("hard-coded RTMP default address parses"),
            go_live: None,
        }
    }

    /// Convenience to build a config with a custom HLS root, keeping `rtmp_listen` at the default.
    #[must_use]
    pub fn with_hls_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.hls_dir = dir.into();
        self
    }

    /// Convenience to build a config with a custom RTMP bind address.
    #[must_use]
    pub fn with_rtmp_listen(mut self, addr: SocketAddr) -> Self {
        self.rtmp_listen = addr;
        self
    }
}

/// Resolves the on-disk HLS output directory for a given stream id, rooted at `hls_dir`.
#[must_use]
pub fn hls_path_for(hls_dir: &Path, stream_id: Ulid) -> PathBuf {
    hls_dir.join(stream_id.to_string())
}

/// Relative URL used by the web client to request the HLS manifest.
#[must_use]
pub fn hls_url_for(stream_id: Ulid) -> String {
    format!("/hls/{stream_id}/index.m3u8")
}

/// Events emitted by an ingest implementation as streams come and go.
///
/// These are intentionally lightweight — observers can hang notifications, bus
/// publishing, or metrics off this without depending on the underlying protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IngestEvent {
    Started {
        #[serde(with = "ulid::serde::ulid_as_uuid")]
        stream_id: Ulid,
        hls_path: PathBuf,
    },
    Ended {
        #[serde(with = "ulid::serde::ulid_as_uuid")]
        stream_id: Ulid,
        reason: String,
    },
}

/// Errors that the ingest pipeline can surface to its caller.
#[derive(Debug, thiserror::Error)]
pub enum LiveError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("database: {0}")]
    Database(#[from] sqlx::Error),

    #[error("unknown stream key: {0}")]
    UnknownStreamKey(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("internal: {0}")]
    Internal(#[from] anyhow::Error),
}

pub type LiveResult<T> = Result<T, LiveError>;

/// An object-safe trait every ingest backend (RTMP, SRT, WHIP) implements.
///
/// `run` blocks until the listener is dropped or fatally errors. Implementations
/// own the spawning of per-connection tasks themselves.
#[async_trait]
pub trait LiveIngest: Send + Sync {
    async fn run(&self, repo: StreamRepo, cfg: Arc<LiveStreamConfig>) -> LiveResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hls_url_format_is_stable() {
        let id: Ulid = "01H0000000000000000000000A".parse().unwrap();
        assert_eq!(hls_url_for(id), format!("/hls/{id}/index.m3u8"));
    }

    #[test]
    fn hls_path_is_under_root() {
        let id = Ulid::new();
        let root = PathBuf::from("/var/aero/hls");
        let p = hls_path_for(&root, id);
        assert!(p.starts_with(&root));
        assert_eq!(p.file_name().and_then(|s| s.to_str()), Some(id.to_string().as_str()));
    }

    #[test]
    fn local_dev_config_parses() {
        let cfg = LiveStreamConfig::local_dev();
        assert_eq!(cfg.rtmp_listen.port(), 1935);
    }
}
