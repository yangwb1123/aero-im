//! Message-retention sweep + data-lifecycle cleanup loops.
use std::sync::Arc;
use tokio_util::task::TaskTracker;
use tokio_util::sync::CancellationToken;
use aero_storage::{MessageRepo, ParticipantRepo};
use aero_server::state::AppState;
use tracing::{info, warn};

pub(crate) fn spawn(
    tracker: &TaskTracker,
    state: &AppState,
    ai_shutdown: &CancellationToken,
) {
    let workspaces = state.workspaces.clone();
    let messages_repo = state.messages.clone();
    let stream_mod_pool = state.pg.clone();
    let erasure_pool = state.pg.clone();
    let lifecycle_pool = state.pg.clone();

    let notif_retention_days = std::env::var("AERO__SERVER__NOTIFICATION_RETENTION_DAYS")
        .ok().and_then(|s| s.parse::<i64>().ok()).unwrap_or(90);
    let audit_retention_days = std::env::var("AERO__SERVER__AUDIT_RETENTION_DAYS")
        .ok().and_then(|s| s.parse::<i64>().ok()).unwrap_or(365);
    let ai_job_retention_days = std::env::var("AERO__SERVER__AI_JOB_RETENTION_DAYS")
        .ok().and_then(|s| s.parse::<i64>().ok()).unwrap_or(7);
    // AI usage ledger: a billing record, so kept far longer than ai_jobs (default
    // ~400 days = 13 months). Bounds the ledger's otherwise-unbounded growth.
    let ai_usage_retention_days = std::env::var("AERO__SERVER__AI_USAGE_RETENTION_DAYS")
        .ok().and_then(|s| s.parse::<i64>().ok()).unwrap_or(400);
    let webhook_log_retention_days = std::env::var("AERO__SERVER__WEBHOOK_LOG_RETENTION_DAYS")
        .ok().and_then(|s| s.parse::<i64>().ok()).unwrap_or(30);
    let search_click_retention_days = std::env::var("AERO__SERVER__SEARCH_CLICK_RETENTION_DAYS")
        .ok().and_then(|s| s.parse::<i64>().ok()).unwrap_or(90);
    let login_event_retention_days = std::env::var("AERO__SERVER__LOGIN_EVENT_RETENTION_DAYS")
        .ok().and_then(|s| s.parse::<i64>().ok()).unwrap_or(180);
    // Failed-login trail (方向五). Same default window as login_events — the
    // anomaly value is in *recent* failures; older rows are dead weight (and PII).
    let login_failure_retention_days = std::env::var("AERO__SERVER__LOGIN_FAILURE_RETENTION_DAYS")
        .ok().and_then(|s| s.parse::<i64>().ok()).unwrap_or(180);
    let viewer_raw_retention_days = std::env::var("AERO__SERVER__VIEWER_RAW_RETENTION_DAYS")
        .ok().and_then(|s| s.parse::<i32>().ok()).unwrap_or(2);
    let viewer_rollup_retention_days = std::env::var("AERO__SERVER__VIEWER_ROLLUP_RETENTION_DAYS")
        .ok().and_then(|s| s.parse::<i32>().ok()).unwrap_or(90);
    // Notification-bundle staleness window. Bundles are normally consumed within
    // seconds by the flush task, but a crash/disabled-flusher could leave rows
    // behind; this sweep bounds their lifetime (GDPR + unbounded-growth guard).
    // Default 1 day; 0 disables.
    let bundle_retention_days = std::env::var("AERO__SERVER__NOTIFICATION_BUNDLE_RETENTION_DAYS")
        .ok().and_then(|s| s.parse::<i64>().ok()).unwrap_or(1);
    // Revoked-token blacklist window (P2-3 unbounded-growth fix). An entry is only
    // worth keeping until its token expires by its own natural TTL — a 7-day
    // refresh-token lifetime is the default (`AuthConfig::refresh_ttl_secs`), so a
    // conservative 8-day window (7d TTL + 1d margin for clock skew / TTL changes)
    // guarantees we only purge entries whose token can no longer authenticate.
    // Operators who raise `refresh_ttl_secs` MUST raise this to match. 0 disables.
    let revoked_token_retention_days = std::env::var("AERO__SERVER__REVOKED_TOKEN_RETENTION_DAYS")
        .ok().and_then(|s| s.parse::<i64>().ok()).unwrap_or(8);
    let sweep_im = state.im.clone();
    let cancel = ai_shutdown.clone();
    let sweep_secs = std::env::var("AERO__SERVER__RETENTION_SWEEP_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(3600);

    if sweep_secs == 0 {
        info!("retention sweep disabled (AERO__SERVER__RETENTION_SWEEP_SECS=0)");
        return;
    }
    info!(interval_secs = sweep_secs, "retention sweep enabled");

    tracker.spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(sweep_secs));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.tick().await; // skip first tick
        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    info!("retention sweep: shutdown signal received, exiting");
                    break;
                }
                _ = tick.tick() => {
                    let now = time::OffsetDateTime::now_utc();
                    sweep_messages(&workspaces, &sweep_im, now).await;
                    sweep_ephemeral(&messages_repo, &sweep_im).await;
                    sweep_bans(&stream_mod_pool).await;
                    sweep_channel_points(&stream_mod_pool).await;
                    sweep_deferred_erasure(&erasure_pool).await;
                    sweep_notifications(&lifecycle_pool, notif_retention_days, now).await;
                    sweep_audit(&lifecycle_pool, audit_retention_days, now).await;
                    sweep_audit_partitions(&lifecycle_pool, audit_retention_days).await;
                    sweep_ai_jobs(&lifecycle_pool, ai_job_retention_days, now).await;
                    sweep_ai_usage(&lifecycle_pool, ai_usage_retention_days, now).await;
                    sweep_webhook_logs(&lifecycle_pool, webhook_log_retention_days, now).await;
                    sweep_search_clicks(&lifecycle_pool, search_click_retention_days, now).await;
                    sweep_login_events(&lifecycle_pool, login_event_retention_days, now).await;
                    sweep_login_failures(&lifecycle_pool, login_failure_retention_days, now).await;
                    sweep_notification_bundles(&lifecycle_pool, bundle_retention_days, now).await;
                    sweep_revoked_tokens(&lifecycle_pool, revoked_token_retention_days, now).await;
                    sweep_viewer_samples(&lifecycle_pool, viewer_raw_retention_days, viewer_rollup_retention_days).await;
                    sweep_viewer_partitions(&lifecycle_pool, viewer_raw_retention_days).await;
                }
            }
        }
    });
}

async fn sweep_messages(
    workspaces: &aero_storage::WorkspaceRepo,
    sweep_im: &Arc<aero_im_core::ImService>,
    now: time::OffsetDateTime,
) {
    match workspaces.sweep_expired_messages(now, None).await {
        Ok(deleted) if deleted.is_empty() => {}
        Ok(deleted) => {
            let n = deleted.len();
            for (message_id, room_id) in deleted {
                sweep_im.announce_message_deleted(room_id, message_id).await;
            }
            info!(swept = n, "retention sweep soft-deleted messages");
        }
        Err(e) => warn!(error = ?e, "retention sweep failed"),
    }
}

async fn sweep_ephemeral(
    messages_repo: &MessageRepo,
    sweep_im: &Arc<aero_im_core::ImService>,
) {
    match messages_repo.sweep_ephemeral().await {
        Ok(deleted) if deleted.is_empty() => {}
        Ok(deleted) => {
            let n = deleted.len();
            for (message_id, room_id) in deleted {
                sweep_im.announce_message_deleted(room_id, message_id).await;
            }
            info!(swept = n, "ephemeral sweep hard-deleted expired messages");
        }
        Err(e) => warn!(error = ?e, "ephemeral sweep failed"),
    }
}

async fn sweep_bans(pool: &sqlx::PgPool) {
    match aero_storage::StreamModRepo::new(pool.clone()).sweep_expired_bans().await {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "expired bans cleaned up"),
        Err(e) => warn!(error = ?e, "ban expiry sweep failed"),
    }
}

async fn sweep_channel_points(pool: &sqlx::PgPool) {
    match aero_storage::ChannelPointsRepo::new(pool.clone()).sweep_expired_points().await {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "expired channel points zeroed"),
        Err(e) => warn!(error = ?e, "channel points expiry sweep failed"),
    }
}

async fn sweep_deferred_erasure(pool: &sqlx::PgPool) {
    match ParticipantRepo::new(pool.clone()).sweep_deferred_erasure().await {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "deferred GDPR erasure completed (released holds)"),
        Err(e) => warn!(error = ?e, "deferred erasure sweep failed"),
    }
}

async fn sweep_notifications(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 { return; }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::NotificationRepo::new(pool.clone()).sweep_read_before(cutoff).await {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "old read notifications purged"),
        Err(e) => warn!(error = ?e, "notification retention sweep failed"),
    }
}

async fn sweep_audit(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 { return; }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::AuditRepo::new(pool.clone()).sweep_before(cutoff).await {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "old audit events purged"),
        Err(e) => warn!(error = ?e, "audit retention sweep failed"),
    }
}

/// Maintain the daily `RANGE` partitions of `audit_events` (migration 0146):
/// pre-create the next few days' partitions and DROP daily partitions older than
/// the audit-retention window. This is the cheaper metadata-only counterpart to
/// the row-by-row `sweep_audit` DELETE above (both are kept: the DELETE also reaps
/// any stray rows that landed in the catch-all DEFAULT partition, while the DROP
/// reclaims whole expired daily partitions instantly). The DEFAULT partition means
/// a transient failure here never blocks inserts — the next tick simply retries.
/// `days == 0` disables audit retention entirely, so partition-dropping is skipped
/// too (only the safe pre-create of future partitions would otherwise run).
async fn sweep_audit_partitions(pool: &sqlx::PgPool, days: i64) {
    if days == 0 { return; }
    // keep_days mirrors the audit-retention window: a daily partition is only
    // dropped once every row in it is already past `sweep_audit`'s cutoff.
    let keep_days = i32::try_from(days).unwrap_or(i32::MAX);
    match aero_storage::AuditRepo::new(pool.clone())
        .ensure_partitions(keep_days, 3)
        .await
    {
        Ok(()) => {}
        Err(e) => warn!(error = ?e, "audit-event partition maintenance failed"),
    }
}

async fn sweep_ai_jobs(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 { return; }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::AiJobRepo::new(pool.clone()).sweep_terminal_before(cutoff).await {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "completed ai_jobs purged"),
        Err(e) => warn!(error = ?e, "ai_job retention sweep failed"),
    }
}

async fn sweep_ai_usage(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 { return; }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::AiUsageRepo::new(pool.clone()).sweep_older_than(cutoff).await {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "stale ai_usage_ledger rows purged"),
        Err(e) => warn!(error = ?e, "ai_usage retention sweep failed"),
    }
}

async fn sweep_webhook_logs(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 { return; }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::WebhookDeliveryRepo::new(pool.clone()).sweep_terminal_before(cutoff).await {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "terminal webhook deliveries purged"),
        Err(e) => warn!(error = ?e, "webhook delivery-log retention sweep failed"),
    }
}

async fn sweep_search_clicks(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 { return; }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::SearchFeedbackRepo::new(pool.clone()).sweep_before(cutoff).await {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "old search click events purged"),
        Err(e) => warn!(error = ?e, "search-click retention sweep failed"),
    }
}

async fn sweep_login_events(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 { return; }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::LoginEventRepo::new(pool.clone()).sweep_before(cutoff).await {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "old login events purged"),
        Err(e) => warn!(error = ?e, "login-event retention sweep failed"),
    }
}

async fn sweep_login_failures(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 { return; }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::LoginFailureRepo::new(pool.clone()).sweep_before(cutoff).await {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "old login failures purged"),
        Err(e) => warn!(error = ?e, "login-failure retention sweep failed"),
    }
}

async fn sweep_notification_bundles(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 { return; }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::NotificationBundleRepo::new(pool.clone()).sweep_before(cutoff).await {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "stale notification bundles purged"),
        Err(e) => warn!(error = ?e, "notification-bundle retention sweep failed"),
    }
}

async fn sweep_revoked_tokens(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 { return; }
    // `cutoff = now - window`; only entries revoked before this — i.e. whose token
    // has already expired by its own natural TTL — are dropped. A token that could
    // still be valid was revoked after `cutoff` and is kept (no security regression;
    // see `RevokedTokenRepo::sweep_before`).
    let cutoff = now - time::Duration::days(days);
    match aero_storage::RevokedTokenRepo::new(pool.clone()).sweep_before(cutoff).await {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "expired revoked-token blacklist entries purged"),
        Err(e) => warn!(error = ?e, "revoked-token retention sweep failed"),
    }
}

async fn sweep_viewer_samples(
    pool: &sqlx::PgPool,
    raw_days: i32,
    rollup_days: i32,
) {
    if raw_days == 0 { return; }
    match aero_storage::StreamViewerSampleRepo::new(pool.clone())
        .rollup_and_downsample(raw_days, rollup_days)
        .await
    {
        Ok(o) if o.is_empty() => {}
        Ok(o) => info!(
            rolled_up = o.rolled_up,
            raw_deleted = o.raw_deleted,
            rollups_pruned = o.rollups_pruned,
            "viewer samples rolled up + downsampled"
        ),
        Err(e) => warn!(error = ?e, "viewer rollup+downsample failed"),
    }
}

/// Maintain the daily `RANGE` partitions of `stream_viewer_samples` (migration
/// 0144): pre-create the next few days' partitions and drop ones older than the
/// retention window. `keep_days` is the raw-sample retention plus a margin so a
/// partition is never dropped while it could still hold un-rolled-up raw samples
/// (the `rollup_and_downsample` sweep deletes individual raw rows by age; this
/// reclaims the whole partition once every row in it is safely past retention).
/// The catch-all DEFAULT partition means a transient failure here never blocks
/// inserts — the next tick simply retries.
async fn sweep_viewer_partitions(pool: &sqlx::PgPool, raw_days: i32) {
    if raw_days == 0 { return; }
    // Keep partitions a few days beyond the raw-retention window: raw rows are
    // deleted by `rollup_and_downsample` at `raw_days`, so a partition is only
    // dropped well after its last row would have been downsampled away.
    let keep_days = raw_days.saturating_add(3);
    match aero_storage::StreamViewerSampleRepo::new(pool.clone())
        .ensure_partitions(keep_days, 3)
        .await
    {
        Ok(()) => {}
        Err(e) => warn!(error = ?e, "viewer-sample partition maintenance failed"),
    }
}
