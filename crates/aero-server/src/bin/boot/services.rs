//! Service construction: Auth, IM, Live, AI.
use std::sync::Arc;

use aero_ai::AiService;
use aero_auth::AuthService;
use aero_common::config::AppConfig;
use aero_im_core::ImService;
use aero_server::live::LiveService;
use aero_server::rate_limit::RateLimiter;
use aero_server::ws_rate::WsRateEnforcer;
use anyhow::Context;
use tracing::info;

/// Wired-up top-level services.
pub(crate) struct Services {
    pub(crate) auth: AuthService,
    pub(crate) im: Arc<ImService>,
    pub(crate) live: LiveService,
    pub(crate) ai_service: Arc<AiService>,
}

pub(crate) struct ServicesDeps<'a> {
    pub(crate) cfg: &'a AppConfig,
    pub(crate) pg: sqlx::PgPool,
    pub(crate) participants: aero_storage::ParticipantRepo,
    pub(crate) rooms: aero_storage::RoomRepo,
    pub(crate) workspaces: aero_storage::WorkspaceRepo,
    pub(crate) messages: aero_storage::MessageRepo,
    pub(crate) notifications: aero_storage::NotificationRepo,
    pub(crate) notification_prefs: aero_storage::NotificationPrefsRepo,
    pub(crate) receipts: aero_storage::ReceiptRepo,
    pub(crate) reactions: aero_storage::ReactionRepo,
    pub(crate) calls: aero_storage::CallRepo,
    pub(crate) pins: aero_storage::PinRepo,
    pub(crate) ai_jobs: aero_storage::AiJobRepo,
    pub(crate) bus: Arc<aero_bus::JetStreamBus>,
    pub(crate) seq_store: Arc<aero_storage::SeqStore>,
    pub(crate) blob_store: Arc<dyn aero_storage::BlobStore>,
    pub(crate) streams: aero_storage::StreamRepo,
    pub(crate) live_repo: aero_storage::LiveRepo,
    pub(crate) ai_context: aero_storage::AiContextStore,
    pub(crate) presence: aero_storage::PresenceStore,
    pub(crate) redis_client: fred::prelude::RedisClient,
}

pub(crate) fn build(deps: ServicesDeps<'_>) -> anyhow::Result<Services> {
    // ---------- Auth ----------
    let priv_pem = deps.cfg.auth.jwt_private_key_pem.trim();
    let pub_pem = deps.cfg.auth.jwt_public_key_pem.trim();
    let jwt_codec = aero_auth::JwtCodec::from_pems(
        priv_pem,
        pub_pem,
        &deps.cfg.auth.jwt_additional_public_keys,
        deps.cfg.auth.issuer.clone(),
        std::time::Duration::from_secs(deps.cfg.auth.access_ttl_secs),
        std::time::Duration::from_secs(deps.cfg.auth.refresh_ttl_secs),
    )
    .context("init JWT codec")?;
    let mut auth = AuthService::new(deps.participants.clone(), jwt_codec)
        .with_pat_verifier(Arc::new(aero_storage::PatRepo::new(deps.pg.clone())))
        // 方向三 — open platform: bot tokens (`bot_…`) authenticate as the bot's
        // participant. Mirrors the PAT verifier; a deleted bot's token is rejected
        // by the `participants.deleted_at` guard in `BotRepo::verify_token`.
        .with_bot_verifier(Arc::new(aero_storage::BotRepo::new(deps.pg.clone())));
    // Cross-node aggregation when AERO_LOGIN_LOCKOUT_REDIS is set (shared INCR/SETEX
    // over the same Redis the cache uses); otherwise the in-process default.
    if let Some(throttle) =
        aero_auth::LoginThrottle::from_env_with_redis(Some(deps.redis_client.clone()))
    {
        tracing::info!("per-account login lockout enabled (AERO_LOGIN_LOCKOUT)");
        auth = auth.with_login_throttle(Arc::new(throttle));
    }

    // ---------- IM service ----------
    let mut im_svc = ImService::new(
        deps.rooms.clone(),
        deps.messages.clone(),
        deps.participants.clone(),
        deps.receipts.clone(),
        deps.reactions.clone(),
        deps.calls.clone(),
        deps.ai_jobs.clone(),
        deps.bus.clone(),
    )
    .with_workspaces(deps.workspaces.clone())
    .with_notifications(deps.notifications.clone())
    .with_notification_prefs(deps.notification_prefs.clone())
    .with_pins(deps.pins.clone())
    .with_user_groups(aero_storage::UserGroupRepo::new(deps.pg.clone()))
    .with_message_edits(aero_storage::MessageEditRepo::new(deps.pg.clone()))
    .with_keyword_alerts(aero_storage::KeywordAlertRepo::new(deps.pg.clone()))
    .with_thread_subs(aero_storage::ThreadSubscriptionRepo::new(deps.pg.clone()))
    .with_deactivations(aero_storage::DeactivationRepo::new(deps.pg.clone()))
    .with_totp(aero_storage::TotpRepo::new(deps.pg.clone()))
    .with_thread_notification_prefs(aero_storage::ThreadNotificationPrefsRepo::new(
        deps.pg.clone(),
    ))
    .with_workspace_mutes(aero_storage::WorkspaceMuteRepo::new(deps.pg.clone()))
    .with_thread_mutes(aero_storage::ThreadMuteRepo::new(deps.pg.clone()))
    .with_block_repo(aero_storage::BlockRepo::new(deps.pg.clone()))
    .with_workspace_notif_defaults(aero_storage::WorkspaceNotifDefaultsRepo::new(
        deps.pg.clone(),
    ))
    .with_presence(deps.presence.clone())
    .with_seq(deps.seq_store.clone());

    // Slack-style `@here` online-only fan-out is ACTIVE via `.with_presence(..)`
    // in the builder chain above: an `@here` broadcast is narrowed to the room's
    // *online* members (`@channel`/`@everyone`/`@all` keep full fan-out). Strictly
    // fail-open — no presence / Redis error / empty roster ⇒ notify all members,
    // so it can only ever NARROW `@here`, never drop a notification.

    // Deferred reply-notification aggregation (notification bundles).
    //
    // Opt-in via AERO_NOTIFICATION_BUNDLES because activating the builder
    // routes reply notifications through the bundle table instead of inserting
    // them immediately; the corresponding periodic flush task in
    // boot::background only runs when this same flag is set, so the two are
    // always activated together (an active builder without a flusher would
    // strand reply notifications in the bundle table). Default off keeps the
    // legacy immediate-insert delivery semantics unchanged.
    if std::env::var("AERO_NOTIFICATION_BUNDLES").is_ok() {
        im_svc = im_svc
            .with_notification_bundles(aero_storage::NotificationBundleRepo::new(deps.pg.clone()));
        info!("notification reply-aggregation enabled (AERO_NOTIFICATION_BUNDLES)");
    }

    if std::env::var("AERO_SPAM_GUARD").is_ok() {
        let thresholds = aero_im_core::SpamThresholds::default();
        // Cross-node aggregation when AERO_SPAM_GUARD_REDIS is set (three shared
        // sorted-set sliding windows over the same Redis the cache uses); otherwise
        // the in-process DashMap default. The Redis backend is strictly fail-open,
        // so a Redis outage degrades to "no behavioural throttle", never to dropping
        // legitimate messages.
        let guard = if std::env::var("AERO_SPAM_GUARD_REDIS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
        {
            info!("behavioral spam guard: cross-node Redis backend (AERO_SPAM_GUARD_REDIS)");
            aero_im_core::SpamGuard::with_redis(thresholds, deps.redis_client.clone())
        } else {
            aero_im_core::SpamGuard::new(thresholds)
        };
        im_svc = im_svc.with_spam_guard(Arc::new(guard));
        info!("behavioral spam guard enabled (AERO_SPAM_GUARD)");
    }

    // The management API is always mounted, so enforcement must be wired in the
    // same production lifecycle. An empty rule set is a cheap no-op; configured
    // rules may never be accepted by the API and then silently ignored because
    // an unrelated process environment flag was absent.
    im_svc = im_svc.with_auto_mod_rules(aero_storage::AutoModRuleRepo::new(deps.pg.clone()));
    info!("workspace auto-moderation rules enabled");
    if let Some(detector) = aero_im_core::PiiDetector::from_env() {
        im_svc = im_svc.with_pii_detector(Arc::new(detector));
        info!("PII guard enabled (AERO_PII_GUARD)");
    }
    let im = Arc::new(im_svc);

    // ---------- Live service ----------
    let live = LiveService::new(
        deps.streams.clone(),
        deps.live_repo,
        deps.participants.clone(),
        deps.bus.clone(),
    )
    .with_seq(deps.seq_store);

    // ---------- AI service ----------
    let ai_service = Arc::new(
        AiService::from_env(
            deps.ai_jobs.clone(),
            deps.messages.clone(),
            deps.rooms.clone(),
            Some(deps.ai_context.clone()),
        )
        .with_blob_store(deps.blob_store.clone())
        // Paid providers reserve a stable usage id in PostgreSQL before the
        // network request and finalize afterward. The relay starts later; the
        // sink is attached before the AI worker can process its first job.
        .with_usage_sink(Arc::new(aero_server::ai_usage::PgUsageSink::new(
            deps.pg.clone(),
        )))
        // Cross-room AI persona repo (方向三). Wired unconditionally; the feature
        // stays dark until AERO_AI_CROSS_ROOM_PROFILE is set, and is GDPR-erasable.
        .with_ai_profiles(aero_storage::AiProfileRepo::new(deps.pg.clone())),
    );
    info!("AI service constructed");

    Ok(Services {
        auth,
        im,
        live,
        ai_service,
    })
}
