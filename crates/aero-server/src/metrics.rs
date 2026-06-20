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
use aero_common::WorkspaceId;
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

/// Counter (server-local name): participant-profile cache lookups, labeled by
/// `result` = `hit` | `miss`. A fresh cache hit avoids a `ParticipantRepo::get`
/// DB round-trip; a miss falls through to Postgres and backfills the cache
/// (ROADMAP6 方向四 多级缓存). Hit-ratio = hit / (hit + miss).
///
/// Server-local (not a cross-crate [`names`] constant) because the participant
/// cache is purely a gateway concern. Emitted from
/// [`crate::participant_cache::ParticipantCache`]'s call sites via
/// [`record_participant_cache`].
pub const PARTICIPANT_CACHE_LOOKUPS_TOTAL: &str = "aero_participant_cache_lookups_total";

/// Records one participant-cache lookup outcome. `hit` ⇒ served from the local
/// TTL cache (no DB); `false` ⇒ a miss that fell through to the repo.
pub fn record_participant_cache(hit: bool) {
    let result = if hit { "hit" } else { "miss" };
    metrics::inc_counter_labeled(PARTICIPANT_CACHE_LOOKUPS_TOTAL, 1, &[("result", result)]);
}

/// Gauge (server-local name): on-disk size in bytes of a tracked index, labeled
/// by `index` = the index relation name. The companion observability for the
/// mig-0136 partial-index slimming: it lets an operator watch the GIN
/// (`search_tsv`, `searchable_text` trigram), HNSW (`embedding`), and
/// `messages_room_created_idx` indexes grow over time and spot bloat before a
/// `REINDEX`/`VACUUM` is overdue.
///
/// Cardinality is **bounded**: the label only takes values from
/// [`tracked_indexes`] (a fixed, code-defined list plus an optional bounded set
/// from `AERO_METRICS_EXTRA_INDEXES`), never anything attacker-controlled.
///
/// Server-local (not a cross-crate [`names`] constant) because index-bloat
/// sampling is a gateway operational concern; emitted from the periodic sampler
/// in `bin/boot/metrics_tasks.rs` via [`sample_index_sizes`].
pub const INDEX_SIZE_BYTES: &str = "aero_index_size_bytes";

/// The default set of indexes the size gauge samples — the hot, bloat-prone
/// `messages` indexes touched by migration 0136 (partial GIN/HNSW slimming) plus
/// the room/created lookup index. Kept small and fixed so the `index` label
/// stays bounded.
const DEFAULT_TRACKED_INDEXES: &[&str] = &[
    "messages_search_tsv_gin",   // full-text GIN(search_tsv)
    "messages_searchable_trgm",  // trigram GIN(searchable_text)
    "messages_embedding_hnsw",   // vector HNSW(embedding)
    "messages_room_created_idx", // (room_id, created_at DESC)
];

/// Returns the bounded list of index names whose on-disk size the periodic
/// sampler reports as [`INDEX_SIZE_BYTES`].
///
/// This is [`DEFAULT_TRACKED_INDEXES`] plus, optionally, a small operator-defined
/// set from the comma-separated `AERO_METRICS_EXTRA_INDEXES` env var. The extra
/// set is **capped** (at 8) and de-duplicated so the gauge's `index` label can
/// never explode the series count, and each entry is trimmed/non-empty.
#[must_use]
pub fn tracked_indexes() -> Vec<String> {
    let mut out: Vec<String> = DEFAULT_TRACKED_INDEXES.iter().map(|s| (*s).to_owned()).collect();
    if let Ok(extra) = std::env::var("AERO_METRICS_EXTRA_INDEXES") {
        for name in extra.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if out.len() >= 12 {
                break; // hard cardinality cap (4 defaults + up to 8 extra)
            }
            let name = name.to_owned();
            if !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

/// Samples the on-disk size of each [`tracked_indexes`] entry and publishes it to
/// the [`INDEX_SIZE_BYTES`] gauge (label `index=<name>`).
///
/// Resolution uses `to_regclass(<name>)` so a missing index (e.g. a slimmed-down
/// deploy that never built the HNSW index) resolves to `NULL` and is simply
/// **skipped**, never erroring. Per-index query failures only `warn!` —
/// sampling is fail-open and must never crash the sampler loop. Returns the
/// number of indexes that were resolved and reported.
pub async fn sample_index_sizes(pool: &sqlx::PgPool) -> usize {
    let mut reported = 0usize;
    for index in tracked_indexes() {
        // `pg_relation_size(to_regclass($1))` → bigint, or NULL when the index
        // does not exist. `query_scalar::<_, Option<i64>>` yields:
        //   Ok(Some(Some(bytes)))  → index exists, report it
        //   Ok(Some(None)) / Ok(None) → index absent, skip
        //   Err(_)                 → query failed, warn and skip (fail-open)
        let row: Result<Option<i64>, _> = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT pg_relation_size(to_regclass($1))",
        )
        .bind(&index)
        .fetch_one(pool)
        .await;
        match row {
            Ok(Some(bytes)) => {
                #[allow(clippy::cast_precision_loss)]
                metrics::set_gauge_labeled(
                    INDEX_SIZE_BYTES,
                    bytes as f64,
                    &[("index", index.as_str())],
                );
                reported += 1;
            }
            Ok(None) => {
                tracing::debug!(%index, "index size sample skipped: index does not exist");
            }
            Err(e) => {
                tracing::warn!(error = %e, %index, "index size query failed");
            }
        }
    }
    reported
}

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

/// Response-extension marker carrying the tenant a request was attributed to, so
/// the [`http_metrics_layer`] can attach a per-tenant `workspace` label to the
/// HTTP RED signals **without** any of the architecturally-expensive
/// alternatives (a `workspace` JWT claim that would force re-issuing every token,
/// or a DB lookup on the request hot path).
///
/// ## How it works (the "response-extension pass-through" path)
///
/// A handler that has *already* cheaply resolved the owning workspace (e.g. it
/// parsed `workspace_id` off the request) stamps the response with this marker
/// via [`attach_workspace_label`]. The metrics layer, which sees every response,
/// reads it back with `response.extensions().get::<WorkspaceLabel>()`. Nothing in
/// the hot path changes for handlers that don't (or can't cheaply) know their
/// tenant — they simply emit the original, un-`workspace`-labeled series.
///
/// ## Cardinality / safety
///
/// The `workspace` label is added **only** when (a) the request carried this
/// marker and (b) [`per_tenant_http_metrics_enabled`] (the `AERO_PER_TENANT_METRICS`
/// opt-in, default OFF) is on. The label value is a [`WorkspaceId`] rendered as
/// its ULID — a bounded set (one series per active tenant), never anything
/// attacker-controlled. With the flag off, or no marker present, the series are
/// byte-for-byte the same as before this mechanism existed.
#[derive(Debug, Clone, Copy)]
pub struct WorkspaceLabel(pub WorkspaceId);

/// Stamp the response with the [`WorkspaceLabel`] for tenant `ws`, so the
/// [`http_metrics_layer`] can attribute this request's RED metrics to that
/// workspace. A no-op for metrics when `AERO_PER_TENANT_METRICS` is off (the
/// layer just ignores the marker), so handlers can call this unconditionally.
///
/// Returns the (now-stamped) response, for ergonomic use in a handler tail:
/// `Ok(attach_workspace_label(Json(v).into_response(), ws))`.
#[must_use]
pub fn attach_workspace_label(mut response: Response, ws: WorkspaceId) -> Response {
    response.extensions_mut().insert(WorkspaceLabel(ws));
    response
}

/// Whether per-tenant HTTP metrics are enabled, read ONCE from
/// `AERO_PER_TENANT_METRICS` and cached for the process lifetime.
///
/// OPT-IN (default OFF) because a `workspace` label multiplies the HTTP RED
/// series' cardinality by the active-tenant count — a deliberate operator
/// choice. Shares the **same** env switch as the message-throughput per-tenant
/// metric (`aero_im_core`), so one flag governs all per-tenant breakdowns.
/// Resolved at most once so the request hot path never re-reads env.
#[must_use]
pub fn per_tenant_http_metrics_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("AERO_PER_TENANT_METRICS")
            .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    })
}

/// Axum middleware recording HTTP RED metrics for every request.
///
/// Emits, on completion:
/// * [`names::HTTP_REQUESTS_TOTAL`] labeled `method` + `status`, and
/// * [`names::HTTP_REQUEST_DURATION_SECONDS`] (a histogram) labeled by the
///   matched `route` template.
///
/// When [`per_tenant_http_metrics_enabled`] is on AND the response carries a
/// [`WorkspaceLabel`] (stamped by a handler that cheaply knew its tenant), both
/// signals additionally carry a bounded `workspace` label. Otherwise the series
/// are emitted exactly as before — no marker, no extra dimension.
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

    // Per-tenant breakdown: only when the operator opted in AND a handler stamped
    // the response with the resolved tenant. Bounded by active-workspace count;
    // absent ⇒ the original, un-`workspace`-labeled series (no unbounded label).
    let workspace = if per_tenant_http_metrics_enabled() {
        response.extensions().get::<WorkspaceLabel>().map(|w| w.0.to_string())
    } else {
        None
    };

    if let Some(ws) = workspace.as_deref() {
        metrics::inc_counter_labeled(
            names::HTTP_REQUESTS_TOTAL,
            1,
            &[("method", method.as_str()), ("status", &status_str), ("workspace", ws)],
        );
        metrics::observe_histogram_labeled(
            names::HTTP_REQUEST_DURATION_SECONDS,
            elapsed,
            &[("route", &route), ("workspace", ws)],
        );
    } else {
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
    }
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
    fn tracked_indexes_includes_the_mig0136_message_indexes() {
        let idx = tracked_indexes();
        for expected in [
            "messages_search_tsv_gin",
            "messages_searchable_trgm",
            "messages_embedding_hnsw",
            "messages_room_created_idx",
        ] {
            assert!(idx.iter().any(|i| i == expected), "missing default index {expected}");
        }
        // Default list is exactly the four hot message indexes (no env extras here).
        assert_eq!(idx.len(), 4, "unexpected default tracked-index set: {idx:?}");
    }

    #[test]
    fn index_size_gauge_uses_server_local_name() {
        assert_eq!(INDEX_SIZE_BYTES, "aero_index_size_bytes");
        // The gauge is auto-created on first `set`; assert it renders with the
        // bounded `index` label and shows up in the global exposition.
        metrics::set_gauge_labeled(INDEX_SIZE_BYTES, 4096.0, &[("index", "messages_search_tsv_gin")]);
        let body = metrics::render_prometheus();
        assert!(body.contains(INDEX_SIZE_BYTES), "index gauge missing:\n{body}");
        assert!(body.contains("index=\"messages_search_tsv_gin\""), "index label missing");
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

    #[test]
    fn attach_workspace_label_stamps_a_bounded_marker() {
        // The marker carries the tenant id; the layer renders it as the ULID
        // (a bounded label value, not free-text).
        let ws = WorkspaceId(ulid::Ulid(0));
        let resp = attach_workspace_label("ok".into_response(), ws);
        let got = resp.extensions().get::<WorkspaceLabel>().expect("marker present");
        assert_eq!(got.0, ws);
        assert_eq!(got.0.to_string(), ws.to_string());
    }

    #[test]
    fn unstamped_response_carries_no_workspace_marker() {
        // A handler that didn't stamp the marker ⇒ no `workspace` dimension; the
        // layer falls back to the original series (this is the "no extension ⇒ no
        // label" / unbounded-safe branch).
        let resp = "ok".into_response();
        assert!(resp.extensions().get::<WorkspaceLabel>().is_none());
    }

    #[test]
    fn per_tenant_flag_reflects_env() {
        // The cached reader mirrors the same `AERO_PER_TENANT_METRICS` switch the
        // message-throughput metric uses. Default (unset in CI) is OFF; assert the
        // cached value matches whatever the env says at first read.
        let expected = std::env::var("AERO_PER_TENANT_METRICS")
            .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
        assert_eq!(per_tenant_http_metrics_enabled(), expected);
    }

    #[tokio::test]
    async fn http_metrics_layer_passes_stamped_response_through_unchanged() {
        // A handler stamps the marker; the layer must still return the response
        // intact (status + marker) regardless of the env flag's cached value.
        let ws = WorkspaceId(ulid::Ulid(7));
        async fn stamped() -> Response {
            super::attach_workspace_label("ok".into_response(), WorkspaceId(ulid::Ulid(7)))
        }
        let app: Router = Router::new()
            .route("/probe-ws", get(stamped))
            .layer(axum::middleware::from_fn(http_metrics_layer));
        let resp = app
            .oneshot(HttpRequest::builder().uri("/probe-ws").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // The marker survives the layer (it only reads it; never strips it).
        assert_eq!(resp.extensions().get::<WorkspaceLabel>().map(|w| w.0), Some(ws));
        assert!(metrics::render_prometheus().contains(names::HTTP_REQUESTS_TOTAL));
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
