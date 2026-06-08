//! Out-of-office auto-responder bot.
//!
//! Subscribes to `im.room.*` on the bus and watches for incoming
//! `RoomEvent::Message`s. When a message lands in a 1:1 DM (a `direct`-kind room
//! with exactly two members) and the OTHER member has an *active* out-of-office
//! status, the bot posts that member's OOO message back into the DM — ONCE per
//! sender — on the absent member's behalf via
//! [`ImService::send_message`](aero_im_core::ImService::send_message). The
//! one-reply-per-sender rule is enforced by
//! [`OutOfOfficeRepo::should_autoreply`] / [`record_autoreply`], so a chatty
//! sender is not spammed.
//!
//! Delivered OUT-OF-BAND (a bus listener), exactly like [`crate::unfurl_bot`] and
//! [`crate::agent_bot`] — the message hot path (`send_message`) is never touched.
//! The auto-reply is itself a normal message, which re-enters this listener; that
//! is harmless because the bot only ever replies as the OTHER member (so it never
//! replies to itself) and the per-sender dedupe terminates any exchange.
//!
//! Best-effort throughout: a non-DM room, an inactive OOO, an already-replied
//! sender, or a failed send is logged-and-skipped — it never aborts the listener.
//!
//! [`record_autoreply`]: OutOfOfficeRepo::record_autoreply

use aero_common::{MessageEnvelope, RoomEvent};
use aero_storage::{DmRepo, OutOfOfficeRepo};
use futures::StreamExt;
use tracing::{debug, info, warn};

use crate::state::AppState;

/// Run the out-of-office listener until the bus stream ends. Mirrors
/// [`crate::unfurl_bot::run`]'s signature so the server binary spawns it the same
/// way.
///
/// # Errors
/// Returns an error if subscribing to the event bus fails.
pub async fn run(state: AppState) -> anyhow::Result<()> {
    let bus = state.bus.clone();
    let mut stream = bus
        .subscribe("im.room.*", Some("aero-ooo"))
        .await
        .map_err(|e| anyhow::anyhow!("ooo_bot subscribe: {e}"))?;
    info!("ooo_bot listener started");
    while let Some(sub) = stream.next().await {
        if let Ok(RoomEvent::Message(env)) = serde_json::from_slice::<RoomEvent>(sub.payload()) {
            if let Err(e) = handle(&state, env).await {
                warn!(error = ?e, "ooo_bot handle failed");
            }
        }
        let _ = sub.ack().await;
    }
    Ok(())
}

async fn handle(state: &AppState, env: MessageEnvelope) -> anyhow::Result<()> {
    let room = env.message.room_id;
    let sender = env.message.sender_id;

    // Only a 1:1 DM qualifies. Pull the room's members; a 1:1 has exactly two.
    let members = state.rooms.members(room).await?;
    if members.len() != 2 {
        return Ok(());
    }
    // The OOO candidate is the OTHER member (not the sender). If the sender is not
    // one of the two, there is no "other" to reply as — bail.
    let Some(other) = members.iter().copied().find(|&m| m != sender) else {
        return Ok(());
    };
    // Confirm the room really is a 1:1 `direct` DM (kind='direct', member-count 2)
    // using the same identification as `DmRepo`, so the bot only auto-replies in
    // DMs (a 2-member `channel`/`group` room is excluded).
    let dms = DmRepo::new(state.pg.clone());
    if dms.find_direct(sender, other).await? != Some(room) {
        return Ok(());
    }

    let ooo = OutOfOfficeRepo::new(state.pg.clone());
    let now = time::OffsetDateTime::now_utc();

    // The other member must be currently out-of-office...
    if !ooo.is_active(other, now).await? {
        return Ok(());
    }
    // ...and we must not have already auto-replied to this sender.
    if !ooo.should_autoreply(other, sender).await? {
        debug!(ooo_user = %other, %sender, "ooo already auto-replied; skipping");
        return Ok(());
    }
    // Load the message body (still present unless cleared in a race).
    let Some(status) = ooo.get(other).await? else {
        return Ok(());
    };

    // Post the auto-reply as the absent member (never as the sender, so the bot
    // never replies to itself), then record it so the next message from the same
    // sender is suppressed. Record only after a successful send.
    let blocks = vec![aero_common::Block::text(status.message)];
    match state.im.send_message(other, room, blocks, None).await {
        Ok(_) => {
            if let Err(e) = ooo.record_autoreply(other, sender, room).await {
                warn!(error = ?e, ooo_user = %other, %sender, "record_autoreply failed");
            } else {
                info!(ooo_user = %other, %sender, %room, "out-of-office auto-reply sent");
            }
        }
        Err(e) => warn!(error = ?e, ooo_user = %other, %room, "ooo auto-reply send failed"),
    }
    Ok(())
}
