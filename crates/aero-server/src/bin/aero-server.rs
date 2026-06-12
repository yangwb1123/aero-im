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
use aero_live_core::{LiveIngest, LiveStreamConfig};
use aero_live_rtmp::spawn_rtmp_ingest;
use aero_live_whip::WhipRegistry;
use aero_live_webrtc::{MediaForwarder, SfuForwarder, SfuRouter};
use aero_server::{
    ai_adapter::AiServiceAdapter,
    call_bridge_supervisor::{CallBridgeSupervisor, NodeRtpPullerFactory, UpstreamFactory},
    config::{GatewayConfig, WsConfig},
    hub::Hub,
    live::LiveService,
    metrics::{self as server_metrics, MetricsConfig},
    rate_limit::{self, RateLimiter},
    routes,
    state::AppState,
    ws,
    ws_rate::WsRateEnforcer,
};
use aero_storage::{
    connect_pg, migrate, AiContextStore, AiJobRepo, AuditRepo, BlobRepo, CallRepo, CallRosterStore,
    CallRouteRegistry, KeyPackageRepo, LiveRepo, MessageRepo, MlsGroupRepo, NotificationPrefsRepo,
    NotificationRepo, DeactivationRepo, KeywordAlertRepo, MessageEditRepo, ParticipantRepo, PatRepo,
    PinRepo, TotpRepo, PresenceStore, ReactionRepo, ReceiptRepo, RecurringMessageRepo, RedisCache,
    RoomRepo, SeqStore, StreamRepo, StreamRouteRegistry, StreamViewerStore,
    ThreadNotificationPrefsRepo, ThreadReadStateRepo, ThreadSubscriptionRepo, TopicHistoryRepo,
    UserGroupRepo, WorkspaceMuteRepo, WorkspaceRepo, WsRateStore,
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
    // ROADMAP 第三版 方向五: connect with bounded exponential backoff so a brief
    // DNS / dependency blip at boot doesn't trigger a K8s restart storm (which
    // would drop every active WS connection). Configurable attempts; gives up
    // loudly after the budget so a genuinely-down dependency still surfaces.
    let connect_attempts: u32 = std::env::var("AERO_STARTUP_CONNECT_ATTEMPTS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let pg = connect_with_retry("postgres", connect_attempts, || {
        connect_pg(&cfg.database.url, cfg.database.max_connections)
    })
    .await?;
    migrate(&pg).await.context("run migrations")?;
    info!("postgres migrated");

    let cache = connect_with_retry("redis", connect_attempts, || RedisCache::connect(&cfg.redis.url)).await?;
    info!("redis connected");

    // ---------- Repos ----------
    let participants = ParticipantRepo::new(pg.clone());
    let rooms = RoomRepo::new(pg.clone());
    let workspaces = WorkspaceRepo::new(pg.clone());
    let audit = AuditRepo::new(pg.clone());
    let messages = MessageRepo::new(pg.clone());
    let notifications = NotificationRepo::new(pg.clone());
    let notification_prefs = NotificationPrefsRepo::new(pg.clone());
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
    let ai_context = AiContextStore::new(cache.client().clone());
    let topic_history_repo = TopicHistoryRepo::new(pg.clone());
    let thread_read_state_repo = ThreadReadStateRepo::new(pg.clone());
    let presence = PresenceStore::new(cache.client().clone());
    // Cross-node live presence (ROADMAP 方向二/五): viewer counts + call rosters
    // live in Redis (sorted-set-with-heartbeat) so multi-node audiences/calls are
    // counted once cluster-wide, sharing the same Redis client as `PresenceStore`.
    let stream_viewers = StreamViewerStore::new(cache.client().clone());
    let call_roster = CallRosterStore::new(cache.client().clone());
    let stream_routes = StreamRouteRegistry::new(cache.client().clone());
    // Cross-node group-call routing (ROADMAP3 方向二): `call -> {participant ->
    // hosting node}` in Redis (TTL). The call-route heartbeat below re-arms this
    // node's entries; the bridge supervisor turns a multi-node census into one
    // CallBridge pull per peer node. Single-node boot leaves both dormant.
    let call_routes = CallRouteRegistry::new(cache.client().clone());
    // Per-process SFU roster + media forwarder. The supervisor fans bridged
    // remote RTP into this same router/forwarder, so local subscribers receive
    // cross-node media through the identical forwarding path as local publishers.
    let sfu_router = SfuRouter::new();
    let sfu_forwarder: Arc<dyn MediaForwarder> = Arc::new(SfuForwarder::new(sfu_router.clone()));
    // Cluster-wide event sequencer (ROADMAP 第三版 方向一): one Redis INCR
    // counter per NATS subject so every node stamps RoomEvent/StreamEvent
    // publishes from the same per-room/per-stream sequence (clients dedup and
    // order on it). Wired into ImService + LiveService below via `with_seq`.
    let seq_store = Arc::new(SeqStore::new(cache.client().clone()));

    // ---------- Blob storage ----------
    // ROADMAP 方向一/方向五: pick the backend from AERO_BLOB_BACKEND. Fail LOUD —
    // if AERO_BLOB_BACKEND=s3 but the S3 config is incomplete, abort startup
    // rather than silently writing to node-local disk (a cluster that thinks
    // it's on S3 but isn't = attachments unreachable across nodes, lost on restart).
    let blob_root = std::path::PathBuf::from(&cfg.server.blob_dir);
    let (blob_store, blob_backend) = aero_storage::blob_store_from_env_checked(blob_root.clone())
        .context("configure blob store")?;
    info!(blob_dir = %blob_root.display(), backend = %blob_backend, "blob store ready");

    // ---------- Bus ----------
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
    .context("init AuthService")?
    // Personal Access Tokens (0019): accept an `aero_pat_*` bearer credential on
    // every AuthUser route, resolved against `pat_tokens`. Without this the
    // extractor accepts JWTs only.
    .with_pat_verifier(Arc::new(PatRepo::new(pg.clone())));

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
        .with_notification_prefs(notification_prefs.clone())
        .with_pins(pins.clone())
        // Wave 10: @-usergroup mention fan-out, message edit-history capture, and
        // keyword/highlight-alert dispatch — all hang off the shared PG pool.
        .with_user_groups(UserGroupRepo::new(pg.clone()))
        .with_message_edits(MessageEditRepo::new(pg.clone()))
        .with_keyword_alerts(KeywordAlertRepo::new(pg.clone()))
        // Wave 11: thread-follow notifications (reply → root-message subscribers).
        .with_thread_subs(ThreadSubscriptionRepo::new(pg.clone()))
        // Wave 14: workspace deactivation gate in assert_room_access.
        .with_deactivations(DeactivationRepo::new(pg.clone()))
        // Wave 24: workspace-wide 2FA enforcement gate in assert_room_access.
        .with_totp(TotpRepo::new(pg.clone()))
        // ROADMAP6 Lane A: per-thread notification levels + workspace-wide mute.
        .with_thread_notification_prefs(ThreadNotificationPrefsRepo::new(pg.clone()))
        .with_workspace_mutes(WorkspaceMuteRepo::new(pg.clone()))
        // ROADMAP8: user-block store for notification suppression.
        .with_block_repo(aero_storage::BlockRepo::new(pg.clone()))
        // ROADMAP 第三版 方向一: cluster-correct publish-time event-seq stamp.
        .with_seq(seq_store.clone()),
    );

    // ---------- Live service (danmaku / gifts / viewers) ----------
    let live = LiveService::new(
        streams.clone(),
        live_repo,
        participants.clone(),
        bus_dyn.clone(),
    )
    // Same Redis-backed sequencer for `live.stream.{id}` StreamEvent publishes.
    .with_seq(seq_store);

    // ---------- AI service ----------
    // The AiService is always constructed — it falls back to a deterministic local
    // embedder + heuristic summary when ANTHROPIC_API_KEY / VOYAGE_API_KEY are absent,
    // so the routes never need to gate on env at request time.
    let ai_service = Arc::new(AiService::from_env(
        ai_jobs.clone(),
        messages.clone(),
        rooms.clone(),
        Some(ai_context),
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

    // ---------- Live ingest (SRT -> HLS) ----------
    // ROADMAP3 方向二: spawn the SRT (Secure Reliable Transport) listener in
    // parallel to RTMP, with the SAME best-effort posture — a bind failure logs
    // a warning and the server keeps booting (a missing SRT listener must never
    // crash the gateway). `SrtIngest` derives its bind address from
    // `cfg.rtmp_listen` (SRT = RTMP port + 1 by convention); to honour a
    // configurable `AERO__LIVE__SRT_LISTEN` we hand it a purpose-built
    // `LiveStreamConfig` whose `rtmp_listen` is the requested SRT port *minus
    // one*, so the derivation lands exactly on the configured SRT address while
    // the HLS root stays shared with RTMP. The real SRT push (ffmpeg/OBS) over
    // the wire remains the documented infra seam; this wires the listener, which
    // boot-verifies.
    let srt_cfg = Arc::new(LiveStreamConfig {
        hls_dir: live_cfg.hls_dir.clone(),
        rtmp_listen: srt_backing_rtmp_addr(&live_cfg.rtmp_listen),
    });
    let srt_listen = srt_cfg.rtmp_listen.ip().to_string();
    let srt_port = srt_cfg.rtmp_listen.port().saturating_add(1);
    {
        let repo = streams.clone();
        let srt_cfg = srt_cfg.clone();
        // Pacing knob (bytes/sec) + optional shared passphrase are env-sourced;
        // `SrtIngest::new` already reads AERO_SRT_MAX_BANDWIDTH_BYTES_PER_SEC.
        let mut srt = aero_live_srt::SrtIngest::new();
        if let Ok(pass) = std::env::var("AERO_SRT_PASSPHRASE") {
            if !pass.is_empty() {
                srt = srt.with_passphrase(pass.into_bytes());
            }
        }
        let srt_bandwidth = srt.max_bandwidth();
        tokio::spawn(async move {
            // Mirror the RTMP block precisely: a bind/run error is a WARN, never
            // a panic — the gateway continues serving HTTP/WS/RTMP regardless.
            if let Err(e) = srt.run(repo, srt_cfg).await {
                tracing::warn!(error = ?e, "SRT ingest task ended");
            }
        });
        info!(
            addr = %format!("{srt_listen}:{srt_port}"),
            max_bandwidth_bytes_per_sec = srt_bandwidth,
            "srt ingest listening"
        );
    }

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
    // ---------- Mobile push gateways (ROADMAP 方向二) ----------
    // Resolve FCM/APNs gateways from env. Each is None unless its credentials are
    // present, so push degrades to "disabled" rather than failing boot. The real
    // OAuth2 (FCM) / ES256-JWT (APNs) credential minting plugs into the
    // TokenProvider seam — env vars AERO_PUSH_FCM_TOKEN / AERO_PUSH_APNS_TOKEN
    // supply a static bearer for environments that mint it out-of-process (e.g. a
    // sidecar); leaving them unset keeps that platform disabled. See aero-push.
    let push_gateways = build_push_gateways();
    info!(
        fcm = push_gateways.fcm.is_some(),
        apns = push_gateways.apns.is_some(),
        "push gateways resolved"
    );

    // ---------- Cross-node group-call orchestrator + bridge supervisor ----------
    // (ROADMAP3/4 方向二) Built here so they share the SAME SfuRouter +
    // CallRouteRegistry as the heartbeat loop below. The WS CallJoin/CallLeave
    // handlers drive these to register participants cross-node, compute bridge
    // topology, and spawn/cancel bridges. The full-mesh CallEvent signaling is
    // untouched (additive). Real node-to-node RTP transport is the documented
    // infra seam (NodeRtpPullerFactory::connect → None), so single-node stays
    // dormant: BridgeTo never fires (no 2nd node in the registry).
    let call_orchestrator = Arc::new(
        aero_im_call::CallOrchestrator::new(calls.clone())
            .with_sfu(sfu_router.clone())
            .with_call_routes(Arc::new(call_routes.clone()), public_base_url.clone()),
    );
    let bridge_factory: Arc<dyn UpstreamFactory> = Arc::new(NodeRtpPullerFactory);
    let call_supervisor = Arc::new(CallBridgeSupervisor::new(
        sfu_router.clone(),
        sfu_forwarder.clone(),
        bridge_factory,
    ));

    let state = AppState {
        auth,
        im,
        pg: pg.clone(),
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
        blob_backend,
        streams,
        key_packages,
        mls_groups,
        presence,
        stream_viewers,
        call_roster,
        call_orchestrator: call_orchestrator.clone(),
        call_supervisor: call_supervisor.clone(),
        stream_routes,
        bus: bus_dyn,
        hub,
        ws_config: ws_cfg,
        rate_limiter: RateLimiter::new(gateway_cfg.rate_limit),
        auth_rate_limiter: RateLimiter::new(gateway_cfg.auth_rate_limit),
        // Per-route credential limits (ROADMAP 方向三): 5/min on login, 3/hour on
        // forgot-password — fractional refill rates the integer config can't express.
        login_rate_limiter: RateLimiter::with_rate(5.0 / 60.0, 5.0),
        forgot_rate_limiter: RateLimiter::with_rate(3.0 / 3600.0, 3.0),
        // Per-WORKSPACE ceiling (ROADMAP3 方向五 — 租户公平): cluster-wide Redis
        // window counter on the shared cache client; tier limits from
        // AERO_WS_RATE_{STANDARD,PREMIUM}_PER_MIN.
        ws_rate: WsRateEnforcer::from_env(WsRateStore::new(cache.client().clone())),
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
        push: push_gateways,
        topic_history: topic_history_repo,
        thread_read_state: thread_read_state_repo,
        blocks: aero_storage::BlockRepo::new(pg.clone()),
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
        let messages_repo = state.messages.clone();
        let stream_mod_pool = state.pg.clone();
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
                            match messages_repo.sweep_ephemeral().await {
                                Ok(0) => {}
                                Ok(n) => info!(swept = n, "ephemeral sweep hard-deleted expired messages"),
                                Err(e) => warn!(error = ?e, "ephemeral sweep failed"),
                            }
                            // Temp-ban expiry sweep (migration 0108): hard-delete
                            // stream_bans rows whose expires_at has passed.
                            match aero_storage::StreamModRepo::new(stream_mod_pool.clone())
                                .sweep_expired_bans()
                                .await
                            {
                                Ok(0) => {}
                                Ok(n) => info!(swept = n, "expired bans cleaned up"),
                                Err(e) => warn!(error = ?e, "ban expiry sweep failed"),
                            }
                            // Channel-points expiry sweep (migration 0113): zero
                            // balances whose earn_expires_at has passed.
                            match aero_storage::ChannelPointsRepo::new(stream_mod_pool.clone())
                                .sweep_expired_points()
                                .await
                            {
                                Ok(0) => {}
                                Ok(n) => info!(swept = n, "expired channel points zeroed"),
                                Err(e) => warn!(error = ?e, "channel points expiry sweep failed"),
                            }
                        }
                    }
                }
            });
        }
    }

    // The cross-node call-bridge supervisor + orchestrator were constructed above
    // (before AppState) and wired into `state.call_supervisor`/`call_orchestrator`
    // so the WS CallJoin/CallLeave handlers drive ensure_bridges/cancel_call. They
    // share this node's SfuRouter + CallRouteRegistry with the heartbeat loop below.
    info!(
        node = %state.public_base_url,
        "call-bridge supervisor + orchestrator ready (dormant until cross-node group calls form)"
    );

    // ---------- Cross-node call-route heartbeat (ROADMAP3 方向二) ----------
    // The CallRouteRegistry entry for each of this node's call participants is
    // stamped with a TTL at join and must be re-armed while the participant is
    // connected, or a call outliving the TTL becomes cross-node-unreachable
    // (stale bridge targets stop being handed out). Every
    // AERO_CALL_ROUTE_HEARTBEAT_SECS (default 30) we refresh the TTL for every
    // (call, participant) this node hosts, sourced from the SFU router roster —
    // the orchestrator's local source of truth. Gated on call-routes being wired
    // (Redis present, which it is here); a value of 0 disables the loop. The
    // loop is harmless when the roster is empty (single-node / no active calls).
    {
        let call_route_reg = call_routes.clone();
        let sfu = sfu_router.clone();
        let node_url = state.public_base_url.clone();
        let cancel = ai_shutdown.clone();
        let secs = std::env::var("AERO_CALL_ROUTE_HEARTBEAT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30);
        if secs == 0 {
            info!("call-route heartbeat disabled (AERO_CALL_ROUTE_HEARTBEAT_SECS=0)");
        } else {
            info!(interval_secs = secs, "call-route heartbeat enabled");
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(secs));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                tick.tick().await; // skip the immediate first tick
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => {
                            info!("call-route heartbeat: shutdown received, exiting");
                            break;
                        }
                        _ = tick.tick() => {
                            let roster = sfu.roster_snapshot();
                            let mut refreshed = 0u32;
                            for (call, participant) in roster {
                                match call_route_reg.heartbeat(call, participant, &node_url).await {
                                    Ok(()) => refreshed += 1,
                                    Err(e) => warn!(error = ?e, %call, "call-route heartbeat failed"),
                                }
                            }
                            if refreshed > 0 {
                                tracing::debug!(refreshed, "call-route TTLs refreshed");
                            }
                        }
                    }
                }
            });
        }
    }

    // ---------- Cross-node stream-route heartbeat (ROADMAP3 方向二) ----------
    // A live stream's StreamRouteRegistry entry (`stream_id -> ingesting node`)
    // gets its TTL stamped at publish (whip/rtmp) and was never refreshed — so a
    // stream longer than the TTL became cross-node-unreachable (sticky-routing
    // redirects stopped). Every AERO_STREAM_ROUTE_HEARTBEAT_SECS (default 30) we
    // refresh the TTL for this node's locally-live streams, sourced from
    // StreamRepo::list_live. Gated on stream-routes being wired (Redis present);
    // a value of 0 disables the loop.
    {
        let routes = state.stream_routes.clone();
        let stream_repo = state.streams.clone();
        let node_url = state.public_base_url.clone();
        let cancel = ai_shutdown.clone();
        let secs = std::env::var("AERO_STREAM_ROUTE_HEARTBEAT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30);
        if secs == 0 {
            info!("stream-route heartbeat disabled (AERO_STREAM_ROUTE_HEARTBEAT_SECS=0)");
        } else {
            info!(interval_secs = secs, "stream-route heartbeat enabled");
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(secs));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                tick.tick().await; // skip the immediate first tick
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => {
                            info!("stream-route heartbeat: shutdown received, exiting");
                            break;
                        }
                        _ = tick.tick() => {
                            match stream_repo.list_live().await {
                                Ok(live) => {
                                    let mut refreshed = 0u32;
                                    for stream in &live {
                                        match routes.heartbeat(stream.id, &node_url).await {
                                            Ok(()) => refreshed += 1,
                                            Err(e) => warn!(error = ?e, stream = %stream.id, "stream-route heartbeat failed"),
                                        }
                                    }
                                    if refreshed > 0 {
                                        tracing::debug!(refreshed, "stream-route TTLs refreshed");
                                    }
                                }
                                Err(e) => warn!(error = ?e, "stream-route heartbeat: list_live failed"),
                            }
                        }
                    }
                }
            });
        }
    }
    // The supervisor + orchestrator now live in AppState (state.call_supervisor /
    // call_orchestrator), driven by the WS CallJoin/CallLeave handlers, so they
    // stay alive for the process lifetime via the router — no extra binding needed.

    // ai_shutdown is kept alive here; it will be cancelled by shutdown_signal
    // when SIGTERM/Ctrl-C arrives (wired into axum::serve below), which in turn
    // causes every background task that holds a clone to exit.
    let _ai_shutdown_guard = ai_shutdown.clone();

    // ---------- DB pool + live session gauges (ROADMAP 方向四/五) ----------
    // Periodically publish sqlx pool stats so dashboards can alert on pool
    // exhaustion (in-use approaching size = requests will start queueing).
    // Also emits the live WHIP session count for media-plane observability.
    {
        let pool = pg.clone();
        let whip_reg = state.whip.clone();
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
                // Active live ingest sessions (ROADMAP 方向五 media-plane metrics).
                common_metrics::set_gauge(
                    common_metrics::names::LIVE_WHIP_SESSIONS,
                    whip_reg.active_sessions() as f64,
                );
            }
        });
    }

    // ---------- AI dead-letter queue size gauge (ROADMAP 方向五) ----------
    {
        let dlq_pool = pg.clone();
        tokio::spawn(async move {
            // Emit one gauge series per job kind so an operator can see WHICH AI
            // workflow (moderation vs summarisation vs …) is dead-lettering.
            const KINDS: &[&str] = &["embed", "summarize", "moderate", "answer"];
            let repo = aero_storage::AiJobRepo::new(dlq_pool);
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                for kind in KINDS {
                    match repo.count_dead(Some(kind)).await {
                        Ok(n) => common_metrics::set_gauge_labeled(
                            common_metrics::names::AI_DEAD_LETTER_QUEUE_SIZE,
                            n as f64,
                            &[("kind", kind)],
                        ),
                        Err(e) => tracing::warn!(error = %e, %kind, "ai dlq count query failed"),
                    }
                }
            }
        });
    }

    // ---------- NATS consumer backlog gauges (ROADMAP 方向五) ----------
    // Poll durable consumers every 30 s and emit pending-message gauges so ops
    // can see if the AI worker or WS fan-out is falling behind.
    {
        let js_pending = jetstream.clone();
        tokio::spawn(async move {
            // Pairs of (stream, consumer) to monitor.
            const CONSUMERS: &[(&str, &str)] = &[
                ("IM_MESSAGES",  "aero-server"),  // WS fan-out
                ("AI_QUEUE",     "aero-ai"),       // AI moderation/summarisation worker
            ];
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                for (stream, consumer) in CONSUMERS {
                    match js_pending.consumer_pending(stream, consumer).await {
                        Ok(Some(n)) => {
                            common_metrics::set_gauge_labeled(
                                common_metrics::names::NATS_CONSUMER_PENDING_MESSAGES,
                                n as f64,
                                &[("stream", *stream), ("consumer", *consumer)],
                            );
                        }
                        Ok(None) => {} // consumer not yet registered, skip
                        Err(e) => tracing::warn!(error = %e, %stream, %consumer, "consumer_pending query failed"),
                    }
                }
            }
        });
    }

    // ---------- Blob GC (ROADMAP 方向四 — GDPR right-to-erasure) ----------
    // Drain the blob_gc_queue once per minute: delete the storage object, then
    // ack the queue entry. Best-effort — a failure is logged and retried next
    // tick (the queue row is only removed on successful store deletion).
    {
        let gc_repo = aero_storage::BlobGcRepo::new(pg.clone());
        let blob_store_gc = state.blob_store.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                match gc_repo.drain(50).await {
                    Err(e) => tracing::warn!(error = %e, "blob_gc drain query failed"),
                    Ok(ids) => {
                        for id in ids {
                            match blob_store_gc.delete(id).await {
                                Ok(()) => {
                                    if let Err(e) = gc_repo.ack(id).await {
                                        tracing::warn!(error = %e, blob_id = %id, "blob_gc ack failed");
                                    }
                                }
                                Err(e) => tracing::warn!(error = %e, blob_id = %id, "blob_gc delete failed"),
                            }
                        }
                    }
                }
            }
        });
    }

    // ---------- Async full-export worker (ROADMAP 方向四) ----------
    // Drains export_jobs: assembles each participant's COMPLETE (uncapped) data
    // archive, stores it as a participant-owned blob, marks the job done. The
    // synchronous /api/me/export stays capped; this is the GDPR-complete path.
    {
        let state_clone = state.clone();
        let cancel = ai_shutdown.clone();
        tokio::spawn(async move {
            aero_server::me_export::run_export_dispatcher(state_clone, cancel, 15).await;
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

    // ---------- Scheduled-message dispatcher ("Send later" + reminders) ----------
    // Polls for due rows (`scheduled_at <= now`, undelivered) and delivers them
    // via ImService::send_message. Exits on the shutdown token.
    {
        let state_clone = state.clone();
        let cancel = ai_shutdown.clone();
        tokio::spawn(async move {
            aero_server::scheduled::run_scheduled_dispatcher(state_clone, cancel).await;
        });
    }

    // ---------- Recurring-message dispatcher (Wave 12) ----------
    // Posts each active recurring message when due (next_run <= now) via
    // ImService::send_message, then advances next_run by its cadence.
    {
        let recurring_repo = RecurringMessageRepo::new(state.pg.clone());
        let im_clone = state.im.clone();
        tokio::spawn(async move {
            aero_server::recurring::run_recurring_dispatcher(recurring_repo, im_clone, 30).await;
        });
    }

    // ---------- Scheduled AI-digest dispatcher (AI-native) ----------
    // Polls due digest subscriptions (next_run_at <= now), summarizes the room or
    // workspace via the AI backend, delivers the summary (room message / activity
    // feed), then advances next_run_at by its frequency. Exits on the shutdown
    // token; idles when no AI backend is wired.
    {
        let state_clone = state.clone();
        let cancel = ai_shutdown.clone();
        tokio::spawn(async move {
            aero_server::digests::run_digest_dispatcher(state_clone, cancel).await;
        });
    }

    // ---------- Concurrent-viewer sampler (peak/avg concurrent viewers) ----------
    // Every 30s, sample each live stream's cluster-wide Redis viewer count into
    // stream_viewer_samples so analytics can report peak/avg concurrent viewers.
    {
        let pg_sampler = state.pg.clone();
        let viewers = state.stream_viewers.clone();
        let stream_repo = state.streams.clone();
        tokio::spawn(async move {
            aero_server::stream_analytics::run_viewer_sampler(pg_sampler, viewers, stream_repo).await;
        });
    }

    // ---------- Embedding backfill (ROADMAP 第三版 方向三 — RAG completeness) ----------
    // Messages can have embedding=NULL — an edit clears it, and pre-AI history was
    // never embedded — so semantic search silently misses them. Every 5 min,
    // enqueue dedup-guarded Embed jobs for a bounded batch of embedding-less
    // messages; the AI worker fills them in (bounded by the existing per-ws + global
    // AI budget). list_without_embedding shrinks as the worker catches up.
    {
        let msgs_bf = MessageRepo::new(pg.clone());
        let jobs_bf = AiJobRepo::new(pg.clone());
        let rooms_bf = RoomRepo::new(pg.clone());
        let cancel = ai_shutdown.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(300));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }
                match msgs_bf.list_without_embedding(200).await {
                    Ok(batch) if !batch.is_empty() => {
                        let mut enqueued = 0u32;
                        for m in &batch {
                            let ws = rooms_bf.room_workspace(m.room_id).await.ok().flatten().map(|w| w.to_uuid());
                            match jobs_bf
                                .enqueue_unique(
                                    aero_storage::AiJobKind::Embed,
                                    m.id.to_uuid(),
                                    ws,
                                    serde_json::json!({"room_id": m.room_id.to_string(), "backfill": true}),
                                )
                                .await
                            {
                                Ok(Some(_)) => enqueued += 1,
                                Ok(None) => {}
                                Err(e) => tracing::warn!(error = %e, msg = %m.id, "embed backfill enqueue failed"),
                            }
                        }
                        if enqueued > 0 {
                            info!(enqueued, scanned = batch.len(), "embedding backfill enqueued");
                        }
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "embed backfill scan failed"),
                }
            }
        });
    }

    // ---------- Outgoing webhook dispatcher ----------
    // Subscribes to `im.room.*` (durable "aero-webhooks", distinct cursor from the
    // WS listener) and delivers RoomEvent::Message to each room's active outgoing
    // webhooks via the real reqwest sender — best-effort, logs non-2xx/transport.
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            if let Err(e) = aero_server::webhooks::run_webhook_dispatcher(state_clone).await {
                tracing::error!(error = ?e, "webhook dispatcher exited");
            }
        });
    }

    // ---------- Outgoing webhook retry loop (DLQ-backed) ----------
    // Polls webhook_delivery_log for due `failed` deliveries and re-sends them with
    // exponential backoff; exhausted ones land in the DLQ (admin: webhook_admin).
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            aero_server::webhooks::run_webhook_retry_loop(state_clone, 30).await;
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

    // ---------- Out-of-office auto-responder bot (Wave 17) ----------
    // Subscribes to room messages and, for any 1:1 DM whose other member has an
    // active out-of-office status, posts their OOO message back once per sender.
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            if let Err(e) = aero_server::ooo_bot::run(state_clone).await {
                tracing::error!(error = ?e, "ooo_bot listener exited");
            }
        });
    }

    // ---------- Mobile push-dispatch bot (ROADMAP 方向二) ----------
    // Subscribes to RoomEvent::Notify and pushes mentions/replies to the
    // recipient's registered FCM/APNs devices. Only spawned when a gateway is
    // configured — with push disabled it would be inert anyway.
    if state.push.any_enabled() {
        let state_clone = state.clone();
        tokio::spawn(async move {
            if let Err(e) = aero_server::push_bot::run(state_clone).await {
                tracing::error!(error = ?e, "push_bot listener exited");
            }
        });
        info!("mobile push dispatch enabled");
    }

    // ---------- "Went live" follower notifications (Wave 21) ----------
    // Subscribes to the live bus and, when a stream goes live, fans out a durable
    // "stream_live" activity-feed entry to each of the creator's followers.
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            if let Err(e) = aero_server::golive_bot::run(state_clone).await {
                tracing::error!(error = ?e, "golive_bot listener exited");
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

    // ---------- Link unfurling (opt-in: AERO_UNFURL) ----------
    // Subscribes to room messages, fetches OG metadata for any URLs, and appends
    // a link-preview Card by editing the message. The live HTTP fetch is the seam.
    if std::env::var("AERO_UNFURL").is_ok() {
        let state_clone = state.clone();
        let cache = aero_storage::UnfurlRepo::new(pg.clone());
        tokio::spawn(async move {
            if let Err(e) = aero_server::unfurl_bot::run(state_clone, cache).await {
                tracing::error!(error = ?e, "unfurl_bot listener exited");
            }
        });
        info!("link unfurling enabled");
    }

    // ---------- Router ----------
    let hls_dir = std::path::PathBuf::from(&cfg.server.hls_dir);
    if let Err(e) = std::fs::create_dir_all(&hls_dir) {
        warn!(error = %e, dir = %hls_dir.display(), "create hls dir failed");
    }

    // CORS fail-closed gate (ROADMAP 第三版 方向五): a production deployment sets
    // AERO_CORS_REQUIRE_ORIGINS=1, which refuses to start with the permissive
    // any-origin default — forcing an explicit allow-list rather than silently
    // shipping a wide-open CORS policy. Dev (flag unset) keeps the convenient default.
    if std::env::var("AERO_CORS_REQUIRE_ORIGINS").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        && gateway_cfg.cors_allowed_origins.is_empty()
    {
        anyhow::bail!(
            "AERO_CORS_REQUIRE_ORIGINS is set but no AERO_CORS_ALLOWED_ORIGINS configured — \
             refusing to start with a permissive any-origin CORS policy"
        );
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
    //
    // Graceful shutdown: wait for SIGTERM (unix) or Ctrl-C, then tell the AI
    // worker and all background tasks to stop via the CancellationToken before
    // the HTTP server stops accepting new connections. In-flight requests
    // continue until axum drains them.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(ai_shutdown.clone()))
    .await?;
    Ok(())
}

/// Wait for SIGTERM (unix) or Ctrl-C, log a message, then cancel the
/// shared [`CancellationToken`] so all background workers exit cleanly.
async fn shutdown_signal(ai_shutdown: tokio_util::sync::CancellationToken) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("ctrl-c handler failed")
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler failed")
            .recv()
            .await
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }

    tracing::info!("shutdown signal received, starting graceful shutdown");
    ai_shutdown.cancel(); // signal background tasks (AI worker, heartbeats, etc.)
}

/// Resolve the mobile push gateways from environment (ROADMAP 方向二).
///
/// FCM is enabled when `AERO_PUSH_FCM_PROJECT` is set; APNs when
/// `AERO_PUSH_APNS_TOPIC` is set. Each uses a [`TokenProvider`] seam — the real
/// short-lived credential (FCM OAuth2 bearer / APNs ES256 JWT) is minted there.
/// For deployments that mint the credential out-of-process (a sidecar or an
/// init container writing it to env), a static bearer can be supplied via
/// `AERO_PUSH_FCM_TOKEN` / `AERO_PUSH_APNS_TOKEN`; absent that, the provider
/// returns an `Auth` error at send time (logged, best-effort) so a
/// misconfiguration never crashes the server. Returns all-`None` when neither
/// platform is configured, leaving push disabled.
fn build_push_gateways() -> aero_server::state::PushGateways {
    use std::sync::Arc;

    /// A [`TokenProvider`] that yields a fixed bearer read from `env_key`, or an
    /// `Auth` error when that env var is unset. The seam where real OAuth2/JWT
    /// minting replaces the static value.
    fn static_bearer_provider(env_key: &'static str) -> aero_push::TokenProvider {
        Arc::new(move || {
            let key = env_key;
            Box::pin(async move {
                std::env::var(key).map_err(|_| {
                    aero_push::PushError::Auth(format!("{key} unset (no push credential configured)"))
                })
            })
        })
    }

    let fcm = std::env::var("AERO_PUSH_FCM_PROJECT").ok().map(|project| {
        let gw = aero_push::FcmGateway::new(project, static_bearer_provider("AERO_PUSH_FCM_TOKEN"));
        Arc::new(gw) as Arc<dyn aero_push::PushGateway>
    });
    let apns = std::env::var("AERO_PUSH_APNS_TOPIC").ok().map(|topic| {
        let gw = aero_push::ApnsGateway::new(topic, static_bearer_provider("AERO_PUSH_APNS_TOKEN"));
        Arc::new(gw) as Arc<dyn aero_push::PushGateway>
    });
    aero_server::state::PushGateways { fcm, apns }
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

/// Resolve the RTMP-shaped backing address that makes [`aero_live_srt::SrtIngest`]
/// bind to the operator-configured SRT listen address.
///
/// `SrtIngest` derives its bind address as `rtmp_listen.port() + 1` over the
/// RTMP host. To support a configurable `AERO__LIVE__SRT_LISTEN` (`host:port`)
/// without changing that derivation, we return `(srt_host, srt_port - 1)` so the
/// `+1` lands exactly on the configured SRT port. When the env var is unset (or
/// unparsable) we keep the convention: the same host as RTMP, SRT = RTMP + 1
/// (i.e. the backing address is simply `rtmp_addr` unchanged).
fn srt_backing_rtmp_addr(rtmp_addr: &SocketAddr) -> SocketAddr {
    match std::env::var("AERO__LIVE__SRT_LISTEN")
        .ok()
        .and_then(|s| s.trim().parse::<SocketAddr>().ok())
    {
        Some(srt) => {
            // Back off one port so SrtIngest's `+1` derivation hits `srt.port()`.
            // A port of 0 (let-the-OS-choose) can't be back-shifted meaningfully;
            // fall through to the RTMP-relative default in that case.
            let backing_port = srt.port().checked_sub(1);
            match backing_port {
                Some(p) => SocketAddr::new(srt.ip(), p),
                None => *rtmp_addr,
            }
        }
        None => *rtmp_addr,
    }
}

/// Connect to a startup dependency with bounded exponential backoff (ROADMAP
/// 第三版 方向五). Retries `attempts` times (backoff 1s,2s,4s,… capped at 30s)
/// before giving up loudly, so a brief boot-time DNS/availability blip doesn't
/// crash the process into a K8s restart storm — but a genuinely-down dependency
/// still surfaces as a hard startup failure.
async fn connect_with_retry<T, E, F, Fut>(what: &str, attempts: u32, mut f: F) -> anyhow::Result<T>
where
    E: std::fmt::Display,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let attempts = attempts.max(1);
    let mut delay = std::time::Duration::from_secs(1);
    for attempt in 1..=attempts {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < attempts => {
                warn!(
                    dependency = %what, attempt, max = attempts, error = %e,
                    backoff_secs = delay.as_secs(), "connect failed; retrying"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_secs(30));
            }
            Err(e) => {
                anyhow::bail!("{what} connect failed after {attempts} attempts: {e}");
            }
        }
    }
    unreachable!("loop returns on the final attempt")
}

