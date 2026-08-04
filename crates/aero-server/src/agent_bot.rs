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
use aero_common::{Block, MessageEnvelope, ParticipantId, ParticipantKind, RoomEvent};
use aero_im_core::ImService;
use aero_storage::{ConsumerEventReceiptRepo, ParticipantRepo};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    state::AppState,
    task_shutdown::{self, NextOrCancelled},
};

pub async fn run(state: AppState, ai: Arc<AiService>) -> anyhow::Result<()> {
    run_until_cancelled(state, ai, CancellationToken::new()).await
}

/// Run until `cancel` is triggered, finishing and ACKing any event already
/// received before returning.
pub async fn run_until_cancelled(
    state: AppState,
    ai: Arc<AiService>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let bus = state.bus.clone();
    let receipts = ConsumerEventReceiptRepo::new(state.pg.clone());
    // Resubscribe across NATS reconnects so a dropped stream never permanently
    // stops the bot (mirrors `ws::run_bus_listener`). Durable consumer "aero-bot"
    // resumes from its committed cursor; every message is acked, so none is replayed.
    loop {
        let subscribed =
            task_shutdown::subscribe_or_cancelled(&bus, "im.room.*", Some("aero-bot"), &cancel)
                .await;
        let mut stream = match subscribed {
            None => return Ok(()),
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                warn!(error = %e, "agent_bot subscribe failed; retrying");
                if task_shutdown::delay_or_cancelled(std::time::Duration::from_secs(1), &cancel)
                    .await
                {
                    return Ok(());
                }
                continue;
            }
        };
        info!("agent_bot listener started");
        loop {
            let sub = match task_shutdown::next_or_cancelled(&mut stream, &cancel).await {
                NextOrCancelled::Item(sub) => sub,
                NextOrCancelled::Ended => break,
                NextOrCancelled::Cancelled => return Ok(()),
            };
            let event = serde_json::from_slice::<RoomEvent>(sub.payload());
            let handler_state = &state;
            let handler_ai = &ai;
            let _ =
                crate::consumer_event_receipt::process(&receipts, "aero-bot", sub, || async move {
                    match event {
                        Ok(RoomEvent::Message(env)) => handle(handler_state, handler_ai, env).await,
                        Ok(_) | Err(_) => Ok(()),
                    }
                })
                .await;
        }
        if cancel.is_cancelled() {
            return Ok(());
        }
        warn!("agent_bot subscription stream ended; resubscribing");
        if task_shutdown::delay_or_cancelled(std::time::Duration::from_secs(1), &cancel).await {
            return Ok(());
        }
    }
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
    let workspace = state
        .rooms
        .room_workspace(room)
        .await?
        .map(|value| value.to_uuid());

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

        let usage_context = aero_ai::usage::UsageContext::for_request(
            &env.message.id.to_string(),
            bot.id.to_uuid(),
            &format!("agent_bot:{room}:{}:{question}", bot.id),
            workspace,
        );
        let answer = match ai
            .answer_question_with_usage_context(room, &question, 8, usage_context)
            .await
        {
            Ok((answer, _)) => answer,
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
            blocks.push(Block::Card {
                schema: "citation".into(),
                payload,
            });
        }
        if let Err(e) = im
            .send_message(bot.id, room, blocks, Some(env.message.id), None)
            .await
        {
            warn!(error = ?e, "bot reply send failed");
        } else {
            info!(bot = %bot.id, %room, "bot replied");
        }
    }
    Ok(())
}
