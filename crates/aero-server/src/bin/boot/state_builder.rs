//! AppState composition.
use std::sync::Arc;
use aero_server::metrics::MetricsConfig;
use aero_server::config::WsConfig;
use aero_server::hub::Hub;
use aero_server::rate_limit::RateLimiter;
use aero_server::state::AppState;
use aero_server::ws_rate::WsRateEnforcer;
use aero_storage::WsRateStore;
use aero_live_whip::WhipRegistry;

pub(crate) struct StateDeps {
    pub(crate) auth: aero_auth::AuthService,
    pub(crate) im: Arc<aero_im_core::ImService>,
    pub(crate) live: aero_server::live::LiveService,
    pub(crate) ai_service: Option<Arc<aero_ai::AiService>>,
    pub(crate) pg: sqlx::PgPool,
    pub(crate) redis_client: fred::prelude::RedisClient,
    pub(crate) bus: Arc<dyn aero_bus::EventBus>,
    pub(crate) hub: Hub,
    pub(crate) ws_config: WsConfig,
    pub(crate) metrics: Arc<MetricsConfig>,
    pub(crate) public_base_url: String,
    pub(crate) ingest_host: String,
    pub(crate) ingest_port: u16,
    pub(crate) gateway_cfg: aero_server::config::GatewayConfig,
    pub(crate) push: aero_server::state::PushGateways,
    pub(crate) call_orchestrator: Arc<aero_im_call::CallOrchestrator>,
    pub(crate) call_supervisor: Arc<aero_server::call_bridge_supervisor::CallBridgeSupervisor>,
    pub(crate) bridge_subscribers: aero_server::call_bridge_supervisor::BridgeSubscriberRegistry,
    // From repos
    pub(crate) participants: aero_storage::ParticipantRepo,
    pub(crate) rooms: aero_storage::RoomRepo,
    pub(crate) workspaces: aero_storage::WorkspaceRepo,
    pub(crate) audit: aero_storage::AuditRepo,
    pub(crate) messages: aero_storage::MessageRepo,
    pub(crate) notifications: aero_storage::NotificationRepo,
    pub(crate) pins: aero_storage::PinRepo,
    pub(crate) receipts: aero_storage::ReceiptRepo,
    pub(crate) reactions: aero_storage::ReactionRepo,
    pub(crate) calls: aero_storage::CallRepo,
    pub(crate) ai_jobs: aero_storage::AiJobRepo,
    pub(crate) blobs: aero_storage::BlobRepo,
    pub(crate) streams: aero_storage::StreamRepo,
    pub(crate) key_packages: aero_storage::KeyPackageRepo,
    pub(crate) mls_groups: aero_storage::MlsGroupRepo,
    pub(crate) presence: aero_storage::PresenceStore,
    pub(crate) stream_viewers: aero_storage::StreamViewerStore,
    pub(crate) call_roster: aero_storage::CallRosterStore,
    pub(crate) stream_routes: aero_storage::StreamRouteRegistry,
    pub(crate) topic_history: aero_storage::TopicHistoryRepo,
    pub(crate) thread_read_state: aero_storage::ThreadReadStateRepo,
    // From persistence
    pub(crate) blob_store: Arc<dyn aero_storage::BlobStore>,
    pub(crate) blob_backend: &'static str,
}

pub(crate) fn build(d: StateDeps) -> AppState {
    let rate_config = d.gateway_cfg.rate_limit;
    let auth_rate = d.gateway_cfg.auth_rate_limit;

    let pg_for_blocks = d.pg.clone();
    let redis_for_ws_rate = d.redis_client.clone();
    AppState {
        auth: d.auth,
        im: d.im,
        pg: d.pg,
        live: d.live,
        participants: d.participants,
        // ROADMAP6 方向四: per-process TTL cache fronting ParticipantRepo::get on
        // the hot read paths. Default 60s TTL; invalidated on profile writes.
        participant_cache: aero_server::participant_cache::ParticipantCache::with_default_ttl(),
        rooms: d.rooms,
        workspaces: d.workspaces,
        audit: d.audit,
        messages: d.messages,
        notifications: d.notifications,
        pins: d.pins,
        receipts: d.receipts,
        reactions: d.reactions,
        calls: d.calls,
        ai_jobs: d.ai_jobs,
        blobs: d.blobs,
        blob_store: d.blob_store,
        blob_backend: d.blob_backend,
        streams: d.streams,
        key_packages: d.key_packages,
        mls_groups: d.mls_groups,
        presence: d.presence,
        stream_viewers: d.stream_viewers,
        call_roster: d.call_roster,
        call_orchestrator: d.call_orchestrator,
        call_supervisor: d.call_supervisor,
        stream_routes: d.stream_routes,
        bus: d.bus,
        hub: Arc::new(d.hub),
        ws_config: d.ws_config,
        rate_limiter: RateLimiter::new(rate_config),
        auth_rate_limiter: RateLimiter::new(auth_rate),
        login_rate_limiter: RateLimiter::with_rate(5.0 / 60.0, 5.0),
        forgot_rate_limiter: RateLimiter::with_rate(3.0 / 3600.0, 3.0),
        ws_rate: WsRateEnforcer::from_env(WsRateStore::new(redis_for_ws_rate)),
        metrics: d.metrics,
        ai: d.ai_service.map(|s| Arc::new(aero_server::ai_adapter::AiServiceAdapter::new(s)) as Arc<dyn aero_server::state::AiBackend>),
        public_base_url: d.public_base_url,
        whip: WhipRegistry::new(),
        ingest_host: d.ingest_host,
        ingest_port: d.ingest_port,
        push: d.push,
        topic_history: d.topic_history,
        thread_read_state: d.thread_read_state,
        blocks: aero_storage::BlockRepo::new(pg_for_blocks),
        shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        bridge_subscribers: d.bridge_subscribers,
    }
}
