//! Message-retention sweep + data-lifecycle cleanup loops.
use aero_server::state::AppState;
use aero_storage::{MessageRepo, ParticipantRepo};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{info, warn};

pub(crate) fn spawn(
    tracker: &TaskTracker,
    state: &AppState,
    ai_shutdown: &CancellationToken,
    refresh_ttl_secs: u64,
) {
    let workspaces = state.workspaces.clone();
    let messages_repo = state.messages.clone();
    let stream_mod_pool = state.pg.clone();
    let erasure_pool = state.pg.clone();
    let lifecycle_pool = state.pg.clone();

    let notif_retention_days = std::env::var("AERO__SERVER__NOTIFICATION_RETENTION_DAYS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(90);
    let audit_retention_days = std::env::var("AERO__SERVER__AUDIT_RETENTION_DAYS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(365);
    let ai_job_retention_days = std::env::var("AERO__SERVER__AI_JOB_RETENTION_DAYS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(7);
    // AI usage ledger: a billing record, so kept far longer than ai_jobs (default
    // ~400 days = 13 months). Bounds the ledger's otherwise-unbounded growth.
    let ai_usage_retention_days = std::env::var("AERO__SERVER__AI_USAGE_RETENTION_DAYS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(400);
    let webhook_log_retention_days = std::env::var("AERO__SERVER__WEBHOOK_LOG_RETENTION_DAYS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(30);
    let search_click_retention_days = std::env::var("AERO__SERVER__SEARCH_CLICK_RETENTION_DAYS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(90);
    let login_event_retention_days = std::env::var("AERO__SERVER__LOGIN_EVENT_RETENTION_DAYS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(180);
    // Failed-login trail (方向五). Same default window as login_events — the
    // anomaly value is in *recent* failures; older rows are dead weight (and PII).
    let login_failure_retention_days = std::env::var("AERO__SERVER__LOGIN_FAILURE_RETENTION_DAYS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(180);
    // B5-1 auth slice (F-4 third leg): terminal audit-governance DLQ rows
    // (`status='dead'` or replayed) older than N days are hard-deleted;
    // never-replayed `pending` rows stay (the gauge's alert surface).
    let governance_failed_pairs_retention_days =
        std::env::var("AERO__SERVER__GOVERNANCE_FAILED_PAIRS_RETENTION_DAYS")
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(30);
    // R-D2 §10.2: governance-outbox retention — TERMINAL rows only
    // (status 2/3 delivered/dead; the sink holds the ledger once delivered),
    // aligned with the 365d audit-partition DROP so the outbox copy of
    // content digests is bounded (GDPR Art. 5(1)(e) exposure). Live rows
    // (status 0/1) are NEVER swept while the relay runs (0246 header).
    // 0 disables.
    let governance_outbox_retention_days =
        std::env::var("AERO__SERVER__GOVERNANCE_OUTBOX_RETENTION_DAYS")
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(365);
    let viewer_raw_retention_days = std::env::var("AERO__SERVER__VIEWER_RAW_RETENTION_DAYS")
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(2);
    let viewer_rollup_retention_days = std::env::var("AERO__SERVER__VIEWER_ROLLUP_RETENTION_DAYS")
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(90);
    // Notification-bundle staleness window. Bundles are normally consumed within
    // seconds by the flush task, but a crash/disabled-flusher could leave rows
    // behind; this sweep bounds their lifetime (GDPR + unbounded-growth guard).
    // Default 1 day; 0 disables.
    let bundle_retention_days = std::env::var("AERO__SERVER__NOTIFICATION_BUNDLE_RETENTION_DAYS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(1);
    // Sender idempotency tombstones outlive ordinary reconnect/retry windows so
    // an old client key cannot create a duplicate after a transient outage.
    // Zero disables cleanup.
    let message_send_key_retention_days =
        std::env::var("AERO__SERVER__MESSAGE_SEND_KEY_RETENTION_DAYS")
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(30)
            .clamp(0, 3650);
    // Published outbox rows remain briefly for idempotent-send resolution and
    // operator diagnosis. Pending rows are never swept by the repository.
    let configured_event_outbox_days = std::env::var("AERO__SERVER__EVENT_OUTBOX_RETENTION_DAYS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(30)
        .clamp(0, 3650);
    let event_outbox_retention_days =
        if configured_event_outbox_days == 0 || message_send_key_retention_days == 0 {
            0
        } else {
            configured_event_outbox_days.max(message_send_key_retention_days)
        };
    if configured_event_outbox_days > 0
        && event_outbox_retention_days != configured_event_outbox_days
    {
        warn!(
            configured_days = configured_event_outbox_days,
            effective_days = event_outbox_retention_days,
            message_send_key_retention_days,
            "event-outbox retention raised to cover sender idempotency keys"
        );
    }
    let message_side_effect_retention_days =
        std::env::var("AERO__SERVER__MESSAGE_SIDE_EFFECT_RETENTION_DAYS")
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(30)
            .clamp(0, 3650);
    // Completed durable-consumer receipts bound late producer replays outside
    // NATS's finite duplicate window. Pending producer-outbox rows override this
    // age in the repository and retain their receipts indefinitely.
    let consumer_receipt_retention_days =
        std::env::var("AERO__SERVER__CONSUMER_RECEIPT_RETENTION_DAYS")
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(30)
            .clamp(0, 3650);
    let password_reset_retention_days =
        std::env::var("AERO__SERVER__PASSWORD_RESET_RETENTION_DAYS")
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(7)
            .clamp(0, 3650);
    // Never let blacklist retention fall below the configured refresh-token TTL
    // plus a one-day safety margin. This prevents a long-lived token from becoming
    // valid again merely because an operator raised its TTL without updating the
    // sweep window. Explicit 0 still disables the sweep (the safest setting).
    let configured_revoked_days = std::env::var("AERO__SERVER__REVOKED_TOKEN_RETENTION_DAYS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(8);
    let revoked_token_retention_days =
        effective_revoked_token_retention_days(configured_revoked_days, refresh_ttl_secs);
    if configured_revoked_days > 0 && revoked_token_retention_days != configured_revoked_days {
        warn!(
            configured_days = configured_revoked_days,
            effective_days = revoked_token_retention_days,
            refresh_ttl_secs,
            "revoked-token retention raised to cover refresh-token TTL"
        );
    }
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
                    sweep_messages(&workspaces, now).await;
                    sweep_ephemeral(&messages_repo).await;
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
                    sweep_governance_failed_pairs(
                        &lifecycle_pool,
                        governance_failed_pairs_retention_days,
                        now,
                    ).await;
                    sweep_governance_outbox(
                        &lifecycle_pool,
                        governance_outbox_retention_days,
                        now,
                    ).await;
                    sweep_notification_bundles(&lifecycle_pool, bundle_retention_days, now).await;
                    sweep_message_send_keys(
                        &lifecycle_pool,
                        message_send_key_retention_days,
                        now,
                    ).await;
                    sweep_event_outbox(
                        &lifecycle_pool,
                        event_outbox_retention_days,
                        now,
                    ).await;
                    sweep_message_side_effects(
                        &lifecycle_pool,
                        message_side_effect_retention_days,
                        now,
                    ).await;
                    sweep_consumer_event_receipts(
                        &lifecycle_pool,
                        consumer_receipt_retention_days,
                        now,
                    ).await;
                    sweep_password_reset_tokens(
                        &lifecycle_pool,
                        password_reset_retention_days,
                        now,
                    ).await;
                    sweep_revoked_tokens(&lifecycle_pool, revoked_token_retention_days, now).await;
                    sweep_viewer_samples(&lifecycle_pool, viewer_raw_retention_days, viewer_rollup_retention_days).await;
                    sweep_viewer_partitions(&lifecycle_pool, viewer_raw_retention_days).await;
                }
            }
        }
    });
}

fn effective_revoked_token_retention_days(configured_days: i64, refresh_ttl_secs: u64) -> i64 {
    if configured_days == 0 {
        return 0;
    }
    let ttl_days = refresh_ttl_secs.saturating_add(86_399) / 86_400;
    let minimum = i64::try_from(ttl_days.saturating_add(1)).unwrap_or(i64::MAX);
    configured_days.max(minimum)
}

async fn sweep_messages(workspaces: &aero_storage::WorkspaceRepo, now: time::OffsetDateTime) {
    match workspaces.sweep_expired_messages(now, None).await {
        Ok(deleted) if deleted.is_empty() => {}
        Ok(deleted) => {
            let n = deleted.len();
            // Each tombstone event was appended on the sweep transaction. The
            // normal outbox relay owns publication/retry after commit.
            info!(swept = n, "retention sweep soft-deleted messages");
        }
        Err(e) => warn!(error = ?e, "retention sweep failed"),
    }
}

async fn sweep_ephemeral(messages_repo: &MessageRepo) {
    match messages_repo.sweep_ephemeral().await {
        Ok(deleted) if deleted.is_empty() => {}
        Ok(deleted) => {
            let n = deleted.len();
            // Hard-delete and tombstone outbox append committed atomically.
            info!(swept = n, "ephemeral sweep hard-deleted expired messages");
        }
        Err(e) => warn!(error = ?e, "ephemeral sweep failed"),
    }
}

async fn sweep_bans(pool: &sqlx::PgPool) {
    match aero_storage::StreamModRepo::new(pool.clone())
        .sweep_expired_bans()
        .await
    {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "expired bans cleaned up"),
        Err(e) => warn!(error = ?e, "ban expiry sweep failed"),
    }
}

async fn sweep_channel_points(pool: &sqlx::PgPool) {
    match aero_storage::ChannelPointsRepo::new(pool.clone())
        .sweep_expired_points()
        .await
    {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "expired channel points zeroed"),
        Err(e) => warn!(error = ?e, "channel points expiry sweep failed"),
    }
}

async fn sweep_deferred_erasure(pool: &sqlx::PgPool) {
    match ParticipantRepo::new(pool.clone())
        .sweep_deferred_erasure()
        .await
    {
        Ok(0) => {}
        Ok(n) => info!(
            swept = n,
            "deferred GDPR erasure completed (released holds)"
        ),
        Err(e) => warn!(error = ?e, "deferred erasure sweep failed"),
    }
}

async fn sweep_notifications(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 {
        return;
    }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::NotificationRepo::new(pool.clone())
        .sweep_read_before(cutoff)
        .await
    {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "old read notifications purged"),
        Err(e) => warn!(error = ?e, "notification retention sweep failed"),
    }
}

async fn sweep_audit(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 {
        return;
    }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::AuditRepo::new(pool.clone())
        .sweep_before(cutoff)
        .await
    {
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
    if days == 0 {
        return;
    }
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
    if days == 0 {
        return;
    }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::AiJobRepo::new(pool.clone())
        .sweep_terminal_before(cutoff)
        .await
    {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "completed ai_jobs purged"),
        Err(e) => warn!(error = ?e, "ai_job retention sweep failed"),
    }
}

async fn sweep_ai_usage(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 {
        return;
    }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::AiUsageRepo::new(pool.clone())
        .sweep_older_than(cutoff)
        .await
    {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "stale ai_usage_ledger rows purged"),
        Err(e) => warn!(error = ?e, "ai_usage retention sweep failed"),
    }
}

async fn sweep_webhook_logs(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 {
        return;
    }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::WebhookDeliveryRepo::new(pool.clone())
        .sweep_terminal_before(cutoff)
        .await
    {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "terminal webhook deliveries purged"),
        Err(e) => warn!(error = ?e, "webhook delivery-log retention sweep failed"),
    }
}

async fn sweep_search_clicks(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 {
        return;
    }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::SearchFeedbackRepo::new(pool.clone())
        .sweep_before(cutoff)
        .await
    {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "old search click events purged"),
        Err(e) => warn!(error = ?e, "search-click retention sweep failed"),
    }
}

async fn sweep_login_events(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 {
        return;
    }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::LoginEventRepo::new(pool.clone())
        .sweep_before(cutoff)
        .await
    {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "old login events purged"),
        Err(e) => warn!(error = ?e, "login-event retention sweep failed"),
    }
}

async fn sweep_login_failures(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 {
        return;
    }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::LoginFailureRepo::new(pool.clone())
        .sweep_before(cutoff)
        .await
    {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "old login failures purged"),
        Err(e) => warn!(error = ?e, "login-failure retention sweep failed"),
    }
}

async fn sweep_governance_failed_pairs(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 {
        return;
    }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::audit_governance::FailedPairRepo::new(pool.clone())
        .sweep_terminal_before(cutoff)
        .await
    {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "terminal failed-pair DLQ rows purged"),
        Err(e) => warn!(error = ?e, "governance-failed-pairs retention sweep failed"),
    }
}

/// R-D2 §10.2: governance-outbox retention — DELETE only terminal rows
/// (status 2/3) older than the TTL; live rows (status 0/1) are never swept
/// while the relay runs (0246 header). Aligned with the 365d audit-partition
/// DROP so content digests in the outbox copy stay bounded (GDPR). `days == 0`
/// disables.
async fn sweep_governance_outbox(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 {
        return;
    }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::audit_governance::AuditGovernanceOutboxRepo::new(pool.clone())
        .sweep_terminal_before(cutoff)
        .await
    {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "terminal governance-outbox rows purged"),
        Err(e) => warn!(error = ?e, "governance-outbox retention sweep failed"),
    }
}

async fn sweep_notification_bundles(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 {
        return;
    }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::NotificationBundleRepo::new(pool.clone())
        .sweep_before(cutoff)
        .await
    {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "stale notification bundles purged"),
        Err(e) => warn!(error = ?e, "notification-bundle retention sweep failed"),
    }
}

async fn sweep_message_send_keys(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 {
        return;
    }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::MessageRepo::new(pool.clone())
        .sweep_send_keys_before(cutoff)
        .await
    {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "expired message-send idempotency keys purged"),
        Err(e) => warn!(error = ?e, "message-send idempotency sweep failed"),
    }
}

async fn sweep_event_outbox(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 {
        return;
    }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::EventOutboxRepo::new(pool.clone())
        .sweep_published_before(cutoff)
        .await
    {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "published message outbox rows purged"),
        Err(e) => warn!(error = ?e, "message outbox retention sweep failed"),
    }
}

async fn sweep_message_side_effects(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 {
        return;
    }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::MessageSideEffectRepo::new(pool.clone())
        .sweep_completed_before(cutoff)
        .await
    {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "completed message side-effect rows purged"),
        Err(e) => warn!(error = ?e, "message side-effect retention sweep failed"),
    }
}

async fn sweep_consumer_event_receipts(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 {
        return;
    }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::ConsumerEventReceiptRepo::new(pool.clone())
        .sweep_completed_before(cutoff)
        .await
    {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "completed consumer event receipts purged"),
        Err(e) => warn!(error = ?e, "consumer event receipt retention sweep failed"),
    }
    match aero_storage::ConsumerEventReceiptRepo::new(pool.clone())
        .sweep_abandoned_processing_before(cutoff)
        .await
    {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "abandoned consumer event receipt leases purged"),
        Err(e) => warn!(error = ?e, "abandoned consumer receipt sweep failed"),
    }
}

async fn sweep_password_reset_tokens(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 {
        return;
    }
    let cutoff = now - time::Duration::days(days);
    match aero_storage::PasswordResetRepo::new(pool.clone())
        .sweep_terminal_before(cutoff)
        .await
    {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "password-reset token history purged"),
        Err(e) => warn!(error = ?e, "password-reset retention sweep failed"),
    }
}

async fn sweep_revoked_tokens(pool: &sqlx::PgPool, days: i64, now: time::OffsetDateTime) {
    if days == 0 {
        return;
    }
    // `cutoff = now - window`; only entries revoked before this — i.e. whose token
    // has already expired by its own natural TTL — are dropped. A token that could
    // still be valid was revoked after `cutoff` and is kept (no security regression;
    // see `RevokedTokenRepo::sweep_before`).
    let cutoff = now - time::Duration::days(days);
    match aero_storage::RevokedTokenRepo::new(pool.clone())
        .sweep_before(cutoff)
        .await
    {
        Ok(0) => {}
        Ok(n) => info!(swept = n, "expired revoked-token blacklist entries purged"),
        Err(e) => warn!(error = ?e, "revoked-token retention sweep failed"),
    }
}

async fn sweep_viewer_samples(pool: &sqlx::PgPool, raw_days: i32, rollup_days: i32) {
    if raw_days == 0 {
        return;
    }
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
    if raw_days == 0 {
        return;
    }
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

#[cfg(test)]
mod tests {
    use super::effective_revoked_token_retention_days;

    #[test]
    fn revoked_token_retention_covers_refresh_ttl_and_margin() {
        assert_eq!(effective_revoked_token_retention_days(8, 86_400), 8);
        assert_eq!(effective_revoked_token_retention_days(2, 8 * 86_400), 9);
        assert_eq!(effective_revoked_token_retention_days(1, 86_401), 3);
        assert_eq!(effective_revoked_token_retention_days(0, u64::MAX), 0);
    }
}
