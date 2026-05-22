//! Aero IM server binary.
//!
//! Wires together every crate, opens the HTTP/WS port, starts the NATS listener,
//! and (when enabled) launches the AI worker + RTMP ingest in the background.

use std::{net::SocketAddr, sync::Arc};

use aero_ai::{AiService, AiWorker};
use aero_auth::AuthService;
use aero_bus::{EventBus, JetStreamBus, JetStreamConfig};
use aero_common::{config::AppConfig, telemetry};
use aero_im_core::ImService;
use aero_live_core::LiveStreamConfig;
use aero_live_rtmp::spawn_rtmp_ingest;
use aero_live_whip::WhipRegistry;
use aero_server::{ai_adapter::AiServiceAdapter, hub::Hub, routes, state::AppState, ws};
use aero_storage::{
    connect_pg, migrate, AiJobRepo, BlobRepo, CallRepo, KeyPackageRepo, LocalFsBlobStore,
    MessageRepo, MlsGroupRepo, ParticipantRepo, PresenceStore, ReactionRepo, ReceiptRepo,
    RedisCache, RoomRepo, StreamRepo,
};
use tokio_util::sync::CancellationToken;
use anyhow::Context;
use tower_http::{cors::CorsLayer, services::ServeDir, trace::TraceLayer};
use tracing::{info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = AppConfig::load().context("load config")?;
    let _guard = telemetry::init(&cfg.telemetry, "aero-server");

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
    let messages = MessageRepo::new(pg.clone());
    let receipts = ReceiptRepo::new(pg.clone());
    let reactions = ReactionRepo::new(pg.clone());
    let calls = CallRepo::new(pg.clone());
    let ai_jobs = AiJobRepo::new(pg.clone());
    let blobs = BlobRepo::new(pg.clone());
    let streams = StreamRepo::new(pg.clone());
    let key_packages = KeyPackageRepo::new(pg.clone());
    let mls_groups = MlsGroupRepo::new(pg.clone());
    let presence = PresenceStore::new(cache.client().clone());

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
    let im = Arc::new(ImService::new(
        rooms.clone(),
        messages.clone(),
        participants.clone(),
        receipts.clone(),
        reactions.clone(),
        calls.clone(),
        ai_jobs.clone(),
        jetstream.clone(),
    ));

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

    // ---------- Hub ----------
    let hub = Hub::new();

    // ---------- Compose state ----------
    let public_base_url = std::env::var("AERO_PUBLIC_BASE_URL")
        .unwrap_or_else(|_| format!("http://{}:{}", cfg.server.host, cfg.server.port));
    let state = AppState {
        auth,
        im,
        participants,
        rooms,
        messages,
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
        bus: bus_dyn,
        hub,
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
    let _ = ai_shutdown; // keep token alive for the worker

    // ---------- Bus listener ----------
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            if let Err(e) = ws::run_bus_listener(state_clone).await {
                tracing::error!(error = ?e, "bus listener exited");
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

    // ---------- Router ----------
    let hls_dir = std::path::PathBuf::from(&cfg.server.hls_dir);
    if let Err(e) = std::fs::create_dir_all(&hls_dir) {
        warn!(error = %e, dir = %hls_dir.display(), "create hls dir failed");
    }
    let app = routes::build(state.clone())
        .nest_service("/hls", ServeDir::new(&hls_dir))
        .fallback_service(ServeDir::new(&cfg.server.web_dir))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive());

    let addr: SocketAddr = format!("{}:{}", cfg.server.host, cfg.server.port).parse()?;
    info!(%addr, "listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

