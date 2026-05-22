//! Aero IM server binary.
//!
//! Wires together every crate, opens the HTTP/WS port, and runs the NATS listener.

use std::{net::SocketAddr, sync::Arc};

use aero_auth::AuthService;
use aero_bus::{EventBus, JetStreamBus, JetStreamConfig};
use aero_common::{config::AppConfig, telemetry};
use aero_im_core::ImService;
use aero_server::{hub::Hub, routes, state::AppState, ws};
use aero_storage::{
    connect_pg, migrate, MessageRepo, ParticipantRepo, PresenceStore, RedisCache, RoomRepo,
};
use anyhow::Context;
use tower_http::{cors::CorsLayer, services::ServeDir, trace::TraceLayer};
use tracing::info;

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
    let presence = PresenceStore::new(cache.client().clone());

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
    // PEM strings from TOML triple-quoted blocks can carry leading/trailing
    // whitespace which jsonwebtoken refuses; trim before handing off.
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
        jetstream.clone(),
    ));

    // ---------- Hub ----------
    let hub = Hub::new();

    // ---------- Compose state ----------
    let state = AppState {
        auth,
        im,
        participants,
        rooms,
        messages,
        presence,
        bus: bus_dyn,
        hub,
    };

    // ---------- Bus listener ----------
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            if let Err(e) = ws::run_bus_listener(state_clone).await {
                tracing::error!(error = ?e, "bus listener exited");
            }
        });
    }

    // ---------- Router ----------
    let app = routes::build(state.clone())
        .fallback_service(ServeDir::new(&cfg.server.web_dir))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive());

    let addr: SocketAddr = format!("{}:{}", cfg.server.host, cfg.server.port).parse()?;
    info!(%addr, "listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
