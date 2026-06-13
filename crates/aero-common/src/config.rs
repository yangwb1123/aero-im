//! Application configuration.
//!
//! Layered loading: defaults → `config.toml` (if present) → environment variables (`AERO_*`).

use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub redis: RedisConfig,
    pub nats: NatsConfig,
    pub auth: AuthConfig,
    pub telemetry: TelemetryConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub web_dir: String,
    #[serde(default = "default_blob_dir")]
    pub blob_dir: String,
    #[serde(default = "default_hls_dir")]
    pub hls_dir: String,
    #[serde(default = "default_rtmp_listen")]
    pub rtmp_listen: String,
}

fn default_blob_dir() -> String {
    "data/blobs".into()
}
fn default_hls_dir() -> String {
    "data/hls".into()
}
fn default_rtmp_listen() -> String {
    "0.0.0.0:1935".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    pub url: String,
    #[serde(default = "default_pool_max")]
    pub max_connections: u32,
}

fn default_pool_max() -> u32 {
    16
}

#[derive(Debug, Clone, Deserialize)]
pub struct RedisConfig {
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NatsConfig {
    pub url: String,
}

#[derive(Clone, Deserialize)]
pub struct AuthConfig {
    /// PEM-encoded RSA private key.
    pub jwt_private_key_pem: String,
    /// PEM-encoded RSA public key (matches the private key).
    pub jwt_public_key_pem: String,
    /// JWT issuer claim (`iss`).
    #[serde(default = "default_issuer")]
    pub issuer: String,
    /// Access-token lifetime in seconds.
    #[serde(default = "default_access_ttl")]
    pub access_ttl_secs: u64,
    /// Refresh-token lifetime in seconds.
    #[serde(default = "default_refresh_ttl")]
    pub refresh_ttl_secs: u64,
    /// Extra PEM-encoded RSA **public** keys kept as verify-only keys for
    /// zero-downtime JWT key rotation: when the active signing key is rotated, the
    /// previous public key is listed here so tokens it signed keep verifying until
    /// they expire. Defaults to empty (no rotation in progress).
    #[serde(default)]
    pub jwt_additional_public_keys: Vec<String>,
}

/// Manual `Debug` that **redacts the RSA private key** (ROADMAP 方向三). The
/// derived impl would print `jwt_private_key_pem` verbatim, so a single stray
/// `info!(?cfg)` / `debug!(?auth)` anywhere downstream — or a panic that formats
/// `AppConfig` — would leak the signing key into logs or telemetry. `AppConfig`
/// transitively derives `Debug`, so this guard travels with it.
impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("jwt_private_key_pem", &"<redacted>")
            .field("jwt_public_key_pem", &self.jwt_public_key_pem)
            .field("issuer", &self.issuer)
            .field("access_ttl_secs", &self.access_ttl_secs)
            .field("refresh_ttl_secs", &self.refresh_ttl_secs)
            .field("jwt_additional_public_keys", &self.jwt_additional_public_keys.len())
            .finish()
    }
}

fn default_issuer() -> String {
    "aero-im".into()
}
fn default_access_ttl() -> u64 {
    3600
}
fn default_refresh_ttl() -> u64 {
    7 * 24 * 3600
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelemetryConfig {
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// OpenTelemetry trace sampling ratio in `[0.0, 1.0]`.
    ///
    /// `1.0` (the default) samples every trace — preserving always-on behavior;
    /// operators lower it in production to cap exporter volume. Values are
    /// clamped into range by [`TelemetryConfig::with_env_overrides`].
    #[serde(default = "default_trace_sample_rate")]
    pub trace_sample_rate: f64,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            otlp_endpoint: None,
            log_level: default_log_level(),
            trace_sample_rate: default_trace_sample_rate(),
        }
    }
}

fn default_log_level() -> String {
    "info,aero=debug".into()
}

fn default_trace_sample_rate() -> f64 {
    1.0
}

/// Clamp an arbitrary sampling ratio into the valid `[0.0, 1.0]` range.
fn clamp_sample_rate(rate: f64) -> f64 {
    rate.clamp(0.0, 1.0)
}

impl TelemetryConfig {
    /// Apply standalone-env overrides, then clamp sampling into range.
    ///
    /// `AERO_TRACE_SAMPLE_RATE` (single-underscore, distinct from figment's
    /// `AERO__TELEMETRY__*` path) lets operators tune sampling without a config
    /// file. Unparseable values are ignored; whatever rate results is clamped to
    /// `[0.0, 1.0]`.
    #[must_use]
    pub fn with_env_overrides(mut self) -> Self {
        if let Ok(raw) = std::env::var("AERO_TRACE_SAMPLE_RATE") {
            if let Ok(parsed) = raw.trim().parse::<f64>() {
                self.trace_sample_rate = parsed;
            }
        }
        self.trace_sample_rate = clamp_sample_rate(self.trace_sample_rate);
        self
    }
}

impl AppConfig {
    /// Load from `config.toml` (optional) + `AERO__SECTION__KEY` env vars.
    pub fn load() -> Result<Self, figment::Error> {
        let _ = dotenvy::dotenv();
        let mut cfg: Self = Figment::new()
            .merge(Toml::file("config.toml"))
            .merge(Env::prefixed("AERO__").split("__"))
            .extract()?;
        cfg.telemetry = cfg.telemetry.with_env_overrides();
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_trace_sample_rate_is_one() {
        assert!((TelemetryConfig::default().trace_sample_rate - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn auth_config_debug_redacts_private_key() {
        let cfg = AuthConfig {
            jwt_private_key_pem: "TOP_SECRET_RSA_PRIVATE_KEY_MATERIAL".into(),
            jwt_public_key_pem: "public-key-pem".into(),
            issuer: "aero".into(),
            access_ttl_secs: 900,
            refresh_ttl_secs: 86_400,
            jwt_additional_public_keys: Vec::new(),
        };
        let dbg = format!("{cfg:?}");
        assert!(
            !dbg.contains("TOP_SECRET_RSA_PRIVATE_KEY_MATERIAL"),
            "private key must never appear in Debug output:\n{dbg}"
        );
        assert!(dbg.contains("<redacted>"), "redaction marker present");
        // Non-secret fields still render for diagnostics.
        assert!(dbg.contains("aero") && dbg.contains("900"));
    }

    #[test]
    fn sample_rate_clamps_into_unit_range() {
        assert!((clamp_sample_rate(-1.0) - 0.0).abs() < f64::EPSILON);
        assert!((clamp_sample_rate(2.0) - 1.0).abs() < f64::EPSILON);
        assert!((clamp_sample_rate(0.05) - 0.05).abs() < f64::EPSILON);
        assert!((clamp_sample_rate(0.0) - 0.0).abs() < f64::EPSILON);
        assert!((clamp_sample_rate(1.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn with_env_overrides_clamps_out_of_range_value() {
        let cfg = TelemetryConfig {
            trace_sample_rate: 2.0,
            ..TelemetryConfig::default()
        }
        .with_env_overrides();
        // No env var set in this test: out-of-range struct value is still clamped.
        assert!(cfg.trace_sample_rate <= 1.0);
        assert!(cfg.trace_sample_rate >= 0.0);
    }
}
