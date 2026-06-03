//! Aero IM server binary.
//!
//! Wires together every crate, opens the HTTP/WS port, starts the NATS listener,
//! and (when enabled) launches the AI worker + RTMP ingest in the background.

use std::{net::SocketAddr, sync::Arc};

use aero_ai::{AiService, AiWorker};
use aero_auth::AuthService;
use aero_bus::{EventBus, JetStreamBus, JetStreamConfig};
use aero_common::{config::AppConfig, metrics as common_metrics, telemetry};
use aero_im_core::ImService;
use aero_live_core::LiveStreamConfig;
use aero_live_rtmp::spawn_rtmp_ingest;
use aero_live_whip::WhipRegistry;
use aero_server::{
    ai_adapter::AiServiceAdapter,
    config::{GatewayConfig, WsConfig},
    hub::Hub,
    live::LiveService,
    metrics::{self as server_metrics, MetricsConfig},
    rate_limit::{self, RateLimiter},
    routes,
    state::AppState,
    ws,
};
use aero_storage::{
    connect_pg, migrate, AiJobRepo, AuditRepo, BlobRepo, CallRepo, CallRosterStore, KeyPackageRepo,
    LiveRepo, LocalFsBlobStore, MessageRepo, MlsGroupRepo, NotificationRepo, ParticipantRepo,
    PinRepo, PresenceStore, ReactionRepo, ReceiptRepo, RedisCache, RoomRepo, StreamRepo,
    StreamRouteRegistry, StreamViewerStore, WorkspaceRepo,
};
use tokio_util::sync::CancellationToken;
use anyhow::Context;
use axum::extract::DefaultBodyLimit;
use tower::ServiceBuilder;
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    services::ServeDir,
    timeout::TimeoutLayer,
    trace::TraceLayer,
};
use tracing::{info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = AppConfig::load().context("load config")?;
    let _guard = telemetry::init(&cfg.telemetry, "aero-server");

    // Seed the process-global metrics registry with HELP/TYPE for the well-known
    // ROADMAP 方向四 signals so `/metrics` advertises them even before first use.
    common_metrics::register_known_metrics(common_metrics::global());

    info!(version = env!("CARGO_PKG_VERSION"), "starting aero-server");

    // ---------- Persistence ----------
    let pg = connect_pg(&cfg.database.url, cfg.database.max_connections)
        .await
        .context("connect postgres")?;
    migrate(&pg).await.context("run migrations")?;
    info!("postgres migrated");

    let cache = RedisCache::connect(&cfg.redis.url).await.context("redis connect")?;
    info!("redis connected");

    // ---------- Repos ----------
    let participants = ParticipantRepo::new(pg.clone());
    let rooms = RoomRepo::new(pg.clone());
    let workspaces = WorkspaceRepo::new(pg.clone());
    let audit = AuditRepo::new(pg.clone());
    let messages = MessageRepo::new(pg.clone());
    let notifications = NotificationRepo::new(pg.clone());
    let pins = PinRepo::new(pg.clone());
    let receipts = ReceiptRepo::new(pg.clone());
    let reactions = ReactionRepo::new(pg.clone());
    let calls = CallRepo::new(pg.clone());
    let ai_jobs = AiJobRepo::new(pg.clone());
    let blobs = BlobRepo::new(pg.clone());
    let streams = StreamRepo::new(pg.clone());
    let live_repo = LiveRepo::new(pg.clone());
    let key_packages = KeyPackageRepo::new(pg.clone());
    let mls_groups = MlsGroupRepo::new(pg.clone());
    let presence = PresenceStore::new(cache.client().clone());
    // Cross-node live presence (ROADMAP 方向二/五): viewer counts + call rosters
    // live in Redis (sorted-set-with-heartbeat) so multi-node audiences/calls are
    // counted once cluster-wide, sharing the same Redis client as `PresenceStore`.
    let stream_viewers = StreamViewerStore::new(cache.client().clone());
    let call_roster = CallRosterStore::new(cache.client().clone());
    let stream_routes = StreamRouteRegistry::new(cache.client().clone());

    // ---------- Blob storage ----------
    let blob_root = std::path::PathBuf::from(&cfg.server.blob_dir);
    let blob_store = Arc::new(
        LocalFsBlobStore::new(&blob_root).context("create blob dir")?,
    );
    info!(blob_dir = %blob_root.display(), "blob store ready");

    // ---------- Bus ----------
    let jetstream: Arc<JetStreamBus> = Arc::new(
        JetStreamBus::connect(JetStreamConfig {
            url: cfg.nats.url.clone(),
            bootstrap_streams: true,
        })
        .await
        .map_err(|e| anyhow::anyhow!("nats: {e}"))?,
    );
    let bus_dyn: Arc<dyn EventBus> = jetstream.clone();
    info!("nats connected");

    // ---------- Auth ----------
    let priv_pem = cfg.auth.jwt_private_key_pem.trim();
    let pub_pem = cfg.auth.jwt_public_key_pem.trim();
    let auth = AuthService::from_pem(
        participants.clone(),
        priv_pem,
        pub_pem,
        cfg.auth.issuer.clone(),
        std::time::Duration::from_secs(cfg.auth.access_ttl_secs),
        std::time::Duration::from_secs(cfg.auth.refresh_ttl_secs),
    )
    .context("init AuthService")?;

    // ---------- IM service ----------
    // `with_workspaces` wires the tenant repo so the workspace-scoped methods
    // (`create_room_in_workspace`, `assert_room_access`) have their backing store;
    // without it they return an internal error instead of enforcing tenancy.
    let im = Arc::new(
        ImService::new(
            rooms.clone(),
            messages.clone(),
            participants.clone(),
            receipts.clone(),
            reactions.clone(),
            calls.clone(),
            ai_jobs.clone(),
            jetstream.clone(),
        )
        .with_workspaces(workspaces.clone())
        .with_notifications(notifications.clone())
        .with_pins(pins.clone()),
    );

    // ---------- Live service (danmaku / gifts / viewers) ----------
    let live = LiveService::new(
        streams.clone(),
        live_repo,
        participants.clone(),
        bus_dyn.clone(),
    );

    // ---------- AI service ----------
    // The AiService is always constructed — it falls back to a deterministic local
    // embedder + heuristic summary when ANTHROPIC_API_KEY / VOYAGE_API_KEY are absent,
    // so the routes never need to gate on env at request time.
    let ai_service = Arc::new(AiService::from_env(
        ai_jobs.clone(),
        messages.clone(),
        rooms.clone(),
    ));
    info!("AI service constructed");

    // Spawn the embed/summarize/answer worker.
    let ai_shutdown = CancellationToken::new();
    {
        let svc = ai_service.clone();
        let cancel = ai_shutdown.clone();
        tokio::spawn(async move {
            AiWorker::new(svc).run(cancel).await;
        });
    }

    // ---------- Live ingest (RTMP -> HLS placeholder) ----------
    let live_cfg = Arc::new(LiveStreamConfig {
        hls_dir: std::path::PathBuf::from(&cfg.server.hls_dir),
        rtmp_listen: cfg
            .server
            .rtmp_listen
            .parse()
            .unwrap_or_else(|_| "0.0.0.0:1935".parse().expect("default rtmp addr")),
    });
    {
        let repo = streams.clone();
        let live_cfg = live_cfg.clone();
        tokio::spawn(async move {
            let handle = spawn_rtmp_ingest(repo, live_cfg);
            if let Err(e) = handle.await {
                tracing::warn!(error = ?e, "rtmp ingest task ended");
            }
        });
    }
    info!(addr = %live_cfg.rtmp_listen, "rtmp ingest listening");

    // ---------- Gateway hardening config ----------
    let gateway_cfg = GatewayConfig::from_env();
    let ws_cfg = WsConfig::from_env();
    let metrics_cfg = MetricsConfig::from_env();
    info!(
        timeout_secs = gateway_cfg.request_timeout.as_secs(),
        max_body_bytes = gateway_cfg.max_body_bytes,
        max_concurrency = gateway_cfg.max_concurrency,
        cors_origins = gateway_cfg.cors_allowed_origins.len(),
        rl_per_sec = gateway_cfg.rate_limit.per_second,
        rl_burst = gateway_cfg.rate_limit.burst,
        ws_send_queue = ws_cfg.send_queue_capacity,
        metrics_enabled = metrics_cfg.enabled,
        metrics_auth = metrics_cfg.bearer_token.is_some(),
        "gateway hardening configured"
    );

    // ---------- Hub ----------
    let hub = Hub::with_ws_config(ws_cfg);

    // ---------- Compose state ----------
    let public_base_url = std::env::var("AERO_PUBLIC_BASE_URL")
        .unwrap_or_else(|_| format!("http://{}:{}", cfg.server.host, cfg.server.port));
    let state = AppState {
        auth,
        im,
        live,
        participants,
        rooms,
        workspaces,
        audit,
        messages,
        notifications,
        pins,
        receipts,
        reactions,
        calls,
        ai_jobs,
        blobs,
        blob_store,
        streams,
        key_packages,
        mls_groups,
        presence,
        stream_viewers,
        call_roster,
        stream_routes,
        bus: bus_dyn,
        hub,
        ws_config: ws_cfg,
        rate_limiter: RateLimiter::new(gateway_cfg.rate_limit),
        metrics: Arc::new(metrics_cfg.clone()),
        ai: Some(Arc::new(AiServiceAdapter::new(ai_service.clone()))),
        public_base_url,
        whip: WhipRegistry::new(),
        ingest_host: std::env::var("AERO_INGEST_HOST")
            .unwrap_or_else(|_| cfg.server.host.clone()),
        ingest_port: std::env::var("AERO_INGEST_UDP_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(40000),
    };
    // ---------- Message-retention sweep (ROADMAP 方向一 合规) ----------
    // Periodically soft-delete messages whose workspace set a retention window
    // and whose age exceeds it (per-workspace cutoff evaluated in one set-based
    // UPDATE). Best-effort: errors are logged, never panic the server; the task
    // exits on the shutdown token. Interval is configurable via
    // `AERO__SERVER__RETENTION_SWEEP_SECS` (default 3600s = hourly); a value of 0
    // disables the sweep entirely.
    {
        let workspaces = state.workspaces.clone();
        let cancel = ai_shutdown.clone();
        let sweep_secs = std::env::var("AERO__SERVER__RETENTION_SWEEP_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(3600);
        if sweep_secs == 0 {
            info!("retention sweep disabled (AERO__SERVER__RETENTION_SWEEP_SECS=0)");
        } else {
            info!(interval_secs = sweep_secs, "retention sweep enabled");
            tokio::spawn(async move {
                let mut tick =
                    tokio::time::interval(std::time::Duration::from_secs(sweep_secs));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                // First tick fires immediately; skip it so startup isn't a sweep,
                // then sweep on each subsequent interval until shutdown.
                tick.tick().await;
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => {
                            info!("retention sweep: shutdown signal received, exiting");
                            break;
                        }
                        _ = tick.tick() => {
                            let now = time::OffsetDateTime::now_utc();
                            // None = sweep every policied workspace (the global periodic sweep).
                            match workspaces.sweep_expired_messages(now, None).await {
                                Ok(0) => {}
                                Ok(n) => info!(swept = n, "retention sweep soft-deleted messages"),
                                Err(e) => warn!(error = ?e, "retention sweep failed"),
                            }
                        }
                    }
                }
            });
        }
    }

    let _ = ai_shutdown; // keep token alive for the worker

    // ---------- DB pool saturation gauges (ROADMAP 方向四) ----------
    // Periodically publish sqlx pool stats so dashboards can alert on pool
    // exhaustion (in-use approaching size = requests will start queueing).
    {
        let pool = pg.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let size = pool.size();
                // `num_idle()` is usize; pool sizes are tiny, so a saturating
                // narrowing to u32 is exact in practice and lint-clean.
                let idle = u32::try_from(pool.num_idle()).unwrap_or(u32::MAX);
                let in_use = size.saturating_sub(idle);
                let max = pool.options().get_max_connections();
                common_metrics::set_gauge(common_metrics::names::DB_POOL_SIZE, f64::from(max));
                // In-use = total open connections minus those sitting idle.
                common_metrics::set_gauge(common_metrics::names::DB_POOL_IN_USE, f64::from(in_use));
            }
        });
    }

    // ---------- Bus listener ----------
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            if let Err(e) = ws::run_bus_listener(state_clone).await {
                tracing::error!(error = ?e, "bus listener exited");
            }
        });
    }

    // ---------- Live bus listener (danmaku / gifts / viewers) ----------
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            if let Err(e) = ws::run_live_bus_listener(state_clone).await {
                tracing::error!(error = ?e, "live bus listener exited");
            }
        });
    }

    // ---------- Agent bot dispatcher ----------
    {
        let state_clone = state.clone();
        let ai = ai_service.clone();
        tokio::spawn(async move {
            if let Err(e) = aero_server::agent_bot::run(state_clone, ai).await {
                tracing::error!(error = ?e, "agent_bot listener exited");
            }
        });
    }

    // ---------- Voice transcript dispatcher ----------
    {
        let state_clone = state.clone();
        let ai = ai_service.clone();
        tokio::spawn(async move {
            if let Err(e) = aero_server::transcribe_bot::run(state_clone, ai).await {
                tracing::error!(error = ?e, "transcribe_bot listener exited");
            }
        });
    }

    // ---------- AI content moderation (opt-in: AERO_AI_MODERATION) ----------
    if std::env::var("AERO_AI_MODERATION").is_ok() {
        let state_clone = state.clone();
        tokio::spawn(async move {
            if let Err(e) = aero_server::moderation_bot::run(state_clone).await {
                tracing::error!(error = ?e, "moderation_bot listener exited");
            }
        });
        info!("AI moderation enabled");
    }

    // ---------- Router ----------
    let hls_dir = std::path::PathBuf::from(&cfg.server.hls_dir);
    if let Err(e) = std::fs::create_dir_all(&hls_dir) {
        warn!(error = %e, dir = %hls_dir.display(), "create hls dir failed");
    }

    // CORS: tighten to a configured allow-list in production; fall back to the
    // permissive dev default only when no origins are configured.
    let cors = build_cors(&gateway_cfg);

    // Global protective middleware stack (outermost → innermost):
    //   trace → http-metrics → cors → concurrency-limit → rate-limit → timeout
    //   → body-limit.
    // Concurrency + timeout are optional (0 disables). Rate limiting wires the
    // dormant `Error::RateLimited` (429). Tracing stays outermost so even
    // rejected requests are observed; HTTP metrics sit just inside it so the RED
    // counters/histogram capture *every* response — including rate-limit 429s,
    // concurrency rejections, and timeouts produced by the inner layers.
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

    let app = routes::build(state.clone())
        .nest_service("/hls", ServeDir::new(&hls_dir))
        .fallback_service(ServeDir::new(&cfg.server.web_dir))
        .layer(middleware);

    let addr: SocketAddr = format!("{}:{}", cfg.server.host, cfg.server.port).parse()?;
    info!(%addr, "listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    // `into_make_service_with_connect_info` exposes the peer address to the
    // rate-limit middleware (for IP keying of unauthenticated requests).
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// Build the CORS layer from config: an explicit origin allow-list when one is
/// configured, otherwise the permissive dev default.
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

