//! Shared application state injected into Axum handlers.

use std::sync::Arc;

use aero_auth::AuthService;
use aero_bus::EventBus;
use aero_im_core::ImService;
use aero_live_whip::WhipRegistry;

use crate::live::LiveService;
use aero_storage::{
    AiJobRepo, AuditRepo, BlobRepo, BlobStore, CallRepo, CallRosterStore, KeyPackageRepo,
    MessageRepo, MlsGroupRepo, ParticipantRepo, PresenceStore, ReactionRepo, ReceiptRepo, RoomRepo,
    StreamRepo, StreamViewerStore, WorkspaceRepo,
};
use axum::extract::FromRef;

use crate::config::WsConfig;
use crate::hub::Hub;
use crate::metrics::MetricsConfig;
use crate::rate_limit::RateLimiter;

/// Type-erased handle to whatever AI backend is wired (or `None`).
/// Concrete type lives in `aero-ai`; we route through this trait to keep the
/// server crate compile-clean when AI is disabled.
#[axum::async_trait]
pub trait AiBackend: Send + Sync + 'static {
    async fn summarize_room(
        &self,
        room: aero_common::RoomId,
        last_n: usize,
    ) -> Result<String, String>;
    async fn answer_question(
        &self,
        room: aero_common::RoomId,
        question: &str,
        k: usize,
    ) -> Result<AiAnswer, String>;
    /// Compute an embedding for the given text. Used by the search route's
    /// `mode=vector` path. Falls back to a deterministic local hash embedder
    /// when no remote API key is configured.
    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, String>;
    /// Translate text into `target_lang`. Used for live call captions (P3).
    /// Echoes the source text when no LLM is configured.
    async fn translate(&self, text: &str, target_lang: &str) -> Result<String, String>;
    /// Moderate a message body (P5). `Some(reason)` blocks, `None` allows.
    /// Returns `None` when no LLM is configured.
    async fn moderate(&self, text: &str) -> Result<Option<String>, String>;
}

#[derive(Debug, Clone)]
pub struct AiAnswer {
    pub answer: String,
    pub citations: Vec<aero_common::MessageId>,
}

#[derive(Clone)]
pub struct AppState {
    pub auth: AuthService,
    pub im: Arc<ImService>,
    pub live: LiveService,
    pub participants: ParticipantRepo,
    pub rooms: RoomRepo,
    /// Workspace / org (tenant) membership + role store. Backs the
    /// ROADMAP 方向一 management API in [`crate::workspaces`].
    pub workspaces: WorkspaceRepo,
    /// Append-only workspace audit trail (ROADMAP 方向一 合规).
    pub audit: AuditRepo,
    pub messages: MessageRepo,
    pub receipts: ReceiptRepo,
    pub reactions: ReactionRepo,
    pub calls: CallRepo,
    pub ai_jobs: AiJobRepo,
    pub blobs: BlobRepo,
    pub blob_store: Arc<dyn BlobStore>,
    pub streams: StreamRepo,
    pub key_packages: KeyPackageRepo,
    pub mls_groups: MlsGroupRepo,
    pub presence: PresenceStore,
    /// Cluster-correct live-stream viewer set (Redis sorted-set-with-heartbeat).
    /// Authoritative source of the viewer COUNT broadcast in [`aero_common::StreamEvent::Viewers`]
    /// so audiences spread across nodes are not each counted locally (ROADMAP 方向二/五).
    /// The local [`Hub`] still tracks watchers for per-process event fan-out.
    pub stream_viewers: StreamViewerStore,
    /// Cluster-correct group-call roster (Redis). Authoritative source of the
    /// roster/count reported to clients so a multi-node call is consistent across
    /// nodes (ROADMAP 方向二). The local [`Hub`] still tracks the roster for
    /// per-process mesh delivery.
    pub call_roster: CallRosterStore,
    pub bus: Arc<dyn EventBus>,
    pub hub: Arc<Hub>,
    /// Per-connection WS back-pressure policy (bounded send-queue capacity etc.).
    pub ws_config: WsConfig,
    /// Per-client API rate limiter (in-memory token buckets).
    pub rate_limiter: RateLimiter,
    /// `/metrics` exposure policy (enable flag + optional scrape token).
    pub metrics: Arc<MetricsConfig>,
    /// Optional — only present when an Anthropic / Voyage API key is configured.
    pub ai: Option<Arc<dyn AiBackend>>,
    /// Public-facing base URL (used to render absolute ingest/playback URLs).
    pub public_base_url: String,
    /// In-memory WHIP registry — keyed by stream id; persisted lifecycle is in PG.
    pub whip: Arc<WhipRegistry>,
    /// Configured ingest host:port advertised in SDP candidates.
    pub ingest_host: String,
    pub ingest_port: u16,
}

impl FromRef<AppState> for AuthService {
    fn from_ref(state: &AppState) -> Self {
        state.auth.clone()
    }
}

impl FromRef<AppState> for Arc<MetricsConfig> {
    fn from_ref(state: &AppState) -> Self {
        state.metrics.clone()
    }
}
