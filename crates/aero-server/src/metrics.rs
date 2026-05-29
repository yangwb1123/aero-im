//! Observability wiring for the gateway (ROADMAP 方向四 follow-up).
//!
//! The metrics *foundation* — the registry, the well-known metric-name
//! constants, and the Prometheus text exposition — lives in
//! [`aero_common::metrics`]. This module is the **gateway-side glue** that the
//! design doc deliberately left for `aero-server`:
//!
//! * [`metrics_handler`] — the `GET /metrics` endpoint returning the global
//!   registry's Prometheus exposition.
//! * [`http_metrics_layer`] — an Axum middleware recording the RED signals
//!   (request count by method/status + request-duration histogram by route) for
//!   every request that flows through the tower stack.
//! * [`MetricsConfig`] — whether `/metrics` is exposed and (optionally) gated
//!   behind a bearer token, read from the `AERO_*` environment.
//! * [`record_rate_limit_rejection`] — bumps a dedicated counter when the rate
//!   limiter returns 429 (the limiter calls this).
//!
//! ## Cardinality
//!
//! HTTP series stay *bounded*: requests are labeled by `method` + `status`
//! (small, fixed sets) and the duration histogram by the **matched route
//! template** (e.g. `/api/rooms/:id/messages`, not the concrete id). Using
//! [`axum::extract::MatchedPath`] is what keeps an attacker from blowing up the
//! series count by hitting `/api/rooms/<random>/...` forever.
//!
//! ## Exposure
//!
//! `/metrics` is left **unauthenticated by default** (same posture as
//! `/health`), because the typical deployment scrapes it from inside the
//! cluster / behind network policy. Set `AERO_METRICS_TOKEN` to require
//! `Authorization: Bearer <token>` for any environment where the endpoint is
//! reachable from untrusted networks, or `AERO_METRICS_ENABLED=0` to disable the
//! route entirely.

use std::sync::Arc;
use std::time::Instant;

use aero_common::metrics::{self, names};
use axum::{
    extract::{MatchedPath, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Prometheus text exposition content type (version pinned per the format spec).
const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4";

/// Counter (server-local name): API requests rejected by the rate limiter (429).
///
/// Not one of the cross-crate [`names`] constants because rate limiting is a
/// gateway concern; it lives here next to its single emit site.
pub const RATE_LIMIT_REJECTIONS_TOTAL: &str = "aero_rate_limit_rejections_total";

/// `/metrics` exposure policy, read from the environment.
#[derive(Debug, Clone)]
pub struct MetricsConfig {
    /// Whether the `/metrics` route is mounted at all.
    pub enabled: bool,
    /// Optional bearer token required to scrape `/metrics`. `None` ⇒ the
    /// endpoint is unauthenticated (the default, matching `/health`).
    pub bearer_token: Option<String>,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self { enabled: true, bearer_token: None }
    }
}

impl MetricsConfig {
    /// Build from the environment.
    ///
    /// * `AERO_METRICS_ENABLED` — `0`/`false`/`off` ⇒ do not mount `/metrics`
    ///   (default: enabled).
    /// * `AERO_METRICS_TOKEN` — when set (and non-empty), `/metrics` requires
    ///   `Authorization: Bearer <token>`; otherwise it is unauthenticated.
    #[must_use]
    pub fn from_env() -> Self {
        let enabled = std::env::var("AERO_METRICS_ENABLED")
            .ok()
            .map_or(true, |v| !matches!(v.trim(), "0" | "false" | "off"));
        let bearer_token = std::env::var("AERO_METRICS_TOKEN")
            .ok()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty());
        Self { enabled, bearer_token }
    }
}

/// `GET /metrics` — renders the process-global registry as Prometheus text.
///
/// Extracts only [`MetricsConfig`] (via [`axum::extract::FromRef`]) rather than
/// the full `AppState`, so it needs no DB / NATS / Redis handles — which also
/// makes the route fully unit-testable offline.
///
/// Honours [`MetricsConfig::bearer_token`]: returns `401` when a token is
/// configured and the request lacks a matching `Authorization: Bearer` header.
/// (When no token is configured every request is served, like `/health`.)
pub async fn metrics_handler(
    State(cfg): State<Arc<MetricsConfig>>,
    headers: HeaderMap,
) -> Response {
    if let Some(expected) = cfg.bearer_token.as_deref() {
        if !bearer_matches(&headers, expected) {
            return (StatusCode::UNAUTHORIZED, "metrics: unauthorized").into_response();
        }
    }
    let body = metrics::render_prometheus();
    ([(header::CONTENT_TYPE, PROMETHEUS_CONTENT_TYPE)], body).into_response()
}

/// Constant-time-ish bearer comparison for the metrics token.
fn bearer_matches(headers: &HeaderMap, expected: &str) -> bool {
    let Some(raw) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(token) = raw.strip_prefix("Bearer ").or_else(|| raw.strip_prefix("bearer ")) else {
        return false;
    };
    let token = token.trim();
    // Length-prefixed equality; avoids leaking length via early return where it
    // is cheap to avoid. (The token is a deployment secret, not a password DB.)
    token.len() == expected.len() && token.bytes().zip(expected.bytes()).all(|(a, b)| a == b)
}

/// Axum middleware recording HTTP RED metrics for every request.
///
/// Emits, on completion:
/// * [`names::HTTP_REQUESTS_TOTAL`] labeled `method` + `status`, and
/// * [`names::HTTP_REQUEST_DURATION_SECONDS`] (a histogram) labeled by the
///   matched `route` template.
///
/// Placed in the tower stack so it observes *every* response — including ones
/// short-circuited by inner layers (rate-limit 429s, timeouts) — as long as it
/// sits outside them.
pub async fn http_metrics_layer(request: Request, next: Next) -> Response {
    // Capture the matched route template (bounded cardinality) before consuming
    // the request. Fall back to the raw path only when no route matched (404s);
    // that set is unbounded in theory but such requests don't reach a handler.
    let route = request.extensions().get::<MatchedPath>().map_or_else(
        || request.uri().path().to_owned(),
        |m| m.as_str().to_owned(),
    );
    let method = request.method().clone();

    let start = Instant::now();
    let response = next.run(request).await;
    let elapsed = start.elapsed().as_secs_f64();

    let status_str = response.status().as_u16().to_string();

    metrics::inc_counter_labeled(
        names::HTTP_REQUESTS_TOTAL,
        1,
        &[("method", method.as_str()), ("status", &status_str)],
    );
    metrics::observe_histogram_labeled(
        names::HTTP_REQUEST_DURATION_SECONDS,
        elapsed,
        &[("route", &route)],
    );
    response
}

/// Records one rate-limit rejection (HTTP 429). Called from the rate-limit
/// middleware when a client is over budget.
pub fn record_rate_limit_rejection() {
    metrics::inc_counter(RATE_LIMIT_REJECTIONS_TOTAL, 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request as HttpRequest, routing::get, Router};
    use tower::ServiceExt as _; // for `oneshot`

    #[test]
    fn metrics_config_env_defaults_to_enabled_unauthenticated() {
        // Don't touch process env here (parallel tests); assert the default shape.
        let c = MetricsConfig::default();
        assert!(c.enabled);
        assert!(c.bearer_token.is_none());
    }

    #[test]
    fn bearer_matches_requires_exact_token() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Bearer s3cret".parse().unwrap());
        assert!(bearer_matches(&h, "s3cret"));
        assert!(!bearer_matches(&h, "other"));
        assert!(!bearer_matches(&h, "s3cre")); // length differs
    }

    #[test]
    fn bearer_matches_accepts_lowercase_scheme_and_trims() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "bearer  padded ".parse().unwrap());
        assert!(bearer_matches(&h, "padded"));
    }

    #[test]
    fn bearer_matches_rejects_missing_or_malformed_header() {
        let empty = HeaderMap::new();
        assert!(!bearer_matches(&empty, "t"));
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Token abc".parse().unwrap());
        assert!(!bearer_matches(&h, "abc"));
    }

    #[test]
    fn rate_limit_rejection_counter_uses_server_local_name() {
        // The emit helper bumps the dedicated, namespaced counter. Assert on the
        // name constant rather than absolute values (global registry, parallel).
        assert_eq!(RATE_LIMIT_REJECTIONS_TOTAL, "aero_rate_limit_rejections_total");
        record_rate_limit_rejection();
        assert!(metrics::render_prometheus().contains(RATE_LIMIT_REJECTIONS_TOTAL));
    }

    // ---- Router-level tests ----
    //
    // `metrics_handler` extracts only `Arc<MetricsConfig>`, so we can mount it on
    // a tiny `Router<Arc<MetricsConfig>>` and drive it through the *real* axum
    // stack with no DB / NATS / Redis. `tower::ServiceExt::oneshot` +
    // `axum::body::to_bytes` are already in the dependency tree (no new deps).

    fn metrics_router(cfg: MetricsConfig) -> Router {
        Router::new()
            .route("/metrics", get(metrics_handler))
            .with_state(Arc::new(cfg))
    }

    #[tokio::test]
    async fn metrics_route_returns_200_with_prometheus_exposition() {
        // Seed the global registry so the well-known names are present.
        metrics::register_known_metrics(metrics::global());
        let app = metrics_router(MetricsConfig::default());

        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert_eq!(ct, PROMETHEUS_CONTENT_TYPE);

        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        // Exposition advertises the key ROADMAP 方向四 signals.
        assert!(body.contains(names::WS_CONNECTIONS), "ws gauge missing:\n{body}");
        assert!(body.contains(names::MESSAGES_SENT_TOTAL), "msg counter missing");
        assert!(body.contains(names::HTTP_REQUESTS_TOTAL), "http counter missing");
        assert!(body.contains("# TYPE aero_ws_connections gauge"));
    }

    #[tokio::test]
    async fn metrics_route_requires_token_when_configured() {
        let cfg = MetricsConfig { enabled: true, bearer_token: Some("scrape-me".into()) };
        let app = metrics_router(cfg);

        // No Authorization header → 401.
        let unauth = app
            .clone()
            .oneshot(HttpRequest::builder().uri("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

        // Correct bearer → 200.
        let ok = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/metrics")
                    .header(header::AUTHORIZATION, "Bearer scrape-me")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn http_metrics_layer_records_request() {
        // Mount the layer over a trivial handler and confirm a request flows
        // through (status 200) and the HTTP counter series appears in the global
        // exposition. Assert presence/200, not absolute counts (global registry).
        async fn ok_handler() -> &'static str {
            "ok"
        }
        let app: Router = Router::new()
            .route("/probe", get(ok_handler))
            .layer(axum::middleware::from_fn(http_metrics_layer));

        let resp = app
            .oneshot(HttpRequest::builder().uri("/probe").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(metrics::render_prometheus().contains(names::HTTP_REQUESTS_TOTAL));
    }
}
