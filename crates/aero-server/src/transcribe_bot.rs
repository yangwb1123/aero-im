//! Voice transcript dispatcher.
//!
//! Subscribes to `im.room.*`, watches for incoming `RoomEvent::Message`s whose
//! blocks contain a `Voice { blob_id, transcript: None }`. For each, fetches
//! the audio bytes from the blob store, calls the configured Transcriber
//! (OpenAI Whisper when `OPENAI_API_KEY` is set, else a placeholder), and then
//! patches the message in place via `MessageRepo::update_voice_transcript`.
//!
//! The patched message is re-broadcast as `RoomEvent::Edited` so connected
//! clients can refresh their UI without re-fetching history.

use std::sync::Arc;

use aero_ai::AiService;
use aero_common::{Block, BlobId, MessageEnvelope, RoomEvent};
use aero_storage::{BlobStore, MessageRepo};
use bytes::Bytes;
use futures::StreamExt;
use tracing::{debug, info, warn};

use crate::state::AppState;

pub async fn run(state: AppState, ai: Arc<AiService>) -> anyhow::Result<()> {
    let bus = state.bus.clone();
    // Resubscribe across NATS reconnects (mirrors `ws::run_bus_listener`); durable
    // consumer "aero-transcribe" resumes from its cursor, every message is acked.
    loop {
        let mut stream = match bus.subscribe("im.room.*", Some("aero-transcribe")).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "transcribe_bot subscribe failed; retrying");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        info!("transcribe_bot listener started");
        while let Some(sub) = stream.next().await {
            match serde_json::from_slice::<RoomEvent>(sub.payload()) {
                Ok(RoomEvent::Message(env)) => {
                    if let Err(e) = handle(&state, &ai, env).await {
                        warn!(error = ?e, "transcribe_bot handle failed");
                    }
                }
                _ => {}
            }
            let _ = sub.ack().await;
        }
        warn!("transcribe_bot subscription stream ended; resubscribing");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

async fn handle(state: &AppState, ai: &Arc<AiService>, env: MessageEnvelope) -> anyhow::Result<()> {
    let msg = env.message;
    let mut targets: Vec<(BlobId, String)> = Vec::new();
    for b in &msg.blocks {
        if let Block::Voice { blob_id, transcript, .. } = b {
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

    let store: Arc<dyn BlobStore> = state.blob_store.clone();
    let messages: &MessageRepo = &state.messages;
    let room_id = msg.room_id;

    for (blob_id, mime) in targets {
        let bytes: Bytes = match store.get(blob_id).await {
            Ok(b) => b,
            Err(e) => {
                warn!(error = ?e, %blob_id, "blob store fetch failed");
                continue;
            }
        };
        let transcript = match ai.transcribe(bytes, &mime).await {
            Ok(t) if t.trim().is_empty() => {
                debug!(%blob_id, "transcribe returned empty");
                continue;
            }
            Ok(t) => t,
            Err(e) => {
                warn!(error = ?e, %blob_id, "transcribe failed");
                continue;
            }
        };
        match messages.update_voice_transcript(msg.id, &transcript).await {
            Ok(Some(updated)) => {
                // Through the stamped seam (ROADMAP3 方向一) so this Edited
                // carries a `seq` like every hot-path publish; best-effort.
                state
                    .im
                    .broadcast_room_event(room_id, RoomEvent::Edited(updated))
                    .await;
                info!(%blob_id, message_id = %msg.id, "voice transcript applied");
            }
            Ok(None) => debug!(message_id = %msg.id, "message missing during transcript update"),
            Err(e) => warn!(error = ?e, message_id = %msg.id, "update_voice_transcript failed"),
        }
    }
    Ok(())
}
