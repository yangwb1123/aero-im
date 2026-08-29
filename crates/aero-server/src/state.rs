//! Shared application state injected into Axum handlers.

use std::pin::Pin;
use std::sync::Arc;

use crate::live::LiveService;
use aero_auth::AuthService;
use aero_bus::EventBus;
use aero_im_core::ImService;
use aero_storage::{
    AiJobRepo, AuditRepo, BlobRepo, BlobStore, BlockRepo, CallRepo, CallRosterStore,
    DeliveryCursorRepo, KeyPackageRepo, MessageRepo, MlsGroupRepo, NotificationRepo,
    ParticipantRepo, PgPool, PinRepo, PresenceStore, ReactionRepo, ReceiptRepo, RegionRouter,
    RoomRepo, StreamRepo, StreamRouteRegistry, StreamViewerStore, ThreadReadStateRepo,
    TopicHistoryRepo, WorkspaceRepo,
};
use axum::extract::FromRef;

use crate::config::WsConfig;
use crate::hub::Hub;
use crate::metrics::MetricsConfig;
use crate::rate_limit::RateLimiter;
use crate::whip_media::WhipMediaRegistry;

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
    async fn summarize_room_with_usage_context(
        &self,
        room: aero_common::RoomId,
        last_n: usize,
        _usage_context: aero_ai::usage::UsageContext,
    ) -> Result<String, String> {
        self.summarize_room(room, last_n).await
    }
    async fn answer_question(
        &self,
        room: aero_common::RoomId,
        question: &str,
        k: usize,
    ) -> Result<AiAnswer, String>;
    async fn answer_question_with_usage_context(
        &self,
        room: aero_common::RoomId,
        question: &str,
        k: usize,
        _usage_context: aero_ai::usage::UsageContext,
    ) -> Result<AiAnswer, String> {
        self.answer_question(room, question, k).await
    }
    /// Agentic tool-use answer: a multi-turn Anthropic loop with the message-search
    /// + attachment-read tools (degrades to the one-shot grounded answer without an
    /// LLM key). Reachable from `POST /api/ai/ask {"agentic":true}` (gated by
    /// `AERO_AGENTIC_ANSWERS`). Default delegates to the one-shot answer, so a
    /// backend with no agent loop (e.g. a test mock) still works; the real
    /// `AiServiceAdapter` overrides it with `AiService::answer_question_agentic`.
    async fn answer_question_agentic(
        &self,
        room: aero_common::RoomId,
        question: &str,
        _max_iters: usize,
    ) -> Result<AiAnswer, String> {
        self.answer_question(room, question, 8).await
    }
    async fn answer_question_agentic_with_usage_context(
        &self,
        room: aero_common::RoomId,
        question: &str,
        max_iters: usize,
        _usage_context: aero_ai::usage::UsageContext,
    ) -> Result<AiAnswer, String> {
        self.answer_question_agentic(room, question, max_iters)
            .await
    }
    /// Answer a question across EVERY room the caller belongs to in a workspace
    /// (the "ask your workspace" RAG flow). Retrieval is membership- and
    /// workspace-bounded; degrades without an LLM key exactly like
    /// [`AiBackend::answer_question`]. Backs `POST /api/workspaces/:id/ask`.
    async fn answer_question_workspace(
        &self,
        participant: aero_common::ParticipantId,
        workspace: aero_common::WorkspaceId,
        question: &str,
        k: usize,
    ) -> Result<AiAnswer, String>;
    async fn answer_question_workspace_with_usage_context(
        &self,
        participant: aero_common::ParticipantId,
        workspace: aero_common::WorkspaceId,
        question: &str,
        k: usize,
        _usage_context: aero_ai::usage::UsageContext,
    ) -> Result<AiAnswer, String> {
        self.answer_question_workspace(participant, workspace, question, k)
            .await
    }
    /// Compute an embedding for the given text. Used by the search route's
    /// `mode=vector` path. Falls back to a deterministic local hash embedder
    /// when no remote API key is configured.
    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, String>;
    async fn embed_text_with_usage_context(
        &self,
        text: &str,
        _usage_context: aero_ai::usage::UsageContext,
        _operation: &str,
    ) -> Result<Vec<f32>, String> {
        self.embed_text(text).await
    }
    /// Translate text into `target_lang`. Used for live call captions (P3).
    /// Echoes the source text when no LLM is configured.
    async fn translate(&self, text: &str, target_lang: &str) -> Result<String, String>;
    async fn translate_with_usage_context(
        &self,
        text: &str,
        target_lang: &str,
        _usage_context: aero_ai::usage::UsageContext,
    ) -> Result<String, String> {
        self.translate(text, target_lang).await
    }
    /// Summarize an arbitrary block of text into a short recap + action items.
    /// Used for the post-call meeting recap (the caller joins a call's persisted
    /// transcript lines into one block). Degrades to a heuristic first-lines digest
    /// when no LLM key is configured; never errors on a missing key.
    async fn summarize_text(&self, text: &str) -> Result<String, String>;
    async fn summarize_text_with_usage_context(
        &self,
        text: &str,
        _usage_context: aero_ai::usage::UsageContext,
    ) -> Result<String, String> {
        self.summarize_text(text).await
    }
    /// Summarize a thread — the flat chain of replies hanging off a root message.
    /// Mirrors [`AiBackend::summarize_room`] but sources the thread's reply chain.
    /// Degrades to a heuristic when no LLM key is configured; never errors on a
    /// missing key. Backs `POST /api/messages/:id/thread-summary`.
    async fn summarize_thread(
        &self,
        root: aero_common::MessageId,
        max_replies: usize,
    ) -> Result<String, String>;
    async fn summarize_thread_with_usage_context(
        &self,
        root: aero_common::MessageId,
        max_replies: usize,
        _usage_context: aero_ai::usage::UsageContext,
    ) -> Result<String, String> {
        self.summarize_thread(root, max_replies).await
    }
    /// Summarize recent activity across every channel the caller belongs to within
    /// a workspace (the workspace twin of [`AiBackend::summarize_room`]). Backs the
    /// scheduled workspace digest. Degrades to a heuristic without an LLM key.
    async fn summarize_workspace(
        &self,
        participant: aero_common::ParticipantId,
        workspace: aero_common::WorkspaceId,
        last_n: usize,
    ) -> Result<String, String>;
    async fn summarize_workspace_with_usage_context(
        &self,
        participant: aero_common::ParticipantId,
        workspace: aero_common::WorkspaceId,
        last_n: usize,
        _usage_context: aero_ai::usage::UsageContext,
    ) -> Result<String, String> {
        self.summarize_workspace(participant, workspace, last_n)
            .await
    }
    /// Rank workspace members by topical authority on `topic` — embed the topic,
    /// run the membership-bounded cross-room vector search, aggregate hits by
    /// author, return the top-`k` experts. Backs `POST /api/workspaces/:id/find-expert`.
    /// Never errors on a missing LLM key (it is retrieval + aggregation only); an
    /// empty/lexically-degraded result rather than an error when nothing matches.
    async fn find_expert(
        &self,
        participant: aero_common::ParticipantId,
        workspace: aero_common::WorkspaceId,
        topic: &str,
        k: usize,
    ) -> Result<Vec<AiExpert>, String>;
    async fn find_expert_with_usage_context(
        &self,
        participant: aero_common::ParticipantId,
        workspace: aero_common::WorkspaceId,
        topic: &str,
        k: usize,
        _usage_context: aero_ai::usage::UsageContext,
    ) -> Result<Vec<AiExpert>, String> {
        self.find_expert(participant, workspace, topic, k).await
    }
    /// Rank channel candidates the caller isn't in into "channels to join"
    /// recommendations. `candidates` is the pre-fetched candidate set —
    /// `(room, name, recent_activity)` triples — and ranking is a pure,
    /// deterministic aggregation over the activity counts (the no-embeddings
    /// degrade path). Never errors; empty in ⇒ empty out. Backs
    /// `GET /api/workspaces/:id/recommendations/channels`.
    async fn recommend_channels(
        &self,
        candidates: Vec<(aero_common::RoomId, String, i64)>,
        k: usize,
    ) -> Result<Vec<AiChannelRec>, String>;
    /// Rank people candidates the caller doesn't already follow into "people to
    /// follow" recommendations. `candidates` is the pre-filtered
    /// `(participant, shared_room_count)` set; ranking is a pure deterministic
    /// aggregation (the shared-channel-count degrade path). Never errors. Backs
    /// `GET /api/workspaces/:id/recommendations/people`.
    async fn recommend_people(
        &self,
        candidates: Vec<(aero_common::ParticipantId, i64)>,
        k: usize,
    ) -> Result<Vec<AiPersonRec>, String>;
    /// Moderate a message body (P5). `Some(reason)` blocks, `None` allows.
    /// Returns `None` when no LLM is configured.
    async fn moderate(&self, text: &str) -> Result<Option<String>, String>;
    async fn moderate_with_context(
        &self,
        text: &str,
        _usage_context: aero_ai::usage::UsageContext,
    ) -> Result<Option<String>, String> {
        self.moderate(text).await
    }
    /// Whether a real Anthropic key is configured, i.e. whether [`Self::moderate`]
    /// makes a PAID upstream call. Lets the moderation pipeline charge the
    /// resulting cost only when a paid call actually happened (ROADMAP 方向四).
    /// Defaults to `false` so a stub/test backend records no spend.
    fn has_anthropic(&self) -> bool {
        false
    }
    /// Generate a short (5-10 word) title for the thread rooted at `root` — the
    /// root message anchors the title, the (clamped) reply chain is supporting
    /// context. Mirrors [`AiBackend::summarize_thread`]'s sourcing. Degrades SAFELY
    /// without an LLM key to a deterministic first-~8-words-of-the-root heuristic;
    /// never errors on a missing key. Backs `POST /api/messages/:id/thread-title`.
    async fn generate_thread_title(
        &self,
        root: aero_common::MessageId,
        max_replies: usize,
    ) -> Result<String, String>;
    async fn generate_thread_title_with_usage_context(
        &self,
        root: aero_common::MessageId,
        max_replies: usize,
        _usage_context: aero_ai::usage::UsageContext,
    ) -> Result<String, String> {
        self.generate_thread_title(root, max_replies).await
    }
    /// Score a message's affect (additive to the binary [`AiBackend::moderate`]
    /// gate — this never blocks). Returns a coarse sentiment, a `[0.0, 1.0]`
    /// toxicity likelihood, and a short tone label. Degrades SAFELY without an LLM
    /// key to a deterministic keyword/punctuation heuristic; never errors on a
    /// missing key. Backs `POST /api/messages/:id/sentiment`.
    async fn score_sentiment(&self, text: &str) -> Result<AiSentiment, String>;
    async fn score_sentiment_with_usage_context(
        &self,
        text: &str,
        _usage_context: aero_ai::usage::UsageContext,
    ) -> Result<AiSentiment, String> {
        self.score_sentiment(text).await
    }
    /// Streaming variant of [`AiBackend::answer_question`].
    ///
    /// Returns `(citations, stream)` so the UI can render source chips before
    /// the first token arrives. Stream items are `Ok(text_chunk)` or
    /// `Err(message)` on a mid-stream error.
    async fn answer_question_stream(
        &self,
        room: aero_common::RoomId,
        question: &str,
        k: usize,
    ) -> Result<
        (
            Vec<aero_common::MessageId>,
            Pin<Box<dyn futures::Stream<Item = Result<String, String>> + Send + 'static>>,
        ),
        String,
    >;
    async fn answer_question_stream_with_usage_context(
        &self,
        room: aero_common::RoomId,
        question: &str,
        k: usize,
        _usage_context: aero_ai::usage::UsageContext,
    ) -> Result<
        (
            Vec<aero_common::MessageId>,
            Pin<Box<dyn futures::Stream<Item = Result<String, String>> + Send + 'static>>,
        ),
        String,
    > {
        self.answer_question_stream(room, question, k).await
    }
    /// RAG answer with rolling session context.
    ///
    /// Same as [`AiBackend::answer_question`] but prepends the last few
    /// Q&A turns from Redis so the model can refer back to them, then appends
    /// the new question and answer to the session history.
    async fn ask_with_context(
        &self,
        participant: aero_common::ParticipantId,
        room: aero_common::RoomId,
        question: &str,
        k: usize,
    ) -> Result<AiAnswer, String>;
    async fn ask_with_context_and_usage_context(
        &self,
        participant: aero_common::ParticipantId,
        room: aero_common::RoomId,
        question: &str,
        k: usize,
        _usage_context: aero_ai::usage::UsageContext,
    ) -> Result<AiAnswer, String> {
        self.ask_with_context(participant, room, question, k).await
    }
}

#[derive(Debug, Clone)]
pub struct AiAnswer {
    pub answer: String,
    pub citations: Vec<aero_common::MessageId>,
}

/// One ranked expert returned by [`AiBackend::find_expert`] — a candidate
/// authority on a topic, their summed relevance `score`, and a few `citations`
/// (message ids) backing the ranking.
#[derive(Debug, Clone)]
pub struct AiExpert {
    pub participant: aero_common::ParticipantId,
    pub score: f32,
    pub citations: Vec<aero_common::MessageId>,
}

/// One recommended channel returned by [`AiBackend::recommend_channels`] — a
/// channel the caller isn't in, its display `name`, an affinity `score`, and a
/// short human-readable `reason`.
#[derive(Debug, Clone)]
pub struct AiChannelRec {
    pub room: aero_common::RoomId,
    pub name: String,
    pub score: f32,
    pub reason: String,
}

/// One recommended person returned by [`AiBackend::recommend_people`] — a
/// workspace member the caller doesn't already follow, an affinity `score`, and a
/// short `reason`.
#[derive(Debug, Clone)]
pub struct AiPersonRec {
    pub participant: aero_common::ParticipantId,
    pub score: f32,
    pub reason: String,
}

/// Affect read on a single message returned by [`AiBackend::score_sentiment`].
///
/// Additive to moderation (never blocks): `sentiment` is the coarse polarity
/// (`"negative" | "neutral" | "positive"`), `toxicity` is a `[0.0, 1.0]`
/// hostility likelihood, and `tone` is a short human-readable label. Serializes
/// directly as the `POST /api/messages/:id/sentiment` response body.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AiSentiment {
    pub sentiment: String,
    pub toxicity: f32,
    pub tone: String,
}

#[derive(Clone)]
pub struct AppState {
    pub auth: AuthService,
    /// Dedicated Aero ID account-summary target binding. `None` is the safe
    /// default: the internal projection route remains unavailable until its
    /// issuer, audience, JWKS, and replay contract are configured.
    pub account_summary_binding:
        Option<Arc<crate::account_summary_binding::AccountSummaryTargetVerifier>>,
    pub im: Arc<ImService>,
    /// Optional Snaplink commercial projector and durable usage/audit relay.
    /// Readiness consults its local entitlement projection, never live central
    /// reachability; liveness remains independent.
    pub snaplink_commercial: Option<Arc<crate::snaplink_commercial::SnaplinkCommercialRuntime>>,
    /// Shared Postgres pool. Lets feature modules construct their own repositories
    /// inline (`XRepo::new(state.pg.clone())`) without threading a new field
    /// through `AppState` for every addition — repos are cheap `Arc<PgPool>` wrappers.
    pub pg: PgPool,
    /// Consistency-aware primary/read-replica routing. Security and write paths
    /// continue to use `pg`; only explicitly eventual reads use this seam.
    pub query_router: aero_storage::QueryRouter,
    pub live: LiveService,
    pub participants: ParticipantRepo,
    /// Per-process TTL cache fronting [`ParticipantRepo::get`] on the hot read
    /// paths (push/notification fan-out, mention resolution). Lookups go through
    /// `participant_cache.get_or_fetch(pid, &participants)`; profile writes
    /// (`update_me`) call `participant_cache.invalidate(pid)`. ROADMAP6 方向四.
    pub participant_cache: crate::participant_cache::ParticipantCache,
    /// Per-process TTL cache for room membership lists (ROADMAP6 方向四).
    /// Fronts `RoomRepo::members()` on the hot fan-out paths in the bus
    /// listener. Membership writes (add/remove member) invalidate the cached
    /// entry so the next fan-out re-fetches from the DB.
    pub room_member_cache: crate::room_member_cache::RoomMemberCache,
    /// Transactional email sender (password reset, invitation). `None` when
    /// SMTP is not configured — in that mode reset tokens are only logged.
    pub mailer: Option<crate::mailer::Mailer>,
    pub rooms: RoomRepo,
    /// Workspace / org (tenant) membership + role store. Backs the
    /// ROADMAP 方向一 management API in [`crate::workspaces`].
    pub workspaces: WorkspaceRepo,
    /// Append-only workspace audit trail (ROADMAP 方向一 合规).
    pub audit: AuditRepo,
    pub messages: MessageRepo,
    /// Per-recipient notification inbox (mentions / thread replies).
    pub notifications: NotificationRepo,
    /// Pinned-messages store (per-room).
    pub pins: PinRepo,
    pub receipts: ReceiptRepo,
    /// Per-(participant, room) persistent DELIVERY cursor (ROADMAP 方向三·A): the
    /// client-ACKed Last-Known-Good delivery point driving per-room reconnect
    /// catch-up + multi-device convergence. Distinct from `receipts` (the *seen*
    /// cursor for unread badges).
    pub delivery_cursors: DeliveryCursorRepo,
    pub reactions: ReactionRepo,
    pub calls: CallRepo,
    pub ai_jobs: AiJobRepo,
    pub blobs: BlobRepo,
    pub blob_store: Arc<dyn BlobStore>,
    /// Regional blob store router for data residency (ROADMAP 方向五·1).
    /// Routes blob operations to region-specific backends by workspace region.
    pub region_router: RegionRouter,
    /// Active blob backend label (`"s3"` / `"local"`), surfaced on `/health` so
    /// operators can confirm storage is wired as intended (ROADMAP 第三版 方向五).
    pub blob_backend: &'static str,
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
    /// Cross-node group-call orchestrator (ROADMAP3/4 方向二): on a group-call
    /// join the WS handler drives this to register the participant in the SFU
    /// router + cross-node `CallRouteRegistry` and compute the bridge topology;
    /// on leave it unregisters. Shares the SFU router + registry with the
    /// call-route heartbeat loop so the heartbeat refreshes real TTLs. The
    /// full-mesh `CallEvent` signaling path is unchanged — this is additive.
    pub call_orchestrator: Arc<aero_im_call::CallOrchestrator>,
    /// Cross-node call-bridge supervisor: on a `CallTopology::BridgeTo` the WS
    /// handler calls `ensure_bridges`; on last-local-leave, `cancel_call`. The
    /// real node-to-node RTP transport stays the documented infra seam, so this
    /// is dormant single-node.
    pub call_supervisor: Arc<crate::call_bridge_supervisor::CallBridgeSupervisor>,
    /// Cross-node stream routing: `stream_id -> ingesting node's base URL`
    /// (Redis, TTL). Lets a WHEP pull on a node that isn't ingesting the stream
    /// redirect the client to the node that is — sticky routing (ROADMAP 方向二).
    pub stream_routes: StreamRouteRegistry,
    pub bus: Arc<dyn EventBus>,
    pub hub: Arc<Hub>,
    /// Per-connection WS back-pressure policy (bounded send-queue capacity etc.).
    pub ws_config: WsConfig,
    /// Per-client API rate limiter (in-memory token buckets).
    pub rate_limiter: RateLimiter,
    /// Stricter limiter applied to credential-accepting auth endpoints
    /// (register / reset / refresh).
    pub auth_rate_limiter: RateLimiter,
    /// Tightest limiter, login only — 5 / minute / client (ROADMAP 方向三).
    pub login_rate_limiter: RateLimiter,
    /// Forgot-password limiter — 3 / hour / client, throttles email enumeration
    /// and reset-mail spam (ROADMAP 方向三).
    pub forgot_rate_limiter: RateLimiter,
    /// Set once graceful shutdown begins (SIGTERM/Ctrl-C). While set,
    /// `/health/ready` returns 503 `"draining"` so the load-balancer pulls this
    /// pod from rotation BEFORE it stops accepting connections — closing the
    /// rolling-deploy race where new traffic lands on a tearing-down pod
    /// (ROADMAP 方向三). `/health/live` stays 200 so the pod is drained, not
    /// killed.
    pub shutting_down: Arc<std::sync::atomic::AtomicBool>,
    /// Process-wide cooperative shutdown signal. Long-running background and
    /// media tasks share the same token; each WebSocket uses it to send an RFC
    /// 6455 code 1001 Close before its connection task exits.
    pub shutdown: tokio_util::sync::CancellationToken,
    /// Per-WORKSPACE request ceiling (ROADMAP3 方向五 — 租户公平): tier limits,
    /// the cluster-wide Redis window counter, and the room→workspace /
    /// workspace→tier TTL resolution caches. Enforced at the high-traffic
    /// choke points via [`crate::ws_rate::check_ws_rate`] and friends.
    pub ws_rate: crate::ws_rate::WsRateEnforcer,
    /// Raw shared Redis handle, for the few cross-cutting consumers that need ad-hoc
    /// Redis access outside a dedicated store (e.g. the bus consumer invalidating
    /// the AI answer cache on `Edited`/`Deleted`).
    pub redis_client: fred::prelude::RedisClient,
    /// `/metrics` exposure policy (enable flag + optional scrape token).
    pub metrics: Arc<MetricsConfig>,
    /// Optional — only present when an Anthropic / Voyage API key is configured.
    pub ai: Option<Arc<dyn AiBackend>>,
    /// Public-facing HTTP base URL used for WHIP, playback and application links.
    pub public_base_url: String,
    /// Protocol-correct public RTMP/WHIP/SRT endpoints. Media listener ports are
    /// resolved at boot and never inferred from the HTTP gateway port.
    pub live_ingest_urls: crate::live::LiveIngestUrls,
    /// Process-local WHIP/WHEP media lifecycle: resource metadata, one relay per
    /// publisher, viewer cancellation, and generation-fenced task cleanup.
    pub whip: Arc<WhipMediaRegistry>,
    /// Numeric ICE host and preferred UDP port. Each media session binds an
    /// exclusive socket; collisions fall back to a kernel-assigned port.
    pub ingest_host: String,
    pub ingest_port: u16,
    /// Shared HLS root used by RTMP, SRT and WHIP (`hls_dir/{stream_id}`).
    pub hls_dir: std::path::PathBuf,
    /// Request-originated runtime tasks (media and named webhook delivery) join
    /// the process-wide graceful-shutdown drain.
    pub runtime_tasks: tokio_util::task::TaskTracker,
    /// Mobile push gateways (FCM/APNs). `None` per platform when the
    /// corresponding credentials are not configured — the push-dispatch bot
    /// then simply skips that platform (ROADMAP 方向二).
    pub push: PushGateways,
    /// Channel topic change history (ROADMAP7 Lane A).
    pub topic_history: TopicHistoryRepo,
    /// Thread-level read state + unread count tracking (ROADMAP7 Lane A).
    pub thread_read_state: ThreadReadStateRepo,
    /// User-level block / ignore store (migration 0106).
    /// Block/unblock another participant; gates DM creation and notification
    /// delivery for blocked senders.
    pub blocks: BlockRepo,
    /// Cross-node call-bridge subscriber registry (ROADMAP 方向五): which pulling
    /// nodes want each call's bridged RTP. Written by the internal subscribe
    /// endpoint, read by each call's egress sender.
    pub bridge_subscribers: crate::call_bridge_supervisor::BridgeSubscriberRegistry,
}

/// The per-platform mobile push gateways, resolved from env at startup. Each is
/// `None` when its credentials are absent so push degrades to "disabled" rather
/// than failing server boot. Cheap to clone (each is an `Arc`).
#[derive(Clone, Default)]
pub struct PushGateways {
    /// Firebase Cloud Messaging (Android + Web). Present when `AERO_PUSH_FCM_*` set.
    pub fcm: Option<Arc<dyn aero_push::PushGateway>>,
    /// Apple Push Notification service (iOS + macOS). Present when `AERO_PUSH_APNS_*` set.
    pub apns: Option<Arc<dyn aero_push::PushGateway>>,
}

impl PushGateways {
    /// The gateway for a stored platform string (`"fcm"` / `"apns"`), if configured.
    #[must_use]
    pub fn for_platform(&self, platform: &str) -> Option<&Arc<dyn aero_push::PushGateway>> {
        match platform {
            "fcm" => self.fcm.as_ref(),
            "apns" => self.apns.as_ref(),
            _ => None,
        }
    }

    /// True when at least one platform gateway is configured — lets the binary
    /// skip spawning the push-dispatch listener entirely when push is disabled.
    #[must_use]
    pub fn any_enabled(&self) -> bool {
        self.fcm.is_some() || self.apns.is_some()
    }
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
