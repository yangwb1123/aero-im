//! Voice transcript dispatcher.
//!
//! Subscribes to `im.room.*`, watches for incoming `RoomEvent::Message`s whose
//! blocks contain a `Voice { blob_id, transcript: None }`. For each, fetches
//! the audio bytes from the blob store, calls the configured Transcriber
//! (`OpenAI` Whisper when `OPENAI_API_KEY` is set, else a placeholder), and then
//! patches the message in place via `MessageRepo::update_voice_transcript_outboxed`
//! — the transactional, outbox-appending variant (the lockless
//! `update_voice_transcript` SQL path is test-only). The recall fence for this
//! path is the row lock + Rust re-check of `recalled_at`, NOT a SQL WHERE
//! clause; see `message/recall_index_fence_tests.rs`.
//!
//! The patched message is re-broadcast as `RoomEvent::Edited` so connected
//! clients can refresh their UI without re-fetching history.

use std::sync::Arc;

use aero_ai::{usage::UsageContext, AiError, AiService};
use aero_common::{BlobId, Block, MessageEnvelope, RoomEvent};
use aero_storage::{BlobStore, ConsumerEventReceiptRepo, MessageRepo};
use bytes::Bytes;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    state::AppState,
    task_shutdown::{self, NextOrCancelled},
};

pub async fn run(state: AppState, ai: Arc<AiService>) -> anyhow::Result<()> {
    run_until_cancelled(state, ai, CancellationToken::new()).await
}

/// Run until `cancel` is triggered, finishing and `ACKing` any event already
/// received before returning.
pub async fn run_until_cancelled(
    state: AppState,
    ai: Arc<AiService>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let bus = state.bus.clone();
    let receipts = ConsumerEventReceiptRepo::new(state.pg.clone());
    // Resubscribe across NATS reconnects (mirrors `ws::run_bus_listener`); durable
    // consumer "aero-transcribe" resumes from its cursor, every message is acked.
    loop {
        let subscribed = task_shutdown::subscribe_or_cancelled(
            &bus,
            "im.room.*",
            Some("aero-transcribe"),
            &cancel,
        )
        .await;
        let mut stream = match subscribed {
            None => return Ok(()),
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                warn!(error = %e, "transcribe_bot subscribe failed; retrying");
                if task_shutdown::delay_or_cancelled(std::time::Duration::from_secs(1), &cancel)
                    .await
                {
                    return Ok(());
                }
                continue;
            }
        };
        info!("transcribe_bot listener started");
        loop {
            let sub = match task_shutdown::next_or_cancelled(&mut stream, &cancel).await {
                NextOrCancelled::Item(sub) => sub,
                NextOrCancelled::Ended => break,
                NextOrCancelled::Cancelled => return Ok(()),
            };
            let event = serde_json::from_slice::<RoomEvent>(sub.payload());
            let handler_state = &state;
            let handler_ai = &ai;
            let _ = crate::consumer_event_receipt::process(
                &receipts,
                "aero-transcribe",
                sub,
                || async move {
                    match event {
                        Ok(RoomEvent::Message(env)) => handle(handler_state, handler_ai, env).await,
                        Ok(_) | Err(_) => Ok(()),
                    }
                },
            )
            .await;
        }
        if cancel.is_cancelled() {
            return Ok(());
        }
        warn!("transcribe_bot subscription stream ended; resubscribing");
        if task_shutdown::delay_or_cancelled(std::time::Duration::from_secs(1), &cancel).await {
            return Ok(());
        }
    }
}

async fn handle(state: &AppState, ai: &Arc<AiService>, env: MessageEnvelope) -> anyhow::Result<()> {
    let msg = env.message;
    let mut targets: Vec<(BlobId, String)> = Vec::new();
    for b in &msg.blocks {
        if let Block::Voice {
            blob_id,
            transcript,
            ..
        } = b
        {
            if transcript.is_none() {
                // Look up the blob's mime to feed Whisper correctly.
                let mime = match state.blobs.get(*blob_id).await {
                    Ok(Some(meta)) => meta.mime,
                    Ok(None) => continue,
                    Err(e) => {
                        warn!(error = ?e, %blob_id, "blob meta lookup failed");
                        continue;
                    }
                };
                targets.push((*blob_id, mime));
            }
        }
    }
    if targets.is_empty() {
        return Ok(());
    }

    let workspace = state
        .rooms
        .room_workspace(msg.room_id)
        .await?
        .map(|value| value.to_uuid());
    let usage_context = UsageContext::for_message(msg.id.to_uuid(), workspace);
    let store: Arc<dyn BlobStore> = state.blob_store.clone();
    let messages: &MessageRepo = &state.messages;
    for (blob_id, mime) in targets {
        let bytes: Bytes = match store.get(blob_id).await {
            Ok(b) => b,
            Err(e) => {
                warn!(error = ?e, %blob_id, "blob store fetch failed");
                continue;
            }
        };
        let operation = format!("whisper_transcribe:{blob_id}");
        let transcript = match ai
            .transcribe_with_context(bytes, &mime, usage_context, &operation)
            .await
        {
            Ok(t) if t.trim().is_empty() => {
                debug!(%blob_id, "transcribe returned empty");
                continue;
            }
            Ok(t) => t,
            Err(AiError::Storage(error)) if error.starts_with("AI usage accounting:") => {
                return Err(anyhow::anyhow!(
                    "voice transcription usage accounting failed for {blob_id}: {error}"
                ));
            }
            Err(e) => {
                warn!(error = ?e, %blob_id, "transcribe failed");
                continue;
            }
        };
        let traceparent = aero_common::telemetry::current_traceparent();
        match messages
            .update_voice_transcript_outboxed(msg.id, &transcript, traceparent.as_deref())
            .await
        {
            Ok(Some(updated)) => {
                if let Err(error) = state.im.dispatch_event_outbox_id(updated.outbox_id).await {
                    warn!(
                        ?error,
                        outbox_id = %updated.outbox_id,
                        message_id = %msg.id,
                        "transcript edited-event fast dispatch failed"
                    );
                }
                if let Err(error) = state.im.dispatch_message_side_effects_for(msg.id).await {
                    warn!(?error, message_id = %msg.id, "transcript AI side-effect dispatch failed");
                }
                info!(%blob_id, message_id = %msg.id, "voice transcript applied");
            }
            Ok(None) => debug!(message_id = %msg.id, "message missing during transcript update"),
            Err(e) => warn!(error = ?e, message_id = %msg.id, "update_voice_transcript failed"),
        }
    }
    Ok(())
}
