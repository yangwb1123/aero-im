//! Persistence layer: PG, Redis, NATS, blob, bus.
use std::sync::Arc;

use aero_bus::{EventBus, JetStreamBus, JetStreamConfig};
use aero_common::metrics as common_metrics;
use aero_storage::RedisCache;
use anyhow::Context;
use tracing::{info, warn};

use crate::boot::connect_with_retry;

/// All persistence connections established at boot.
pub(crate) struct Persistence {
    pub(crate) pg: sqlx::PgPool,
    /// Explicit primary/optional-replica routing seam.
    pub(crate) query_router: aero_storage::QueryRouter,
    pub(crate) cache: RedisCache,
    pub(crate) jetstream: Arc<JetStreamBus>,
    pub(crate) bus_dyn: Arc<dyn EventBus>,
    pub(crate) blob_store: Arc<dyn aero_storage::BlobStore>,
    pub(crate) blob_backend: String,
}

pub(crate) async fn connect(
    cfg: &aero_common::config::AppConfig,
    connect_attempts: u32,
) -> anyhow::Result<Persistence> {
    // ---------- PG ----------
    let pg = connect_with_retry("postgres", connect_attempts, || {
        aero_storage::connect_pg(&cfg.database.url, cfg.database.max_connections)
    })
    .await?;
    aero_storage::migrate(&pg).await.context("run migrations")?;
    info!("postgres migrated");

    // Optional read replica (ROADMAP 方向四). QueryRouter exposes it only to
    // call sites that explicitly request eventual consistency; when absent,
    // those calls naturally select primary. Migrations run on PRIMARY only —
    // the replica is read-only and receives the schema via replication.
    let replica = match cfg
        .database
        .replica_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        Some(url) => match connect_with_retry("postgres-replica", connect_attempts, || {
            aero_storage::connect_pg(url, cfg.database.max_connections)
        })
        .await
        {
            Ok(replica) => {
                info!("postgres read replica connected");
                Some(replica)
            }
            Err(error) => {
                // The replica is an optional capacity aid, never a serving
                // dependency. Strong reads remain available on primary and
                // eventual call sites naturally fall back through QueryRouter.
                warn!(
                    ?error,
                    "postgres read replica unavailable at boot; using primary"
                );
                None
            }
        },
        None => None,
    };
    let query_router = aero_storage::QueryRouter::new(pg.clone(), replica);

    // ---------- Redis ----------
    let cache = connect_with_retry("redis", connect_attempts, || {
        RedisCache::connect(&cfg.redis.url)
    })
    .await?;
    info!("redis connected");

    // ---------- Blob storage ----------
    let blob_root = std::path::PathBuf::from(&cfg.server.blob_dir);
    let (blob_store, blob_backend) = aero_storage::blob_store_from_env_checked(blob_root.clone())
        .context("configure blob store")?;
    info!(blob_dir = %blob_root.display(), backend = %blob_backend, "blob store ready");

    // ---------- NATS JetStream ----------
    let jetstream: Arc<JetStreamBus> = Arc::new(
        connect_with_retry("nats", connect_attempts, || {
            JetStreamBus::connect(JetStreamConfig {
                url: cfg.nats.url.clone(),
                bootstrap_streams: true,
            })
        })
        .await?,
    );
    let bus_dyn: Arc<dyn EventBus> = jetstream.clone();
    info!("nats connected");

    Ok(Persistence {
        pg,
        query_router,
        cache,
        jetstream,
        bus_dyn,
        blob_store,
        blob_backend: blob_backend.to_string(),
    })
}
