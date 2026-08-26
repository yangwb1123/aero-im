//! Aero IM server binary — thin entry point.
//!
//! All boot logic is delegated to `boot/` sub-modules:
//! - `boot/persistence.rs` — PG, Redis, NATS, blob, bus
//! - `boot/repos.rs` — repository/store constructors
//! - `boot/services.rs` — Auth, IM, Live, AI
//! - `boot/ingest.rs` — RTMP + SRT
//! - `boot/orchestration.rs` — call orchestrator + bridge supervisor
//! - `boot/state_builder.rs` — `AppState` composition
//! - `boot/background.rs` — bots, dispatchers, webhooks, GC
//! - `boot/metrics_tasks.rs` — gauge samplers + heartbeats
//! - `boot/retention.rs` — retention sweeps
//! - `boot/serve.rs` — HTTP server + shutdown

mod boot;

use aero_ai::AiWorker;
use aero_audit_connector::heartbeat::HeartbeatRecorder as _;
use aero_common::config::AppConfig;
use aero_common::metrics as common_metrics;
use aero_common::telemetry;
use aero_server::config::{GatewayConfig, WsConfig};
use aero_server::hub::Hub;
use anyhow::Context;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = AppConfig::load().context("load config")?;
    let _guard = telemetry::init(&cfg.telemetry, "aero-server");
    // Keep the optional metrics provider alive for the full process lifetime.
    // This is deliberately independent of trace export: Prometheus remains
    // available, while AERO_OTLP_METRICS opt-in mirrors the same registry
    // updates to an OTLP collector.
    let _metrics_guard = common_metrics::otlp::install_from_env(
        "aero-server",
        cfg.telemetry.otlp_endpoint.as_deref(),
    );
    common_metrics::register_known_metrics(common_metrics::global());
    info!(version = env!("CARGO_PKG_VERSION"), "starting aero-server");

    let connect_attempts: u32 = std::env::var("AERO_STARTUP_CONNECT_ATTEMPTS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);

    // ---------- Persistence ----------
    let persistence = boot::connect_persistence(&cfg, connect_attempts).await?;
    let snaplink_commercial =
        aero_server::snaplink_commercial::SnaplinkCommercialRuntime::from_env(
            persistence.pg.clone(),
        )
        .await
        .context("initialize Snaplink commercial integration")?;

    // ---------- Repos ----------
    let repos = boot::build_repos(&persistence.pg, &persistence.cache);
    // Snapshot-based regional routing is the single BlobStore surface handed to
    // HTTP, AI, export, transcription, and GC. It resolves each operation from
    // the blob row rather than the workspace's mutable current setting.
    let region_router =
        build_region_router(persistence.blob_store.clone(), cfg.storage_regions.as_ref());
    let blob_store: Arc<dyn aero_storage::BlobStore> = Arc::new(
        aero_storage::PersistedRegionBlobStore::new(repos.blobs.clone(), region_router.clone()),
    );

    // ---------- Gateway config ----------
    let gateway_cfg = GatewayConfig::from_env();
    let ws_cfg = WsConfig::from_env();
    let metrics_cfg = aero_server::metrics::MetricsConfig::from_env();
    let gw_cfg_clone = gateway_cfg.clone();
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

    // ---------- Services ----------
    let services = boot::build_services(boot::ServicesDeps {
        cfg: &cfg,
        pg: persistence.pg.clone(),
        participants: repos.participants.clone(),
        rooms: repos.rooms.clone(),
        workspaces: repos.workspaces.clone(),
        messages: repos.messages.clone(),
        notifications: repos.notifications.clone(),
        notification_prefs: repos.notification_prefs.clone(),
        receipts: repos.receipts.clone(),
        reactions: repos.reactions.clone(),
        calls: repos.calls.clone(),
        pins: repos.pins.clone(),
        ai_jobs: repos.ai_jobs.clone(),
        bus: persistence.jetstream.clone(),
        seq_store: repos.seq_store.clone(),
        blob_store: blob_store.clone(),
        streams: repos.streams.clone(),
        live_repo: repos.live_repo.clone(),
        ai_context: repos.ai_context.clone(),
        presence: repos.presence.clone(),
        redis_client: persistence.cache.client().clone(),
    })?;
    // The same composed Q0+heartbeat capability is injected into AuthService
    // and the relay below; keeping one Arc prevents boot-time split-brain
    // between the claim and future machine-token acceptance paths.
    let relay_scope_provisioner = services.relay_scope_provisioner.clone();

    // ---------- AI worker ----------
    let ai_shutdown = CancellationToken::new();
    let tracker = TaskTracker::new();
    {
        let svc = services.ai_service.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            AiWorker::new(svc).run(cancel).await;
        });
    }
    info!("AI service constructed");

    // ---------- Public + media ingest endpoints ----------
    let public_base_url = std::env::var("AERO_PUBLIC_BASE_URL")
        .unwrap_or_else(|_| format!("http://{}:{}", cfg.server.host, cfg.server.port));
    let configured_ingest_host = std::env::var("AERO_INGEST_HOST")
        .ok()
        .map(|host| host.trim().to_owned())
        .filter(|host| !host.is_empty());
    let ingest_cfg = boot::ingest_config(&cfg);
    let live_ingest_urls = aero_server::live::LiveIngestUrls::new(
        &public_base_url,
        configured_ingest_host.as_deref(),
        ingest_cfg.rtmp,
        ingest_cfg.srt_listen(),
    );
    let _live_cfg = boot::spawn_ingest(&tracker, &repos.streams, ingest_cfg, &ai_shutdown);

    // ---------- Orchestration ----------
    let orchestration = boot::build_orchestration(
        &repos.calls,
        &repos.sfu_router,
        repos.sfu_forwarder.clone(),
        &Arc::new(repos.call_routes.clone()),
        &public_base_url,
        cfg.server.host.clone(),
    );

    // ---------- Push gateways ----------
    let push = boot::build_push_gateways();
    info!(
        fcm = push.fcm.is_some(),
        apns = push.apns.is_some(),
        "push gateways resolved"
    );

    // ---------- AppState ----------
    let hls_dir = std::path::PathBuf::from(&cfg.server.hls_dir);
    if let Err(e) = std::fs::create_dir_all(&hls_dir) {
        tracing::warn!(error = %e, dir = %hls_dir.display(), "create hls dir failed");
    }
    let ingest_host = configured_ingest_host.unwrap_or_else(|| {
        let configured = cfg.server.host.trim();
        match configured.parse::<std::net::IpAddr>() {
            Ok(ip) if ip.is_unspecified() => {
                tracing::warn!(
                    configured,
                    "server bind host is not a usable ICE candidate; defaulting WHIP/WHEP \
                     candidate to loopback (set AERO_INGEST_HOST in production)"
                );
                "127.0.0.1".to_string()
            }
            _ => configured.to_string(),
        }
    });
    // `with_ws_config` hands back an `Arc<Hub>`; `StateDeps.hub` wants the owned
    // `Hub` (build_state re-wraps it in a single Arc). The Arc is freshly built
    // with refcount 1, so `try_unwrap` always succeeds. Build it ONCE here and
    // move it into the struct below (previously a dead Hub was allocated here and
    // a second one rebuilt inline — the unused-variable warning flagged it).
    let hub = Arc::try_unwrap(Hub::with_ws_config(ws_cfg))
        .unwrap_or_else(|_arc| panic!("freshly built hub Arc unexpectedly shared"));
    let state = boot::build_state(boot::StateDeps {
        auth: services.auth,
        im: services.im,
        snaplink_commercial: snaplink_commercial.clone(),
        live: services.live,
        ai_service: Some(services.ai_service.clone()),
        pg: persistence.pg.clone(),
        query_router: persistence.query_router.clone(),
        redis_client: persistence.cache.client().clone(),
        bus: persistence.bus_dyn,
        hub,
        ws_config: ws_cfg,
        shutdown: ai_shutdown.clone(),
        metrics: Arc::new(metrics_cfg),
        public_base_url: public_base_url.clone(),
        live_ingest_urls,
        ingest_host,
        ingest_port: std::env::var("AERO_INGEST_UDP_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(40000),
        hls_dir: hls_dir.clone(),
        runtime_tasks: tracker.clone(),
        gateway_cfg: gw_cfg_clone,
        push,
        call_orchestrator: orchestration.call_orchestrator.clone(),
        call_supervisor: orchestration.call_supervisor.clone(),
        bridge_subscribers: orchestration.bridge_subscribers,
        participants: repos.participants,
        rooms: repos.rooms,
        workspaces: repos.workspaces,
        audit: repos.audit,
        messages: repos.messages,
        notifications: repos.notifications,
        pins: repos.pins,
        receipts: repos.receipts,
        reactions: repos.reactions,
        calls: repos.calls,
        ai_jobs: repos.ai_jobs,
        blobs: repos.blobs,
        streams: repos.streams,
        key_packages: repos.key_packages,
        mls_groups: repos.mls_groups,
        presence: repos.presence,
        stream_viewers: repos.stream_viewers,
        call_roster: repos.call_roster,
        stream_routes: repos.stream_routes,
        topic_history: repos.topic_history,
        thread_read_state: repos.thread_read_state,
        blob_store,
        blob_backend: Box::leak(persistence.blob_backend.into_boxed_str()),
        mailer: aero_server::mailer::build_mailer(cfg.email.as_ref()),
        region_router,
    });

    // ---------- Background tasks ----------
    boot::spawn_background(&tracker, &state, &ai_shutdown, &services.ai_service);
    if let Some(runtime) = snaplink_commercial {
        runtime.spawn(&tracker, ai_shutdown.clone());
    }

    // ---------- Audit connector relay (B5-2 + B5-4) ----------
    // Presence-gated on AERO_AUDIT_TOKEN_ENDPOINT; any AERO_AUDIT_* variable
    // set while incomplete is a boot error (fail-loud). The relay claims only
    // the B5-1 governance outbox through the OutboxRepo seam and never touches
    // the v1 snaplink_delivery_outbox table; booting before the 0239 table
    // lands degrades to logged claim errors, not a crash.
    //
    // B5-4: the repo is wrapped in the heartbeat freshness decorator (settles
    // are fail-closed while the durable provisioning row is absent/stale),
    // with a one-shot bootstrap arm at spawn + a fixed 60s tick keeping the
    // row fresh — liveness is traffic-independent (a quiet period can never
    // stale the gate; worst case ~60s stale vs the 300s default freshness,
    // and a missed tick self-heals on the next tick).
    match aero_audit_connector::config::RelayConfig::from_env() {
        Ok(Some(relay_cfg)) => {
            let pool = persistence.pg.clone();
            let inner: Arc<dyn aero_audit_connector::outbox::OutboxRepo> =
                Arc::new(aero_audit_connector::pg::PgOutboxRepo::new(pool.clone()));
            let recorder = Arc::new(
                aero_server::audit_relay_heartbeat::PgHeartbeatRecorder::new(
                    aero_storage::audit_relay_provision::AuditRelayProvisionRepo::new(pool),
                    relay_cfg.provision_freshness,
                ),
            );
            let repo: Arc<dyn aero_audit_connector::outbox::OutboxRepo> = Arc::new(
                aero_audit_connector::heartbeat::HeartbeatOutboxRepo::new(inner, recorder.clone()),
            );
            // Bootstrap arm (one-shot): arm the settle fence before the first
            // event. Failure → warn and continue (fail-closed default: settles
            // stay rejected until a heartbeat row exists). Bounded false-green
            // ≤ provision_freshness.
            if let Err(error) = recorder.record_heartbeat().await {
                tracing::warn!(
                    ?error,
                    "audit relay bootstrap heartbeat failed; settles fail-closed until the row exists"
                );
            }
            let client = aero_audit_connector::client::AuditClient::new(relay_cfg.clone())
                .context("initialize audit connector client")?;
            let relay = aero_audit_connector::relay::AuditRelay::new_with_scope_provisioner(
                repo,
                client,
                relay_cfg,
                relay_scope_provisioner.clone(),
            );
            tracker.spawn(relay.spawn(ai_shutdown.clone()));
            // Tick-driven heartbeat (C-1): fixed 60s, independent of settle
            // traffic and of the Tier-2 sampler's 300s cadence. With strict
            // `age < freshness` (default 300s) the age can never reach the
            // boundary (worst case ~60s stale), so the quiet-period permanent
            // stall is structurally impossible; FM-B (no row, e.g. DB down at
            // boot) self-heals ≤ next tick — this tick is the second creator
            // path.
            {
                let heartbeat = recorder.clone();
                let cancel = ai_shutdown.clone();
                tracker.spawn(async move {
                    let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    loop {
                        tokio::select! {
                            () = cancel.cancelled() => break,
                            _ = tick.tick() => {}
                        }
                        if let Err(error) = heartbeat.record_heartbeat().await {
                            tracing::warn!(
                                ?error,
                                "audit relay heartbeat tick failed; retrying next tick (FM-B)"
                            );
                        }
                    }
                });
            }
            info!("audit connector relay enabled (heartbeat-gated settles)");
        }
        Ok(None) => {}
        Err(error) => {
            return Err(error).context("initialize audit connector relay");
        }
    }

    // ---------- Metrics + heartbeats ----------
    boot::spawn_metrics_tasks(
        &tracker,
        &state,
        &ai_shutdown,
        &persistence.jetstream,
        &Arc::new(repos.call_routes.clone()),
        &repos.sfu_router,
        &repos.sfu_forwarder_concrete,
    );

    // ---------- Retention sweep ----------
    boot::spawn_retention(&tracker, &state, &ai_shutdown, cfg.auth.refresh_ttl_secs);

    // ---------- AI shutdown guard ----------
    let _ai_shutdown_guard = ai_shutdown.clone();

    // ---------- HTTP server ----------
    boot::serve::serve(
        state,
        cfg.clone(),
        gateway_cfg.clone(),
        ai_shutdown,
        hls_dir,
        tracker,
    )
    .await
}

/// Build the [`RegionRouter`] from config's `storage_regions` section.
fn build_region_router(
    default: std::sync::Arc<dyn aero_storage::BlobStore>,
    config: Option<&std::collections::HashMap<String, aero_common::config::StorageRegionConfig>>,
) -> aero_storage::RegionRouter {
    let mut regions = std::collections::HashMap::new();
    if let Some(cfgs) = config {
        for (code, sr) in cfgs {
            let s3cfg = aero_storage::S3Config {
                bucket: sr.bucket.clone(),
                region: sr.region.clone(),
                endpoint: sr.endpoint.clone(),
                access_key: sr.access_key.clone(),
                secret_key: sr.secret_key.clone(),
                kms_key_id: sr.kms_key_id.clone(),
            };
            regions.insert(code.clone(), s3cfg);
        }
    }
    aero_storage::RegionRouter::new(default, regions).expect("build region router from config")
}
