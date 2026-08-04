//! Axum router + HTTP server + graceful shutdown.
use anyhow::Context;
use axum::extract::DefaultBodyLimit;
use std::net::SocketAddr;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tower::ServiceBuilder;
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    services::ServeDir,
    set_header::SetResponseHeaderLayer,
    timeout::TimeoutLayer,
    trace::{MakeSpan, TraceLayer},
};
use tracing::{info, warn, Span};

use aero_server::config::GatewayConfig;
use aero_server::metrics as server_metrics;
use aero_server::rate_limit;
use aero_server::state::AppState;

#[derive(Clone, Copy)]
struct SanitizedHttpMakeSpan;

fn trace_request_path<B>(request: &axum::http::Request<B>) -> &str {
    request.uri().path()
}

impl<B> MakeSpan<B> for SanitizedHttpMakeSpan {
    fn make_span(&mut self, request: &axum::http::Request<B>) -> Span {
        tracing::info_span!(
            "request",
            method = %request.method(),
            path = %trace_request_path(request),
            version = ?request.version(),
        )
    }
}

pub(crate) async fn serve(
    state: AppState,
    cfg: aero_common::config::AppConfig,
    gateway_cfg: GatewayConfig,
    ai_shutdown: CancellationToken,
    hls_dir: std::path::PathBuf,
    tracker: TaskTracker,
) -> anyhow::Result<()> {
    // CORS fail-closed gate
    if std::env::var("AERO_CORS_REQUIRE_ORIGINS")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        && gateway_cfg.cors_allowed_origins.is_empty()
    {
        anyhow::bail!(
            "AERO_CORS_REQUIRE_ORIGINS is set but no AERO_CORS_ALLOWED_ORIGINS configured — \
             refusing to start with a permissive any-origin CORS policy"
        );
    }
    let cors = build_cors(&gateway_cfg);

    let timeout_layer = (!gateway_cfg.request_timeout.is_zero()).then(|| {
        TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            gateway_cfg.request_timeout,
        )
    });
    let concurrency_layer = (gateway_cfg.max_concurrency > 0)
        .then(|| tower::limit::ConcurrencyLimitLayer::new(gateway_cfg.max_concurrency));
    let middleware = ServiceBuilder::new()
        // Keep tower-http's response timing/classification callbacks, but never
        // attach the request query (notably `/ws?token=...`) to the request span.
        .layer(TraceLayer::new_for_http().make_span_with(SanitizedHttpMakeSpan))
        .layer(axum::middleware::from_fn(
            server_metrics::http_metrics_layer,
        ))
        .layer(cors)
        // Security response headers (ROADMAP 第四次分析·方向三):
        //   X-Frame-Options DENY           → prevent clickjacking
        //   X-Content-Type-Options nosniff  → prevent MIME-type sniffing
        //   Strict-Transport-Security       → force HTTPS (production)
        //   Referrer-Policy                 → reasonable privacy default
        //   Permissions-Policy              → restrict sensitive browser APIs
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::X_FRAME_OPTIONS,
            axum::http::HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::X_CONTENT_TYPE_OPTIONS,
            axum::http::HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::STRICT_TRANSPORT_SECURITY,
            axum::http::HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        ))
        // Preserve stricter route-specific policies (the OIDC callback uses
        // `no-referrer` so its single-use authorization code never appears in a
        // subsequent same-origin request's Referer header).
        .layer(SetResponseHeaderLayer::if_not_present(
            axum::http::header::REFERRER_POLICY,
            axum::http::HeaderValue::from_static("strict-origin-when-cross-origin"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::HeaderName::from_static("permissions-policy"),
            // The bundled same-origin web client uses camera/microphone for calls
            // and voice messages. Keep those capabilities confined to this
            // origin while continuing to deny geolocation entirely.
            axum::http::HeaderValue::from_static(
                "camera=(self), microphone=(self), geolocation=()",
            ),
        ))
        // Content-Security-Policy (ROADMAP 第四次分析·方向三): opt-in via
        // `AERO_CSP_POLICY` env var. Not set by default because the correct
        // policy depends on the deployment's CDN dependencies. Example for the
        // bundled web app (which loads hls.js from jsdelivr):
        //   default-src 'self'; script-src 'self' https://cdn.jsdelivr.net;
        //   style-src 'self' 'unsafe-inline';
        //   img-src 'self' data: https://giphy.com https://*.giphy.com;
        //   connect-src 'self' ws: wss:
        .option_layer(std::env::var("AERO_CSP_POLICY").ok().map(|policy| {
            SetResponseHeaderLayer::overriding(
                axum::http::header::CONTENT_SECURITY_POLICY,
                axum::http::HeaderValue::from_str(&policy)
                    .expect("AERO_CSP_POLICY is not a valid header value"),
            )
        }))
        .option_layer(concurrency_layer)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            rate_limit::layer,
        ))
        // IP-allowlist enforcement: 403 a client whose IP is outside a workspace's
        // configured allowlist on its `/api/workspaces/:id/*` routes (after the
        // rate-limit layer, so a flood is throttled before it hits the cidrs query).
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            aero_server::ip_allowlist::enforce_layer,
        ))
        .option_layer(timeout_layer)
        .layer(DefaultBodyLimit::max(gateway_cfg.max_body_bytes));

    let app = aero_server::routes::build(state.clone())
        .nest_service("/hls", ServeDir::new(&hls_dir))
        .fallback_service(ServeDir::new(&cfg.server.web_dir))
        .layer(middleware);

    // Rate-limiter idle-bucket sweep
    {
        let sweep_state = state.clone();
        let cancel = ai_shutdown.clone();
        let sweep_secs = std::env::var("AERO_RATE_LIMIT_SWEEP_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(60)
            .max(1);
        let idle_secs = std::env::var("AERO_RATE_LIMIT_IDLE_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(600);
        tracker.spawn(async move {
            let idle = std::time::Duration::from_secs(idle_secs);
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(sweep_secs));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {
                        let now = std::time::Instant::now();
                        let evicted = sweep_state.rate_limiter.sweep_idle(now, idle)
                            + sweep_state.auth_rate_limiter.sweep_idle(now, idle)
                            + sweep_state.login_rate_limiter.sweep_idle(now, idle)
                            + sweep_state.forgot_rate_limiter.sweep_idle(now, idle);
                        if evicted > 0 {
                            tracing::debug!(evicted, "rate-limiter idle-bucket sweep");
                        }
                        // Spam-guard per-sender accounting map (in-process backend):
                        // same unbounded-growth risk as the rate-limit buckets, so
                        // evict idle senders on the same cadence. No-op for the Redis
                        // backend (self-expiring) or when the guard is disabled.
                        let spam_evicted = sweep_state.im.sweep_spam_guard(now);
                        if spam_evicted > 0 {
                            tracing::debug!(evicted = spam_evicted, "spam-guard idle-sender sweep");
                        }
                        // Slow-mode last-post map (process-static, NOT in AppState, so
                        // it is unreachable from the limiter sweeps above). Evict only
                        // entries older than the max slow-mode window — younger ones
                        // can still gate a post, so MIN_SAFE_SWEEP_IDLE (not the short
                        // limiter `idle`) is the floor.
                        let slowmode_evicted = aero_server::stream_chat_modes::sweep_idle(
                            now,
                            aero_server::stream_chat_modes::MIN_SAFE_SWEEP_IDLE,
                        );
                        if slowmode_evicted > 0 {
                            tracing::debug!(evicted = slowmode_evicted, "slow-mode last-post sweep");
                        }
                        // In-process login-throttle failure map (no-op when the
                        // throttle is off/Redis-backed). Lives in AuthService, not a
                        // limiter, and self-determines staleness from its own window/
                        // lockout config, so it takes a unix-seconds `now`.
                        let now_unix = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        let throttle_evicted = sweep_state.auth.sweep_login_throttle(now_unix).await;
                        if throttle_evicted > 0 {
                            tracing::debug!(evicted = throttle_evicted, "login-throttle idle sweep");
                        }
                    }
                }
            }
        });
    }

    let addr: SocketAddr = format!("{}:{}", cfg.server.host, cfg.server.port).parse()?;
    info!(%addr, "listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(super::shutdown::shutdown_signal(
        state.shutting_down.clone(),
        Duration::from_secs(
            std::env::var("AERO_SHUTDOWN_DRAIN_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(5),
        ),
        ai_shutdown.clone(),
    ))
    .await?;

    // Drain background tasks
    tracker.close();
    let drain = Duration::from_secs(
        std::env::var("AERO_TASK_DRAIN_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(10),
    );
    if tokio::time::timeout(drain, tracker.wait()).await.is_err() {
        tracing::warn!(
            drain_secs = drain.as_secs(),
            "background task drain timed out; abandoning still-running tasks"
        );
    } else {
        tracing::info!("background tasks drained cleanly");
    }
    Ok(())
}

fn build_cors(cfg: &GatewayConfig) -> CorsLayer {
    if cfg.cors_allowed_origins.is_empty() {
        warn!("CORS permissive (dev default) — set AERO_CORS_ALLOWED_ORIGINS for production");
        return CorsLayer::permissive();
    }
    let origins: Vec<axum::http::HeaderValue> = cfg
        .cors_allowed_origins
        .iter()
        .filter_map(|o| o.parse().ok())
        .collect();
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods(tower_http::cors::Any)
        .allow_headers(tower_http::cors::Any)
}

#[cfg(test)]
mod tests {
    use super::trace_request_path;

    #[test]
    fn http_trace_path_excludes_query_and_bearer() {
        let request = axum::http::Request::builder()
            .uri("/ws?token=super-secret-bearer&room=abc")
            .body(())
            .expect("valid test request");

        let path = trace_request_path(&request);
        assert_eq!(path, "/ws");
        assert!(!path.contains("token"));
        assert!(!path.contains("super-secret-bearer"));
        assert!(!path.contains("room=abc"));
    }
}
