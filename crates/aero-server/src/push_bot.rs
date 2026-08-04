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

use std::str::FromStr;

use aero_common::{NotificationKind, ParticipantId, RoomEvent, RoomId};
use aero_push::{PushError, PushPayload};
use aero_storage::{ConsumerEventReceiptRepo, PushTokenRepo};
use futures::{stream, StreamExt as _};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    state::AppState,
    task_shutdown::{self, NextOrCancelled},
};

/// Maximum characters of the message body included in the push preview. Keeps
/// the notification compact and avoids leaking long content into the lock screen.
const PREVIEW_CHARS: usize = 140;
/// Bound recipient fan-out for one `NotifyBatch` event.
const EVENT_RECIPIENT_CONCURRENCY: usize = 8;
/// Bound upstream provider calls for one recipient.
const TOKEN_DELIVERY_CONCURRENCY: usize = 4;

/// Run the push-dispatch listener until the bus stream ends. Mirrors
/// [`crate::ooo_bot::run`]'s signature so the server binary spawns it the same way.
///
/// # Errors
/// Returns an error if subscribing to the event bus fails.
pub async fn run(state: AppState) -> anyhow::Result<()> {
    run_until_cancelled(state, CancellationToken::new()).await
}

/// Run until `cancel` is triggered, finishing and `ACK`ing any event already
/// received before returning.
pub async fn run_until_cancelled(state: AppState, cancel: CancellationToken) -> anyhow::Result<()> {
    let bus = state.bus.clone();
    let receipts = ConsumerEventReceiptRepo::new(state.pg.clone());
    // Resubscribe across NATS reconnects (mirrors `ws::run_bus_listener`); durable
    // consumer "aero-push" resumes from its cursor, every message is acked.
    loop {
        let subscribed =
            task_shutdown::subscribe_or_cancelled(&bus, "im.room.*", Some("aero-push"), &cancel)
                .await;
        let mut stream = match subscribed {
            None => return Ok(()),
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                warn!(error = %e, "push_bot subscribe failed; retrying");
                if task_shutdown::delay_or_cancelled(std::time::Duration::from_secs(1), &cancel)
                    .await
                {
                    return Ok(());
                }
                continue;
            }
        };
        info!("push_bot listener started");
        loop {
            let sub = match task_shutdown::next_or_cancelled(&mut stream, &cancel).await {
                NextOrCancelled::Item(sub) => sub,
                NextOrCancelled::Ended => break,
                NextOrCancelled::Cancelled => return Ok(()),
            };
            let event = serde_json::from_slice::<RoomEvent>(sub.payload());
            let handler_state = &state;
            let _ = crate::consumer_event_receipt::process(
                &receipts,
                "aero-push",
                sub,
                || async move {
                    match event {
                        Ok(RoomEvent::Notify {
                            message_id,
                            mentioned,
                            by,
                            kind,
                            ..
                        }) => handle(handler_state, message_id, mentioned, by, kind).await,
                        // Batched notify: process every recipient before marking
                        // the one source event complete.
                        Ok(RoomEvent::NotifyBatch {
                            message_id,
                            by,
                            recipients,
                            ..
                        }) => {
                            let results = stream::iter(recipients)
                                .map(|target| {
                                    handle(
                                        handler_state,
                                        message_id,
                                        target.participant,
                                        by,
                                        target.kind,
                                    )
                                })
                                .buffer_unordered(EVENT_RECIPIENT_CONCURRENCY)
                                .collect::<Vec<_>>()
                                .await;
                            // Finish every bounded in-flight recipient before
                            // settling the source receipt, then surface the first
                            // lookup error so the whole event remains retryable.
                            for result in results {
                                result?;
                            }
                            Ok(())
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
        warn!("push_bot subscription stream ended; resubscribing");
        if task_shutdown::delay_or_cancelled(std::time::Duration::from_secs(1), &cancel).await {
            return Ok(());
        }
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
    let body = msg
        .as_ref()
        .map(|m| preview(&m.searchable_text()))
        .unwrap_or_default();
    let room_id = msg.as_ref().map(|m| m.room_id.to_string());
    let title = match kind {
        NotificationKind::Mention => format!("{sender_name} mentioned you"),
        NotificationKind::Reply => format!("{sender_name} replied"),
        NotificationKind::Reaction => format!("{sender_name} reacted to your message"),
        NotificationKind::SavedSearch => "New match for your saved search".to_string(),
        NotificationKind::AggregateReply => format!("{sender_name} replied (batched)"),
    };

    // Coalesce by room so multiple messages to the same conversation replace one
    // another on the lock screen (FCM `android.collapse_key` / APNs
    // `apns-collapse-id`) instead of stacking N separate entries. Derived from the
    // room id, which is well under the APNs 64-byte cap.
    let collapse_key = room_id.as_deref().map(collapse_key_for_room);

    let payload = PushPayload {
        title,
        body,
        room_id,
        message_id: Some(message_id.to_string()),
        badge: None,
        collapse_key,
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
pub async fn push_to_participant(
    state: &AppState,
    recipient: ParticipantId,
    payload: &PushPayload,
) {
    // Room-scoped pushes carry message/call/task content outside the authenticated
    // app. Re-check the canonical access guard at the final external-send edge so
    // a queued notification cannot leak after the recipient was removed,
    // workspace-deactivated, or fell behind an enforced 2FA policy.
    if let Some(raw_room) = payload.room_id.as_deref() {
        let room = match RoomId::from_str(raw_room) {
            Ok(room) => room,
            Err(error) => {
                warn!(%recipient, %raw_room, %error, "push has malformed room id; skipping");
                return;
            }
        };
        if let Err(error) = state.im.assert_room_access(recipient, room).await {
            debug!(%recipient, %room, %error, "push recipient no longer has room access; skipping");
            return;
        }
    }

    // No registered devices ⇒ nothing to do (the common case for web-only users).
    let token_repo = PushTokenRepo::new(state.pg.clone());
    let tokens = match token_repo.list_for_participant(recipient).await {
        Ok(t) => t,
        Err(e) => {
            warn!(error = ?e, %recipient, "push token lookup failed");
            return;
        }
    };

    stream::iter(tokens)
        .for_each_concurrent(Some(TOKEN_DELIVERY_CONCURRENCY), |token| {
            let gateway = state.push.for_platform(&token.platform).cloned();
            let token_repo = token_repo.clone();
            async move {
                let Some(gateway) = gateway else {
                    debug!(platform = %token.platform, "no gateway configured for platform; skipping");
                    return;
                };
                match gateway.send(&token.token, payload).await {
                    Ok(()) => debug!(%recipient, platform = %token.platform, "push delivered"),
                    Err(PushError::Rejected(reason)) => {
                        // Dead/unregistered token — drop it so we stop trying.
                        warn!(
                            %recipient,
                            platform = %token.platform,
                            %reason,
                            "push token rejected; reaping"
                        );
                        if let Err(e) = token_repo.unregister(recipient, &token.token).await {
                            warn!(error = ?e, "failed to reap rejected push token");
                        }
                    }
                    Err(e) => {
                        warn!(%recipient, platform = %token.platform, error = %e, "push send failed");
                    }
                }
            }
        })
        .await;
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
        // Badge refreshes are not room-scoped, so they do not coalesce.
        collapse_key: None,
    }
}

/// Derive the stable per-room coalescing key used for both FCM `android.collapse_key`
/// and APNs `apns-collapse-id`. Same room ⇒ same key ⇒ the OS replaces an older
/// undelivered notification for that conversation instead of stacking a new one.
#[must_use]
fn collapse_key_for_room(room_id: &str) -> String {
    format!("room:{room_id}")
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
    fn collapse_key_is_room_scoped_and_stable() {
        // Same room ⇒ identical key (so notifications coalesce); different rooms differ.
        assert_eq!(collapse_key_for_room("abc"), "room:abc");
        assert_eq!(collapse_key_for_room("abc"), collapse_key_for_room("abc"));
        assert_ne!(collapse_key_for_room("abc"), collapse_key_for_room("xyz"));
    }

    #[test]
    fn room_collapse_key_drives_fcm_and_apns_coalescing() {
        // The derived key flows into the FCM body and the APNs collapse-id helper,
        // so both platforms coalesce per-room.
        let payload = PushPayload {
            collapse_key: Some(collapse_key_for_room("r5")),
            ..badge_only_payload(0)
        };
        let fcm = aero_push::fcm_message_json("tok", &payload);
        assert_eq!(fcm["message"]["android"]["collapse_key"], "room:r5");
        assert_eq!(
            aero_push::apns_collapse_id(&payload).as_deref(),
            Some("room:r5")
        );
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
