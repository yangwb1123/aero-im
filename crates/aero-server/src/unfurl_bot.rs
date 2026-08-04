//! Link-unfurl dispatcher (Open Graph URL previews, Slack-style).
//!
//! Subscribes to `im.room.*` and watches for incoming `RoomEvent::Message`s
//! whose `Text` blocks contain URL(s). For each URL it consults the
//! [`UnfurlRepo`] cache, then (on a miss) fetches the page via the injectable
//! [`Unfurler`] seam, parses its Open Graph metadata, and caches the result. Any
//! resolved previews are appended to the message as `link_preview`
//! [`Block::Card`]s — the patched message is persisted via the message repo
//! ([`MessageRepo::edit`]) and re-broadcast as `RoomEvent::Edited` so connected
//! clients refresh without re-fetching history. This mirrors the
//! [`crate::transcribe_bot`] pattern (a system action that patches a message
//! in place and re-broadcasts it), bypassing the sender-only
//! `ImService::edit_message` check because the unfurler acts on the system's
//! behalf, not the author's.
//!
//! Best-effort throughout: an unreachable URL, a parse that yields nothing, or a
//! failed cache/persist is logged and skipped — it never aborts the listener.
//! Messages that already carry a `link_preview` card are skipped entirely, so the
//! `Edited` re-broadcast our own patch produces does not re-trigger unfurling.
//!
//! The live HTTP fetch is the only impure edge and lives behind [`Unfurler`];
//! the URL extraction, OG parsing, card building, and TTL/cache logic are pure
//! and unit-tested in [`aero_storage::unfurl`]. The actual `tokio::spawn` of this
//! listener is opt-in behind the `AERO_UNFURL` env var (see the server binary).

use std::sync::Arc;

use aero_common::{Block, MessageEnvelope, RoomEvent};
use aero_storage::unfurl::{
    extract_urls, is_link_preview_card, parse_og, preview_to_card, LinkPreview, ReqwestUnfurler,
    UnfurlRepo, Unfurler,
};
use aero_storage::{ConsumerEventReceiptRepo, MessageRepo};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    state::AppState,
    task_shutdown::{self, NextOrCancelled},
};

/// Run the unfurl listener until the bus stream ends. `cache` is the cache repo
/// (constructed by the binary from the shared pool); the real [`ReqwestUnfurler`]
/// is used for the live fetch.
pub async fn run(state: AppState, cache: UnfurlRepo) -> anyhow::Result<()> {
    run_until_cancelled(state, cache, CancellationToken::new()).await
}

/// Run with the production transport until `cancel` is triggered.
pub async fn run_until_cancelled(
    state: AppState,
    cache: UnfurlRepo,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    run_with_until_cancelled(state, cache, Arc::new(ReqwestUnfurler::new()), cancel).await
}

/// Run the listener with an injected [`Unfurler`] — the seam used to drive the
/// loop offline in tests (real callers use [`run`], which supplies the reqwest
/// transport).
pub async fn run_with(
    state: AppState,
    cache: UnfurlRepo,
    fetcher: Arc<dyn Unfurler>,
) -> anyhow::Result<()> {
    run_with_until_cancelled(state, cache, fetcher, CancellationToken::new()).await
}

/// Run with an injected transport until `cancel` is triggered, finishing and
/// ACKing any event already received before returning.
pub async fn run_with_until_cancelled(
    state: AppState,
    cache: UnfurlRepo,
    fetcher: Arc<dyn Unfurler>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let bus = state.bus.clone();
    let receipts = ConsumerEventReceiptRepo::new(state.pg.clone());
    // Resubscribe across NATS reconnects (mirrors `ws::run_bus_listener`); durable
    // consumer "aero-unfurl" resumes from its cursor, every message is acked.
    loop {
        let subscribed =
            task_shutdown::subscribe_or_cancelled(&bus, "im.room.*", Some("aero-unfurl"), &cancel)
                .await;
        let mut stream = match subscribed {
            None => return Ok(()),
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                warn!(error = %e, "unfurl_bot subscribe failed; retrying");
                if task_shutdown::delay_or_cancelled(std::time::Duration::from_secs(1), &cancel)
                    .await
                {
                    return Ok(());
                }
                continue;
            }
        };
        info!("unfurl_bot listener started");
        loop {
            let sub = match task_shutdown::next_or_cancelled(&mut stream, &cancel).await {
                NextOrCancelled::Item(sub) => sub,
                NextOrCancelled::Ended => break,
                NextOrCancelled::Cancelled => return Ok(()),
            };
            let event = serde_json::from_slice::<RoomEvent>(sub.payload());
            let handler_state = &state;
            let handler_cache = &cache;
            let handler_fetcher = fetcher.as_ref();
            let _ = crate::consumer_event_receipt::process(
                &receipts,
                "aero-unfurl",
                sub,
                || async move {
                    match event {
                        Ok(RoomEvent::Message(env)) => {
                            handle(handler_state, handler_cache, handler_fetcher, env).await
                        }
                        Ok(_) | Err(_) => Ok(()),
                    }
                },
            )
            .await;
        }
        if cancel.is_cancelled() {
            return Ok(());
        }
        warn!("unfurl_bot subscription stream ended; resubscribing");
        if task_shutdown::delay_or_cancelled(std::time::Duration::from_secs(1), &cancel).await {
            return Ok(());
        }
    }
}

async fn handle(
    state: &AppState,
    cache: &UnfurlRepo,
    fetcher: &dyn Unfurler,
    env: MessageEnvelope,
) -> anyhow::Result<()> {
    let msg = env.message;

    // Loop guard: if the message already carries a link-preview card it was either
    // already unfurled by us (the Edited re-broadcast lands here too) or arrived
    // with one — either way, do nothing.
    if msg.blocks.iter().any(is_link_preview_card) {
        return Ok(());
    }

    let urls = extract_urls(&msg.blocks);
    if urls.is_empty() {
        return Ok(());
    }

    // Resolve each URL to a preview (cache-first), keeping only previews that
    // carry real metadata — a bare {url} card adds no value over the link itself.
    let mut cards: Vec<Block> = Vec::new();
    for url in urls {
        match resolve(cache, fetcher, &url).await {
            Some(preview) if preview.has_metadata() => cards.push(preview_to_card(&preview)),
            _ => {}
        }
    }
    if cards.is_empty() {
        return Ok(());
    }

    let messages: &MessageRepo = &state.messages;
    // Append the preview cards to the message's existing blocks and persist. Use
    // the message repo's `edit` directly (a system patch, like transcribe_bot),
    // not `ImService::edit_message`, which is sender-only.
    let mut new_blocks = msg.blocks.clone();
    new_blocks.extend(cards);

    // Optimistic lock: fetch the current version before editing (migration 0157).
    let version = match messages.get_version(msg.id).await {
        Ok(Some(v)) => v,
        Ok(None) => {
            debug!(msg_id = %msg.id, "unfurl skip: message deleted concurrently");
            return Ok(());
        }
        Err(_) => {
            warn!(msg_id = %msg.id, "unfurl: failed to read version, skipping");
            return Ok(());
        }
    };

    let traceparent = aero_common::telemetry::current_traceparent();
    match messages
        .edit_outboxed_system(msg.id, new_blocks, version, None, traceparent.as_deref())
        .await
    {
        Ok(Some(updated)) => {
            if let Err(error) = state.im.dispatch_event_outbox_id(updated.outbox_id).await {
                warn!(
                    ?error,
                    outbox_id = %updated.outbox_id,
                    message_id = %msg.id,
                    "unfurl edited-event fast dispatch failed"
                );
            }
            if let Err(error) = state.im.dispatch_message_side_effects_for(msg.id).await {
                warn!(?error, message_id = %msg.id, "unfurl AI side-effect dispatch failed");
            }
            info!(message_id = %msg.id, "link preview(s) attached");
        }
        Ok(None) => debug!(message_id = %msg.id, "message missing/deleted during unfurl edit"),
        Err(e) => warn!(error = ?e, message_id = %msg.id, "unfurl edit failed"),
    }
    Ok(())
}

/// Resolve one URL to a [`LinkPreview`]: return a fresh cached preview if present,
/// otherwise fetch + parse + cache. `None` when the page can't be fetched (the
/// caller skips it). Cache read/write failures are logged but non-fatal — a cache
/// glitch degrades to a live fetch, never an error.
async fn resolve(cache: &UnfurlRepo, fetcher: &dyn Unfurler, url: &str) -> Option<LinkPreview> {
    match cache.get(url).await {
        Ok(Some(hit)) => {
            debug!(%url, "unfurl cache hit");
            return Some(hit);
        }
        Ok(None) => {}
        Err(e) => warn!(error = ?e, %url, "unfurl cache read failed; fetching"),
    }

    let html = fetcher.fetch(url).await?;
    let preview = parse_og(&html, url);
    if let Err(e) = cache.put(url, &preview).await {
        warn!(error = ?e, %url, "unfurl cache write failed");
    }
    Some(preview)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_storage::unfurl::FakeUnfurler;

    // The listener's I/O (bus + DB) is integration-tested elsewhere; here we lock
    // in the loop-guard predicate the handler relies on, since misfiring it would
    // re-unfurl our own Edited re-broadcast forever.
    #[test]
    fn already_unfurled_message_is_detected() {
        let preview = LinkPreview {
            url: "https://x.com".into(),
            title: Some("T".into()),
            ..LinkPreview::default()
        };
        let blocks = [Block::text("https://x.com"), preview_to_card(&preview)];
        assert!(
            blocks.iter().any(is_link_preview_card),
            "guard catches our own card"
        );

        let plain = [Block::text("https://x.com")];
        assert!(
            !plain.iter().any(is_link_preview_card),
            "plain message not skipped"
        );
    }

    // Confirm the seam wiring compiles + the fake yields a parseable preview, so
    // `run_with(_, _, Arc<dyn Unfurler>)` is the offline-drivable entry point.
    #[test]
    fn fake_seam_resolves_to_preview() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let fetcher: Arc<dyn Unfurler> = Arc::new(FakeUnfurler::with_html(
                r#"<meta property="og:title" content="Hi"><title>t</title>"#,
            ));
            let html = fetcher
                .fetch("https://x.com")
                .await
                .expect("fake returns html");
            let preview = parse_og(&html, "https://x.com");
            assert_eq!(preview.title.as_deref(), Some("Hi"));
        });
    }
}
