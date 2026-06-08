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
//! The go-live event itself is published by the WHIP ingest path
//! ([`crate::routes`]) right after it marks the stream live; that publish is the
//! seam that lets THIS out-of-band listener react without touching the ingest hot
//! path (exactly like [`crate::ooo_bot`] and [`crate::unfurl_bot`]).
//!
//! COVERAGE NOTE: only the WHIP ingest currently publishes the go-live event. The
//! RTMP ([`aero_live_rtmp`]) and SRT (`aero-live-srt`) ingests call
//! `StreamRepo::mark_live` but carry only a `StreamRepo` (no `EventBus`) — their
//! `LiveIngest::run` signature has no bus to publish on — so a stream that goes
//! live over RTMP/SRT does NOT yet notify followers. To extend coverage, thread an
//! `Arc<dyn EventBus>` through those ingests and publish the same
//! `StreamEvent::Status { stream_id, status: Live }` to `live.stream.{id}` after
//! `mark_live`; this bot then fans it out unchanged.
//!
//! Best-effort throughout: a non-go-live event, an unknown stream, or a failed
//! insert is logged-and-skipped — it never aborts the listener. A queue-group
//! durable consumer (`aero-golive`) ensures exactly one node fans out each event
//! in a cluster.

use aero_common::{StreamEvent, StreamStatus};
use aero_storage::{ActivityFeedRepo, StreamFollowRepo};
use futures::StreamExt;
use tracing::{debug, info, warn};

use crate::state::AppState;

/// Run the go-live listener until the bus stream ends. Mirrors
/// [`crate::ooo_bot::run`]'s signature so the server binary spawns it the same way.
///
/// # Errors
/// Returns an error if subscribing to the event bus fails.
pub async fn run(state: AppState) -> anyhow::Result<()> {
    let bus = state.bus.clone();
    let mut stream = bus
        .subscribe("live.stream.*", Some("aero-golive"))
        .await
        .map_err(|e| anyhow::anyhow!("golive_bot subscribe: {e}"))?;
    info!("golive_bot listener started");
    while let Some(sub) = stream.next().await {
        if let Ok(StreamEvent::Status { stream_id, status: StreamStatus::Live }) =
            serde_json::from_slice::<StreamEvent>(sub.payload())
        {
            if let Err(e) = handle(&state, stream_id).await {
                warn!(error = ?e, %stream_id, "golive_bot handle failed");
            }
        }
        let _ = sub.ack().await;
    }
    Ok(())
}

/// Fan a single go-live out to the streamer's followers.
async fn handle(state: &AppState, stream_id: ulid::Ulid) -> anyhow::Result<()> {
    // Resolve the owner + title from the stream row. A pruned/unknown stream has
    // no one to attribute the notice to, so skip it.
    let Some(stream) = state.streams.get(stream_id).await? else {
        debug!(%stream_id, "golive_bot: stream not found; skipping");
        return Ok(());
    };
    let owner = stream.owner_id;

    let follows = StreamFollowRepo::new(state.pg.clone());
    let feed = ActivityFeedRepo::new(state.pg.clone());
    let followers = follows.followers(owner).await?;
    if followers.is_empty() {
        return Ok(());
    }

    let summary = format!("{} is live", stream.title);
    let mut delivered = 0u64;
    for follower in followers {
        match feed
            .insert(follower, "stream_live", Some(owner), Some(stream_id), &summary)
            .await
        {
            Ok(_) => delivered += 1,
            Err(e) => warn!(error = ?e, %follower, %stream_id, "golive_bot insert failed"),
        }
    }
    info!(%owner, %stream_id, delivered, "go-live fanned out to followers");
    Ok(())
}
