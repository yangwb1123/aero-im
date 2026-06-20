//! Background tasks: sweeps, heartbeats, bots, dispatchers.
//! Spawned AFTER AppState is built; all hold the shutdown token.
use std::sync::Arc;
use tokio_util::task::TaskTracker;
use tokio_util::sync::CancellationToken;
use aero_storage::MessageRepo;
use aero_server::state::AppState;

pub(crate) fn spawn_all(
    tracker: &TaskTracker,
    state: &AppState,
    ai_shutdown: &CancellationToken,
    ai_service: &Arc<aero_ai::AiService>,
) {
    // Bus listener
    {
        let s = state.clone();
        tracker.spawn(async move {
            if let Err(e) = aero_server::ws::run_bus_listener(s).await {
                tracing::error!(error = ?e, "bus listener exited");
            }
        });
    }

    // Live bus listener
    {
        let s = state.clone();
        tracker.spawn(async move {
            if let Err(e) = aero_server::ws::run_live_bus_listener(s).await {
                tracing::error!(error = ?e, "live bus listener exited");
            }
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
        tracker.spawn(async move {
            aero_server::recurring::run_recurring_dispatcher(repo, im, 30).await;
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
                aero_server::saved_search_monitor::run_saved_search_monitor(s, monitor_secs, cancel)
                    .await;
            });
        }
    }

    // Concurrent-viewer sampler
    {
        let pg = state.pg.clone();
        let viewers = state.stream_viewers.clone();
        let stream_repo = state.streams.clone();
        tracker.spawn(async move {
            aero_server::stream_analytics::run_viewer_sampler(pg, viewers, stream_repo).await;
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
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }
                match msgs.list_without_embedding(200).await {
                    Ok(batch) if !batch.is_empty() => {
                        let mut enqueued = 0u32;
                        for m in &batch {
                            let ws = rooms.room_workspace(m.room_id).await.ok().flatten().map(|w| w.to_uuid());
                            match jobs.enqueue_unique(
                                aero_storage::AiJobKind::Embed,
                                m.id.to_uuid(),
                                ws,
                                serde_json::json!({"room_id": m.room_id.to_string(), "backfill": true}),
                            ).await {
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
        tracker.spawn(async move {
            if let Err(e) = aero_server::webhooks::run_webhook_dispatcher(s).await {
                tracing::error!(error = ?e, "webhook dispatcher exited");
            }
        });
    }

    // Webhook retry loop
    {
        let s = state.clone();
        tracker.spawn(async move {
            aero_server::webhooks::run_webhook_retry_loop(s, 30).await;
        });
    }

    // Agent bot
    {
        let s = state.clone();
        let ai = ai_service.clone();
        tracker.spawn(async move {
            if let Err(e) = aero_server::agent_bot::run(s, ai).await {
                tracing::error!(error = ?e, "agent_bot listener exited");
            }
        });
    }

    // Bot event-subscription dispatcher: delivers bus RoomEvents to user bots'
    // `bot_event_subscriptions` webhooks (方向三). Always on — a no-op when no bot
    // has subscribed, like the other built-in bus listeners.
    {
        let s = state.clone();
        tracker.spawn(async move {
            if let Err(e) = aero_server::bot_dispatch::run(s).await {
                tracing::error!(error = ?e, "bot_dispatch listener exited");
            }
        });
    }

    // OOO bot
    {
        let s = state.clone();
        tracker.spawn(async move {
            if let Err(e) = aero_server::ooo_bot::run(s).await {
                tracing::error!(error = ?e, "ooo_bot listener exited");
            }
        });
    }

    // Push bot (conditional)
    if state.push.any_enabled() {
        let s = state.clone();
        tracker.spawn(async move {
            if let Err(e) = aero_server::push_bot::run(s).await {
                tracing::error!(error = ?e, "push_bot listener exited");
            }
        });
        tracing::info!("mobile push dispatch enabled");
    }

    // Golive bot
    {
        let s = state.clone();
        tracker.spawn(async move {
            if let Err(e) = aero_server::golive_bot::run(s).await {
                tracing::error!(error = ?e, "golive_bot listener exited");
            }
        });
    }

    // Transcribe bot
    {
        let s = state.clone();
        let ai = ai_service.clone();
        tracker.spawn(async move {
            if let Err(e) = aero_server::transcribe_bot::run(s, ai).await {
                tracing::error!(error = ?e, "transcribe_bot listener exited");
            }
        });
    }

    // Moderation bot (conditional)
    if std::env::var("AERO_AI_MODERATION").is_ok() {
        let s = state.clone();
        tracker.spawn(async move {
            if let Err(e) = aero_server::moderation_bot::run(s).await {
                tracing::error!(error = ?e, "moderation_bot listener exited");
            }
        });
        tracing::info!("AI moderation enabled");
    }

    // Unfurl bot (conditional)
    if std::env::var("AERO_UNFURL").is_ok() {
        let s = state.clone();
        let cache = aero_storage::UnfurlRepo::new(state.pg.clone());
        tracker.spawn(async move {
            if let Err(e) = aero_server::unfurl_bot::run(s, cache).await {
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
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }
                im.flush_notification_bundles().await;
            }
        });
        tracing::info!(interval_secs = flush_secs, "notification-bundle flush enabled");
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

    // Blob GC
    {
        let gc_repo = aero_storage::BlobGcRepo::new(state.pg.clone());
        let blob = state.blob_store.clone();
        tracker.spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                match gc_repo.drain(50).await {
                    Err(e) => tracing::warn!(error = %e, "blob_gc drain query failed"),
                    Ok(ids) => {
                        for id in ids {
                            match blob.delete(id).await {
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
}
