//! Periodic metrics gauge samplers + cross-node heartbeats.
use aero_common::metrics as common_metrics;
use aero_server::state::AppState;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub(crate) fn spawn_all(
    tracker: &TaskTracker,
    state: &AppState,
    ai_shutdown: &CancellationToken,
    jetstream: &Arc<aero_bus::JetStreamBus>,
    call_routes: &Arc<aero_storage::CallRouteRegistry>,
    sfu_router: &aero_live_webrtc::SfuRouter,
    sfu_forwarder: &Arc<aero_live_webrtc::SfuForwarder>,
) {
    // DB pool + WHIP session gauges
    {
        let pool = state.pg.clone();
        let whip = state.whip.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }
                let size = pool.size();
                let idle = u32::try_from(pool.num_idle()).unwrap_or(u32::MAX);
                let in_use = size.saturating_sub(idle);
                let max = pool.options().get_max_connections();
                common_metrics::set_gauge(common_metrics::names::DB_POOL_SIZE, f64::from(max));
                common_metrics::set_gauge(common_metrics::names::DB_POOL_IN_USE, f64::from(in_use));
                common_metrics::set_gauge(
                    common_metrics::names::LIVE_WHIP_SESSIONS,
                    whip.active_sessions() as f64,
                );
            }
        });
    }

    // Index-size (bloat) gauges — companion observability for mig 0136's
    // partial GIN/HNSW slimming. Samples a bounded set of hot `messages`
    // indexes; fail-open (missing index skipped, query error warns).
    {
        let pool = state.pg.clone();
        let cancel = ai_shutdown.clone();
        common_metrics::global().register_help(
            aero_server::metrics::INDEX_SIZE_BYTES,
            common_metrics::MetricKind::Gauge,
            "On-disk size in bytes of a tracked index (label: index).",
        );
        let secs = std::env::var("AERO_INDEX_SIZE_SAMPLE_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|s| *s != 0)
            .unwrap_or(60);
        tracker.spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(secs));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }
                let reported = aero_server::metrics::sample_index_sizes(&pool).await;
                tracing::debug!(reported, "index-size gauges sampled");
            }
        });
    }

    // PostgreSQL maintenance/query-efficiency gauges. These query only bounded
    // schema-owned labels from pg_stat_* and fail open, retaining prior samples
    // when one statistics view is temporarily unavailable.
    {
        let secs = std::env::var("AERO_PG_STATS_SAMPLE_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(60);
        if secs != 0 {
            for (name, help) in [
                (
                    aero_server::metrics::PG_TABLE_DEAD_TUPLE_RATIO,
                    "Estimated dead-tuple ratio for a public table (label: table).",
                ),
                (
                    aero_server::metrics::PG_TABLE_SEQ_SCANS_TOTAL,
                    "PostgreSQL cumulative sequential scans for a public table (label: table).",
                ),
                (
                    aero_server::metrics::PG_INDEX_SCANS_TOTAL,
                    "PostgreSQL cumulative scans for a public index (label: index).",
                ),
                (
                    aero_server::metrics::PG_IDLE_IN_TRANSACTION_COUNT,
                    "Sessions currently idle while holding an open transaction.",
                ),
                (
                    aero_server::metrics::PG_IDLE_IN_TRANSACTION_MAX_SECONDS,
                    "Age in seconds of the oldest idle-in-transaction session.",
                ),
            ] {
                common_metrics::global().register_help(
                    name,
                    common_metrics::MetricKind::Gauge,
                    help,
                );
            }
            let pool = state.pg.clone();
            let cancel = ai_shutdown.clone();
            tracker.spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(secs));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => break,
                        _ = tick.tick() => {}
                    }
                    let sample = aero_server::metrics::sample_pg_health(&pool).await;
                    tracing::debug!(
                        tables = sample.tables,
                        indexes = sample.indexes,
                        idle_activity = sample.idle_activity,
                        "PostgreSQL health gauges sampled"
                    );
                }
            });
        }
    }

    // AI dead-letter queue size gauge
    {
        let dlq_pool = state.pg.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            const KINDS: &[&str] = &["embed", "summarize", "moderate", "answer"];
            let repo = aero_storage::AiJobRepo::new(dlq_pool);
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }
                for kind in KINDS {
                    match repo.count_dead(Some(kind)).await {
                        Ok(n) => common_metrics::set_gauge_labeled(
                            common_metrics::names::AI_DEAD_LETTER_QUEUE_SIZE,
                            n as f64,
                            &[("kind", kind)],
                        ),
                        Err(e) => tracing::warn!(error = %e, %kind, "ai dlq count query failed"),
                    }
                }
            }
        });
    }

    // NATS consumer backlog gauges
    {
        let js = jetstream.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            const CONSUMERS: &[(&str, &str)] = &[("IM_MESSAGES", "aero-server"), ("AI_QUEUE", "aero-ai")];
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }
                for (stream, consumer) in CONSUMERS {
                    match js.consumer_pending(stream, consumer).await {
                        Ok(Some(n)) => common_metrics::set_gauge_labeled(
                            common_metrics::names::NATS_CONSUMER_PENDING_MESSAGES,
                            n as f64,
                            &[("stream", *stream), ("consumer", *consumer)],
                        ),
                        Ok(None) => {}
                        Err(e) => tracing::warn!(error = %e, %stream, %consumer, "consumer_pending query failed"),
                    }
                }
            }
        });
    }

    // Cross-node media subscriber lease/tombstone cleanup. This remains active
    // even when call-route heartbeats are explicitly disabled: an idle egress
    // has no RTP fanout on which to perform lazy pruning.
    {
        let subscribers = state.bridge_subscribers.clone();
        let cancel = ai_shutdown.clone();
        tracker.spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {
                        let pruned = subscribers.prune_expired();
                        if pruned > 0 {
                            tracing::debug!(pruned, "call-bridge subscriber leases pruned");
                        }
                    }
                }
            }
        });
    }

    // Call-route heartbeat
    {
        let reg = call_routes.clone();
        let sfu = sfu_router.clone();
        let url = state.public_base_url.clone();
        let orchestrator = state.call_orchestrator.clone();
        let supervisor = state.call_supervisor.clone();
        let call_roster = state.call_roster.clone();
        let cancel = ai_shutdown.clone();
        let secs = std::env::var("AERO_CALL_ROUTE_HEARTBEAT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30);
        if secs != 0 {
            tracing::info!(interval_secs = secs, "call-route heartbeat enabled");
            tracker.spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(secs));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                tick.tick().await;
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => {
                            tracing::info!("call-route heartbeat: shutdown received, exiting");
                            break;
                        }
                        _ = tick.tick() => {
                            let roster = sfu.roster_snapshot();
                            let calls = roster
                                .iter()
                                .map(|(call, _)| *call)
                                .collect::<std::collections::HashSet<_>>();
                            let mut refreshed = 0u32;
                            for (call, participant) in roster {
                                let Some(generation) = orchestrator
                                    .local_leg_generation(call, participant)
                                    .await
                                else {
                                    continue;
                                };
                                match reg
                                    .heartbeat_generation(
                                        call,
                                        participant,
                                        &url,
                                        generation,
                                    )
                                    .await
                                {
                                    Ok(true) => refreshed += 1,
                                    Ok(false) => tracing::debug!(
                                        %call,
                                        %participant,
                                        generation,
                                        "stale call-route heartbeat rejected"
                                    ),
                                    Err(e) => tracing::warn!(error = ?e, %call, "call-route heartbeat failed"),
                                }
                                match call_roster
                                    .heartbeat_generation(call, participant, generation)
                                    .await
                                {
                                    Ok(true) => {}
                                    Ok(false) => tracing::debug!(
                                        %call,
                                        %participant,
                                        generation,
                                        "stale call-roster heartbeat rejected"
                                    ),
                                    Err(e) => tracing::warn!(
                                        error = ?e,
                                        %call,
                                        "call-roster heartbeat failed"
                                    ),
                                }
                            }
                            if refreshed > 0 {
                                tracing::debug!(refreshed, "call-route TTLs refreshed");
                            }
                            // The same bounded heartbeat is also the repair loop:
                            // discover nodes that joined after our local users,
                            // retry refused/naturally-ended pulls, and remove
                            // targets that disappeared from the census.
                            for call in calls {
                                let Some(topology) = orchestrator.current_topology(call).await else {
                                    continue;
                                };
                                let peers = match topology {
                                    aero_live_webrtc::CallTopology::ServeLocal => Vec::new(),
                                    aero_live_webrtc::CallTopology::BridgeTo(peers) => peers,
                                };
                                let (spawned, cancelled) =
                                    supervisor.reconcile_bridges(call, &peers).await;
                                if spawned > 0 || cancelled > 0 {
                                    tracing::debug!(
                                        %call,
                                        desired = peers.len(),
                                        spawned,
                                        cancelled,
                                        "call-route bridges reconciled"
                                    );
                                }
                            }
                        }
                    }
                }
            });
        }
    }

    // Stream-route heartbeat
    {
        let routes = state.stream_routes.clone();
        let stream_repo = state.streams.clone();
        let url = state.public_base_url.clone();
        let cancel = ai_shutdown.clone();
        let secs = std::env::var("AERO_STREAM_ROUTE_HEARTBEAT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30);
        if secs != 0 {
            tracing::info!(interval_secs = secs, "stream-route heartbeat enabled");
            tracker.spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(secs));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                tick.tick().await;
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => {
                            tracing::info!("stream-route heartbeat: shutdown received, exiting");
                            break;
                        }
                        _ = tick.tick() => {
                            match stream_repo.list_live().await {
                                Ok(live) => {
                                    let mut refreshed = 0u32;
                                    for stream in &live {
                                        match routes.heartbeat(stream.id, &url).await {
                                            Ok(()) => refreshed += 1,
                                            Err(e) => tracing::warn!(error = ?e, stream = %stream.id, "stream-route heartbeat failed"),
                                        }
                                    }
                                    if refreshed > 0 {
                                        tracing::debug!(refreshed, "stream-route TTLs refreshed");
                                    }
                                }
                                Err(e) => tracing::warn!(error = ?e, "stream-route heartbeat: list_live failed"),
                            }
                        }
                    }
                }
            });
        }
    }

    // SFU publisher-facing REMB housekeeping tick.
    //
    // The adaptive-bitrate loop toward publishers folds each subscriber's
    // bandwidth estimate into one aggregated REMB per published track
    // (`SfuForwarder`'s `PublisherRembAggregator`). Subscriber feedback emits a
    // fresh aggregate only when it crosses the hysteresis band; `tick_remb`
    // re-emits the current target on a steady cadence so a publisher's encoder
    // keeps a live REMB even when subscriber feedback is flat. Without this
    // periodic driver the only caller was the unit tests — the wiring gap this
    // closes.
    //
    // STAGING SEAM: `tick_remb` + the subscriber-feedback path enqueue
    // `PendingRemb`s (the aggregation/encode half is fully unit-tested in
    // `aero-live-webrtc::forward`); we drain them here via `poll_remb_requests`.
    // Writing each drained REMB onto the *live* publisher peer's outbound RTCP
    // stream over real DTLS-SRTP — so an OBS/browser encoder actually receives
    // it — requires the per-peer `SfuPeer` set, which is only populated by a
    // real WebRTC offer through `SfuMediaSession` (the unbuildable infra seam,
    // needing a browser/second node). Until that E2E ingress exists the queue
    // stays empty in CI, so this tick is a fail-safe no-op there; it lights up
    // automatically once live SFU peers feed `on_subscriber_rtcp`.
    {
        let fwd = sfu_forwarder.clone();
        let cancel = ai_shutdown.clone();
        let secs = std::env::var("AERO_SFU_REMB_TICK_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(1);
        if secs != 0 {
            tracing::info!(interval_secs = secs, "SFU publisher REMB tick enabled");
            tracker.spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(secs));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                tick.tick().await;
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => {
                            tracing::info!("SFU REMB tick: shutdown received, exiting");
                            break;
                        }
                        _ = tick.tick() => {
                            // Re-evaluate every published track's aggregate and
                            // enqueue any REMB whose target moved.
                            fwd.tick_remb();
                            // Drain enqueued REMBs. The relay onto the live
                            // publisher peer's DTLS-SRTP RTCP stream is the
                            // staging seam (no live SfuPeer set without a real
                            // WebRTC offer ingress), so for now we only surface
                            // the queue depth for observability.
                            let pending = fwd.poll_remb_requests();
                            if !pending.is_empty() {
                                tracing::debug!(
                                    count = pending.len(),
                                    "SFU REMB tick: aggregated publisher REMBs ready (relay to publisher RTCP is the DTLS-SRTP staging seam)"
                                );
                            }
                        }
                    }
                }
            });
        }
    }
}
