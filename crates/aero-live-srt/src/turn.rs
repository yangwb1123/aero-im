use std::time::Duration;

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
    pub fn ice_server(&self, host: &str, name: &str, ttl: Duration, now_unix: i64) -> IceServer {
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
pub(crate) fn hmac_sha1_base64(key: &[u8], msg: &[u8]) -> String {
    use base64::prelude::{Engine as _, BASE64_STANDARD};
    use hmac::{Hmac, Mac};
    use sha1::Sha1;

    let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(msg);
    let tag = mac.finalize().into_bytes();
    BASE64_STANDARD.encode(tag)
}
