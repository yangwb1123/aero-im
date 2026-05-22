//! Auto-responder: AI agents respond when @mentioned in a room.
//!
//! Subscribes to `im.room.*` on the bus. When a `RoomEvent::Message` arrives,
//! checks if any of the room's bot/agent participants are `@`-mentioned. For
//! each mention, calls the AI service to compose a reply and posts it back into
//! the room as the bot's identity.
//!
//! This deliberately bypasses the AI worker queue — for chat replies the
//! request/response loop is interactive (sub-second target) and routing it
//! through `ai_jobs` adds latency without buying durability we need.

use std::sync::Arc;

use aero_ai::AiService;
use aero_common::{
    Block, MessageEnvelope, ParticipantId, ParticipantKind, RoomEvent,
};
use aero_im_core::ImService;
use aero_storage::ParticipantRepo;
use futures::StreamExt;
use tracing::{debug, info, warn};

use crate::state::AppState;

pub async fn run(state: AppState, ai: Arc<AiService>) -> anyhow::Result<()> {
    let bus = state.bus.clone();
    let mut stream = bus
        .subscribe("im.room.*", Some("aero-bot"))
        .await
        .map_err(|e| anyhow::anyhow!("agent_bot subscribe: {e}"))?;
    info!("agent_bot listener started");
    while let Some(sub) = stream.next().await {
        match serde_json::from_slice::<RoomEvent>(sub.payload()) {
            Ok(RoomEvent::Message(env)) => {
                if let Err(e) = handle(&state, &ai, env).await {
                    warn!(error = ?e, "agent_bot handle failed");
                }
            }
            Ok(_) | Err(_) => {}
        }
        let _ = sub.ack().await;
    }
    Ok(())
}

async fn handle(state: &AppState, ai: &Arc<AiService>, env: MessageEnvelope) -> anyhow::Result<()> {
    let mentions: Vec<ParticipantId> = env
        .message
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Mention { participant } => Some(*participant),
            _ => None,
        })
        .collect();
    if mentions.is_empty() {
        return Ok(());
    }

    let participants: &ParticipantRepo = &state.participants;
    let im: &Arc<ImService> = &state.im;
    let room = env.message.room_id;
    let sender = env.message.sender_id;

    for mention in mentions {
        let bot = match participants.get(mention).await? {
            Some(p) => p,
            None => continue,
        };
        if !matches!(bot.kind, ParticipantKind::Bot | ParticipantKind::Agent) {
            continue;
        }
        if bot.id == sender {
            continue; // bot shouldn't reply to itself
        }
        if !state.rooms.is_member(room, bot.id).await.unwrap_or(false) {
            debug!(bot = %bot.id, %room, "bot not a member; skipping");
            continue;
        }

        let question = env.message.searchable_text();
        if question.trim().is_empty() {
            continue;
        }

        let answer = match ai.answer_question(room, &question, 8).await {
            Ok(a) => a,
            Err(e) => {
                warn!(error = ?e, "ai.answer_question failed");
                aero_ai::AnswerResult {
                    answer: format!("(AI 暂不可用:{e})"),
                    citations: Vec::new(),
                }
            }
        };

        let mut blocks: Vec<Block> = Vec::new();
        blocks.push(Block::text(answer.answer));
        // Append citations as a compact card.
        if !answer.citations.is_empty() {
            let payload = serde_json::json!({
                "title": "引用",
                "body": answer.citations.iter().map(|m| m.to_string()).collect::<Vec<_>>().join(", "),
            });
            blocks.push(Block::Card { schema: "citation".into(), payload });
        }
        if let Err(e) = im.send_message(bot.id, room, blocks, Some(env.message.id)).await {
            warn!(error = ?e, "bot reply send failed");
        } else {
            info!(bot = %bot.id, %room, "bot replied");
        }
    }
    Ok(())
}
