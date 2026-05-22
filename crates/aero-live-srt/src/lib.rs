//! SRT (Secure Reliable Transport) ingest — TCP-shaped UDP for low-latency live.
//!
//! ## Scope
//!
//! The full SRT protocol is non-trivial (handshake, ACK/NAK, congestion control,
//! AEAD). This crate ships the **integration surface** the rest of the workspace
//! depends on; the wire-level protocol implementation is deferred and the only
//! real network behavior right now is a UDP listener that logs the first
//! handshake packet from each new peer.
//!
//! When wiring the production-grade SRT stack (e.g. `srt-tokio` or a custom
//! handshake), reuse the existing `StreamRepo::get_by_key` / `mark_live` /
//! `mark_ended` lifecycle that RTMP already exercises.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aero_live_core::{LiveError, LiveIngest, LiveResult, LiveStreamConfig};
use aero_storage::StreamRepo;
use async_trait::async_trait;
use tokio::net::UdpSocket;
use tokio::time::timeout;
use tracing::{info, warn};

/// Placeholder SRT ingest.
///
/// Binds a UDP socket and acknowledges that something dialed in. Real SRT
/// handshake / packet processing belongs in a follow-up. The point of this
/// crate today is to occupy the integration slot so the server boot-up and
/// metrics surface are stable.
#[derive(Debug, Default, Clone)]
pub struct SrtIngest;

impl SrtIngest {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl LiveIngest for SrtIngest {
    async fn run(&self, _repo: StreamRepo, cfg: Arc<LiveStreamConfig>) -> LiveResult<()> {
        // SRT typically runs on the same port number as RTMP+1 in our config.
        let listen: SocketAddr = format!("{}:{}", cfg.rtmp_listen.ip(), cfg.rtmp_listen.port() + 1)
            .parse()
            .map_err(|e| LiveError::Protocol(format!("bad SRT listen addr: {e}")))?;
        let sock = UdpSocket::bind(listen).await.map_err(LiveError::Io)?;
        info!(%listen, "srt placeholder listener bound");

        let mut buf = vec![0u8; 1500];
        loop {
            match timeout(Duration::from_secs(60), sock.recv_from(&mut buf)).await {
                Ok(Ok((n, peer))) => {
                    info!(%peer, bytes = n, "srt: ignored placeholder packet");
                }
                Ok(Err(e)) => {
                    warn!(error = ?e, "srt recv_from failed");
                    return Err(LiveError::Io(e));
                }
                Err(_) => {
                    // Idle tick — keep listener alive.
                }
            }
        }
    }
}

/// TURN config helper. coturn is the real server; this struct + helper render a
/// usable `turnserver.conf` snippet from env + secrets.
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
        let mut s = String::new();
        use std::fmt::Write;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_config_renders_required_lines() {
        let c = TurnConfig {
            listening_port: 3478,
            realm: "x".into(),
            static_auth_secret: "s".into(),
            external_ip: Some("1.2.3.4".into()),
            min_port: 49152,
            max_port: 65535,
        };
        let body = c.render();
        assert!(body.contains("listening-port=3478"));
        assert!(body.contains("realm=x"));
        assert!(body.contains("static-auth-secret=s"));
        assert!(body.contains("external-ip=1.2.3.4"));
        assert!(body.contains("fingerprint"));
    }

    #[test]
    fn turn_config_skips_external_ip_when_absent() {
        let c = TurnConfig {
            listening_port: 3478,
            realm: "x".into(),
            static_auth_secret: "s".into(),
            external_ip: None,
            min_port: 49152,
            max_port: 65535,
        };
        let body = c.render();
        assert!(!body.contains("external-ip="));
    }
}
