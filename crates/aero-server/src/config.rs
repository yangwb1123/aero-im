//! Gateway hardening configuration.
//!
//! These knobs tune the protective middleware stack (timeout / body-size /
//! concurrency / CORS / rate limit) and the WebSocket back-pressure behaviour.
//! They are read from the environment with safe production-leaning defaults so
//! the binary works out-of-the-box yet stays tunable per deployment.
//!
//! Naming follows the runtime `AERO_*` convention already used elsewhere in this
//! crate (e.g. `AERO_PUBLIC_BASE_URL`, `AERO_STUN_URLS`) rather than the figment
//! `AppConfig` double-underscore sections, which live in `aero-common` and are
//! out of scope to extend here.

use std::time::Duration;

/// Tunables for the global tower middleware stack.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// Per-request wall-clock timeout. `0` disables the timeout layer.
    pub request_timeout: Duration,
    /// Maximum accepted request body size, in bytes.
    pub max_body_bytes: usize,
    /// Maximum number of in-flight requests processed concurrently. `0`
    /// disables the concurrency-limit layer.
    pub max_concurrency: usize,
    /// CORS allow-list. Empty ⇒ permissive (dev default); otherwise only these
    /// exact origins are allowed.
    pub cors_allowed_origins: Vec<String>,
    /// Per-client rate-limit settings for general API endpoints.
    pub rate_limit: RateLimitConfig,
    /// Stricter per-client rate-limit for credential-accepting auth endpoints
    /// (`/api/auth/login`, `/api/auth/register`, `/api/auth/forgot-password`,
    /// `/api/auth/reset-password`, `/api/auth/refresh`).
    pub auth_rate_limit: RateLimitConfig,
}

/// Token-bucket rate-limit tunables (per client key).
#[derive(Debug, Clone, Copy)]
pub struct RateLimitConfig {
    /// Sustained requests allowed per second per client (the refill rate).
    pub per_second: u32,
    /// Burst capacity — the bucket size, allowing short spikes above the
    /// sustained rate.
    pub burst: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        // 20 req/s sustained, bursting to 40 — generous for an interactive UI,
        // tight enough to blunt scripted abuse.
        Self { per_second: 20, burst: 40 }
    }
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            // Covers the 32 MiB blob-content cap (`routes::MAX_BLOB_BYTES`) plus
            // multipart framing overhead, while still bounding every request.
            max_body_bytes: 33 * 1024 * 1024, // 33 MiB
            max_concurrency: 1024,
            cors_allowed_origins: Vec::new(),
            rate_limit: RateLimitConfig::default(),
            // 3 req/s, burst 5 — tight enough to frustrate credential stuffing
            // while allowing a human who fat-fingers their password a few retries.
            auth_rate_limit: RateLimitConfig { per_second: 3, burst: 5 },
        }
    }
}

impl GatewayConfig {
    /// Build from the environment, falling back to [`GatewayConfig::default`] for
    /// any unset / unparsable variable.
    ///
    /// Recognised variables:
    /// * `AERO_HTTP_TIMEOUT_SECS` — request timeout (seconds; `0` disables).
    /// * `AERO_HTTP_MAX_BODY_BYTES` — request body cap (bytes).
    /// * `AERO_HTTP_MAX_CONCURRENCY` — in-flight request cap (`0` disables).
    /// * `AERO_CORS_ALLOWED_ORIGINS` — comma-separated origin allow-list; unset
    ///   or empty keeps the permissive dev default.
    /// * `AERO_RATE_LIMIT_PER_SEC` — sustained per-client request rate.
    /// * `AERO_RATE_LIMIT_BURST` — per-client burst capacity.
    /// * `AERO_AUTH_RATE_LIMIT_PER_SEC` — sustained rate for auth endpoints.
    /// * `AERO_AUTH_RATE_LIMIT_BURST` — burst capacity for auth endpoints.
    #[must_use]
    pub fn from_env() -> Self {
        let d = Self::default();
        let timeout_secs = env_parse("AERO_HTTP_TIMEOUT_SECS").unwrap_or(d.request_timeout.as_secs());
        let cors_allowed_origins = std::env::var("AERO_CORS_ALLOWED_ORIGINS")
            .ok()
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Self {
            request_timeout: Duration::from_secs(timeout_secs),
            max_body_bytes: env_parse("AERO_HTTP_MAX_BODY_BYTES").unwrap_or(d.max_body_bytes),
            max_concurrency: env_parse("AERO_HTTP_MAX_CONCURRENCY").unwrap_or(d.max_concurrency),
            cors_allowed_origins,
            rate_limit: RateLimitConfig {
                per_second: env_parse("AERO_RATE_LIMIT_PER_SEC").unwrap_or(d.rate_limit.per_second),
                burst: env_parse("AERO_RATE_LIMIT_BURST").unwrap_or(d.rate_limit.burst),
            },
            auth_rate_limit: RateLimitConfig {
                per_second: env_parse("AERO_AUTH_RATE_LIMIT_PER_SEC")
                    .unwrap_or(d.auth_rate_limit.per_second),
                burst: env_parse("AERO_AUTH_RATE_LIMIT_BURST")
                    .unwrap_or(d.auth_rate_limit.burst),
            },
        }
    }
}

/// Per-connection WebSocket send-queue settings (OOM back-pressure guard).
#[derive(Debug, Clone, Copy)]
pub struct WsConfig {
    /// Bounded capacity of each connection's outbound message queue. A slow or
    /// stalled client cannot grow this without bound.
    pub send_queue_capacity: usize,
    /// When the queue is full, disconnect the laggy client instead of only
    /// dropping the message. The broadcaster never blocks either way.
    pub disconnect_on_full: bool,
}

impl Default for WsConfig {
    fn default() -> Self {
        Self { send_queue_capacity: 256, disconnect_on_full: true }
    }
}

impl WsConfig {
    /// Build from the environment.
    ///
    /// * `AERO_WS_SEND_QUEUE_CAP` — bounded per-connection send-queue capacity.
    /// * `AERO_WS_DISCONNECT_ON_FULL` — `0`/`false` ⇒ drop-only; otherwise drop
    ///   the message *and* disconnect the laggy client (default).
    #[must_use]
    pub fn from_env() -> Self {
        let d = Self::default();
        let cap = env_parse::<usize>("AERO_WS_SEND_QUEUE_CAP")
            .filter(|c| *c > 0)
            .unwrap_or(d.send_queue_capacity);
        let disconnect_on_full = std::env::var("AERO_WS_DISCONNECT_ON_FULL")
            .ok()
            .map_or(d.disconnect_on_full, |v| !matches!(v.trim(), "0" | "false" | "off"));
        Self { send_queue_capacity: cap, disconnect_on_full }
    }
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_defaults_are_sane() {
        let c = GatewayConfig::default();
        assert_eq!(c.request_timeout, Duration::from_secs(30));
        assert_eq!(c.max_body_bytes, 33 * 1024 * 1024);
        assert_eq!(c.max_concurrency, 1024);
        assert!(c.cors_allowed_origins.is_empty());
        assert_eq!(c.rate_limit.per_second, 20);
        assert_eq!(c.rate_limit.burst, 40);
        assert_eq!(c.auth_rate_limit.per_second, 3);
        assert_eq!(c.auth_rate_limit.burst, 5);
    }

    #[test]
    fn ws_defaults_are_bounded() {
        let c = WsConfig::default();
        assert_eq!(c.send_queue_capacity, 256);
        assert!(c.disconnect_on_full);
    }
}
