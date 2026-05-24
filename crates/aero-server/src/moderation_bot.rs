//! AI content moderation (P5 AI 内容审核).
//!
//! Subscribes to `im.room.*`; for each new text message it asks the AI backend
//! to classify the content. Flagged messages are soft-deleted via
//! [`ImService::moderate_delete`](aero_im_core::ImService::moderate_delete),
//! which broadcasts a `Deleted` event so every client removes the message.
//!
//! Opt-in: only runs when an AI backend is configured **and**
//! `AERO_AI_MODERATION` is set — it spends one LLM call per message, so it is
//! off by default. The synchronous `AERO_BLOCKED_WORDS` keyword pre-filter in
//! `ImService::send_message` is independent and always active.

use aero_common::RoomEvent;
use futures::StreamExt;
use tracing::{info, warn};

use crate::state::AppState;

pub async fn run(state: AppState) -> anyhow::Result<()> {
    let Some(ai) = state.ai.clone() else {
        info!("moderation_bot: no AI backend; not started");
        return Ok(());
    };
    let bus = state.bus.clone();
    let mut stream = bus
        .subscribe("im.room.*", Some("aero-moderation"))
        .await
        .map_err(|e| anyhow::anyhow!("moderation_bot subscribe: {e}"))?;
    info!("moderation_bot listener started");

    while let Some(sub) = stream.next().await {
        if let Ok(RoomEvent::Message(env)) = serde_json::from_slice::<RoomEvent>(sub.payload()) {
            let text = env.message.searchable_text();
            if !text.trim().is_empty() {
                match ai.moderate(&text).await {
                    Ok(Some(reason)) => {
                        if let Err(e) = state.im.moderate_delete(env.message.id, &reason).await {
                            warn!(error = ?e, "moderate_delete failed");
                        }
                    }
                    Ok(None) => {}
                    Err(e) => warn!(error = %e, "ai.moderate failed"),
                }
            }
        }
        let _ = sub.ack().await;
    }
    Ok(())
}
