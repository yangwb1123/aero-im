//! "Went live" follower-notification bot.
//!
//! Subscribes to `live.stream.*` on the bus and watches for a stream going live —
//! a [`StreamEvent::Status`] with `status == Live`. When one fires, it loads the
//! stream's owner, fans out to every follower via
//! [`StreamFollowRepo::followers`](aero_storage::StreamFollowRepo::followers), and
//! appends a `"stream_live"` entry to each follower's activity feed via
//! [`ActivityFeedRepo::insert`](aero_storage::ActivityFeedRepo::insert). This is
//! the durable counterpart to the ephemeral danmaku/gift/viewer fan-out the
//! WebSocket bridge ([`crate::ws`]) already does for the same subject.
//!
//! COVERAGE: all three ingest protocols reserve the publisher through
//! [`StreamRepo::mark_live`](aero_storage::StreamRepo::mark_live). That state
//! transition transactionally appends an immutable producer-outbox row; a
//! tracked poller publishes the same stable-id event for WHIP, RTMP and SRT.
//!
//! Best-effort throughout: a non-go-live event, an unknown stream, or a failed
//! insert is logged-and-skipped — it never aborts the listener. A queue-group
//! durable consumer (`aero-golive`) ensures exactly one node fans out each event
//! in a cluster.

use aero_common::{ParticipantId, StreamEvent, StreamStatus};
use aero_storage::{ActivityFeedRepo, StreamFollowRepo};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    state::AppState,
    task_shutdown::{self, NextOrCancelled},
};

/// Run the go-live listener until the bus stream ends. Mirrors
/// [`crate::ooo_bot::run`]'s signature so the server binary spawns it the same way.
///
/// # Errors
/// Returns an error if subscribing to the event bus fails.
pub async fn run(state: AppState) -> anyhow::Result<()> {
    run_until_cancelled(state, CancellationToken::new()).await
}

/// Run until `cancel` is triggered, finishing and acknowledging any event already
/// received before returning.
pub async fn run_until_cancelled(state: AppState, cancel: CancellationToken) -> anyhow::Result<()> {
    let bus = state.bus.clone();
    // Resubscribe across NATS reconnects (mirrors `ws::run_bus_listener`); durable
    // consumer "aero-golive" resumes from its cursor, every event is acked.
    loop {
        let subscribed = task_shutdown::subscribe_or_cancelled(
            &bus,
            "live.stream.*",
            Some("aero-golive"),
            &cancel,
        )
        .await;
        let mut stream = match subscribed {
            None => return Ok(()),
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                warn!(error = %e, "golive_bot subscribe failed; retrying");
                if task_shutdown::delay_or_cancelled(std::time::Duration::from_secs(1), &cancel)
                    .await
                {
                    return Ok(());
                }
                continue;
            }
        };
        info!("golive_bot listener started");
        loop {
            let sub = match task_shutdown::next_or_cancelled(&mut stream, &cancel).await {
                NextOrCancelled::Item(sub) => sub,
                NextOrCancelled::Ended => break,
                NextOrCancelled::Cancelled => return Ok(()),
            };
            if let Some(notice) = decode_go_live(sub.payload()) {
                let stream_id = notice.stream_id;
                if let Err(e) = handle(&state, notice).await {
                    warn!(error = ?e, %stream_id, "golive_bot handle failed");
                }
            }
            let _ = sub.ack().await;
        }
        if cancel.is_cancelled() {
            return Ok(());
        }
        warn!("golive_bot subscription stream ended; resubscribing");
        if task_shutdown::delay_or_cancelled(std::time::Duration::from_secs(1), &cancel).await {
            return Ok(());
        }
    }
}

/// Fan a single go-live out to the streamer's followers.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GoLiveNotice {
    stream_id: ulid::Ulid,
    owner_id: Option<ParticipantId>,
    title: Option<String>,
}

fn decode_go_live(payload: &[u8]) -> Option<GoLiveNotice> {
    let value = serde_json::from_slice::<serde_json::Value>(payload).ok()?;
    let StreamEvent::Status {
        stream_id,
        status: StreamStatus::Live,
    } = serde_json::from_value::<StreamEvent>(value.clone()).ok()?
    else {
        return None;
    };
    let owner_id = value
        .get("owner_id")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok());
    let title = value
        .get("title")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Some(GoLiveNotice {
        stream_id,
        owner_id,
        title,
    })
}

async fn handle(state: &AppState, notice: GoLiveNotice) -> anyhow::Result<()> {
    // New producer-outbox events carry the immutable owner/title snapshot. The
    // row lookup is only a compatibility fallback for lifecycle events produced
    // before that rollout. This keeps a committed notification deliverable even
    // when its source stream row is deleted before NATS recovers.
    let (owner, title) = if let (Some(owner), Some(title)) = (notice.owner_id, notice.title) {
        (owner, title)
    } else {
        let Some(stream) = state.streams.get(notice.stream_id).await? else {
            debug!(
                stream_id = %notice.stream_id,
                "golive_bot: stream and immutable snapshot unavailable; skipping"
            );
            return Ok(());
        };
        (stream.owner_id, stream.title)
    };

    let follows = StreamFollowRepo::new(state.pg.clone());
    let feed = ActivityFeedRepo::new(state.pg.clone());
    let followers = follows.followers(owner).await?;
    if followers.is_empty() {
        return Ok(());
    }

    let summary = format!("{title} is live");
    let mut delivered = 0u64;
    for follower in followers {
        // Idempotent per (follower, stream_id): an at-least-once NATS redelivery of
        // this go-live event re-runs the fan-out, but the ON CONFLICT DO NOTHING
        // makes the replay a no-op rather than a duplicate "X is live" entry.
        match feed
            .insert_go_live(follower, Some(owner), notice.stream_id, &summary)
            .await
        {
            Ok(Some(_)) => delivered += 1,
            Ok(None) => {} // already delivered (redelivery) — no duplicate
            Err(e) => warn!(
                error = ?e,
                %follower,
                stream_id = %notice.stream_id,
                "golive_bot insert failed"
            ),
        }
    }
    info!(
        %owner,
        stream_id = %notice.stream_id,
        delivered,
        "go-live fanned out to followers"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_outbox_snapshot_and_rejects_non_live_events() {
        let stream_id = ulid::Ulid::new();
        let owner_id = ParticipantId::new();
        let payload = serde_json::json!({
            "kind": "status",
            "stream_id": stream_id,
            "status": "live",
            "event_id": uuid::Uuid::new_v4(),
            "owner_id": owner_id,
            "title": "Launch"
        });
        assert_eq!(
            decode_go_live(&serde_json::to_vec(&payload).unwrap()),
            Some(GoLiveNotice {
                stream_id,
                owner_id: Some(owner_id),
                title: Some("Launch".into())
            })
        );

        let ended = serde_json::json!({
            "kind": "status",
            "stream_id": stream_id,
            "status": "ended"
        });
        assert!(decode_go_live(&serde_json::to_vec(&ended).unwrap()).is_none());
    }
}
