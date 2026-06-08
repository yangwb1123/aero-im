//! Mobile push-dispatch bot (ROADMAP 方向二).
//!
//! Subscribes to `im.room.*` on the bus and watches for `RoomEvent::Notify`
//! events — the targeted "you were mentioned / replied-to" signal that already
//! drives the in-app badge. For each such event it looks up the mentioned
//! participant's registered FCM/APNs device tokens and sends a push so an
//! offline mobile client still surfaces the mention.
//!
//! Delivered OUT-OF-BAND (a bus listener), exactly like [`crate::ooo_bot`] and
//! [`crate::unfurl_bot`] — the message hot path (`ImService::send_message`) is
//! never touched, so a slow/failing FCM round-trip can never add latency to or
//! fail a send. Best-effort throughout: a missing message, a recipient with no
//! tokens, an unconfigured platform, or a failed send is logged-and-skipped.
//!
//! Dead-token reaping: when a gateway returns [`PushError::Rejected`] (the
//! upstream accepted the request but refused the token — i.e. the app was
//! uninstalled), the token row is deleted so it is not retried forever.
//!
//! The binary only spawns this listener when at least one platform gateway is
//! configured ([`PushGateways::any_enabled`]); with push disabled it is inert.

use aero_common::{NotificationKind, RoomEvent};
use aero_push::{PushError, PushPayload};
use aero_storage::PushTokenRepo;
use futures::StreamExt;
use tracing::{debug, info, warn};

use crate::state::AppState;

/// Maximum characters of the message body included in the push preview. Keeps
/// the notification compact and avoids leaking long content into the lock screen.
const PREVIEW_CHARS: usize = 140;

/// Run the push-dispatch listener until the bus stream ends. Mirrors
/// [`crate::ooo_bot::run`]'s signature so the server binary spawns it the same way.
///
/// # Errors
/// Returns an error if subscribing to the event bus fails.
pub async fn run(state: AppState) -> anyhow::Result<()> {
    let bus = state.bus.clone();
    let mut stream = bus
        .subscribe("im.room.*", Some("aero-push"))
        .await
        .map_err(|e| anyhow::anyhow!("push_bot subscribe: {e}"))?;
    info!("push_bot listener started");
    while let Some(sub) = stream.next().await {
        if let Ok(RoomEvent::Notify { message_id, mentioned, by, kind, .. }) =
            serde_json::from_slice::<RoomEvent>(sub.payload())
        {
            if let Err(e) = handle(&state, message_id, mentioned, by, kind).await {
                warn!(error = ?e, "push_bot handle failed");
            }
        }
        let _ = sub.ack().await;
    }
    Ok(())
}

async fn handle(
    state: &AppState,
    message_id: aero_common::MessageId,
    mentioned: aero_common::ParticipantId,
    by: aero_common::ParticipantId,
    kind: NotificationKind,
) -> anyhow::Result<()> {
    // No registered devices ⇒ nothing to do (the common case for web-only users).
    let token_repo = PushTokenRepo::new(state.pg.clone());
    let tokens = token_repo.list_for_participant(mentioned).await?;
    if tokens.is_empty() {
        return Ok(());
    }

    // Build the notification preview from the actual message + the sender's name.
    let sender_name = state
        .participants
        .get(by)
        .await
        .ok()
        .flatten()
        .map_or_else(|| "Someone".to_string(), |p| p.display_name);
    // Fetch the message once: drives both the body preview and the deep-link room.
    let msg = state.messages.get(message_id).await?;
    let body = msg.as_ref().map(|m| preview(&m.searchable_text())).unwrap_or_default();
    let room_id = msg.as_ref().map(|m| m.room_id.to_string());
    let title = match kind {
        NotificationKind::Mention => format!("{sender_name} mentioned you"),
        NotificationKind::Reply => format!("{sender_name} replied"),
        NotificationKind::Reaction => format!("{sender_name} reacted to your message"),
    };

    let payload = PushPayload {
        title,
        body,
        room_id,
        message_id: Some(message_id.to_string()),
        badge: None,
    };

    for t in tokens {
        let Some(gateway) = state.push.for_platform(&t.platform) else {
            debug!(platform = %t.platform, "no gateway configured for platform; skipping");
            continue;
        };
        match gateway.send(&t.token, &payload).await {
            Ok(()) => debug!(%mentioned, platform = %t.platform, "push delivered"),
            Err(PushError::Rejected(reason)) => {
                // Dead/unregistered token — drop it so we stop trying.
                warn!(%mentioned, platform = %t.platform, %reason, "push token rejected; reaping");
                if let Err(e) = token_repo.unregister(mentioned, &t.token).await {
                    warn!(error = ?e, "failed to reap rejected push token");
                }
            }
            Err(e) => warn!(%mentioned, platform = %t.platform, error = %e, "push send failed"),
        }
    }
    Ok(())
}

/// Truncate `text` to [`PREVIEW_CHARS`] on a char boundary, appending an ellipsis
/// when truncated. Collapses internal newlines so the preview is single-line.
#[must_use]
fn preview(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= PREVIEW_CHARS {
        return collapsed;
    }
    let truncated: String = collapsed.chars().take(PREVIEW_CHARS).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_passes_through_short_text() {
        assert_eq!(preview("hello world"), "hello world");
    }

    #[test]
    fn preview_collapses_whitespace() {
        assert_eq!(preview("hello\n\n  world\t!"), "hello world !");
    }

    #[test]
    fn preview_truncates_on_char_boundary_with_ellipsis() {
        let long = "x".repeat(200);
        let p = preview(&long);
        assert_eq!(p.chars().count(), PREVIEW_CHARS + 1); // 140 + ellipsis
        assert!(p.ends_with('…'));
    }

    #[test]
    fn preview_does_not_split_multibyte() {
        // 200 multibyte chars — truncation must not panic on a byte boundary.
        let long = "界".repeat(200);
        let p = preview(&long);
        assert_eq!(p.chars().count(), PREVIEW_CHARS + 1);
    }
}
