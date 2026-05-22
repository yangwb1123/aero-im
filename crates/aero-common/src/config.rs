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

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TelemetryConfig {
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

fn default_log_level() -> String {
    "info,aero=debug".into()
}

impl AppConfig {
    /// Load from `config.toml` (optional) + `AERO__SECTION__KEY` env vars.
    pub fn load() -> Result<Self, figment::Error> {
        let _ = dotenvy::dotenv();
        Figment::new()
            .merge(Toml::file("config.toml"))
            .merge(Env::prefixed("AERO__").split("__"))
            .extract()
    }
}
