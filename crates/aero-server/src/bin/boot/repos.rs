//! All repository/store constructors.
use std::sync::Arc;

use aero_live_webrtc::{MediaForwarder, SfuForwarder, SfuRouter};
use aero_storage::RedisCache;

/// Thin wrapper around all repos so main() doesn't need 40 `let` statements.
pub(crate) struct Repos {
    pub(crate) participants: aero_storage::ParticipantRepo,
    pub(crate) rooms: aero_storage::RoomRepo,
    pub(crate) workspaces: aero_storage::WorkspaceRepo,
    pub(crate) audit: aero_storage::AuditRepo,
    pub(crate) messages: aero_storage::MessageRepo,
    pub(crate) notifications: aero_storage::NotificationRepo,
    pub(crate) notification_prefs: aero_storage::NotificationPrefsRepo,
    pub(crate) pins: aero_storage::PinRepo,
    pub(crate) receipts: aero_storage::ReceiptRepo,
    pub(crate) reactions: aero_storage::ReactionRepo,
    pub(crate) calls: aero_storage::CallRepo,
    pub(crate) ai_jobs: aero_storage::AiJobRepo,
    pub(crate) blobs: aero_storage::BlobRepo,
    pub(crate) streams: aero_storage::StreamRepo,
    pub(crate) live_repo: aero_storage::LiveRepo,
    pub(crate) key_packages: aero_storage::KeyPackageRepo,
    pub(crate) mls_groups: aero_storage::MlsGroupRepo,
    pub(crate) ai_context: aero_storage::AiContextStore,
    pub(crate) topic_history: aero_storage::TopicHistoryRepo,
    pub(crate) thread_read_state: aero_storage::ThreadReadStateRepo,
    pub(crate) presence: aero_storage::PresenceStore,
    pub(crate) stream_viewers: aero_storage::StreamViewerStore,
    pub(crate) call_roster: aero_storage::CallRosterStore,
    pub(crate) stream_routes: aero_storage::StreamRouteRegistry,
    pub(crate) call_routes: aero_storage::CallRouteRegistry,
    pub(crate) sfu_router: SfuRouter,
    pub(crate) sfu_forwarder: Arc<dyn MediaForwarder>,
    /// The same forwarder behind a concrete handle so housekeeping tasks can
    /// reach the publisher-facing REMB feedback API (`tick_remb` /
    /// `poll_remb_requests`) that the `dyn MediaForwarder` trait object hides.
    pub(crate) sfu_forwarder_concrete: Arc<SfuForwarder>,
    pub(crate) seq_store: Arc<aero_storage::SeqStore>,
}

pub(crate) fn new(pg: sqlx::PgPool, cache: &RedisCache) -> Repos {
    let sfu_router = SfuRouter::new();
    let sfu_forwarder_concrete = Arc::new(SfuForwarder::new(sfu_router.clone()));
    let sfu_forwarder: Arc<dyn MediaForwarder> = sfu_forwarder_concrete.clone();

    Repos {
        participants: aero_storage::ParticipantRepo::new(pg.clone()),
        rooms: aero_storage::RoomRepo::new(pg.clone()),
        workspaces: aero_storage::WorkspaceRepo::new(pg.clone()),
        audit: aero_storage::AuditRepo::new(pg.clone()),
        messages: aero_storage::MessageRepo::new(pg.clone()),
        notifications: aero_storage::NotificationRepo::new(pg.clone()),
        notification_prefs: aero_storage::NotificationPrefsRepo::new(pg.clone()),
        pins: aero_storage::PinRepo::new(pg.clone()),
        receipts: aero_storage::ReceiptRepo::new(pg.clone()),
        reactions: aero_storage::ReactionRepo::new(pg.clone()),
        calls: aero_storage::CallRepo::new(pg.clone()),
        ai_jobs: aero_storage::AiJobRepo::new(pg.clone()),
        blobs: aero_storage::BlobRepo::new(pg.clone()),
        streams: aero_storage::StreamRepo::new(pg.clone()),
        live_repo: aero_storage::LiveRepo::new(pg.clone()),
        key_packages: aero_storage::KeyPackageRepo::new(pg.clone()),
        mls_groups: aero_storage::MlsGroupRepo::new(pg.clone()),
        ai_context: aero_storage::AiContextStore::new(cache.client().clone()),
        topic_history: aero_storage::TopicHistoryRepo::new(pg.clone()),
        thread_read_state: aero_storage::ThreadReadStateRepo::new(pg.clone()),
        presence: aero_storage::PresenceStore::new(cache.client().clone()),
        stream_viewers: aero_storage::StreamViewerStore::new(cache.client().clone()),
        call_roster: aero_storage::CallRosterStore::new(cache.client().clone()),
        stream_routes: aero_storage::StreamRouteRegistry::new(cache.client().clone()),
        call_routes: aero_storage::CallRouteRegistry::new(cache.client().clone()),
        sfu_router,
        sfu_forwarder,
        sfu_forwarder_concrete,
        seq_store: Arc::new(aero_storage::SeqStore::new(cache.client().clone())),
    }
}
