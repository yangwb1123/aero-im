//! Bot event-subscription dispatcher (方向三 — 开放平台).
//!
//! User bots can create event subscriptions (`bot_event_subscriptions`, migration
//! 0142) via the storage [`BotRepo::subscribe`] surface — a `(event_type, filters,
//! webhook_url)` triple. Until now those rows sat inert: nothing consumed the bus
//! on their behalf, so a subscribed event was never delivered to the bot's
//! webhook. This listener closes that gap. It mirrors the built-in bus bots
//! (`unfurl_bot`, `transcribe_bot`, …): a resubscribe-on-reconnect loop over the
//! `im.room.*` subject that decodes each [`RoomEvent`], looks up the bot
//! subscriptions whose `event_type` matches, applies each subscription's optional
//! `{room_id, workspace_id, action_id}` filter, and POSTs the (signed) event to
//! every surviving subscription's `webhook_url`.
//!
//! ## Reused delivery pipeline
//!
//! Delivery reuses the existing outbound webhook seam from [`aero_storage`]: the
//! event is signed via [`build_delivery`] (HMAC over the exact JSON bytes, the
//! same `X-Aero-Signature` / `X-Aero-Timestamp` headers the room-scoped outgoing
//! hooks use) and sent through the [`WebhookSender`] trait ([`ReqwestSender`] in
//! production, a fake in tests). No new transport, signing, or HTTP code is
//! introduced here.
//!
//! Note the one structural difference from `webhooks::run_webhook_dispatcher`: the
//! durable *retry / DLQ* machinery (`webhook_delivery_log`) is FK-bound to
//! `outgoing_webhooks(id)` (migration 0085), so it cannot drive retries for a row
//! that lives in the *different* `bot_event_subscriptions` table. Bot-webhook
//! delivery is therefore one-shot (not retried) — like the other built-in bus
//! bots. But each attempt IS now recorded for observability: a dedicated
//! `bot_subscription_deliveries` log (migration 0147, FK-bound to
//! `bot_event_subscriptions(id)`) captures the outcome (`delivered`/`failed`),
//! HTTP status, and error of every send via [`BotRepo::record_delivery`]. That
//! record write is fail-open — a non-2xx or transport error is warned and the next
//! target attempted, and a logging error itself is swallowed; nothing aborts the
//! listener.
//!
//! ## Signing secret
//!
//! `bot_event_subscriptions` carries no per-subscription HMAC secret column, so the
//! delivery is signed with an empty secret. The signature header is still present
//! and deterministic over the body, preserving the wire shape; a future migration
//! adding a secret column would be a one-line change at the [`build_delivery`]
//! call below.
//!
//! ## Purity
//!
//! Event→type mapping, action-id extraction, and the filter predicate are pure and
//! unit-tested; the bus I/O + DB lookups are the only impure edges (integration-
//! tested behind the [`WebhookSender`] seam and `#[ignore]`-gated DB tests).

use std::sync::Arc;

use aero_common::RoomEvent;
use aero_common::{RoomId, WorkspaceId};
use aero_storage::{
    build_delivery, BotRepo, DeliveryStatus, MatchedSubscription, ReqwestSender, RoomRepo,
    WebhookSender,
};
use futures::StreamExt;
use tracing::{debug, info, warn};

use crate::state::AppState;

/// Durable consumer name — distinct from every other bus listener so the
/// dispatcher resumes from its own committed cursor and doesn't compete with the
/// WS / webhook / built-in-bot consumers for one shared cursor.
const CONSUMER: &str = "aero-bot-dispatch";

/// The canonical event-type string a bot subscribes to, matching the
/// `#[serde(tag = "kind", rename_all = "snake_case")]` discriminant on
/// [`RoomEvent`] (so it equals the value in the delivered JSON's `kind` field, and
/// the value `webhooks::event_kind` uses). Pure — unit-tested against the wire tag.
#[must_use]
fn event_type(event: &RoomEvent) -> &'static str {
    match event {
        RoomEvent::Message(_) => "message",
        RoomEvent::Edited(_) => "edited",
        RoomEvent::Deleted { .. } => "deleted",
        RoomEvent::Reaction { .. } => "reaction",
        RoomEvent::Read { .. } => "read",
        RoomEvent::Typing { .. } => "typing",
        RoomEvent::Notify { .. } => "notify",
        RoomEvent::NotifyBatch { .. } => "notify",
        RoomEvent::Pin { .. } => "pin",
        RoomEvent::Membership { .. } => "membership",
        RoomEvent::Poll { .. } => "poll",
        RoomEvent::MessageSeen { .. } => "message_seen",
        RoomEvent::Interaction { .. } => "interaction",
        RoomEvent::Call(_) => "call",
    }
}

/// The `action_id` an event carries, when it has one (only `Interaction` does
/// today). Used to honor a subscription's `{"action_id": "..."}` filter. Pure.
#[must_use]
fn event_action_id(event: &RoomEvent) -> Option<&str> {
    match event {
        RoomEvent::Interaction { action_id, .. } => Some(action_id.as_str()),
        _ => None,
    }
}

/// Whether one subscription's `filters` JSON matches an event, given the event's
/// resolved `room_id`, its `workspace_id` (looked up lazily — `None` when unknown /
/// not yet resolved), and its `action_id`.
///
/// Each present filter key is an AND-constraint: a `room_id`/`workspace_id`/
/// `action_id` in the filter must equal the event's. An absent key (or the empty
/// `{}` default) constrains nothing. A filter that names a dimension the event
/// lacks (e.g. `workspace_id` when the event's workspace is unknown, or `action_id`
/// on a non-interaction event) does NOT match — the subscriber asked to be narrowed
/// to something this event can't satisfy. A non-object `filters` value (malformed)
/// is treated as "no filter" so a bad row still delivers rather than silently
/// black-holing. Pure + total, so it's exhaustively unit-tested without a DB.
#[must_use]
fn filter_matches(
    filters: &serde_json::Value,
    room_id: RoomId,
    workspace_id: Option<WorkspaceId>,
    action_id: Option<&str>,
) -> bool {
    let Some(obj) = filters.as_object() else {
        // `{}` is an object (handled below); a non-object (null/array/scalar) is a
        // malformed filter — treat as unconstrained.
        return true;
    };

    if let Some(want) = obj.get("room_id").and_then(|v| v.as_str()) {
        if want != room_id.to_string() {
            return false;
        }
    }
    if let Some(want) = obj.get("workspace_id").and_then(|v| v.as_str()) {
        // The subscriber narrowed to a workspace; if we couldn't resolve the
        // event's workspace, we can't honor that, so it does not match.
        match workspace_id {
            Some(ws) if want == ws.to_string() => {}
            _ => return false,
        }
    }
    if let Some(want) = obj.get("action_id").and_then(|v| v.as_str()) {
        match action_id {
            Some(got) if want == got => {}
            _ => return false,
        }
    }
    true
}

/// Whether any of `subs` carries a `workspace_id` filter — i.e. whether we must pay
/// for the room→workspace lookup before evaluating the predicate. Pure; lets the
/// dispatcher skip the DB round-trip entirely on the common (room/action only) case.
#[must_use]
fn needs_workspace(subs: &[MatchedSubscription]) -> bool {
    subs.iter().any(|s| {
        s.filters
            .as_object()
            .is_some_and(|o| o.get("workspace_id").and_then(|v| v.as_str()).is_some())
    })
}

/// Run the bot-subscription dispatcher with the real reqwest transport, until the
/// process exits. The public entry point spawned at boot.
///
/// # Errors
/// Propagates only a fatal setup error; the steady-state loop never returns `Err`
/// (it resubscribes on bus failures, like the other built-in bots).
pub async fn run(state: AppState) -> anyhow::Result<()> {
    run_with(state, Arc::new(ReqwestSender::new())).await
}

/// Run the dispatcher with an injected [`WebhookSender`] — the seam tests drive
/// offline with a `FakeSender`. Real callers use [`run`].
pub async fn run_with(state: AppState, sender: Arc<dyn WebhookSender>) -> anyhow::Result<()> {
    let bus = state.bus.clone();
    let bots = BotRepo::new(state.pg.clone());
    let rooms = RoomRepo::new(state.pg.clone());
    // Resubscribe across NATS reconnects (mirrors `unfurl_bot` / the WS listener);
    // the durable consumer resumes from its committed cursor and every event is
    // acked, so nothing is re-delivered after a reconnect.
    loop {
        let mut stream = match bus.subscribe("im.room.*", Some(CONSUMER)).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "bot_dispatch subscribe failed; retrying");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        info!("bot_dispatch listener started");
        while let Some(sub) = stream.next().await {
            // Poison handling: a payload that isn't a RoomEvent is logged at debug
            // and acked (skipped), never retried — so a single bad frame can't hot-
            // spin the loop or wedge the cursor.
            match serde_json::from_slice::<RoomEvent>(sub.payload()) {
                Ok(event) => dispatch(&bots, &rooms, sender.as_ref(), &event).await,
                Err(e) => debug!(error = %e, "bot_dispatch: undecodable event payload skipped"),
            }
            // Broadcast-style consumer: always ack so the cursor advances regardless
            // of delivery outcome (delivery is best-effort, not a work queue).
            let _ = sub.ack().await;
        }
        warn!("bot_dispatch subscription stream ended; resubscribing");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// Fan one decoded event out to every matching bot subscription. No matching
/// subscription (or a roomless event, e.g. a directed call signal) is a no-op — the
/// sender is never touched. Best-effort throughout: a subscription lookup failure
/// is logged and the event is dropped; a per-target send failure is logged and the
/// next target is still attempted.
async fn dispatch<S: WebhookSender + ?Sized>(
    bots: &BotRepo,
    rooms: &RoomRepo,
    sender: &S,
    event: &RoomEvent,
) {
    // Roomless events (Answer/Ice/Offer/Roster call signals) carry no room to
    // filter on; bot subscriptions are room-scoped, so there's nothing to match.
    let Some(room) = event.room_id() else { return };
    let kind = event_type(event);

    let subs = match bots.subscriptions_for_event(kind).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = ?e, %room, kind, "bot_dispatch: subscription lookup failed");
            return;
        }
    };
    if subs.is_empty() {
        return;
    }

    // Resolve the event's workspace only if some subscription filters on it (the
    // common room/action-only case skips this DB round-trip entirely).
    let workspace = if needs_workspace(&subs) {
        match rooms.room_workspace(room).await {
            Ok(ws) => ws,
            Err(e) => {
                warn!(error = ?e, %room, "bot_dispatch: room_workspace lookup failed");
                None
            }
        }
    } else {
        None
    };
    let action_id = event_action_id(event);

    // Sign the SAME JSON bytes the receiver gets, so it can re-verify the signature
    // (empty secret — see the module note; no per-subscription secret column yet).
    let body = serde_json::to_value(event).unwrap_or(serde_json::Value::Null);
    let now = time::OffsetDateTime::now_utc().unix_timestamp();

    for sub in subs {
        if !filter_matches(&sub.filters, room, workspace, action_id) {
            continue;
        }
        let delivery = build_delivery(&sub.webhook_url, "", &body, now);
        // Map the round-trip to a (status, http_status, error) triple — both for
        // structured logging AND for the durable delivery log (migration 0147), so
        // a previously-invisible bot delivery is now observable. A 2xx is
        // `delivered`; a non-2xx or a transport-level error is `failed` (the latter
        // with no http_status). The classification lives in `DeliveryStatus` so the
        // log and the logs agree.
        let (status, http_status, error): (DeliveryStatus, Option<u16>, Option<String>) =
            match sender.deliver(&delivery).await {
                Ok(resp) if (200..300).contains(&resp.status) => {
                    debug!(%room, kind, bot = %sub.bot_id, sub = %sub.id, status = resp.status, "bot_dispatch delivered");
                    (DeliveryStatus::Delivered, Some(resp.status), None)
                }
                Ok(resp) => {
                    warn!(%room, kind, bot = %sub.bot_id, url = %sub.webhook_url, status = resp.status, "bot_dispatch non-2xx");
                    (DeliveryStatus::Failed, Some(resp.status), Some(format!("non-2xx: {}", resp.status)))
                }
                Err(e) => {
                    warn!(%room, kind, bot = %sub.bot_id, url = %sub.webhook_url, error = %e, "bot_dispatch delivery failed");
                    (DeliveryStatus::Failed, None, Some(e))
                }
            };
        // Fail-open: a logging failure must never abort delivery to the remaining
        // subscriptions, so a `record_delivery` error is warned and swallowed.
        if let Err(e) = bots
            .record_delivery(sub.id, sub.bot_id, kind, status, http_status, error.as_deref())
            .await
        {
            warn!(%room, kind, bot = %sub.bot_id, sub = %sub.id, error = ?e, "bot_dispatch: delivery-log write failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::{
        Block, CallEvent, CallId, Message, MessageEnvelope, MessageId, ParticipantId,
    };

    fn message_event(room: RoomId) -> RoomEvent {
        let msg = Message {
            id: MessageId::new(),
            room_id: room,
            sender_id: ParticipantId::new(),
            blocks: vec![Block::text("hi")],
            reply_to: None,
            metadata: serde_json::Value::Null,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            edited_at: None,
            deleted_at: None,
            expires_at: None,
        };
        RoomEvent::Message(MessageEnvelope { message: msg, recipients: Vec::new() })
    }

    // ----- event_type -----

    #[test]
    fn event_type_matches_wire_discriminant() {
        let room = RoomId::new();
        let ev = message_event(room);
        assert_eq!(event_type(&ev), "message");
        // The kind string equals the serde tag emitted on the wire — the exact
        // value a bot stores in `bot_event_subscriptions.event_type`.
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["kind"], event_type(&ev));

        assert_eq!(
            event_type(&RoomEvent::Interaction {
                room_id: room,
                message_id: MessageId::new(),
                participant: ParticipantId::new(),
                action_id: "approve".into(),
            }),
            "interaction"
        );
        assert_eq!(
            event_type(&RoomEvent::Typing {
                room_id: room,
                participant: ParticipantId::new(),
                on: true,
            }),
            "typing"
        );
    }

    // ----- event_action_id -----

    #[test]
    fn action_id_only_for_interaction() {
        let room = RoomId::new();
        let interaction = RoomEvent::Interaction {
            room_id: room,
            message_id: MessageId::new(),
            participant: ParticipantId::new(),
            action_id: "btn_approve".into(),
        };
        assert_eq!(event_action_id(&interaction), Some("btn_approve"));
        assert_eq!(event_action_id(&message_event(room)), None);
    }

    // ----- filter_matches -----

    #[test]
    fn empty_filter_matches_everything() {
        let room = RoomId::new();
        // Both the explicit `{}` (column default) and a malformed non-object are
        // treated as "no constraint".
        assert!(filter_matches(&serde_json::json!({}), room, None, None));
        assert!(filter_matches(&serde_json::Value::Null, room, None, None));
        assert!(filter_matches(&serde_json::json!("garbage"), room, None, None));
    }

    #[test]
    fn room_filter_matches_only_its_room() {
        let room = RoomId::new();
        let other = RoomId::new();
        let f = serde_json::json!({ "room_id": room.to_string() });
        assert!(filter_matches(&f, room, None, None), "same room matches");
        assert!(!filter_matches(&f, other, None, None), "different room excluded");
    }

    #[test]
    fn workspace_filter_requires_resolved_matching_workspace() {
        let room = RoomId::new();
        let ws = WorkspaceId::new();
        let other_ws = WorkspaceId::new();
        let f = serde_json::json!({ "workspace_id": ws.to_string() });
        assert!(filter_matches(&f, room, Some(ws), None), "matching ws delivers");
        assert!(!filter_matches(&f, room, Some(other_ws), None), "other ws excluded");
        // A workspace filter the dispatcher couldn't resolve must NOT match (we
        // can't prove the constraint holds).
        assert!(!filter_matches(&f, room, None, None), "unresolved ws excluded");
    }

    #[test]
    fn action_id_filter_gates_interactions() {
        let room = RoomId::new();
        let f = serde_json::json!({ "action_id": "approve" });
        assert!(filter_matches(&f, room, None, Some("approve")), "matching action delivers");
        assert!(!filter_matches(&f, room, None, Some("reject")), "other action excluded");
        // No action on the event (non-interaction) can't satisfy an action filter.
        assert!(!filter_matches(&f, room, None, None), "missing action excluded");
    }

    #[test]
    fn multiple_filter_keys_are_anded() {
        let room = RoomId::new();
        let ws = WorkspaceId::new();
        let f = serde_json::json!({
            "room_id": room.to_string(),
            "workspace_id": ws.to_string(),
            "action_id": "go",
        });
        // All three satisfied → match.
        assert!(filter_matches(&f, room, Some(ws), Some("go")));
        // Any one violated → no match.
        assert!(!filter_matches(&f, RoomId::new(), Some(ws), Some("go")), "wrong room");
        assert!(!filter_matches(&f, room, Some(WorkspaceId::new()), Some("go")), "wrong ws");
        assert!(!filter_matches(&f, room, Some(ws), Some("no")), "wrong action");
    }

    // ----- needs_workspace -----

    #[test]
    fn needs_workspace_only_when_a_sub_filters_on_it() {
        let bot = ParticipantId::new();
        let mk = |filters: serde_json::Value| MatchedSubscription {
            id: uuid::Uuid::new_v4(),
            bot_id: bot,
            webhook_url: "https://example.test/hook".into(),
            filters,
        };
        // Room-only / empty filters → no workspace lookup required.
        assert!(!needs_workspace(&[mk(serde_json::json!({}))]));
        assert!(!needs_workspace(&[mk(serde_json::json!({ "room_id": "r" }))]));
        // Any workspace_id filter → lookup required.
        assert!(needs_workspace(&[
            mk(serde_json::json!({ "room_id": "r" })),
            mk(serde_json::json!({ "workspace_id": "w" })),
        ]));
    }

    // ----- dispatch (no DB / no match short-circuits, behind the sender seam) -----

    #[tokio::test]
    async fn roomless_event_never_touches_the_sender() {
        // A roomless call signal (Answer) has no room to scope subscriptions to, so
        // `dispatch` must early-return before any DB query or send — exercised here
        // with lazily-connected repos that would error if queried.
        use aero_storage::FakeSender;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://u:p@localhost/db")
            .unwrap();
        let bots = BotRepo::new(pool.clone());
        let rooms = RoomRepo::new(pool);
        let sender = FakeSender::new(200);
        let answer = RoomEvent::Call(CallEvent::Answer {
            call_id: CallId::new(),
            from: ParticipantId::new(),
            to: ParticipantId::new(),
            sdp: String::new(),
        });
        dispatch(&bots, &rooms, &sender, &answer).await;
        assert!(sender.calls().is_empty(), "no room ⇒ no lookup, no delivery");
    }
}
