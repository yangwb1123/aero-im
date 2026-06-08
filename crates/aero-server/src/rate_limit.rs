//! In-memory per-client rate limiting.
//!
//! Wires the dormant [`aero_common::Error::RateLimited`] (HTTP 429) into the API.
//! A hand-rolled token-bucket keyed by client (authenticated participant when
//! available, otherwise peer IP) blunts scripted abuse and cheap denial of
//! service without pulling in a heavyweight dependency — it is backed by the
//! `dashmap` already in the dependency tree.
//!
//! The bucket math is split out into [`RateLimiter`] with an explicit clock
//! parameter so it can be unit-tested deterministically (allow N, then 429, then
//! refill), independent of any HTTP plumbing.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use aero_common::Error as AeroError;
use axum::{
    extract::{ConnectInfo, Request, State},
    http::HeaderMap,
    middleware::Next,
    response::Response,
};
use dashmap::DashMap;

use crate::config::RateLimitConfig;
use crate::error::ApiError;
use crate::state::AppState;

/// A single client's token bucket.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// Available tokens (fractional, so sub-unit refill accumulates).
    tokens: f64,
    /// When the bucket was last refilled.
    last: Instant,
}

/// Token-bucket rate limiter shared across requests.
///
/// Cheap to clone (it is just an `Arc` around the map + config). Each distinct
/// client key gets its own bucket of `burst` capacity that refills at
/// `per_second` tokens/second.
#[derive(Clone)]
pub struct RateLimiter {
    buckets: Arc<DashMap<ClientKey, Bucket>>,
    /// Refill rate, tokens per second.
    rate: f64,
    /// Bucket capacity (burst ceiling).
    capacity: f64,
}

/// Identity a request is rate-limited against.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ClientKey {
    /// Authenticated participant (preferred — survives NAT / shared IPs).
    Participant(aero_common::ParticipantId),
    /// Anonymous caller, keyed by source IP.
    Ip(IpAddr),
}

impl RateLimiter {
    /// Build from the configured sustained rate + burst.
    #[must_use]
    pub fn new(cfg: RateLimitConfig) -> Self {
        Self {
            buckets: Arc::new(DashMap::new()),
            rate: f64::from(cfg.per_second.max(1)),
            capacity: f64::from(cfg.burst.max(1)),
        }
    }

    /// Build a limiter with an explicit fractional refill rate (tokens/second)
    /// and burst capacity. Lets a *window*-style limit be expressed — e.g.
    /// "5 per minute" ⇒ `with_rate(5.0 / 60.0, 5.0)`, "3 per hour" ⇒
    /// `with_rate(3.0 / 3600.0, 3.0)` — which the integer-per-second [`Self::new`]
    /// cannot. Used for the high-risk credential endpoints (ROADMAP 方向三).
    /// Both inputs are floored to a tiny positive value / 1.0 so a misconfig
    /// can't stall or divide by zero.
    #[must_use]
    pub fn with_rate(per_second: f64, burst: f64) -> Self {
        Self {
            buckets: Arc::new(DashMap::new()),
            rate: per_second.max(f64::MIN_POSITIVE),
            capacity: burst.max(1.0),
        }
    }

    /// Account for one request from `key` at instant `now`.
    ///
    /// Returns `true` if the request is permitted (a token was consumed), or
    /// `false` if the bucket is empty and the request should be rejected with
    /// HTTP 429. Time is injected so the policy is deterministically testable.
    pub fn check_at(&self, key: ClientKey, now: Instant) -> bool {
        let mut entry = self
            .buckets
            .entry(key)
            .or_insert(Bucket { tokens: self.capacity, last: now });
        let bucket = entry.value_mut();
        // Refill for the elapsed interval, capped at capacity.
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        if elapsed > 0.0 {
            bucket.tokens = (bucket.tokens + elapsed * self.rate).min(self.capacity);
            bucket.last = now;
        }
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Convenience wrapper using the current monotonic clock.
    pub fn check(&self, key: ClientKey) -> bool {
        self.check_at(key, Instant::now())
    }

    /// Number of distinct client buckets currently tracked (for tests / metrics).
    #[must_use]
    pub fn tracked_clients(&self) -> usize {
        self.buckets.len()
    }
}

/// Auth endpoints that accept credentials — these get a stricter token bucket
/// (the general one, beyond the two per-path limiters below).
const SENSITIVE_AUTH_PATHS: &[&str] = &[
    "/api/auth/login",
    "/api/auth/register",
    "/api/auth/forgot-password",
    "/api/auth/reset-password",
    "/api/auth/refresh",
];

/// Axum middleware enforcing per-client rate limits with per-route severity
/// (ROADMAP 方向三):
///
/// * `/api/auth/login` — `login_rate_limiter` (5 / minute / client): the
///   credential-stuffing front door, so it gets the tightest sustained cap.
/// * `/api/auth/forgot-password` — `forgot_rate_limiter` (3 / hour / client):
///   the email-enumeration / reset-spam vector.
/// * other `SENSITIVE_AUTH_PATHS` (register / reset / refresh) — the general
///   `auth_rate_limiter` (default 3 req/s, burst 5).
/// * everything else — the baseline `rate_limiter` (default 20 req/s, burst 40).
///
/// Keys by the authenticated participant when the request carries a valid bearer
/// token, otherwise by client IP (honouring a single `X-Forwarded-For` hop, then
/// `ConnectInfo`). Over-budget requests short-circuit with
/// [`AeroError::RateLimited`] → 429.
pub async fn layer(
    State(state): State<AppState>,
    connect_info: Option<ConnectInfo<std::net::SocketAddr>>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let key = client_key(&state, &headers, connect_info.map(|ci| ci.0));
    let limiter = match request.uri().path() {
        "/api/auth/login" => &state.login_rate_limiter,
        "/api/auth/forgot-password" => &state.forgot_rate_limiter,
        p if SENSITIVE_AUTH_PATHS.contains(&p) => &state.auth_rate_limiter,
        _ => &state.rate_limiter,
    };
    if limiter.check(key) {
        Ok(next.run(request).await)
    } else {
        // Observability (ROADMAP 方向四): count limiter rejections so a 429 spike
        // (abuse / misbehaving client) is visible on the dashboard.
        crate::metrics::record_rate_limit_rejection();
        Err(ApiError(AeroError::RateLimited))
    }
}

/// Derive the rate-limit key for a request.
fn client_key(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
) -> ClientKey {
    if let Some(pid) = bearer_participant(state, headers) {
        return ClientKey::Participant(pid);
    }
    ClientKey::Ip(client_ip(headers, peer))
}

/// Best-effort: pull the participant id out of a valid `Authorization: Bearer`
/// token so authenticated clients are limited per-identity, not per-IP. Returns
/// `None` for missing/invalid tokens (those fall back to IP keying).
fn bearer_participant(state: &AppState, headers: &HeaderMap) -> Option<aero_common::ParticipantId> {
    let raw = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let token = raw.strip_prefix("Bearer ").or_else(|| raw.strip_prefix("bearer "))?;
    let claims = state.auth.verify(token.trim()).ok()?;
    claims.participant_id().ok()
}

/// Resolve the client IP: first hop of `X-Forwarded-For` if present, else the
/// transport peer, else an unspecified address (so keying still works).
fn client_ip(headers: &HeaderMap, peer: Option<std::net::SocketAddr>) -> IpAddr {
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = xff.split(',').next() {
            if let Ok(ip) = first.trim().parse::<IpAddr>() {
                return ip;
            }
        }
    }
    peer.map_or(IpAddr::from([0, 0, 0, 0]), |a| a.ip())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::ParticipantId;
    use std::time::Duration;

    fn ip_key(n: u8) -> ClientKey {
        ClientKey::Ip(IpAddr::from([10, 0, 0, n]))
    }

    #[test]
    fn allows_burst_then_blocks_then_refills() {
        // Capacity 3, 1 token/sec.
        let rl = RateLimiter::new(RateLimitConfig { per_second: 1, burst: 3 });
        let key = ip_key(1);
        let t0 = Instant::now();

        // First three requests drain the full burst.
        assert!(rl.check_at(key.clone(), t0));
        assert!(rl.check_at(key.clone(), t0));
        assert!(rl.check_at(key.clone(), t0));
        // Fourth in the same instant is rejected → 429.
        assert!(!rl.check_at(key.clone(), t0));

        // After 1 second exactly one token refilled.
        let t1 = t0 + Duration::from_secs(1);
        assert!(rl.check_at(key.clone(), t1));
        assert!(!rl.check_at(key.clone(), t1));

        // After a long idle the bucket refills only up to capacity (no overflow).
        let t2 = t1 + Duration::from_secs(3600);
        assert!(rl.check_at(key.clone(), t2));
        assert!(rl.check_at(key.clone(), t2));
        assert!(rl.check_at(key.clone(), t2));
        assert!(!rl.check_at(key, t2));
    }

    #[test]
    fn buckets_are_per_client() {
        let rl = RateLimiter::new(RateLimitConfig { per_second: 1, burst: 1 });
        let t0 = Instant::now();
        let a = ip_key(1);
        let b = ip_key(2);

        // a exhausts its single token.
        assert!(rl.check_at(a.clone(), t0));
        assert!(!rl.check_at(a, t0));
        // b is unaffected — independent bucket.
        assert!(rl.check_at(b.clone(), t0));
        assert!(!rl.check_at(b, t0));

        assert_eq!(rl.tracked_clients(), 2);
    }

    #[test]
    fn participant_and_ip_keys_are_distinct() {
        let rl = RateLimiter::new(RateLimitConfig { per_second: 1, burst: 1 });
        let t0 = Instant::now();
        let pid = ParticipantId::new();
        let p_key = ClientKey::Participant(pid);
        let i_key = ip_key(7);

        assert!(rl.check_at(p_key.clone(), t0));
        assert!(!rl.check_at(p_key, t0));
        // Same instant, different key kind → its own fresh bucket.
        assert!(rl.check_at(i_key.clone(), t0));
        assert!(!rl.check_at(i_key, t0));
    }

    #[test]
    fn sensitive_auth_paths_are_enumerated() {
        // Verify the path list covers the expected credential endpoints.
        assert!(SENSITIVE_AUTH_PATHS.contains(&"/api/auth/login"));
        assert!(SENSITIVE_AUTH_PATHS.contains(&"/api/auth/register"));
        assert!(SENSITIVE_AUTH_PATHS.contains(&"/api/auth/forgot-password"));
        assert!(SENSITIVE_AUTH_PATHS.contains(&"/api/auth/reset-password"));
        assert!(SENSITIVE_AUTH_PATHS.contains(&"/api/auth/refresh"));
        assert!(!SENSITIVE_AUTH_PATHS.contains(&"/api/messages"));
    }

    #[test]
    fn fractional_refill_accumulates() {
        // 2 tokens/sec, capacity 2. Half a second yields exactly one token.
        let rl = RateLimiter::new(RateLimitConfig { per_second: 2, burst: 2 });
        let key = ip_key(9);
        let t0 = Instant::now();
        assert!(rl.check_at(key.clone(), t0));
        assert!(rl.check_at(key.clone(), t0));
        assert!(!rl.check_at(key.clone(), t0));
        // 0.5s → +1 token.
        let half = t0 + Duration::from_millis(500);
        assert!(rl.check_at(key.clone(), half));
        assert!(!rl.check_at(key, half));
    }

    #[test]
    fn with_rate_enforces_per_minute_window() {
        // 5 / minute, burst 5: five immediate requests pass, the sixth is 429.
        let rl = RateLimiter::with_rate(5.0 / 60.0, 5.0);
        let key = ip_key(11);
        let t0 = Instant::now();
        for _ in 0..5 {
            assert!(rl.check_at(key.clone(), t0));
        }
        assert!(!rl.check_at(key.clone(), t0), "6th within the minute is rejected");
        // 12s ⇒ +1 token at 5/min (one token per 12s); a single request passes.
        let t1 = t0 + Duration::from_secs(12);
        assert!(rl.check_at(key.clone(), t1));
        assert!(!rl.check_at(key, t1));
    }

    #[test]
    fn with_rate_enforces_per_hour_window() {
        // 3 / hour, burst 3: three pass, fourth 429; needs 1200s for a refill.
        let rl = RateLimiter::with_rate(3.0 / 3600.0, 3.0);
        let key = ip_key(12);
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(rl.check_at(key.clone(), t0));
        }
        assert!(!rl.check_at(key.clone(), t0));
        // 10 minutes is NOT enough (need 20 min for one token at 3/hr).
        assert!(!rl.check_at(key.clone(), t0 + Duration::from_secs(600)));
        // 20 minutes ⇒ exactly one token.
        assert!(rl.check_at(key, t0 + Duration::from_secs(1200)));
    }

    #[test]
    fn x_forwarded_for_first_hop_wins() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.7, 10.0.0.1".parse().unwrap());
        let ip = client_ip(&headers, Some("127.0.0.1:5000".parse().unwrap()));
        assert_eq!(ip, "203.0.113.7".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn client_ip_falls_back_to_peer() {
        let headers = HeaderMap::new();
        let ip = client_ip(&headers, Some("198.51.100.4:9000".parse().unwrap()));
        assert_eq!(ip, "198.51.100.4".parse::<IpAddr>().unwrap());
    }
}
