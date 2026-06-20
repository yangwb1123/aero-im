//! Axum router + HTTP server + graceful shutdown.
use std::net::SocketAddr;
use std::time::Duration;
use anyhow::Context;
use axum::extract::DefaultBodyLimit;
use tower::ServiceBuilder;
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    services::ServeDir,
    timeout::TimeoutLayer,
    trace::TraceLayer,
};
use tokio_util::task::TaskTracker;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use aero_server::config::GatewayConfig;
use aero_server::metrics as server_metrics;
use aero_server::rate_limit;
use aero_server::state::AppState;

pub(crate) async fn serve(
    state: AppState,
    cfg: aero_common::config::AppConfig,
    gateway_cfg: GatewayConfig,
    ai_shutdown: CancellationToken,
    hls_dir: std::path::PathBuf,
    tracker: TaskTracker,
) -> anyhow::Result<()> {
    // CORS fail-closed gate
    if std::env::var("AERO_CORS_REQUIRE_ORIGINS").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
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
        .layer(TraceLayer::new_for_http())
        .layer(axum::middleware::from_fn(server_metrics::http_metrics_layer))
        .layer(cors)
        .option_layer(concurrency_layer)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            rate_limit::layer,
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
