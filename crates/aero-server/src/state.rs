//! Shared application state injected into Axum handlers.

use std::sync::Arc;

use aero_auth::AuthService;
use aero_bus::EventBus;
use aero_im_core::ImService;
use aero_live_whip::WhipRegistry;
use aero_storage::{
    AiJobRepo, BlobRepo, BlobStore, CallRepo, KeyPackageRepo, MessageRepo, MlsGroupRepo,
    ParticipantRepo, PresenceStore, ReactionRepo, ReceiptRepo, RoomRepo, StreamRepo,
};
use axum::extract::FromRef;

use crate::hub::Hub;

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
    pub participants: ParticipantRepo,
    pub rooms: RoomRepo,
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
    pub bus: Arc<dyn EventBus>,
    pub hub: Arc<Hub>,
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
