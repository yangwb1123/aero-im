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

use aero_common::{NotificationKind, ParticipantId, RoomEvent};
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
    // Resubscribe across NATS reconnects (mirrors `ws::run_bus_listener`); durable
    // consumer "aero-push" resumes from its cursor, every message is acked.
    loop {
        let mut stream = match bus.subscribe("im.room.*", Some("aero-push")).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "push_bot subscribe failed; retrying");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        info!("push_bot listener started");
        while let Some(sub) = stream.next().await {
            match serde_json::from_slice::<RoomEvent>(sub.payload()) {
                Ok(RoomEvent::Notify { message_id, mentioned, by, kind, .. }) => {
                    if let Err(e) = handle(&state, message_id, mentioned, by, kind).await {
                        warn!(error = ?e, "push_bot handle failed");
                    }
                }
                // Batched notify (ROADMAP 方向二): push each recipient individually,
                // exactly as the old per-recipient Notify events did.
                Ok(RoomEvent::NotifyBatch { message_id, by, recipients, .. }) => {
                    for target in recipients {
                        if let Err(e) =
                            handle(&state, message_id, target.participant, by, target.kind).await
                        {
                            warn!(error = ?e, "push_bot handle failed");
                        }
                    }
                }
                _ => {}
            }
            let _ = sub.ack().await;
        }
        warn!("push_bot subscription stream ended; resubscribing");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

async fn handle(
    state: &AppState,
    message_id: aero_common::MessageId,
    mentioned: ParticipantId,
    by: ParticipantId,
    kind: NotificationKind,
) -> anyhow::Result<()> {
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

    push_to_participant(state, mentioned, &payload).await;
    Ok(())
}

/// Fan a `payload` out to every device `recipient` has registered, best-effort.
///
/// Looks up the recipient's FCM/APNs tokens, sends via each configured platform
/// gateway, and reaps [`PushError::Rejected`] tokens (the upstream accepted the
/// request but refused the token — i.e. the app was uninstalled) so they are not
/// retried forever. Never returns an error: a missing token list, an unconfigured
/// platform, or a failed send is logged-and-skipped, so callers on a hot path
/// (the WS call handler, the task-assign handler) stay best-effort.
///
/// This is the ONE dispatch code path shared by the bus [`run`] listener and the
/// inline call/task push sites. Callers should gate on
/// [`PushGateways::any_enabled`](crate::state::PushGateways::any_enabled) when they
/// want a true no-op while push is disabled (this fn still no-ops, but skips the
/// token lookup that way).
pub async fn push_to_participant(state: &AppState, recipient: ParticipantId, payload: &PushPayload) {
    // No registered devices ⇒ nothing to do (the common case for web-only users).
    let token_repo = PushTokenRepo::new(state.pg.clone());
    let tokens = match token_repo.list_for_participant(recipient).await {
        Ok(t) => t,
        Err(e) => {
            warn!(error = ?e, %recipient, "push token lookup failed");
            return;
        }
    };

    for t in tokens {
        let Some(gateway) = state.push.for_platform(&t.platform) else {
            debug!(platform = %t.platform, "no gateway configured for platform; skipping");
            continue;
        };
        match gateway.send(&t.token, payload).await {
            Ok(()) => debug!(%recipient, platform = %t.platform, "push delivered"),
            Err(PushError::Rejected(reason)) => {
                // Dead/unregistered token — drop it so we stop trying.
                warn!(%recipient, platform = %t.platform, %reason, "push token rejected; reaping");
                if let Err(e) = token_repo.unregister(recipient, &t.token).await {
                    warn!(error = ?e, "failed to reap rejected push token");
                }
            }
            Err(e) => warn!(%recipient, platform = %t.platform, error = %e, "push send failed"),
        }
    }
}

/// Build a silent, badge-only push: no `title`/`body` (so it does not wake the
/// screen with alert content — that is the gateway's concern), just the iOS
/// app-icon `badge` count. Use for content-less badge refreshes; FCM ignores the
/// badge (Android badges are client-driven), so this is effectively an APNs-only
/// signal.
#[must_use]
pub fn badge_only_payload(badge: u32) -> PushPayload {
    PushPayload {
        title: String::new(),
        body: String::new(),
        room_id: None,
        message_id: None,
        badge: Some(badge),
    }
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

    #[test]
    fn badge_only_payload_sets_badge_and_leaves_content_empty() {
        let p = badge_only_payload(7);
        assert_eq!(p.badge, Some(7));
        assert!(p.title.is_empty());
        assert!(p.body.is_empty());
        assert!(p.room_id.is_none());
        assert!(p.message_id.is_none());
    }

    #[test]
    fn badge_only_payload_apns_carries_badge_and_no_alert_content() {
        // The badge rides in `aps.badge`; the alert title/body are empty so no
        // textual content wakes the lock screen (the gateway decides silence).
        let json = aero_push::apns_payload_json(&badge_only_payload(3));
        assert_eq!(json["aps"]["badge"], 3);
        assert_eq!(json["aps"]["alert"]["title"], "");
        assert_eq!(json["aps"]["alert"]["body"], "");
    }
}
