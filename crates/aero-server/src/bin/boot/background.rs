//! Background tasks: sweeps, heartbeats, bots, dispatchers.
//! Spawned AFTER `AppState` is built; all hold the shutdown token.
use aero_server::state::AppState;
use aero_storage::MessageRepo;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub(crate) fn spawn_all(
    tracker: &TaskTracker,
    state: &AppState,
    ai_shutdown: &CancellationToken,
    ai_service: &Arc<aero_ai::AiService>,
) {
    // Bus listener
    {
        let s = state.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            if let Err(e) = aero_server::ws::run_bus_listener(s, cancel).await {
                tracing::error!(error = ?e, "bus listener exited");
            }
        });
    }

    // Spontaneous SFU media-owner exits are not driven by a WS CallLeave frame.
    // Drain the registry's single-consumer lifecycle stream so local topology,
    // cluster publisher state, Redis roster, and call-route state still converge.
    if let Some(events) = state.call_supervisor.take_sfu_lifecycle_events() {
        let s = state.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            aero_server::ws::run_sfu_lifecycle_events(s, events, cancel).await;
        });
    } else {
        tracing::error!("SFU lifecycle stream was already claimed");
    }

    // Live bus listener
    {
        let s = state.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            if let Err(e) = aero_server::ws::run_live_bus_listener(s, cancel).await {
                tracing::error!(error = ?e, "live bus listener exited");
            }
        });
    }

    // Cross-node login-session revocation control plane. Ephemeral per instance:
    // each node must receive new commands for its own local WebSockets.
    {
        let s = state.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            if let Err(e) = aero_server::session_control::run_listener(s, cancel).await {
                tracing::error!(error = ?e, "session-control listener exited");
            }
        });
    }

    // Transactional message-event outbox relay. The post-commit send path tries
    // immediately; this loop is the crash/restart and transient-NATS safety net.
    // Every server instance may run it: PostgreSQL SKIP LOCKED leases divide the
    // work, while stable NATS message ids make ambiguous retries harmless.
    let outbox_poll_ms = std::env::var("AERO__SERVER__EVENT_OUTBOX_POLL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(250);
    if outbox_poll_ms != 0 {
        let im = state.im.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            let mut tick =
                tokio::time::interval(std::time::Duration::from_millis(outbox_poll_ms.max(10)));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {
                        if let Err(error) = im.dispatch_event_outbox_batch(100).await {
                            tracing::warn!(?error, "message event outbox relay batch failed");
                        }
                    }
                }
            }
        });
        tracing::info!(
            poll_ms = outbox_poll_ms,
            "message event outbox relay enabled"
        );
    } else {
        tracing::warn!(
            "message event outbox relay disabled; pending events require a later enabled instance"
        );
    }

    // Transactional stream.live relay shared by WHIP, RTMP and SRT. Polling is
    // the correctness path after process crashes; stable event ids make every
    // stage safe to repeat across multiple server instances.
    let stream_live_poll_ms = std::env::var("AERO__SERVER__STREAM_LIVE_OUTBOX_POLL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(250);
    if stream_live_poll_ms != 0 {
        let s = state.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(
                stream_live_poll_ms.max(10),
            ));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {
                        if let Err(error) =
                            aero_server::stream_live_outbox::dispatch_batch(&s, 100).await
                        {
                            tracing::warn!(
                                ?error,
                                "stream.live transactional outbox relay batch failed"
                            );
                        }
                    }
                }
            }
        });
        tracing::info!(
            poll_ms = stream_live_poll_ms,
            "stream.live transactional outbox relay enabled"
        );
    } else {
        tracing::warn!(
            "stream.live outbox relay disabled; pending lifecycle events require a later enabled instance"
        );
    }

    // Durable post-commit message work. Message transactions enqueue these
    // rows alongside the aggregate event, so a crash cannot strand notifications
    // or AI indexing/moderation between commit and a detached spawn.
    let side_effect_poll_ms = std::env::var("AERO__SERVER__MESSAGE_SIDE_EFFECT_POLL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(250);
    if side_effect_poll_ms != 0 {
        let im = state.im.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(
                side_effect_poll_ms.max(10),
            ));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {
                        if let Err(error) = im.dispatch_message_side_effect_batch(100).await {
                            tracing::warn!(?error, "message side-effect relay batch failed");
                        }
                    }
                }
            }
        });
        tracing::info!(
            poll_ms = side_effect_poll_ms,
            "message side-effect relay enabled"
        );
    } else {
        tracing::warn!(
            "message side-effect relay disabled; pending work requires a later enabled instance"
        );
    }

    // AI usage outbox relay. Paid call paths synchronously append stable ids to
    // PostgreSQL; this leased worker performs the idempotent ledger insertion.
    // No bounded process queue exists, so bursts, DB errors, and restarts cannot
    // discard accepted accounting events.
    {
        let repo = aero_storage::AiUsageRepo::new(state.pg.clone());
        let config = aero_server::ai_usage::UsageWorkerConfig::from_env();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            aero_server::ai_usage::run_usage_ledger_worker(repo, config, cancel).await;
        });
    }

    // Scheduled-message dispatcher
    {
        let s = state.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            aero_server::scheduled::run_scheduled_dispatcher(s, cancel).await;
        });
    }

    // Recurring-message dispatcher
    {
        let repo = aero_storage::RecurringMessageRepo::new(state.pg.clone());
        let im = state.im.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            aero_server::recurring::run_recurring_dispatcher_until_cancelled(repo, im, 30, cancel)
                .await;
        });
    }

    // AI-digest dispatcher
    {
        let s = state.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            aero_server::digests::run_digest_dispatcher(s, cancel).await;
        });
    }

    // Saved-search monitor
    {
        let monitor_secs = std::env::var("AERO__SERVER__SAVED_SEARCH_MONITOR_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(300);
        if monitor_secs != 0 {
            let s = state.clone();
            let cancel = ai_shutdown.clone();
            tracker.spawn(async move {
                aero_server::saved_search_monitor::run_saved_search_monitor(
                    s,
                    monitor_secs,
                    cancel,
                )
                .await;
            });
        }
    }

    // Concurrent-viewer sampler
    {
        let pg = state.pg.clone();
        let viewers = state.stream_viewers.clone();
        let stream_repo = state.streams.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            aero_server::stream_analytics::run_viewer_sampler_until_cancelled(
                pg,
                viewers,
                stream_repo,
                cancel,
            )
            .await;
        });
    }

    // Embedding backfill
    {
        let msgs = MessageRepo::new(state.pg.clone());
        let jobs = aero_storage::AiJobRepo::new(state.pg.clone());
        let rooms = aero_storage::RoomRepo::new(state.pg.clone());
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(300));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }
                match msgs.list_without_embedding(200).await {
                    Ok(batch) if !batch.is_empty() => {
                        let mut enqueued = 0u32;
                        for m in &batch {
                            let ws = rooms
                                .room_workspace(m.room_id)
                                .await
                                .ok()
                                .flatten()
                                .map(|w| w.to_uuid());
                            match jobs
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
                            tracing::info!(enqueued, scanned = batch.len(), "embedding backfill enqueued");
                        }
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "embed backfill scan failed"),
                }
            }
        });
    }

    // Webhook dispatcher
    {
        let s = state.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            if let Err(e) =
                aero_server::webhooks::run_webhook_dispatcher_until_cancelled(s, cancel).await
            {
                tracing::error!(error = ?e, "webhook dispatcher exited");
            }
        });
    }

    // Webhook retry loop
    {
        let s = state.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            aero_server::webhooks::run_webhook_retry_loop_until_cancelled(s, 30, cancel).await;
        });
    }

    // Agent bot
    {
        let s = state.clone();
        let ai = ai_service.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            if let Err(e) = aero_server::agent_bot::run_until_cancelled(s, ai, cancel).await {
                tracing::error!(error = ?e, "agent_bot listener exited");
            }
        });
    }

    // Bot event-subscription dispatcher: delivers bus RoomEvents to user bots'
    // `bot_event_subscriptions` webhooks (方向三). Always on — a no-op when no bot
    // has subscribed, like the other built-in bus listeners.
    {
        let s = state.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            if let Err(e) = aero_server::bot_dispatch::run_until_cancelled(s, cancel).await {
                tracing::error!(error = ?e, "bot_dispatch listener exited");
            }
        });
    }

    // OOO bot
    {
        let s = state.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            if let Err(e) = Box::pin(aero_server::ooo_bot::run_until_cancelled(s, cancel)).await {
                tracing::error!(error = ?e, "ooo_bot listener exited");
            }
        });
    }

    // Push bot (conditional)
    if state.push.any_enabled() {
        let s = state.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            if let Err(e) = aero_server::push_bot::run_until_cancelled(s, cancel).await {
                tracing::error!(error = ?e, "push_bot listener exited");
            }
        });
        tracing::info!("mobile push dispatch enabled");
    }

    // Golive bot
    {
        let s = state.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            if let Err(e) = aero_server::golive_bot::run_until_cancelled(s, cancel).await {
                tracing::error!(error = ?e, "golive_bot listener exited");
            }
        });
    }

    // Transcribe bot
    {
        let s = state.clone();
        let ai = ai_service.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            if let Err(e) = aero_server::transcribe_bot::run_until_cancelled(s, ai, cancel).await {
                tracing::error!(error = ?e, "transcribe_bot listener exited");
            }
        });
    }

    // Moderation bot (conditional)
    if std::env::var("AERO_AI_MODERATION").is_ok() {
        let s = state.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            if let Err(e) = aero_server::moderation_bot::run_until_cancelled(s, cancel).await {
                tracing::error!(error = ?e, "moderation_bot listener exited");
            }
        });
        tracing::info!("AI moderation enabled");
    }

    // Unfurl bot (conditional)
    if std::env::var("AERO_UNFURL").is_ok() {
        let s = state.clone();
        let cache = aero_storage::UnfurlRepo::new(state.pg.clone());
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            if let Err(e) = Box::pin(aero_server::unfurl_bot::run_until_cancelled(
                s, cache, cancel,
            ))
            .await
            {
                tracing::error!(error = ?e, "unfurl_bot listener exited");
            }
        });
        tracing::info!("link unfurling enabled");
    }

    // Async full-export worker
    {
        let s = state.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            aero_server::me_export::run_export_dispatcher(s, cancel, 15).await;
        });
    }

    // Notification-bundle flush (conditional, paired with the
    // AERO_NOTIFICATION_BUNDLES builder in boot::services). Activating the
    // builder without this flusher would strand reply notifications in the
    // bundle table, so both are gated on the same flag.
    if std::env::var("AERO_NOTIFICATION_BUNDLES").is_ok() {
        let flush_secs = std::env::var("AERO_NOTIFICATION_BUNDLE_FLUSH_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|s| *s != 0)
            .unwrap_or(10);
        let im = state.im.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(flush_secs));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }
                im.flush_notification_bundles().await;
            }
        });
        tracing::info!(
            interval_secs = flush_secs,
            "notification-bundle flush enabled"
        );
    }

    // PII backfill scan (report-only, conditional). Closes the original P3 gap:
    // PII that landed before the send-path guard was enabled has no retrospective
    // check. This sweeps history read-only and *reports* suspected hits (metric +
    // PII-free log) for an admin to review — it never modifies any message. Gated
    // on AERO_PII_BACKFILL_SCAN, and additionally requires the PII guard config
    // (AERO_PII_GUARD) so the scan uses the deployment's chosen PII classes.
    if std::env::var("AERO_PII_BACKFILL_SCAN").is_ok() {
        if let Some(detector) = aero_im_core::PiiDetector::from_env() {
            let repo = MessageRepo::new(state.pg.clone());
            let cancel = ai_shutdown.clone();
            tracker.spawn(async move {
                aero_server::pii_backfill::run_backfill_scan(&repo, &detector, &cancel).await;
            });
            tracing::info!("PII backfill scan enabled (report-only)");
        } else {
            tracing::warn!(
                "AERO_PII_BACKFILL_SCAN set but AERO_PII_GUARD is off; \
                 backfill scan skipped (no PII classes configured)"
            );
        }
    }

    // Integration machine-request / idempotency receipt retention. Receipts
    // have a bounded seven-day replay window; the repo sweep preserves live
    // fencing leases and enqueues expired, unreferenced integration blobs for
    // the ordinary (reference-aware) blob-GC path below.
    let integration_receipt_sweep_secs =
        std::env::var("AERO__SERVER__INTEGRATION_RECEIPT_SWEEP_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(60);
    if integration_receipt_sweep_secs != 0 {
        let repo = aero_storage::IntegrationRepo::new(state.pg.clone());
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(
                integration_receipt_sweep_secs,
            ));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }
                match repo.sweep_expired_machine_state(500).await {
                    Ok(swept)
                        if swept.requests > 0
                            || swept.notification_receipts > 0
                            || swept.blob_receipts > 0
                            || swept.blob_ledger_releases > 0 =>
                    {
                        tracing::info!(
                            requests = swept.requests,
                            notification_receipts = swept.notification_receipts,
                            blob_receipts = swept.blob_receipts,
                            blob_ledger_releases = swept.blob_ledger_releases,
                            "expired integration machine state swept"
                        );
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(?error, "integration receipt sweep failed");
                    }
                }
            }
        });
    }

    // Blob GC
    {
        let gc_repo = aero_storage::BlobGcRepo::new(state.pg.clone());
        let blob_repo = aero_storage::BlobRepo::new(state.pg.clone());
        let blob = state.blob_store.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }
                let stale_cutoff = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
                if let Err(e) = gc_repo.enqueue_stale_reservations(stale_cutoff).await {
                    tracing::warn!(error = %e, "stale blob-reservation sweep failed");
                }
                match gc_repo.drain(50).await {
                    Err(e) => tracing::warn!(error = %e, "blob_gc drain query failed"),
                    Ok(items) => {
                        for item in items {
                            let id = item.blob_id;
                            if !item.force_delete {
                                match blob_repo.has_live_references(id).await {
                                    Ok(true) => {
                                        if let Err(e) = gc_repo.cancel(id).await {
                                            tracing::warn!(error = %e, blob_id = %id, "blob_gc cancel failed");
                                        }
                                        continue;
                                    }
                                    Ok(false) => {}
                                    Err(e) => {
                                        tracing::warn!(error = %e, blob_id = %id, "blob_gc reference check failed");
                                        continue;
                                    }
                                }
                            }
                            match blob.delete(id).await {
                                Ok(()) => {
                                    if let Err(e) = gc_repo.ack(id).await {
                                        tracing::warn!(error = %e, blob_id = %id, "blob_gc ack failed");
                                    }
                                }
                                Err(e) => tracing::warn!(error = %e, blob_id = %id, "blob_gc delete failed"),
                            }
                            if cancel.is_cancelled() {
                                break;
                            }
                        }
                    }
                }
                if cancel.is_cancelled() {
                    break;
                }
            }
        });
    }
}
