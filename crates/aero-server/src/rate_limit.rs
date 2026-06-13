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
use std::time::{Duration, Instant};

use aero_common::Error as AeroError;
use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderValue},
    middleware::Next,
    response::{IntoResponse, Response},
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

/// Post-decision snapshot of a client's bucket, used to emit `X-RateLimit-*`
/// headers (and `Retry-After` on a reject).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitStatus {
    /// Whether the request was permitted (a token was consumed).
    pub allowed: bool,
    /// The bucket's burst ceiling — `X-RateLimit-Limit`.
    pub limit: u64,
    /// Whole tokens left after this request — `X-RateLimit-Remaining`.
    pub remaining: u64,
    /// On success, seconds until the bucket refills to full (`X-RateLimit-Reset`);
    /// on reject, seconds until the next token (`Retry-After`), at least 1.
    pub reset_secs: u64,
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
        self.check_status_at(key, now).allowed
    }

    /// Like [`check_at`](Self::check_at) but also reports the post-decision bucket
    /// state, so the HTTP layer can surface `X-RateLimit-*` / `Retry-After`
    /// headers to clients (letting a well-behaved client back off before it ever
    /// trips a 429).
    // `capacity`/`rate` are small positive configured values and `tokens` is
    // clamped to `[0, capacity]` (and each cast is `.max(0.0)`'d), so the f64→u64
    // conversions below never wrap or lose sign — the lint can't see those bounds.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn check_status_at(&self, key: ClientKey, now: Instant) -> RateLimitStatus {
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
        let allowed = bucket.tokens >= 1.0;
        if allowed {
            bucket.tokens -= 1.0;
        }
        let limit = self.capacity.max(0.0).round() as u64;
        let remaining = bucket.tokens.max(0.0).floor() as u64;
        // On success: seconds until the bucket is full again (informational).
        // On reject: seconds until the next whole token (the Retry-After hint),
        // at least 1 so a client never busy-retries.
        let reset_secs = if allowed {
            ((self.capacity - bucket.tokens) / self.rate).ceil().max(0.0) as u64
        } else {
            ((1.0 - bucket.tokens) / self.rate).ceil().max(1.0) as u64
        };
        RateLimitStatus { allowed, limit, remaining, reset_secs }
    }

    /// Convenience wrapper using the current monotonic clock.
    pub fn check(&self, key: ClientKey) -> bool {
        self.check_at(key, Instant::now())
    }

    /// [`check_status_at`](Self::check_status_at) at the current monotonic clock.
    pub fn check_status(&self, key: ClientKey) -> RateLimitStatus {
        self.check_status_at(key, Instant::now())
    }

    /// Number of distinct client buckets currently tracked (for tests / metrics).
    #[must_use]
    pub fn tracked_clients(&self) -> usize {
        self.buckets.len()
    }

    /// Evict idle buckets that have fully refilled, bounding memory (ROADMAP 方向三).
    ///
    /// Without this the keyed [`DashMap`] grows without bound under IP-spray —
    /// a rotating source address mints a permanent bucket on each request — so
    /// the limiter (a denial-of-service *defence*) becomes a memory-exhaustion
    /// *vector*. A periodic caller sweeps idle entries.
    ///
    /// A bucket is dropped only when it has both refilled back to full
    /// `capacity` at `now` **and** has been untouched for at least `idle_after`.
    /// A fully-refilled bucket is byte-for-byte equivalent to one freshly
    /// created on the next request (both start full), so dropping it cannot
    /// change any future decision. A still-depleted bucket is always kept —
    /// discarding one would reset it to full and let a slow abuser slip the
    /// cap — making eviction **fail-safe toward enforcement**. Returns the
    /// number of buckets removed.
    pub fn sweep_idle(&self, now: Instant, idle_after: Duration) -> usize {
        let before = self.buckets.len();
        self.buckets.retain(|_, b| {
            let elapsed = now.saturating_duration_since(b.last);
            // Effective tokens after refill-to-`now`, capped at capacity.
            let refilled = (b.tokens + elapsed.as_secs_f64() * self.rate).min(self.capacity);
            // Keep while still rate-limited (not yet refilled) OR recently active.
            refilled < self.capacity || elapsed < idle_after
        });
        before - self.buckets.len()
    }
}

/// Operational endpoints that must **never** be rate-limited: the k8s
/// liveness/readiness probes and the Prometheus scrape. Sharing the per-IP
/// bucket with these would let an overload (NAT/ingress collapse many callers
/// onto one source IP) 429 the *liveness* probe — k8s then kills the **healthy**
/// pod, amplifying the outage — or the *readiness* probe (pulled from rotation),
/// or `/metrics` (the dashboard goes blind exactly during the incident).
/// Exact-match, so a future business route such as `/healthcheck` is never
/// exempted by accident.
const OPERATIONAL_PATHS: &[&str] = &["/health", "/health/live", "/health/ready", "/metrics"];

/// Whether `path` is an operational endpoint exempt from rate limiting.
fn is_operational_path(path: &str) -> bool {
    OPERATIONAL_PATHS.contains(&path)
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
    // Operational endpoints (probes, /metrics) bypass rate limiting entirely —
    // see OPERATIONAL_PATHS. Checked before keying so an IP-flood cannot starve
    // the liveness probe and get a healthy pod killed.
    if is_operational_path(request.uri().path()) {
        return Ok(next.run(request).await);
    }
    let key = client_key(&state, &headers, connect_info.map(|ci| ci.0));
    let limiter = match request.uri().path() {
        "/api/auth/login" => &state.login_rate_limiter,
        "/api/auth/forgot-password" => &state.forgot_rate_limiter,
        p if SENSITIVE_AUTH_PATHS.contains(&p) => &state.auth_rate_limiter,
        _ => &state.rate_limiter,
    };
    let status = limiter.check_status(key);
    if status.allowed {
        let mut response = next.run(request).await;
        attach_rate_limit_headers(response.headers_mut(), &status);
        Ok(response)
    } else {
        // Observability (ROADMAP 方向四): count limiter rejections so a 429 spike
        // (abuse / misbehaving client) is visible on the dashboard.
        crate::metrics::record_rate_limit_rejection();
        // Build the 429 ourselves (rather than returning `Err`) so the response
        // carries the X-RateLimit-* + Retry-After headers a client needs to back off.
        let mut response = ApiError(AeroError::RateLimited).into_response();
        attach_rate_limit_headers(response.headers_mut(), &status);
        if let Ok(v) = HeaderValue::from_str(&status.reset_secs.to_string()) {
            response.headers_mut().insert("retry-after", v);
        }
        Ok(response)
    }
}

/// Attach the standard `X-RateLimit-Limit` / `-Remaining` / `-Reset` headers from
/// a [`RateLimitStatus`]. Best-effort: a header value that fails to construct
/// (never, for decimal integers) is simply skipped.
fn attach_rate_limit_headers(headers: &mut HeaderMap, status: &RateLimitStatus) {
    for (name, val) in [
        ("x-ratelimit-limit", status.limit),
        ("x-ratelimit-remaining", status.remaining),
        ("x-ratelimit-reset", status.reset_secs),
    ] {
        if let Ok(v) = HeaderValue::from_str(&val.to_string()) {
            headers.insert(name, v);
        }
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
    fn check_status_reports_limit_remaining_and_reset() {
        // capacity 3, 1 token/sec.
        let rl = RateLimiter::with_rate(1.0, 3.0);
        let key = ip_key(21);
        let t0 = Instant::now();

        // First request: allowed, 2 of 3 left, limit 3.
        let s1 = rl.check_status_at(key.clone(), t0);
        assert!(s1.allowed);
        assert_eq!(s1.limit, 3);
        assert_eq!(s1.remaining, 2);

        // Drain the bucket: 2nd and 3rd allowed, remaining hits 0.
        let s2 = rl.check_status_at(key.clone(), t0);
        assert_eq!(s2.remaining, 1);
        let s3 = rl.check_status_at(key.clone(), t0);
        assert_eq!(s3.remaining, 0);
        assert!(s3.allowed);

        // 4th: rejected, remaining 0, Retry-After ≥ 1s (next token at 1/sec).
        let s4 = rl.check_status_at(key, t0);
        assert!(!s4.allowed);
        assert_eq!(s4.remaining, 0);
        assert!(s4.reset_secs >= 1, "reject must advertise a non-zero Retry-After");
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
    fn sweep_evicts_only_fully_refilled_idle_buckets() {
        // Capacity 2, 1 token/sec.
        let rl = RateLimiter::new(RateLimitConfig { per_second: 1, burst: 2 });
        let t0 = Instant::now();
        let a = ip_key(1);
        let b = ip_key(2);
        // Both consume one token (now depleted to 1).
        assert!(rl.check_at(a.clone(), t0));
        assert!(rl.check_at(b.clone(), t0));
        assert_eq!(rl.tracked_clients(), 2);

        // Sweep immediately: neither idle (elapsed 0 < 5s) → both kept.
        assert_eq!(rl.sweep_idle(t0, Duration::from_secs(5)), 0);
        assert_eq!(rl.tracked_clients(), 2);

        // 10s on: touch `a` so it is freshly active; `b` stays idle since t0 and
        // would have refilled to capacity.
        let t1 = t0 + Duration::from_secs(10);
        assert!(rl.check_at(a.clone(), t1));
        // `a` just touched (elapsed 0 < 5s) → kept; `b` idle 10s & full → evicted.
        assert_eq!(rl.sweep_idle(t1, Duration::from_secs(5)), 1);
        assert_eq!(rl.tracked_clients(), 1);
    }

    #[test]
    fn sweep_keeps_depleted_buckets_even_when_idle() {
        // A still-depleted bucket must survive a sweep — dropping it would reset
        // it to full and let a slow abuser past the cap.
        let rl = RateLimiter::with_rate(1.0 / 3600.0, 1.0); // 1/hour, capacity 1
        let t0 = Instant::now();
        let k = ip_key(3);
        assert!(rl.check_at(k.clone(), t0)); // drains the only token
        // 10 min later: idle past idle_after, but at 1/hour it has NOT refilled.
        let t1 = t0 + Duration::from_secs(600);
        assert_eq!(rl.sweep_idle(t1, Duration::from_secs(60)), 0);
        assert_eq!(rl.tracked_clients(), 1);
        // The retained bucket still enforces the limit.
        assert!(!rl.check_at(k, t1));
    }

    #[test]
    fn operational_paths_bypass_is_exact() {
        assert!(is_operational_path("/health"));
        assert!(is_operational_path("/health/live"));
        assert!(is_operational_path("/health/ready"));
        assert!(is_operational_path("/metrics"));
        // Business routes and near-misses are NOT exempt.
        assert!(!is_operational_path("/api/messages"));
        assert!(!is_operational_path("/healthcheck"));
        assert!(!is_operational_path("/metrics/extra"));
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
